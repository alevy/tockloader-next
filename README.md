# tockloader-next

## Layout

```
boards/            board files for the reference boards (bundled into the tool)
crates/tbf         TBF headers and TABs (no I/O)
crates/tock-image  pure planner: discover / plan / render (no I/O)
crates/tock-board  board, project and user config, and resolution with provenance (no I/O)
crates/tock-proto  draft device protocol and framing
crates/tock-cli    the `tock` binary, the subcommands and transports we ship
```

`tock-cli` builds eight binaries: `tock` itself, the `tock-install`,
`tock-uninstall`, `tock-list` and `tock-console` subcommands, and the
`tock-transport-file`, `tock-transport-openocd` and `tock-transport-serial`
transports. They share a crate because we ship
them together, not because anything in the architecture joins them -- each is
reached exactly the way a third party's would be.

## Where to start reading

> **Provisional.** This order is being revised as the crates are audited; it
> may change as things move or shrink.

The system is two halves that meet in a subcommand:

```
tbf  <-  tock-image  <-  tock-board      what should flash look like?
tock-proto                                how do bytes reach a device?
                   \    /
                tock-client               what a subcommand is handed
                      |
                   tock-cli               resolve it, then serve it
```

`tock` implements no command that touches a device. It resolves -- flags,
`Tock.toml` and the user config into one answer about which board, which
transport, where apps go -- and then serves the protocol over a pipe while a
`tock-<name>` binary does the work. `tock list` runs `tock-list`; `tock
tickv` would run `tock-tickv`, by the same route, with the same connection,
and `tock` cannot tell the two apart.

So read it in that order: what `tock` decides, then what a subcommand does
with it, then down into the parts it uses.

| | Read | For |
|---|---|---|
| 1 | `tock-cli/src/main.rs` -- the module doc, then `run` | the shape: four arms, none of which touch a device |
| 2 | `tock-cli/clients/list.rs` | a whole subcommand in 90 lines, and how little one has to know |
| 3 | `tock-client/src/lib.rs` -- `connect`, `context` | the two things `tock` hands a subcommand, and the fallbacks when nobody handed them |
| 4 | `tock-cli/clients/apps.rs` -- `apply` | one path end to end: discover, plan, render, write, verify |
| 5 | `tock-image/src/lib.rs` | the discover / plan / render contract, and how the core asks for bytes it does not have |
| 6 | `tock-proto/src/lib.rs` | the vocabulary: what you may ask a device to do |
| 7 | `tock-cli/transports/file.rs` | the smallest complete transport, and what a new one copies |
| 8 | `tock-board/src/resolve.rs` | where every setting came from, and why |
| 9 | `tbf/` | the on-flash format, when you need it |
| 10 | `tock-image/src/planner.rs` | where apps go. The hard part, and the last thing you need: a pure function with a stated contract |

The last two steps are the formats and the placement algorithm, and both come
last on purpose: `planner.rs` is the largest source file in the repo and the
one with the most detail in it, and nothing about the architecture depends on
either.

## Try it without hardware

```sh
cargo build
cargo run -p tbf --example mktab -- blink 5000 2500 cortex-m4 blink.tab

T=target/debug/tock
$T --board nrf52840dk --transport file --opt path=flash.bin --opt create=true install blink.tab
$T --board nrf52840dk --transport file --opt path=flash.bin list
$T --board nrf52840dk --transport file --opt path=flash.bin --format json install --dry-run blink.tab
$T --board nrf52840dk --transport file --opt path=flash.bin config show
```

Global flags go before the subcommand. `--format` belongs to `tock`, and
`tock` hands a subcommand its arguments untouched -- so `install --format
json` is `tock-install`'s to reject, and it does.

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

A board's console is usually not reachable the way its flash is -- a
USB-to-UART bridge is a console and nothing else -- so a board says which
transport serves what, and `tock` routes each request accordingly:

```toml
[board.transport]        # in a board file; `default` covers the rest
default = "openocd"
console = "serial"
```

```toml
[transports.serial]      # in your user config: which port is a fact about
port = "/dev/ttyUSB0"    # your desk, not about the board
```

Then `tock list` reaches the flash and `tock console` reaches the console,
neither naming a transport. `tock transports` lists what is installed and
what each one reads; `tock config show` prints the routing and where every
part of it came from.

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
