//! `tock-uninstall`: take apps off the board.

use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, Session, connect, context};
use tock_image::Request;

#[derive(Parser)]
#[command(name = "tock-uninstall", about = "Remove apps from the board")]
struct Args {
    /// Names of apps to remove.
    #[arg(required = true)]
    names: Vec<String>,
    /// Show the plan without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Allow moving apps that were not named, to make room.
    #[arg(long)]
    allow_repack: bool,
    /// Allow removing sticky apps.
    #[arg(long)]
    include_sticky: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
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

fn run(args: &Args) -> Result<(), CliError> {
    let board = context()?;
    let request = Request {
        uninstall: args.names.clone(),
        allow_repack: args.allow_repack,
        include_sticky: args.include_sticky,
        ..Default::default()
    };
    let mut session = Session::open(connect()?)?;
    session
        .apply(&board, &request, args.dry_run)?
        .report(args.dry_run);
    Ok(())
}
