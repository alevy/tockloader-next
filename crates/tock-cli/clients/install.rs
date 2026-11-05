//! `tock-install`: put apps on the board.
//!
//! With no arguments it installs what the project declares, which `tock`
//! already resolved to absolute paths before this ran.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, Code, Session, connect, context, load_tab};
use tock_image::Request;

#[derive(Parser)]
#[command(name = "tock-install", about = "Install apps on the board")]
struct Args {
    /// TABs to install. With none, the project's `[[app]]` entries.
    tabs: Vec<PathBuf>,
    /// Show the plan without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Allow moving apps that were not named, to make room.
    #[arg(long)]
    allow_repack: bool,
    /// Allow removing or replacing sticky apps.
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
    let tabs = if args.tabs.is_empty() {
        if board.project_apps.is_empty() {
            return Err(CliError::new(
                Code::Failed,
                "no TABs given and no [[app]] entries in a Tock.toml",
            )
            .hint("tock install path/to/app.tab")
            .hint("or list apps in Tock.toml: [[app]] tab = \"build/app.tab\""));
        }
        board.project_apps.clone()
    } else {
        args.tabs.clone()
    };

    let mut install = Vec::new();
    for tab in &tabs {
        install.push(load_tab(&board, tab)?);
    }
    let request = Request {
        install,
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
