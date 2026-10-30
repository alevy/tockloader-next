# PRD: Next-Generation Tock Loader (Rust)

| | |
|---|---|
| **Status** | Draft v0.1 |
| **Author** | Amit Levy |
| **Date** | 2026-10-30 |
| **Target repo** | `tock/tockloader-rs` (fresh start; replaces current contents) |
| **Working name** | `tock` (placeholder; final name TBD, see [Open Questions](#14-open-questions)) |
| **Source** | Requirements Q&A: [`requirements-qa.md`](./requirements-qa.md) |

---

## 1. Summary

We will build a new Tock loader in Rust to replace the Python `tockloader`. This is a **redesign, not a port**. Today's tool grew one feature at a time into a large monolith: about 14k lines and about 50 subcommands. Board knowledge is hard-coded, board detection is fragile, and the behavior is full of implicit "magic".

The new tool is a **small, explicit core with a git/cargo-style ecosystem of subcommands and pluggable transports**. Its central ideas are:

1. **Board knowledge lives outside the tool.** Declarative board files live with the kernel or downstream board, not in the loader.
2. **Nothing is guessed.** Selection is explicit and deterministic. Configuration can be explained, and the tool never prompts.
3. **Image-centric operation.** All layout decisions are made by a *pure planning engine* over a local image. The device is just a source and sink of bytes, or of app-level operations.
4. **Capability-tiered, extensible transports.** The core ships only a few highly portable transports, and everything else is a plugin. The design is built so that **dynamic app loading** into a running kernel slots in without changing the core.

## 2. Background and problem statement

The current `tockloader` (Python) problems, in order of user impact:

| # | Problem | Consequence |
|---|---|---|
| P1 | **Fragile board detection.** It guesses from serial port descriptions and by probing, then merges settings from CLI flags, kernel attributes and a hard-coded `KNOWN_BOARDS` table (~32 boards). | It's unclear which settings are actually in effect, and it fails in surprising ways when several boards are attached. |
| P2 | **Out-of-tree boards and layout variants are hard.** A new board, or a small layout change, means editing tockloader itself. | Downstream and commercial users have to fork it or carry flag-heavy invocations. |
| P3 | **Hard to maintain.** Concerns are tangled together (e.g. the 1.9k-line `tockloader.py` and 1.6k-line `bootloader_serial.py`), so it's very hard to change one thing on its own. | Features like dynamic app loading are effectively blocked. |
| P4 | **Python install friction**, especially on NixOS and in managed environments. | Onboarding problems in tutorials and courses. |
| P5 | **Complex, obscure UX** with too much magic: apps are silently reordered and padded, prompts are interactive, there are many overlapping commands. | Users don't trust what the tool will do to their board. |

## 3. Goals and non-goals

### 3.1 Goals

- **G1:** Make installing and managing apps on a board simple, predictable and explainable for app developers.
- **G2:** Support new boards, including out-of-tree boards and layout variants, with **zero changes to the tool**.
- **G3:** Keep the core small and maintainable. New functionality defaults to a subcommand or plugin.
- **G4:** Make transports extensible, including restricted ones such as secure provisioning and OTA, and have the architecture **make dynamic app loading easy**.
- **G5:** Make installation frictionless: static binaries where possible, `cargo install`, and a Nix flake.
- **G6:** Be scriptable and embeddable: stable machine-readable output, and core crates that are portable to WASM/WASI.
- **G7:** Work with Tock as it is today (existing kernels, TBF format, attributes), while making it easy to evolve those conventions later.

### 3.2 Non-goals

- A GUI.
- Flashing non-Tock firmware, or acting as a general-purpose flash tool.
- A general debugger front end (GDB server management, RTT, etc.).
- Managing toolchains (installing compilers, OpenOCD, vendor SDKs).
- Changing the TBF format.
- A plugin registry or `plugin install` mechanism (v1 discovers plugins on `PATH`).
- A plugin system that runs inside WASM (see [§7.7](#77-portability-wasm--wasi)).
- Feature parity with every Python `tockloader` command (see [Appendix A](#appendix-a-disposition-of-current-tockloader-commands)).

## 4. Users

In priority order:

| Priority | User | Needs |
|---|---|---|
| 1 | **App developers** (libtock-c / libtock-rs) installing and managing apps on a board that already runs a kernel, often flashed by someone else | One-command install, uninstall and list. No kernel source checkout required. |
| 2 | **Production and deployment** (e.g. OxidOS, Microsoft Pluton, product teams) | Their own signing, provisioning and deployment flows, which differ by organization, plus a sensible default. |
| 3 | **Newcomers and students** (tutorials, courses) | Things "just work" from a checked-in project file. Errors say exactly what to do next. |
| 4 | **Tools using it programmatically** (in-browser IDEs, build systems, vendor tools) | Stable JSON output, an embeddable protocol, and portable core crates. |
| 5 | **CI and hardware-in-the-loop** (a special case of 4) | Non-interactive, deterministic, multi-board-safe, WASI-runnable core logic. |
| 6 | **Kernel developers** flashing kernels many times a day | Fast full-image flashing. |

## 5. Product principles

These are binding design rules. Reviews should cite them.

1. **Explicit over magic.** The tool never guesses the board, the device or the layout. Anything it decides can be shown with `--explain` or `config show`.
2. **Never prompt.** The tool is always non-interactive. When something is ambiguous, it errors out and **suggests the exact command(s) to run**.
3. **Plan, then apply.** Every mutation is computed as a plan over a local image before any bytes move. The plan can be shown (`--dry-run`) and is deterministic.
4. **Small core, rich ecosystem.** The default answer to "should X be in the core?" is "no, make it a subcommand."
5. **No board knowledge in the tool.** Board facts come from board files. A bundled snapshot is only a convenience copy, generated rather than hand-maintained.
6. **Don't touch what you weren't asked to touch.** Apps and regions the user didn't mention are never moved, unless the plan says so and the user explicitly allowed it.
7. **Portable core.** The planning, TBF and board-model crates do no I/O directly.

## 6. Key user stories

- **US1 (app developer):** "I have a board running Tock and `blink.tab`. I run `tock install blink.tab` in my project directory and it installs. Other apps already on the board are kept."
- **US2 (student):** "I cloned the tutorial repo, which has a `Tock.toml`. I plugged in my board and ran `tock install`. When I had two boards plugged in, it told me exactly which flag to add."
- **US3 (out-of-tree board):** "My company's board differs from the nRF52840dk only in its app start address. I copied the board file, changed one value and pointed `Tock.toml` at it. I didn't touch the tool."
- **US4 (production):** "We sign and provision with our own HSM-based flow. We ship `tock-acme-provision` as a subcommand, and it uses the same board resolution and device connection as everything else."
- **US5 (dynamic loading):** "My board runs a kernel with dynamic loading. `tock install app.tab` installs it over UART into the running kernel. The core didn't change to support this; it's just a transport with the `apps` capability."
- **US6 (in-browser IDE):** "Our web IDE compiles the planning and TBF crates to WASM and adds its own WebSerial transport."
- **US7 (CI):** "My HIL runner calls `tock install --format json --board-file ... --device serial=ABC123` and parses the result."
- **US8 (kernel developer):** "`tock flash` builds a full image from the kernel plus the apps listed in `Tock.toml` and flashes it."

## 7. Architecture overview

```
                     ┌───────────────────────────────────────────────┐
  user / scripts ───►│  tock  (core binary)                          │
                     │  • arg parsing & subcommand dispatch          │
                     │  • config resolution (flags/env/project/user) │
                     │  • board-file loading                         │
                     │  • broker: device protocol router             │
                     │  • built-in porcelain: install/uninstall/list │
                     └──────┬───────────────────────────┬────────────┘
             device protocol│(stdio, versioned)          │device protocol
                ┌───────────▼──────────┐       ┌─────────▼──────────────┐
                │ external subcommands │       │ transports             │
                │ tock-tab (elf2tab)   │       │ built-in: tock-bootldr,│
                │ tock-kv  (TicKV)     │       │   openocd and/or       │
                │ tock-listen          │       │   probe-rs             │
                │ tock-<vendor>-…      │       │ plugins: jlink, nrfutil│
                └──────────────────────┘       │   dynamic-load, …      │
                                               └────────────────────────┘
  Shared, portable crates (no direct I/O; WASM/WASI-clean):
    tock-board (board/project model) · tbf (TBF parse/serialize, TAB) ·
    tock-image (image model + pure planning engine) · tock-proto (protocol types)
```

### 7.1 Core binary

- Resolves the **context**: board file, project file, user config, transport and device selector.
- Dispatches subcommands. `tock foo` looks for a built-in first, then `tock-foo` on `PATH` (cargo-style).
- Acts as the **broker**. It is the single owner of the device connection. Subcommands send requests through the core, and the core routes them to the selected transport.
- Ships the built-in **porcelain** for the default workflows: `install`, `uninstall`, `list`, `flash`, `config show`. Whether porcelain is compiled in or shipped as bundled binaries is an implementation choice (see open questions). The interface is the same either way.

### 7.2 Extension contract

v1 leans toward the **broker model**: the core owns the device connection, and subcommands use it through a protocol. Context resolution is also exported, so subcommands get a consistent view (env vars plus a resolved-context document).

- **Protocol:** versioned, with capability negotiation. Control messages are structured; bulk data uses a framed binary path, not JSON-encoded bytes.
- **Carrier-agnostic:** the protocol's message layer is defined separately from its carrier (child-process stdio in v1). That way, in-process or Web Worker carriers can be added later (see [§7.7](#77-portability-wasm--wasi)).
- **Escape hatch:** transports may expose namespaced vendor-specific operations, which the broker passes through unchanged, so extensions aren't blocked waiting for the core to add operations.

**Known costs of the broker model (must be addressed in the M0 design):**
- The protocol becomes a stable public API and needs versioning discipline.
- An extension may need an operation the protocol doesn't expose (mitigated by the vendor pass-through).
- Bulk throughput needs a binary path.
- Session lifecycle: reconnecting per call can be slow or reset targets, so we must decide whether there's a session or daemon mode.
- Debugging across three processes needs good tracing: a `--trace-protocol` flag that logs every message.

### 7.3 Transports and capabilities

Transports are extensible. Each one declares **capabilities**, and the core picks a strategy from them:

| Capability | Operations (illustrative) | Example transports |
|---|---|---|
| `memory.read` / `memory.write` / `memory.erase` | Byte or page-granular access | OpenOCD, probe-rs, tock-bootloader, J-Link |
| `memory.hash` / `memory.compare` (optional) | Hash or compare ranges without transferring them | tock-bootloader (proposed), smart probes, network transports |
| `apps` | `list`, `install(tab)`, `remove(id)` | Dynamic loading into a running kernel, secure provisioning, OTA |
| `reset` | Reset or run the target | Most probes |
| `attributes` (optional) | Read and write kernel attributes | tock-bootloader |
| `console` (optional) | Byte stream for a serial console | tock-bootloader serial, UART transports |

Rules:
- Commands state the capabilities they require. A missing capability produces a clear error that names the capability, and suggests a transport that has it when one is known.
- When a transport has `apps`, `install`, `uninstall` and `list` delegate to it. Otherwise they use the memory path ([§7.5](#75-app-install-on-static-boards)).
- Transports **may implement diff-aware writes**: given the new image and the known prior contents or hashes, they move only what changed. This is important for slow or low-bandwidth links.

**Built-in transports in v1:** tock-bootloader (serial), plus **one or both** of OpenOCD and probe-rs (decided in M0). nrfutil, JLinkExe and the like ship as external transport plugins.

### 7.4 Board files and configuration

**Board file** (`board.toml`, illustrative): one file in the common case, with **layout** separate from **connection** methods.

```toml
[board]
name = "nrf52840dk"
description = "Nordic nRF52840 DK"
arch = "cortex-m4"

[layout]
flash = { start = 0x00000000, size = "1M", page_size = "4K" }
kernel = { start = 0x00000000, size = "256K" }
apps   = { start = 0x00040000, size = "768K" }
ram    = { start = 0x20000000, size = "256K" }
attributes = false            # this board's kernel has no attribute table

[connection]
default = "tock-bootloader"

[connection.tock-bootloader]
baud = 115200

[connection.openocd]
config = ["interface/jlink.cfg", "target/nrf52.cfg"]

[connection.jlink]           # handled by an external transport plugin
device = "nrf52840_xxAA"
```

- **Ownership:** board files live with the kernel or downstream board, next to its linker layout. **They are not compiled into the kernel**; they're copied from there. Older kernels are supported with hand-written board files.
- **Sources, in order of preference:**
  1. A file shipped with the kernel release (or referenced by the project)
  2. A bundled snapshot in the tool, generated from upstream rather than hand-written
  3. Read from the board itself, where the kernel provides enough metadata
  4. Fetched by name over the network: lowest priority, and possibly out of scope for v1
- **Variants:** a board file may `extends = "nrf52840dk"` and override individual fields, so a small layout difference is a few lines.

**Project file** (`Tock.toml`, illustrative), found by walking up from the current directory:

```toml
board = "boards/nrf52840dk.toml"     # path, or name of a bundled board
transport = "tock-bootloader"        # optional default

[kernel]                             # optional; used for full-image flashing
image = "../tock/target/thumbv7em-none-eabi/release/nrf52840dk.bin"

[[app]]
tab = "build/ipc_service.tab"        # e.g. a service another app depends on
[[app]]
tab = "build/blink.tab"
```

**User config** (`~/.config/tock/config.toml`): machine-specific settings such as probe serial numbers, device aliases (`lab-board-3 = { transport = "openocd", serial = "..." }`) and plugin search paths.

**Precedence (fixed and documented):** CLI flag > environment > project file > user config > board-file default.

**Selection:**
- If the selected transport finds **more than one** matching device, the tool fails. It lists the candidates with copy-pasteable `--device` selectors and exits non-zero.
- An optional `tock detect` helper may *suggest* configuration, but it is never on the default path.

### 7.5 Image model and planning engine

The **planning engine** is a pure function. Its input is a description of the current image plus a requested change. Its output is a target image plus a **plan**: an ordered list of erase and write operations, each with a human-readable reason.

Two operating modes:

1. **Static boards (no dynamic loading):**
   - **Full image:** builds a complete image (kernel + apps from `Tock.toml` + attributes) locally, then writes it. This replaces today's "local board" mode as the main model.
   - **App install/uninstall (default):** *pull, modify, push, scoped to the app region.* Pull the entire app region, plan the change locally, and write back only what changed. The kernel region is never touched. Cache prior reads to speed things up where it's safe. When enough is known about the kernel, skip reading the kernel region.
2. **Dynamic loading:** when the transport has the `apps` capability, the per-app operations delegate to it, and the planner is used only for validation.

Rules:
- The planner must honor existing conventions, such as TBF alignment, padding apps and MPU alignment requirements, **as the kernel does today**. This logic is isolated in the planner so it's easy to change later.
- Apps the user didn't mention aren't moved, unless `--allow-repack` (or similar) is given and the plan shows the moves.
- `--dry-run` prints the plan; `--format json` emits it.

> **Open (to resolve in M0):** the exact imperative-vs-declarative balance. The current direction is imperative commands as thin shortcuts over one plan/apply engine, with `install`, `uninstall` and `list` as the main operations. This needs validating with a prototype before it's locked in.

### 7.6 Machine interface

- Every built-in command supports `--format json` with a **versioned, documented schema**, checked in CI.
- Exit codes are stable and documented. They distinguish usage error, configuration ambiguity, a missing capability, a device error and a verification failure.
- The broker protocol may also be offered as an API for external tools in v1 (stretch goal).

### 7.7 Portability: WASM / WASI

- **v1 is native-first.** The **core crates** (`tock-board`, `tbf`, `tock-image`, `tock-proto`) must build for `wasm32-unknown-unknown` and `wasm32-wasip1/p2`. They must have no direct dependency on `std::fs`, process spawning or threads; I/O is abstracted behind traits.
- In the browser, hosts such as in-browser IDEs supply their own glue and transports (e.g. WebSerial). WASI CI runners use the core crates for planning and validation.
- There's no plugin system in WASM for v1. Because the protocol is carrier-agnostic ([§7.2](#72-extension-contract)), in-process or Web Worker carriers can be added later without a redesign.
- WASM components as a universal plugin format are **explicitly rejected**: transports that wrap native tools (OpenOCD, nrfutil, J-Link) can't be expressed that way.
- Publishing the core crates as a semver-stable Rust API is a *maybe* for v1. It may become necessary for the WASM use cases.

## 8. Functional requirements

Priority: **P0** = required for M1, **P1** = required by M2/M3, **P2** = later or design-for.

### Board and configuration
| ID | Requirement | Pri |
|---|---|---|
| FR-1 | Load board definitions from TOML files with separate `layout` and `connection` sections. | P0 |
| FR-2 | Support board-file inheritance (`extends`) with field-level overrides. | P1 |
| FR-3 | Find and parse `Tock.toml` by walking up from the current directory. It names the board, a default transport and optionally the kernel and app list. | P0 |
| FR-4 | User config for device aliases, probe serials and plugin paths. | P0 |
| FR-5 | Fixed precedence: flag > env > project > user > board default. | P0 |
| FR-6 | `config show` / `--explain` prints each resolved value and where it came from. | P0 |
| FR-7 | Bundle a generated snapshot of upstream board files, usable by name. | P1 |
| FR-8 | Read board metadata from the device when the kernel provides it. | P2 |
| FR-9 | Never prompt. When something is ambiguous, error out and list candidates with exact suggested commands. | P0 |

### App management (porcelain)
| ID | Requirement | Pri |
|---|---|---|
| FR-10 | `install <tab>...`: add or replace apps, keeping existing ones (pull, modify, push on the app region). | P0 |
| FR-11 | `uninstall <name>...`: remove apps, keeping the others. | P0 |
| FR-12 | `list`: show the installed apps (name, version, address, size, flags). | P0 |
| FR-13 | `--dry-run` shows the plan without writing; every write is verified (read back or hashed). | P0 |
| FR-14 | Choose the correct TAB architecture or binary for the board automatically, from the board file (deterministic, not a guess). | P0 |
| FR-15 | `install` with no arguments installs the `[[app]]` set from `Tock.toml`. | P1 |
| FR-16 | Delegate to transport `apps` operations when available (dynamic loading). | P1 (M3) |

### Image and flashing
| ID | Requirement | Pri |
|---|---|---|
| FR-20 | Pure planning engine: `(current image, request) → (target image, plan)`, with no I/O. | P0 |
| FR-21 | Planner honors today's kernel conventions (alignment, padding apps, MPU constraints). | P0 |
| FR-22 | `flash`: build and write a full image (kernel + apps + attributes) from `Tock.toml` or arguments. | P1 (M2) |
| FR-23 | Write a full image to a local file (replaces "local board"). | P1 |
| FR-24 | Optional caching of the app region read from the device. | P2 |

### Transports and extensions
| ID | Requirement | Pri |
|---|---|---|
| FR-30 | A versioned broker protocol with capability negotiation and a binary bulk-data path. | P0 |
| FR-31 | Discover `tock-<name>` subcommands and transports on `PATH`. | P0 |
| FR-32 | Built-in tock-bootloader transport. | P0 |
| FR-33 | Built-in OpenOCD and/or probe-rs transport (choose in M0). | P0 |
| FR-34 | Optional diff-aware or hash-based writes in the transport interface. | P1 |
| FR-35 | Pass-through for namespaced vendor-specific operations. | P1 |
| FR-36 | `--trace-protocol` logs all broker traffic. | P0 |
| FR-37 | Pass the resolved context to subcommands (env vars plus a context document). | P0 |
| FR-38 | Message layer independent of the carrier (stdio in v1; others possible later). | P0 (design) |

### Output
| ID | Requirement | Pri |
|---|---|---|
| FR-40 | `--format json` on all built-in commands, with a versioned schema. | P0 |
| FR-41 | Stable, documented exit codes. | P0 |
| FR-42 | Human output is concise. Every error ends with a suggested next command where possible. | P0 |

## 9. Non-functional requirements

| ID | Area | Requirement |
|---|---|---|
| NFR-1 | Platforms | **Tier 1:** Linux and macOS (x86_64 and aarch64), CI-tested. **Tier 2:** Windows (builds, best effort). |
| NFR-2 | Distribution | `cargo install`, prebuilt binaries on GitHub Releases, and a Nix flake in the repo. |
| NFR-3 | Dependencies | The core and built-in transports are static binaries with no system-library requirements wherever possible (e.g. no libudev requirement on Linux for serial). Transports that need external tools or dynamic libraries live outside the core; `cargo install` is the recommended path for those. |
| NFR-4 | Portability | Core crates build for wasm32 and WASI targets (CI-checked). |
| NFR-5 | Performance | Installing a typical app over tock-bootloader is no slower than Python tockloader on the reference boards. Over memory transports, writes are limited to changed pages. |
| NFR-6 | Maintainability | Core size discipline: new features default to subcommands, and additions to the core need a justification in the PR. Clear crate boundaries (board / tbf / image / proto / core / transports). |
| NFR-7 | Compatibility | Works with existing released kernels (via hand-written board files), the existing TBF format and existing kernel attributes. |
| NFR-8 | Safety | Nothing outside the planned regions is written. Every write is verified. The kernel region is never touched by app operations. |

## 10. Compatibility and migration

- **TBF:** the tool only *consumes* the TBF format; no changes.
- **TAB:** the packaging *may* be revisited (e.g. how multiple architectures are bundled). If so, reading legacy TABs must keep working.
- **Kernel:** v1 works with the kernel as it is: existing attributes, app-region conventions and padding apps. The architecture keeps these conventions isolated in the planner so they can change later.
- **Tock-side changes in scope:**
  - tock-bootloader improvements, e.g. hash/compare to support diff-aware writes
  - **Proposing** a standard dynamic-loading protocol
- **Python tockloader:**
  - Both tools exist side by side during a deprecation period.
  - Tock's `make install` / `make program` targets switch over at M2.
  - Python tockloader is then frozen (bug fixes only), then deprecated.
- **`tock/tockloader-rs`:** start fresh in that repo. The existing code is replaced, though individual pieces may be reused where they fit the new architecture.
- **elf2tab:** becomes (or is wrapped as) a subcommand for TAB creation in M4. Its TBF code should converge with the shared `tbf` crate.

## 11. Milestones

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0: Spike** | Image model and pure planner; draft board-file and project-file formats; draft broker/transport protocol; choose between OpenOCD and probe-rs. | `install`/`uninstall` plans computed correctly for the **nRF52840dk** and **RP2350 (Raspberry Pi Pico 2)** against real flash dumps; the protocol carries a full install end to end; open questions in [§14](#14-open-questions) answered or deferred. |
| **M1: App-developer MVP** (target: about one month; no hard deadline) | Core CLI; config resolution and `config show`; `install`/`uninstall`/`list`; tock-bootloader plus one probe transport; JSON output; errors that suggest the next command; board files for the reference and tutorial boards. | US1, US2, US3 and US7 work on both reference boards. |
| **M2: Makefile switch** | `flash` (full image); `Tock.toml` app sets; board files for upstream Tock boards; switch Tock's makefile targets; `listen` available (see below). | Tock's makefiles no longer need Python tockloader. |
| **M3: Dynamic loading** | Propose the dynamic-loading protocol; a transport for it; porcelain delegating to the `apps` capability. | US5 works with **no core changes** beyond the porcelain delegation that was already designed in. This is the "carrot" for switching. |
| **M4: Ecosystem** | TAB creation and elf2tab as a subcommand; TicKV and signing/credential subcommands; external J-Link and nrfutil transports; inspection tools. | Each one ships as a separate crate or binary, with no new core surface. |

## 12. Success criteria

1. A student with only the tool, a board file (or a bundled board name) and a `.tab` can install an app on the nRF52840dk and the RP2350 with **one command and no flags**.
2. Supporting an out-of-tree board or layout variant means writing **one board file**, with **zero changes** to the tool.
3. A new transport, including dynamic loading, can be added with **zero changes to the core**.
4. **Every** resolved value the tool uses can be explained by `config show` / `--explain`.
5. After M2, **Python tockloader is no longer needed** by Tock's makefiles.
6. Contributors can change one behavior (e.g. a padding rule or a transport detail) by touching a single crate.

## 13. Testing and quality

- **Planner:** snapshot tests and property tests over synthetic and real flash images. Properties include "plans never touch the kernel region", "apps not mentioned are never moved without `--allow-repack`" and "applying the plan to the image gives the target image".
- **Mock / in-memory transport** and **recorded protocol traces** for end-to-end CLI tests without hardware.
- **Hardware-in-the-loop** CI on the reference boards (nRF52840dk, RP2350), added once M1 is stable.
- **Schema checks:** the protocol and JSON output schemas are versioned, and CI fails on unversioned changes.
- **Portability checks:** CI builds the core crates for wasm32 and WASI.
- **Core size discipline:** additions to the core binary's surface need explicit justification in review.

## 14. Open questions

| # | Question | Owner / when |
|---|---|---|
| OQ-1 | Final balance between imperative and declarative app management; how far `Tock.toml` drives `install`. | M0 prototype |
| OQ-2 | Built-in porcelain: compiled into the core, or separate binaries shipped with it? | M0 |
| OQ-3 | OpenOCD, probe-rs, or both as the built-in probe transport? | M0 |
| OQ-4 | Session lifecycle for the broker: per-invocation connection, or a session/daemon mode for multi-step subcommands? | M0 |
| OQ-5 | Should tock-bootloader be modeled as a pure `memory` transport, or evolve toward `apps`-level operations? | M0/M3 |
| OQ-6 | The final **name** for the umbrella CLI (working name: `tock`). It must suit `tock app install`-style use and not clash with existing packages or crates. | Before M1 release |
| OQ-7 | Which of today's commands come back as subcommands, and which are dropped (see Appendix A)? | Ongoing |
| OQ-8 | Default production provisioning and signing flow (the built-in default; vendors supply their own). | M4 |
| OQ-9 | TAB packaging changes, if any. | M4 |
| OQ-10 | Should the board-file schema be defined in a shared crate that the Tock kernel repo can also validate against? | M1 |

## 15. Stakeholders and adoption

- No formal sign-off is needed for this PRD.
- **Buy-in is needed to move development over**, especially from **Brad Campbell** (Python tockloader maintainer), and more broadly from the Tock core team, whose makefiles, tutorials and CI depend on the current tool.
- **Adoption levers:**
  - Dynamic loading (M3) as the main reason to switch
  - Frictionless installation (Nix, static binaries)
  - Board files that remove the need to patch the loader
- **Recommended next step:** share this PRD and the M0 spike results with Brad and the core team before M1 to settle OQ-2, OQ-3 and OQ-6.

---

## Appendix A: Disposition of current tockloader commands

None of these is in the v1 core unless listed as porcelain. "Subcommand" means a separate crate or binary, later; any of these may still be dropped.

| Current command(s) | Disposition |
|---|---|
| `install`, `update`, `uninstall`, `list` | **Core porcelain** (`update` merges into `install`) |
| `flash` | **Core porcelain** (M2, full-image) |
| `local-board *` | Replaced by the image model and `Tock.toml` / board files |
| `listen` | Subcommand (`tock-listen`). **Recommended to ship alongside the core by M2**, since it's used heavily in tutorials. |
| `info`, `inspect-tab`, `inspect-kernel` | Subcommand (inspection tools), M4 or later |
| `read`, `write`, `dump-flash-page` | Subcommand (raw memory tools), or dropped |
| `enable-app`, `disable-app`, `sticky-app`, `unsticky-app` | Subcommand or flags; possibly dropped |
| `erase-apps` | Subcommand, or `uninstall --all` |
| `list-attributes`, `set-attribute`, `remove-attribute`, `set-start-address` | Subcommand (kernel-attribute tools) |
| `tbf tlv *`, `tbf credential *`, `tbf convert` | Subcommand (TBF/TAB tools, converging with elf2tab), M4 |
| `tickv *` | Subcommand (`tock-kv`), M4 |
| `list-known-boards` | Replaced by listing the bundled board snapshot |
| Transports: J-Link, nrfutil, STLink, esptool, LinkServer | External transport plugins |
