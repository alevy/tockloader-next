# M0 spike: findings

Status: **in progress.** The software side of M0 is done and tested without
hardware. Checking against real nRF52840dk and RP2350 boards (the M0 exit
criterion) has **not** been done yet; see [What's left](#whats-left).

## What was built

| Crate | Role | I/O? | WASM/WASI |
|---|---|---|---|
| `tbf` | Parse and serialize TBF v2 headers (unknown TLVs kept byte for byte) and read TABs | none | builds |
| `tock-image` | Pure core: `discover` the app list, `plan` installs and uninstalls, `render` the plan into page writes | none | builds |
| `tock-board` | Board files (with `extends`), `Tock.toml`, user config, and `resolve` with a recorded source for each value | none (via `FileSource` trait) | builds |
| `tock-proto` | Draft device protocol: messages, capabilities, framing, in-process and stream carriers | `std::io` traits only | builds |
| `tock-cli` | The `tock` binary: config, porcelain, broker, external subcommands, built-in `file` transport, and a `tock-transport-file-plugin` example plugin | yes | — |

The two reference boards are described in `boards/nrf52840dk.toml` and
`boards/raspberry_pi_pico_2.toml`.

### The core loop

The pure crates never touch the device. Instead, when they need bytes they
don't have, they **ask for them**:

```
discover(region, known_flash) -> Ok(apps) | NeedRead(pages)
plan(region, placement, apps, request) -> Plan        (headers only)
render(region, plan, known_flash) -> Ok(page_writes) | NeedRead(pages)
```

The CLI loops: read the requested pages through the transport, then call the
function again. This gives three things:

- **Reads are minimal by default.** Finding the apps reads one page per app
  header plus one page for the end of the list, not the whole region. An app's
  body is only read when it has to move.
- **Reading everything is just a policy.** The caller can prefill the whole
  app region (the PRD's "pull the entire app region"), e.g. for transports
  where one big read is cheaper.
- **Writes are diffed.** A page whose new contents match what's known to be on
  the device is not written. Reinstalling an identical app writes **0 pages**;
  uninstalling an app writes **1 page** (its header becomes padding).

## How it was validated

`cargo test` runs 37 tests:

- **Planner scenarios** (`tock-image/tests/planner.rs`, 14 tests):
  - Installing on an empty board, largest app first, with MPU alignment
  - Installed apps are never moved by default
  - An uninstalled app's slot is reused
  - Repack is opt-in
  - Sticky apps are protected
  - Fixed-address apps get a matching build, and can be mixed with
    position-independent apps
  - Discovery reads 3 pages for 2 apps
  - An identical reinstall writes nothing
- **Config resolution** (6 tests): precedence, device aliases, `extends`
  overriding one field, layout validation, and no guessing.
- **CLI end to end** (9 tests):
  - Project-file driven install
  - JSON output and `--dry-run`
  - Error messages that include a command to run
  - The plugin transport produces exactly the same image as the built-in one
  - `tock broker` driven over stdio by a client
- **Differential tests against Python tockloader** (run when `TOCKLOADER_PY`
  points at it):
  - Installing 3 apps, uninstalling the middle one, and installing a larger
    app with `--allow-repack` all produce **byte-identical flash images**. The
    only exception is the end-of-list marker (see finding 1).
  - Fixed-address apps land at the same addresses.

The core crates build for `wasm32-unknown-unknown` and `wasm32-wasip1`.

## Findings

These are behaviors of the current Python tool, found while matching its
layout. They're worth confirming with Brad and the kernel team.

1. **Python's end-of-list marker depends on the transport.** The base
   `clear_bytes` does nothing, and the flash-file backend writes a single
   `0x00`. So on some transports, removing the last app (or shrinking the
   list) can leave a valid stale header right after the new end of the list,
   and the kernel would follow it. The new tool always writes an explicit
   8-byte erased marker at the end of the list.
2. **Python treats any FixedAddresses TLV as a fixed address**, even when the
   flash value is `0xFFFFFFFF`. The kernel (`tock-tbf`) treats that value as
   "not fixed". The new tool follows the kernel.
3. **Python doesn't update `binary_end_offset` when it grows a fixed-address
   app's protected region** to line the binary up with its address. So the
   kernel's view of where the binary ends, and where the footers start, is
   off by the amount of growth. The new tool moves `binary_end_offset` too.
   The kernel team should confirm this is the intended semantics.
4. **Python rebuilds footers from the TLVs it parsed** instead of copying
   bytes. Bytes in the footer area that don't parse as TLVs are dropped, and
   stale flash bytes are left in their place. That's harmless for elf2tab
   output, but it's a difference: the new tool copies TBF bytes exactly.
5. **The Pico 2 has no app address in Python's tables.** It falls back to the
   global default `0x30000`, which is the wrong address for that board, so
   users need `--app-address 0x10040000`. The board file fixes this.
6. **Python's `nrf52dk` entry also covers the nRF52840 DK**, and detection maps
   several J-Link descriptions to it. This is an example of the hidden board
   knowledge the PRD wants to move into board files.
7. **Python refuses to mix fixed-address and position-independent apps.** The
   new planner places both kinds into gaps without special cases.
8. **Python limits its fixed-address search with a hard-coded "no board has
   more than 2 MB of flash" cutoff.** The new planner uses the board's app
   region bounds instead.

## Open questions from the PRD (§14)

**OQ-1: imperative vs. declarative.** Recommendation: imperative commands on
top of one planner (built), with two modes:
- **Stable (default):** apps you didn't name never move. New and replaced apps
  go into gaps or after the last app. If an app only fits by moving others,
  the command fails and says to add `--allow-repack`.
- **`--allow-repack`:** Python's full layout, verified byte-identical against
  Python.

`tock install` with no arguments installs the `[[app]]` list from
`Tock.toml`. The missing declarative piece is an `--exact` mode that also
removes apps not listed. It's small to add, but deliberately left out until
we've seen how people use the project file.

**OQ-2: porcelain compiled in, or separate binaries?** Recommendation:
compiled in. External subcommands reach the device through `tock broker`
with exactly the same capabilities, so separate binaries would add
distribution cost without adding flexibility.

**OQ-3: OpenOCD vs. probe-rs.** Not decided. This needs hardware. Leaning
toward **probe-rs built in as a library**: it's Rust, it uses `nusb` for USB
(pure Rust, so static binaries are possible), and it supports both reference
boards. OpenOCD would become a plugin, because it's a separate executable
users install themselves.

**OQ-4: session lifecycle.** One connection per command, for porcelain.
`tock broker` gives external subcommands a session that lasts as long as they
keep it open. No daemon is needed.

**OQ-5: tock-bootloader as a `memory` transport or an `apps` transport?** Not
explored yet. It's next, together with the serial transport itself.

**OQ-10: should the board-file schema be shared with the kernel repo?** The
schema is the `tock-board` crate. The Tock repo could validate its board
files with it in CI.

## Draft protocol (v0)

- **Frame:** `u32 LE json_len | json | u32 LE payload_len | payload`. Bulk data
  goes in the payload, never in JSON.
- **Requests:**
  - Connection: `hello`, `open` (resolved board, flash geometry, transport
    options), `close`
  - Memory: `read`, `write` (whole pages), `hash`
  - App-level: `apps_list`, `apps_install`, `apps_remove`
  - Other: `reset`, `vendor` (namespaced pass-through)
- **Capabilities:** `memory_read`, `memory_write`, `memory_hash`, `apps`,
  `reset`, `console`, `{"vendor": "..."}`.
- **Errors:** carry a `kind` (`unsupported`, `selection`, `device`,
  `invalid_request`) and an optional `suggestion` that the CLI turns into a
  "try:" line.
- **Carriers:**
  - `StreamChannel` frames over any byte stream (plugins over stdio, and
    `tock broker`).
  - `LocalChannel` calls a handler in-process (built-in transports, and WASM
    hosts later).

## What's left

The M0 exit criteria aren't met yet.

- **Real transports:** tock-bootloader (serial) and probe-rs. Python's
  `bootloader_serial.py` is the reference for the bootloader protocol.
- **Hardware checks on the nRF52840dk and RP2350,** using real flash dumps and
  real elf2tab TABs, including footers and credentials.
- **The `apps` capability path** (dynamic loading) is defined in the protocol
  but not wired into the porcelain.
- **`memory_hash`,** so slow transports can skip reading pages to compare.
- **Kernel attributes** (neither reference board has an attribute table).
- **`--opt` values** aren't passed to external subcommands yet (`--board`,
  `--transport` and `--device` are).
