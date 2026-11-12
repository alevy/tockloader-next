//! What a `tock-<name>` subcommand needs, and nothing it does not.
//!
//! A subcommand does not resolve configuration, find a board file, or open a
//! transport. `tock` has done all of that before it ran, and handed over two
//! things:
//!
//! - a **connection** to the device, on the file descriptor named by
//!   `TOCK_CONNECTION` -- see [`connect`];
//! - a **description of the board**, as JSON in `TOCK_CONTEXT` -- see
//!   [`context`].
//!
//! Both have a fallback for a subcommand nobody launched: `connect` will
//! start `tock broker` itself, and `context` says plainly that it has no
//! board rather than guessing one.
//!
//! The protocol itself is [`tock_proto`]; this crate is the part that would
//! otherwise be copied into every subcommand.

mod error;

use std::os::fd::FromRawFd;
use std::process::{Command, Stdio};

use tock_board::Context;

use tock_proto::{
    Capability, Client, Connection, ErrorKind, ProtoError, Request, Response, StreamChannel,
};

pub use error::{CliError, Code};

/// The connection `tock` handed us, or one we start for ourselves.
///
/// `TOCK_CONNECTION` names an inherited file descriptor; that is the usual
/// case and means `tock` owns the device. Without it -- running a subcommand
/// by hand, say -- this starts `tock broker`, which resolves the environment
/// and opens a transport the same way.
///
/// `needs` is what this command intends to do. Say it here rather than
/// discovering it a refusal at a time: a board may be reachable several ways
/// at once, and the far end cannot route what it has not been told. It is
/// also the earliest point at which "nothing here can do that" can be said,
/// which is before any of the work.
pub fn connect(needs: &[Capability]) -> Result<Client, CliError> {
    Client::open(raw_connection()?, &whoami(), needs).map_err(proto)
}

/// How we introduce ourselves in `Hello`: the program running, since that is
/// what a trace of the conversation needs to name.
fn whoami() -> String {
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "a tock subcommand".into());
    format!("{name} {}", env!("CARGO_PKG_VERSION"))
}

/// The carrier itself, for the rare caller that wants it unopened.
fn raw_connection() -> Result<Box<dyn Connection>, CliError> {
    if let Some(fd) = std::env::var("TOCK_CONNECTION")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        // Safe only because `tock` promised us this descriptor and nothing
        // else in this process has touched it.
        let handed: std::os::unix::net::UnixStream = unsafe { FromRawFd::from_raw_fd(fd) };
        let reader = handed
            .try_clone()
            .map_err(|e| CliError::new(Code::Device, format!("TOCK_CONNECTION={fd}: {e}")))?;
        return Ok(Box::new(StreamChannel {
            reader,
            writer: handed,
        }));
    }
    let mut child = Command::new("tock")
        .arg("broker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            CliError::new(Code::Device, format!("cannot start `tock broker`: {e}"))
                .hint("run this through `tock <name>`, or put `tock` on PATH")
        })?;
    let reader = child.stdout.take().expect("piped");
    let writer = child.stdin.take().expect("piped");
    Ok(Box::new(Spawned::new(reader, writer, child)))
}

/// A [`Connection`] to a process we started, reaped when the connection goes.
///
/// Both ends of the tool need this and had written it twice: `tock` spawns a
/// `tock-transport-<name>`, and a subcommand run on its own spawns
/// `tock broker`. Neither cares which, which is the point of the trait.
///
/// Here rather than in [`tock_proto`] because spawning is not portable and
/// that crate has to build for wasm32. This one is native by nature -- it
/// is about inherited file descriptors and child processes.
pub struct Spawned<R: std::io::Read, W: std::io::Write> {
    /// An `Option` only so [`Drop`] can close it before waiting.
    channel: Option<StreamChannel<R, W>>,
    child: std::process::Child,
}

impl<R: std::io::Read, W: std::io::Write> Spawned<R, W> {
    pub fn new(reader: R, writer: W, child: std::process::Child) -> Spawned<R, W> {
        Spawned {
            channel: Some(StreamChannel { reader, writer }),
            child,
        }
    }
}

impl<R: std::io::Read, W: std::io::Write> Connection for Spawned<R, W> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match &mut self.channel {
            Some(channel) => channel.call(request, payload),
            None => Err(ProtoError::Closed),
        }
    }
}

