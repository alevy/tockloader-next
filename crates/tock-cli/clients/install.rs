//! `tock-install`: put apps on the board.
//!
//! With no arguments it installs what the project declares, which `tock`
//! already resolved to absolute paths before this ran.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use tock_board::Resolved;
use tock_client::{CliError, Code, connect, context, finish};
use tock_image::{NewApp, Request};

#[path = "apps.rs"]
mod apps;

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
    finish(run(&Args::parse()))
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

    let install = tabs
        .iter()
        .map(|tab| load_tab(&board, tab))
        .collect::<Result<Vec<NewApp>, _>>()?;

    let request = Request {
        install,
        uninstall: vec![],
        allow_repack: args.allow_repack,
        include_sticky: args.include_sticky,
    };

    let mut device = connect()?;
    apps::apply(&mut device, &board, &request, args.dry_run)?.report(args.dry_run);
    Ok(())
}

/// A TAB, with the builds that suit this board picked out.
///
/// Here rather than in the shared module because `install` is the only
/// command that reads one: `uninstall` takes names and `list` takes nothing.
fn load_tab(board: &Resolved, path: &Path) -> Result<NewApp, CliError> {
    let shown = path.display();
    let bytes =
        std::fs::read(path).map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    let tab = tbf::Tab::parse(&bytes)
        .map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    if let Some(only) = tab.only_for_boards() {
        if !only.contains(&board.board) {
            return Err(CliError::new(
                Code::Failed,
                format!("{shown} is only for boards: {}", only.join(", ")),
            ));
        }
    }
    let tbfs = tab
        .tbfs_for_arch(&board.arch)
        .map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    if tbfs.is_empty() {
        return Err(CliError::new(
            Code::Failed,
            format!(
                "{shown} has no build for `{}` (it has: {})",
                board.arch,
                tab.architectures().join(", ")
            ),
        ));
    }
    let name = tab
        .name()
        .map(str::to_string)
        .or_else(|| tbfs[0].tbf.header.package_name().map(str::to_string))
        .unwrap_or_else(|| shown.to_string());
    Ok(NewApp {
        name,
        candidates: tbfs.into_iter().map(|t| t.tbf).collect(),
    })
}
