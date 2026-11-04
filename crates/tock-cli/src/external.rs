//! Handing off to a subcommand we do not ship.
//!
//! `tock <name>` that is not built in becomes `tock-<name>`, found next to
//! `tock` or on `PATH`. It inherits the resolved context and the flags it
//! needs to run `tock broker`, so it reaches the device the same way a
//! built-in command does.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use tock_board::Resolved;

use crate::broker::Broker;
use crate::cli::Global;
use crate::config::{load_context, project_apps};
use tock_client::{CliError, Code};

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

/// `tock foo ...` runs `tock-foo ...`.
///
/// The subcommand inherits two things. `TOCK_CONTEXT` is a
/// [`Resolved`](tock_board::Resolved) document in JSON: the board, where apps
/// go, how to place them, and the project's declared TABs -- everything
/// needed to plan, and nothing about how a connection would be made.
/// `TOCK_BOARD`, `TOCK_TRANSPORT` and `TOCK_DEVICE` carry the global flags
/// forward so the subcommand can run `tock broker` and reach the device
/// without re-deriving any of it from the command line.
///
/// If resolution failed, `TOCK_CONTEXT_ERROR` says why instead. A subcommand
/// that does not touch a device can still run.
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
    match load_context(g).and_then(|ctx| Ok(Resolved::new(&ctx, project_apps()?))) {
        Ok(resolved) => {
            cmd.env("TOCK_CONTEXT", serde_json::to_string(&resolved).unwrap());
        }
        Err(e) => {
            // The whole error, not just its message: a subcommand reporting
            // "no board selected" without "try: tock --board ..." has lost
            // the half that helps.
            cmd.env(
                "TOCK_CONTEXT_ERROR",
                serde_json::json!({
                    "code": e.code as u8,
                    "message": e.message,
                    "hints": e.hints,
                })
                .to_string(),
            );
        }
    }
    // Forwarded for a subcommand that re-invokes `tock` for something other
    // than a device -- `tock config show`, say. Reaching the device does not
    // need them: that is what fd 3 is for.
    if g.format == crate::cli::Format::Json {
        cmd.env("TOCK_FORMAT", "json");
    }
    for (name, v) in [
        ("TOCK_BOARD", &g.board),
        ("TOCK_TRANSPORT", &g.transport),
        ("TOCK_DEVICE", &g.device),
    ] {
        if let Some(v) = v {
            cmd.env(name, v);
        }
    }

    // One end of this is ours to serve; the other becomes the child's fd 3.
    let (ours, theirs) = UnixStream::pair()
        .map_err(|e| CliError::new(Code::Failed, format!("cannot make a connection: {e}")))?;
    let theirs_fd = theirs.as_raw_fd();
    cmd.env("TOCK_CONNECTION", "3");
    // dup2 clears CLOEXEC on the new descriptor, so fd 3 survives the exec.
    unsafe {
        cmd.pre_exec(move || match libc::dup2(theirs_fd, 3) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
    // Our copy of the child's end must go, or we never see end of stream.
    drop(theirs);

    // Answer the child until it stops asking. Nothing is opened unless it
    // asks for something that needs a device.
    if let Ok(context) = load_context(g) {
        let reader = ours
            .try_clone()
            .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
        let mut broker = Broker::new(context, g.trace_protocol);
        let _ = tock_proto::serve(reader, ours, &mut broker);
    }

    let status = child
        .wait()
        .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
    std::process::exit(status.code().unwrap_or(1));
}
