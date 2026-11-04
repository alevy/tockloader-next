//! Handing off to a subcommand we do not ship.
//!
//! `tock <name>` that is not built in becomes `tock-<name>`, found next to
//! `tock` or on `PATH`. It inherits the resolved context and the flags it
//! needs to run `tock broker`, so it reaches the device the same way a
//! built-in command does.

use std::path::PathBuf;

use crate::cli::Global;
use crate::config::load_context;
use crate::error::{CliError, Code};

pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(name));
    beside.into_iter().chain(on_path).find(|p| p.is_file())
}

/// `tock foo ...` runs `tock-foo ...` with the resolved context in
/// `TOCK_CONTEXT` (JSON), if it resolves.
pub(crate) fn external(g: &Global, args: Vec<String>) -> Result<(), CliError> {
    let name = &args[0];
    let exe = format!("tock-{name}");
    let path = find_executable(&exe).ok_or_else(|| {
        CliError::new(
            Code::Failed,
            format!("unknown command `{name}`, and no `{exe}` next to tock or on PATH"),
        )
        .hint("tock --help")
    })?;
    let mut cmd = std::process::Command::new(path);
    cmd.args(&args[1..]);
    match load_context(g) {
        Ok(ctx) => {
            cmd.env("TOCK_CONTEXT", serde_json::to_string(&ctx).unwrap());
        }
        Err(e) => {
            cmd.env("TOCK_CONTEXT_ERROR", e.message);
        }
    }
    // Pass global flags through so the subcommand can run `tock broker`.
    for (flag, v) in [
        ("--board", &g.board),
        ("--transport", &g.transport),
        ("--device", &g.device),
    ] {
        if let Some(v) = v {
            cmd.env(
                format!("TOCK_{}", flag.trim_start_matches("--").to_uppercase()),
                v,
            );
        }
    }
    let status = cmd
        .status()
        .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
    std::process::exit(status.code().unwrap_or(1));
}