impl<R: std::io::Read, W: std::io::Write> Drop for Spawned<R, W> {
    /// Close our end, *then* reap.
    ///
    /// [`serve`] ends on `Close` or on end of stream, and only the second is
    /// ours to guarantee: waiting while still holding the pipe open means
    /// the child is waiting for a request that will never come, and we are
    /// waiting for it to exit. A missing `Close` should cost a round trip,
    /// not the whole process.
    fn drop(&mut self) {
        self.channel = None;
        let _ = self.child.wait();
    }
}

/// What `tock` resolved: the board, where apps go, and the project's TABs.
///
/// `TOCK_CONTEXT_ERROR` is set instead when resolution failed -- a subcommand
/// that does not need a board can ignore both.
pub fn context() -> Result<Context, CliError> {
    if let Ok(why) = std::env::var("TOCK_CONTEXT_ERROR") {
        return Err(inherited_error(&why));
    }
    let json = match std::env::var("TOCK_CONTEXT") {
        Ok(json) => json,
        // Nobody launched us: ask, the way `connect` starts `tock broker`.
        Err(_) => {
            let out = Command::new("tock").arg("context").output().map_err(|e| {
                CliError::new(Code::Config, format!("cannot run `tock context`: {e}"))
                    .hint("run this through `tock <name>`, or put `tock` on PATH")
            })?;
            if !out.status.success() {
                return Err(CliError::new(
                    Code::Config,
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    };
    serde_json::from_str(&json)
        .map_err(|e| CliError::new(Code::Config, format!("TOCK_CONTEXT is not readable: {e}")))
}

/// Rebuild the error `tock` could not resolve past, hints and all. Falls
/// back to the raw text if it is not the document we expect, so a mangled
/// variable still says something.
fn inherited_error(raw: &str) -> CliError {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return CliError::new(Code::Config, raw.to_string());
    };
    let message = value["message"].as_str().unwrap_or(raw).to_string();
    let mut error = CliError::new(Code::Config, message);
    for detail in value["details"].as_array().into_iter().flatten() {
        if let Some(detail) = detail.as_str() {
            error = error.detail(detail);
        }
    }
    for hint in value["hints"].as_array().into_iter().flatten() {
        if let Some(hint) = hint.as_str() {
            error = error.hint(hint);
        }
    }
    error
}

/// Report a failure and give back the code to exit with.
///
/// Part of the contract, not a convenience: `tock` reads the exit code, and
/// `--format json` means a subcommand's errors must be JSON too. Every
/// subcommand's `main` is this line, and `tock`'s own is as well, so there
/// is one answer rather than one per binary.
pub fn finish(result: Result<(), CliError>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            if json_output() {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": e.message,
                        "details": e.details,
                        "hints": e.hints,
                        "code": e.code as u8,
                    })
                );
            } else {
                eprintln!("error: {}", e.message);
                for detail in &e.details {
                    eprintln!("  {detail}");
                }
                for hint in &e.hints {
                    eprintln!("  try: {hint}");
                }
            }
            std::process::ExitCode::from(e.code as u8)
        }
    }
}

/// Whether `tock` was asked for machine-readable output.
///
/// A client parses its own arguments, but not the global flags: `tock` owns
/// those and forwards what a subcommand must honour.
pub fn json_output() -> bool {
    std::env::var("TOCK_FORMAT").is_ok_and(|v| v == "json")
}

/// A protocol failure, classified the way the CLI classifies its own.
///
/// Without this a mistyped transport name exits 5 rather than 3, because
/// everything that crosses a connection looks like a device problem from the
/// far side.
pub fn code_for(kind: ErrorKind) -> Code {
    match kind {
        ErrorKind::Selection => Code::Config,
        ErrorKind::Unsupported => Code::Capability,
        ErrorKind::Device | ErrorKind::InvalidRequest => Code::Device,
    }
}

pub fn proto(e: ProtoError) -> CliError {
    let code = match &e {
        ProtoError::Failed { kind, .. } => code_for(*kind),
        _ => Code::Device,
    };
    let mut error = CliError::new(code, e.to_string());
    if let ProtoError::Failed {
        suggestion,
        details,
        ..
    } = &e
    {
        for detail in details {
            error = error.detail(detail);
        }
        for line in suggestion.iter().flat_map(|s| s.lines()) {
            error = error.hint(line);
        }
    }
    error
}

#[cfg(test)]
mod tests;
