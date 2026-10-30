# Tockloader (Rust) — Requirements Discovery Notes

Running notes from the Q&A. These will be distilled into the PRD.

## Q1. Users and motivation

**User priority (highest first):**
1. App developers (libtock-c / libtock-rs) installing and managing apps on a board that already runs a kernel
2. Production and deployment users: provisioning, signing and credentials, attributes
3. Newcomers and students: tutorials and courses where things should "just work"
4. Other tools using it as a library (IDEs, web flashers, build systems)
5. CI and hardware-in-the-loop test setups (probably a special case of 4)
6. Kernel developers flashing kernels many times a day

**Pain points with current tockloader:**
- Fragile board detection
- Hard to support out-of-tree boards, or boards with small layout differences
- Hard to maintain; very hard to change one thing on its own
- Python install friction, especially on NixOS
- Complex, obscure UX
- Too much "magic"

## Q2. Board knowledge and selection

- **Yes:** declarative board definition files (a), kernel self-description (b), and explicit board selection with no guessing (c).
- **Acceptable, but not the main path:** a separate detection step (d). It is especially unsuitable when several boards are connected.
- **Ownership:** board definitions live with the kernel or downstream board, next to where its layout and other specifics are already defined. They do *not* live in tockloader.

## Q3. Where an app developer gets the board definition

**Preference order for sources:**
1. Shipped with kernel releases (a `board.toml` next to the prebuilt kernel)
2. A bundled snapshot inside the tool, generated from upstream rather than hand-written
3. Read from the board itself (embedded in the kernel image)
4. Fetched by name from a registry or network

**Layout vs. connection:**
- They are related but should not be 100% coupled. For example, the app start address is the same whether you use OpenOCD, J-Link or the Tock bootloader.
- Needing several files in the common case is undesirable.
- **Proposed resolution, to confirm in the PRD:** one board file with separate sections for layout and for connection methods, where the board lists its supported or default connection methods. A user or project config overrides only the connection choice. In the common case that's one file.

## Q4. App management model

- **Leaning toward (c):** imperative commands as thin shortcuts on top of one planning engine (compute layout → show plan → apply).
- **Main operations:** `install`, `uninstall`, `list`.
- **Not settled.** The user is unsure and expects part of this to come out of discovery during the rewrite, or an in-between stage.
  - **PRD action:** mark as an open question. Plan a prototype or spike phase to validate the planning-engine model before it's locked in.
  - Enable, disable and sticky are not in the main set. Decide later whether they stay, become flags, or move to an advanced or production area.

## Q5. Production tooling scope → architecture direction

**Key architectural direction:** a git/cargo-style tool.
- The core has limited responsibilities. Many use-specific subcommands are mostly implemented as separate binaries (e.g. `<tool>-<sub>` on PATH).
- The main binary may still handle some basic tasks for subcommands, such as shared context and plumbing.

**One CLI entry point for everything:**
- One command-line client should reach all of these: editing TicKV entries, adding signatures, installing and uninstalling on a board, and creating TBFs and TABs from an ELF.
- These features should *not* all live in the same project. The ecosystem must be extensible.

**Different deployers have very different needs:**
- Examples: Microsoft Pluton vs. OxidOS vs. a hobbyist differ on signing and deployment.
- All of them should be supportable. We can't answer the question once for everyone, but we should ship a built-in default.

**Production provisioning workflow details:** still open. Each extension should define its own; the default needs scoping.

## Q6. Core ↔ extension contract

**Contract model:**
- (b) or (c), leaning toward (c): the core owns the device connection and extensions talk to it through a protocol.
- The downsides and limitations of (c) need careful analysis in the PRD.

**Built-in porcelain (d):**
- Some form of it, for the default workflows.
- Either separate binaries shipped with the core, or truly built in. Not decided yet.

**Transports:**
- Must definitely be extensible.
- By default, ship only a small set of highly portable, lightweight transports:
  - tock-bootloader (serial): definitely
  - OpenOCD and/or probe-rs: maybe
  - nrfutil, JLinkExe: almost certainly not built in; they become external transport plugins

## Q7. Transport abstraction level

**Capability tiers (c):**
- Each transport declares what it supports: `memory` (raw read/write/erase), `apps` (app-level list/install/remove), `reset`, and so on.
- The planning engine uses raw memory when available, delegates to app-level operations when that's all there is, and gives clear errors when a capability is missing.

**Dynamic app loading** (install into a running kernel, OTA):
- It is a very near-future need, and is arguably blocked today mostly by tockloader's ossified, complex codebase.
- Not a hard v0 requirement, but it must not be designed out. The architecture should make it *easy*: treat it as a first-class design driver.

## Q8. Relationship to existing tools and naming

- **Python tockloader:** (c) side by side for a deprecation period. Tock's makefiles switch over at a milestone.
- **tockloader-rs:** start fresh *in* the `tock/tockloader-rs` repo, replacing what's there rather than building on it.
- **Name:** rebrand with a name that fits the umbrella-CLI role (exact name TBD).

### Addendum to Q4 and Q7: image-centric flashing

- Today's "local board" (operate on a flash file) is clunky, but arguably the right direction. It was just added late.
- **Possible core design, with two modes:**
  1. **Static boards (no dynamic loading):** build a *complete image* locally (kernel + apps + attributes), then a transport flashes the whole thing.
  2. **Dynamic loading:** install and uninstall individual apps through a running kernel.
- If so, the planning engine (Q4) works on a local image and not on live device flash. That removes most of the on-device read-modify-write "magic".

## Q9. Single-app install on static boards (no dynamic loading)

**Default: pull, modify, push (a), scoped to the app region:**
- Pull the entire app region, modify it locally with the pure planning engine, then re-flash the app region.
- The kernel region is not touched.

