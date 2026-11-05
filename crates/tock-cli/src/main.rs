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
mod device;
mod external;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

use broker::broker;
use cli::{Cli, Command, ConfigCommand, Format};
use config::config_show;
use external::external;
use tock_client::CliError;

fn main() -> ExitCode {
    // `after_help` is built at runtime because the answer is whatever is
    // installed, not whatever we shipped.
    let matches = Cli::command()
        .after_help(external::installed())
        .get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    let format = cli.global.format;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if format == Format::Json {
                println!(
                    "{}",
                    json!({ "error": e.message, "hints": e.hints, "code": e.code as u8 })
                );
            } else {
                eprintln!("error: {}", e.message);
                for h in &e.hints {
                    eprintln!("  try: {h}");
                }
            }
            ExitCode::from(e.code as u8)
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let g = cli.global;
    match cli.command {
        Command::Config {
            command: ConfigCommand::Show,
        } => config_show(&g),
        Command::Context => config::context(&g),
        Command::Broker => broker(&g),
        Command::External(args) => external(&g, args),
    }
}
