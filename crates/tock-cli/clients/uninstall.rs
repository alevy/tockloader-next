//! `tock-uninstall`: take apps off the board.

use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, connect, context, finish};
use tock_image::Change;
use tock_proto::Capability;

#[path = "apps.rs"]
mod apps;

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
    finish(run(&Args::parse()))
}

fn run(args: &Args) -> Result<(), CliError> {
    let ctx = context()?;
    let change = Change {
        uninstall: args.names.clone(),
        allow_repack: args.allow_repack,
        include_sticky: args.include_sticky,
        ..Default::default()
    };
    // Even a dry run reads; only a real one writes, but saying so here
    // would route differently for `--dry-run`, and a command that needs a
    // different transport depending on a flag is a worse surprise.
    let mut device = connect(&[Capability::MemoryRead, Capability::MemoryWrite])?;
    let mut survey = apps::survey(&mut device, &ctx)?;
    apps::apply(&mut device, &mut survey, &change, args.dry_run)?.report(args.dry_run);
    Ok(())
}