**Variants:**
- Optional caching to speed up common cases.
- If enough is known about the kernel, skip reading the kernel. The app region is always pulled, so existing apps are kept.

**Transports may optimize reads and writes by tracking diffs.** This matters for slow or low-bandwidth links (e.g. networked transports). The transport interface should allow it, for example by accepting an image plus the known prior contents, or by exposing hash/compare capabilities.

**Open:** whether tock-bootloader counts as a "static" (memory) transport, or should evolve toward app-level operations.

## Q10. Configuration and selection UX

**Project file:** yes, found by walking up from the current directory.
- It names the board definition and a default transport.
- It also lists the **apps** that make up the image. This is useful for projects that depend on other services (e.g. an IPC service app) and for aggregating projects.

**Never prompt interactively.**
- When something is ambiguous, error out and suggest the commands to run instead. This is almost always better.

**Accepted strawman by default:**
- User config for machine-specific settings (probe serials, aliases, plugin paths)
- Env vars and flags as overrides
- Fixed, documented precedence: flag > env > project > user > board default
- `config show` / `--explain` to show where each resolved value came from

## Q11. Library, embedding and machine-readable output

**v0 requirements:**
- (b) The CLI as the API: machine-readable output (e.g. `--format json`) with a documented, stable schema.
- (d) Maybe: the device and broker protocol as an API for other tools.

**Maybe:**
- (a) Rust crates as a supported, semver-stable public API.
- This may become necessary in order to deliver (c).

**Design for it now, build later:**
- (c) WASM targets.
- Real use cases:
  - **In-browser IDEs**, with a WebSerial/WebUSB transport
  - **WASI-based CI runners**
- **Implication:** the core crates (board definitions, TBF, image planning, protocol) must be portable. They must not depend on direct `std::fs`, process spawning or threads, and I/O must be abstracted. Plugin and transport discovery that relies on spawning processes needs an in-process alternative for WASM/WASI.

## Q12. Plugin model across native and WASM

**Decision: (d) native-first, with an eye toward (b).**
- WASM/WASI v0 scope: only the core crates (planning, TBF, board definitions) as a library. Browser IDEs and WASI CI hosts provide their own glue and transports.
- No plugin system in WASM for now.
- **Keep the door open to (b):** define the plugin and transport protocol so the message layer is separate from the process/stdio carrier. In-process or Web Worker carriers can be added later without redesign.

**Rejected: (c) WASM components as the universal plugin format.** It isn't realistic given transports that wrap native tools (OpenOCD, nrfutil, J-Link).

## Q13. Platforms and distribution

**Platforms:**
- Tier 1: Linux and macOS
- Tier 2: Windows. Putting transports in separate subcommands/plugins may make Windows easier.

**Official distribution channels:**
- `cargo install`
- Prebuilt binaries (GitHub Releases)
- Nix flake in the repo

**System dependencies:**
- Make the core and built-in subcommands static binaries as much as possible.
- Some subcommands or transports depend on external tools (e.g. OpenOCD). That's acceptable; they live outside the core.
- Not allergic to dynamically linked binaries, but for those, `cargo install` is the recommended path (avoids versioning and LD path problems).

**Plugin distribution:** discovery on PATH is enough for now. Many plugins will be installed with `cargo install` if they're written in Rust. No plugin registry or `plugin install` command in v0.

## Q14. Compatibility with on-device formats and Tock

1. **Older kernels:** supported through hand-written board files. Board files living alongside the kernel is an *organizational* choice. They aren't compiled in, just copied from there, so no kernel build changes are required.
2. **TBF:** strictly a consumer of the existing TBF format. **TAB packaging may be revisited.**
3. **Changes to Tock:**
   - The tool must work with Tock as it is.
   - In scope: changes to tock-bootloader (e.g. diff/hash support), and *proposing* a dynamic-loading protocol.
4. **Kernel attributes and app-region conventions (padding apps, etc.):** v1 works with the kernel as it is. The architecture should make it easy to evolve or change these later.

## Q15. Release scope and order

**Order:**
- M0: spike
- M1: app-developer MVP
- M2: makefile switch
- **M3: dynamic loading** (moved earlier, because it's the "carrot" that gets people to switch)
- **M4: ecosystem** (TAB creation and elf2tab subcommand, TicKV, signing, external J-Link/nrfutil transports)

**Reference boards (M0/M1):** nRF52840dk and RP2350.

**Not answered:** target date or event; explicit non-goals (follow-up in Q16).

## Q16. Leftover commands, non-goals and timing

**Leftover commands:**
- `listen`, `read`, `write`, `dump-flash-page`, `inspect-tab`, `info`, `tbf tlv`, `tbf credential`, `enable-app`, `disable-app`, `sticky-app`, `erase-apps`
- None is in the v1 core. Each becomes either a subcommand (later) or is potentially dropped.
- **Note for PRD:** `listen` is heavily used by app developers and newcomers. Flag it as a likely early standalone subcommand, possibly shipped alongside the core, to avoid regressing the tutorial flow.

**Explicit non-goals:**
- GUI
- Flashing non-Tock firmware
- General debugger front end
- Toolchain management

**Timing:** M1 within about a month, but no hard deadline; ships when ready.

## Q17. Success criteria, quality and stakeholders

- **Accepted as proposed:**
  - Success criteria: one-command student install, out-of-tree boards with zero tool changes, new transports with zero core changes, everything explainable, Python dropped from makefiles after M2
  - Testing strategy: pure planner with snapshot/property tests, mock transport, HIL on reference boards, versioned schemas, core-size discipline
- **Sign-off:** not required.
- **Buy-in:** eventually needed to move development over, especially from **Brad** (Brad Campbell, current tockloader maintainer).
