//! `tock-list`: what is installed on the board.
//!
//! A worked example of a subcommand. It resolves nothing, finds no board
//! file, and opens no transport: `tock` did all of that and handed over a
//! connection and a description of the board. What is left is the work.

use std::process::ExitCode;

use tock_client::{CliError, Session, connect, context};
use tock_image::Flash;
use tock_proto::Capability;

fn main() -> ExitCode {
    // `--format` belongs to `tock`, which forwards what a client must
    // honour. A subcommand parses its own arguments and not the global ones.
    match run(tock_client::json_output()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e.message);
            for hint in &e.hints {
                eprintln!("  try: {hint}");
            }
            ExitCode::from(e.code as u8)
        }
    }
}

fn run(json: bool) -> Result<(), CliError> {
    let board = context()?;
    let mut session = Session::open(connect()?)?;
    session.require(Capability::MemoryRead, "reading apps")?;

    let mut flash = Flash::new(&board.apps);
    let found = session.with_reads(&mut flash, |f| tock_image::discover(&board.apps, f))?;

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
