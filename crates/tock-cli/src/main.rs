//! `tock`: resolve what the user meant, then either drive a device or serve
//! the protocol so something else can.
//!
//! Two directions run through this binary, and [`run`] is where they part.
//!
//! **We drive a transport.** `list`, `install` and `uninstall` resolve a
//! context, spawn a `tock-transport-<name>` executable, and send it requests.
//! `config show` stops after resolving and prints what it found.
//!
//! **We serve the protocol.** `broker` resolves and opens a transport, then
//! answers requests on stdio. `tock <name>` that we do not recognise becomes
//! `tock-<name>`, which reaches the device by running `tock broker` itself.
//!
//! Those two are not yet the same path: a built-in command drives the
//! transport directly, while an external one goes through the broker. The
//! PRD's model is that everything goes through the broker and the core owns
//! the one connection, which would make a third-party subcommand structurally
//! identical to a built-in. Until then, `broker` is a door only strangers
//! use.

mod apps;
mod broker;
mod cli;
mod config;
mod device;
mod external;

use std::process::ExitCode;

use clap::Parser;
use serde_json::json;

use apps::{install, list, uninstall};
use broker::broker;
use cli::{Cli, Command, ConfigCommand, Format};
use config::config_show;
use external::external;
use tock_client::CliError;

fn main() -> ExitCode {
    let cli = Cli::parse();
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
        Command::List => list(&g),
        Command::Install { tabs, change } => install(&g, tabs, change),
        Command::Uninstall { names, change } => uninstall(&g, names, change),
        Command::Broker => broker(&g),
        Command::External(args) => external(&g, args),
    }
}
