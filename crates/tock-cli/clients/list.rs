//! `tock-list`: what is installed on the board.
//!
//! A worked example of a subcommand. It resolves nothing, finds no board
//! file, and opens no transport: `tock` did all of that and handed over a
//! connection and a description of the board. What is left is the work.

use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, connect, context, finish};

#[path = "apps.rs"]
mod apps;

/// `tock list` takes nothing, but it still parses: a subcommand owns its
/// own arguments, so `--help` has to reach us rather than be answered by
/// `tock`, and `tock list --oops` has to be an error rather than silence.
#[derive(Parser)]
#[command(name = "tock-list", about = "List the apps on the board")]
struct Args {}

fn main() -> ExitCode {
    Args::parse();
    // `--format` belongs to `tock`, which forwards what a client must
    // honour. A subcommand parses its own arguments and not the global ones.
    finish(run(tock_client::json_output()))
}

fn run(json: bool) -> Result<(), CliError> {
    let board = context()?;
    let mut device = connect()?;
    let (_, found) = apps::installed(&mut device, &board)?;

    let apps: Vec<_> = found
        .apps()
        .map(|(address, header)| {
            serde_json::json!({
                "name": header.package_name().unwrap_or(""),
                "address": address,
                "size": header.total_size,
                "version": header.app_version(),
                "enabled": header.is_enabled(),
                "sticky": header.is_sticky(),
                "fixed_flash": header.fixed_addresses().1,
                "checksum_ok": header.checksum_ok(),
            })
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "board": board.board,
                "apps": apps,
                "end": found.end(&board.apps),
            })
        );
        return Ok(());
    }
    if apps.is_empty() {
        println!("no apps installed");
        return Ok(());
    }

    println!("Apps:");
    for app in &apps {
        let flag = |key: &str, when: bool, text: &str| {
            (app[key].as_bool().unwrap_or(false) == when).then(|| text.to_string())
        };
        let flags: Vec<String> = [
            flag("enabled", false, "disabled"),
            flag("sticky", true, "sticky"),
            flag("checksum_ok", false, "BAD CHECKSUM"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!(
            "{:<20} {:#010x}  {:>7} bytes  {}",
            app["name"].as_str().unwrap_or(""),
            app["address"].as_u64().unwrap_or(0),
            app["size"].as_u64().unwrap_or(0),
            flags.join(", ")
        );
    }
    Ok(())
}
