//! `tock`: resolve what the user meant, then serve the protocol so a
//! subcommand can act on it.
//!
//! `tock` implements no command that touches a device. It answers three
//! things itself -- `context`, `config show` and `broker` -- and all three
//! are about resolution: turning flags, `Tock.toml` and the user config into
//! one answer about which board, which transport, and where apps go.
//!
//! Everything else is a binary. `tock list` runs `tock-list`, passing its
//! arguments untouched and handing it an open connection on fd 3; the
//! subcommand plans and writes, and `tock` answers its requests. `tock-list`
//! ships with us and `tock-tickv` might not, and nothing on this path can
//! tell the difference -- which is the point, and is why there is one rule
//! for arguments rather than one per subcommand.
//!
//! [`run`] is the whole dispatch. `broker` is the same service `external`
//! runs over a pipe, offered on stdio instead, for a subcommand written in
//! a language that cannot link this crate.

mod broker;
mod cli;
mod config;
mod connection;
mod external;

use std::process::ExitCode;

use broker::broker;
use cli::{Command, ConfigCommand, Format, Global};
use config::config_show;
use external::external;
use tock_client::CliError;

fn main() -> ExitCode {
    // The parser is built at runtime because which commands exist is a fact
    // about this machine, not about what we shipped.
    let (global, command) = cli::parse(&external::installed());
    // `--format` is one way to set `TOCK_FORMAT`, which is the actual
    // contract: it is what a subcommand reads, and what we read ourselves a
    // line later. Normalising it here means one notion of format in the
    // process, and children inherit it without being told.
    if global.format == Format::Json {
        unsafe { std::env::set_var("TOCK_FORMAT", "json") };
    }
    // The same reporting every subcommand uses: `tock` is not a special case
    // of its own contract.
    tock_client::finish(run(global, command))
}

fn run(g: Global, command: Command) -> Result<(), CliError> {
    match command {
        Command::Config {
            command: ConfigCommand::Show,
        } => config_show(&g),
        Command::Context => config::context(&g),
        Command::Broker => broker(&g),
        Command::External(args) => external(&g, args),
    }
}
