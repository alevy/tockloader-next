# tockloader-next

## Layout

```
boards/            board files for the reference boards (bundled into the tool)
crates/tbf         TBF headers and TABs (no I/O)
crates/tock-image  pure planner: discover / plan / render (no I/O)
crates/tock-board  board, project and user config, and resolution with provenance (no I/O)
crates/tock-proto  draft device protocol and framing
crates/tock-cli    the `tock` binary and the `tock-transport-file` transport
```

## Where to start reading

> **Provisional.** This order is being revised as the crates are audited; it
> may change as things move or shrink.

The system is two halves that meet in the CLI:

```
tbf  <-  tock-image  <-  tock-board      what should flash look like?
tock-proto                                how do bytes reach a device?
                   \    /
                   tock-cli
```

Start with `run` in `crates/tock-cli/src/main.rs`. Its six arms are the
structure: three porcelain commands where **we drive a transport**, `broker`
where that inverts and **we serve the protocol to someone else**, `external`
where third-party subcommands enter, and `config show` for what was resolved
and from where. Then take one porcelain path all the way down. Then outwards:

> To revisit: `run` shows the shape but does not explain it -- `main.rs` has a
> one-line module doc. Whether the entrée ends up being prose there, or the
> file is split so the structure is visible from the layout, is open.

| | Read | For |
|---|---|---|
| 1 | `tock-cli/src/main.rs` -- `run` | the shape: six commands, two directions |
| 1b | `tock-cli/src/main.rs` -- `change_apps`, `with_reads` | one porcelain path end to end: connect, discover, plan, render, write, verify |
| 2 | `tock-image/src/lib.rs` | the discover / plan / render contract, and how the core asks for bytes it does not have |
| 3 | `tock-proto/src/lib.rs` | the vocabulary: what you may ask a device to do |
| 4 | `tock-cli/transports/file.rs` | the smallest complete transport, and what a new one copies |
| 5 | `tock-board/src/resolve.rs` | where every setting came from, and why |
| 6 | `tbf/` | the on-flash format, when you need it |
| 7 | `tock-image/src/planner.rs` | where apps go. The hard part, and the last thing you need: a pure function with a stated contract |

Steps 6 and 7 are the two largest files in the repo and both come last --
nothing about the architecture depends on them.

## Try it without hardware

```sh
cargo build
cargo run -p tbf --example mktab -- blink 5000 2500 cortex-m4 blink.tab

T=target/debug/tock
$T --board nrf52840dk --transport file --opt path=flash.bin --opt create=true install blink.tab
$T --board nrf52840dk --transport file --opt path=flash.bin list
$T --board nrf52840dk --transport file --opt path=flash.bin install --dry-run --format json blink.tab
$T --board nrf52840dk --transport file --opt path=flash.bin config show
```

Or put the settings in a `Tock.toml`, next to the apps:

```toml
board = "nrf52840dk"        # or a path to a board file
transport = "file"

[transports.file]           # read only when the `file` transport is selected
path = "flash.bin"
create = true

[[app]]
tab = "blink.tab"
```

Then `tock install`, with no flags, from anywhere in the project. Relative
paths resolve against the file they were written in, not the working
directory, so this works from a subdirectory too.

## Tests

```sh
cargo test
# Also compare byte for byte with Python tockloader:
TOCKLOADER_PY=$(which tockloader) cargo test -p tock-cli
# The core crates build for WASM and WASI:
cargo build -p tbf -p tock-image -p tock-board -p tock-proto --target wasm32-unknown-unknown
```
