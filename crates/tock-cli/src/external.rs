//! Handing off to a subcommand.
//!
//! Every command but [`Context`](crate::cli::Command::Context),
//! `config show` and [`Broker`](crate::cli::Command::Broker) arrives here:
//! `tock <name>` becomes `tock-<name>`, found next to `tock` or on `PATH`.
//! It inherits the resolved context and an open connection, and its
//! arguments reach it untouched. `tock-list` is one we ship and `tock-tickv`
//! is one we do not, and nothing below distinguishes them.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use tock_board::Context;

use crate::broker::Broker;
use crate::cli::Global;
use crate::config::{load_settings, project_apps};
use tock_client::{CliError, Code};

/// Every directory we look in, in the order we look.
fn search_path() -> impl Iterator<Item = PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>());
    beside.into_iter().chain(on_path)
}

/// The subcommands that exist on this machine, sorted.
///
/// Transports are excluded: `tock-transport-file` is spoken to over a pipe,
/// not typed.
pub(crate) fn installed() -> Vec<String> {
    let mut names: Vec<String> = search_path()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| executable(&e.path()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let name = name.strip_prefix("tock-")?;
            (!name.starts_with("transport-")).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A file we could actually run. Both callers want this: listing a command
/// we would refuse to spawn, or spawning one we would not list, is the same
/// mistake in two directions.
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    search_path().map(|d| d.join(name)).find(|p| executable(p))
}

/// `tock foo ...` runs `tock-foo ...`.
///
/// The subcommand inherits two things. `TOCK_CONTEXT` is a
/// [`Context`](tock_board::Context) document in JSON: the board, where apps
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
    match load_settings(g).and_then(|s| Ok(Context::new(&s, project_apps()?))) {
        Ok(context) => {
            cmd.env("TOCK_CONTEXT", serde_json::to_string(&context).unwrap());
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
    // `TOCK_FORMAT` is not here: it is set on our own environment in `main`,
    // so the child inherits it. These are forwarded for a subcommand that
    // re-invokes `tock` for something other than a device -- `tock config
    // show`, say. Reaching the device does not need them: fd 3 is for that.
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
    if let Ok(settings) = load_settings(g) {
        let reader = ours
            .try_clone()
            .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
        let mut broker =
            crate::broker::watched(Broker::new(settings, g.trace_protocol), g.trace_protocol);
        let _ = tock_proto::serve(reader, ours, &mut broker);
    }

    let status = child
        .wait()
        .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
    std::process::exit(status.code().unwrap_or(1));
}
