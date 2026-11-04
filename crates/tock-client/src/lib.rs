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

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::process::{Child, Command, Stdio};

use tock_board::Resolved;
use tock_image::{Flash, NeedRead, WriteRun, coalesce};
use tock_proto::{Capability, Connection, ErrorKind, ProtoError, Request, Response, StreamChannel};

pub use error::{CliError, Code};

/// How far apart two reads may be before issuing them separately beats
/// reading the bytes between them. See `tock-cli`'s note on the write side:
/// this is round-trip cost against per-byte cost, and one value for every
/// transport is a placeholder.
const READ_GAP: u64 = 256;

/// The device `tock` opened for us.
///
/// Obtained from [`connect`]. The transport was already opened with the
/// resolved context before this process started, so there is no `Open` here
/// and no options to supply -- just the capabilities it turned out to have.
pub struct Session {
    connection: Box<dyn Connection>,
    pub transport: String,
    pub capabilities: Vec<Capability>,
}

impl Session {
    /// Greet whatever is on the other end and learn what it can do.
    pub fn open(mut connection: Box<dyn Connection>) -> Result<Session, CliError> {
        let hello = Request::Hello {
            protocol: tock_proto::PROTOCOL_VERSION,
            client: format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        };
        match answered(connection.call(hello, vec![]))? {
            (
                Response::Hello {
                    protocol,
                    transport,
                    capabilities,
                    ..
                },
                _,
            ) => {
                if protocol != tock_proto::PROTOCOL_VERSION {
                    return Err(CliError::new(
                        Code::Device,
                        format!(
                            "`{transport}` speaks protocol {protocol}, this tool speaks {}",
                            tock_proto::PROTOCOL_VERSION
                        ),
                    ));
                }
                Ok(Session {
                    connection,
                    transport,
                    capabilities,
                })
            }
            (other, _) => Err(proto(ProtoError::Unexpected(Box::new(other)))),
        }
    }

    /// Fail clearly if the device cannot do what this command needs.
    pub fn require(&self, capability: Capability, doing: &str) -> Result<(), CliError> {
        if self.capabilities.contains(&capability) {
            return Ok(());
        }
        Err(CliError::new(
            Code::Capability,
            format!(
                "`{doing}` needs the `{}` capability, which transport `{}` does not have",
                serde_json::to_value(&capability)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                self.transport
            ),
        )
        .hint("pick a transport that has it with --transport <name>"))
    }

    /// Read byte ranges, merging nearby ones into one request.
    pub fn read_ranges(&mut self, ranges: &[(u64, u64)]) -> Result<Vec<(u64, Vec<u8>)>, CliError> {
        let mut out = Vec::new();
        for (address, len) in coalesce(ranges, READ_GAP) {
            let (_, data) = answered(self.connection.call(Request::Read { address, len }, vec![]))?;
            // A short reply would leave a read loop asking for the same bytes
            // forever. Say so instead of hanging.
            if data.len() as u64 != len {
                return Err(CliError::new(
                    Code::Device,
                    format!(
                        "transport `{}` returned {} of the {len} bytes asked for at {address:#x}",
                        self.transport,
                        data.len()
                    ),
                ));
            }
            out.push((address, data));
        }
        Ok(out)
    }

    /// Write byte runs as they stand. `render` has already merged them, and
    /// erase granularity is the transport's business.
    pub fn write_runs(&mut self, runs: &[WriteRun]) -> Result<(), CliError> {
        for run in runs {
            answered(self.connection.call(
                Request::Write {
                    address: run.address,
                },
                run.data.clone(),
            ))?;
        }
        Ok(())
    }

    /// Pass a request through untouched, including a response that says the
    /// operation failed.
    ///
    /// For relaying on someone else's behalf. A refusal is theirs to see and
    /// decide about, not ours to turn into our own failure -- which is what
    /// [`Session::call`] would do.
    pub fn forward(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        self.connection.call(request, payload)
    }

    /// Send a request and treat a refusal as this command's failure.
    pub fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), CliError> {
        answered(self.connection.call(request, payload))
    }

    /// Run `step` until it stops asking for bytes, fetching what it asks for.
    ///
    /// The planner in [`tock_image`] is pure: when it needs flash it does not
    /// have, it says so rather than reading. This is the loop that answers.
    pub fn with_reads<T>(
        &mut self,
        flash: &mut Flash,
        mut step: impl FnMut(&Flash) -> Result<T, NeedRead>,
    ) -> Result<T, CliError> {
        loop {
            match step(flash) {
                Ok(value) => return Ok(value),
                Err(NeedRead { ranges }) => {
                    for (address, data) in self.read_ranges(&ranges)? {
                        flash.insert(address, &data);
                    }
                }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.connection.call(Request::Close, vec![]);
    }
}

/// The connection `tock` handed us, or one we start for ourselves.
///
/// `TOCK_CONNECTION` names an inherited file descriptor; that is the usual
/// case and means `tock` owns the device. Without it -- running a subcommand
/// by hand, say -- this starts `tock broker`, which resolves the environment
/// and opens a transport the same way.
pub fn connect() -> Result<Box<dyn Connection>, CliError> {
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
    Ok(Box::new(Brokered {
        channel: StreamChannel {
            reader: child.stdout.take().expect("piped"),
            writer: child.stdin.take().expect("piped"),
        },
        child,
    }))
}

/// A `tock broker` we started, reaped when the connection goes.
struct Brokered<R: Read, W: Write> {
    channel: StreamChannel<R, W>,
    child: Child,
}

impl<R: Read, W: Write> Connection for Brokered<R, W> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        self.channel.call(request, payload)
    }
}

impl<R: Read, W: Write> Drop for Brokered<R, W> {
    fn drop(&mut self) {
        let _ = self.child.wait();
    }
}

/// What `tock` resolved: the board, where apps go, and the project's TABs.
///
/// `TOCK_CONTEXT_ERROR` is set instead when resolution failed -- a subcommand
/// that does not need a board can ignore both.
pub fn context() -> Result<Resolved, CliError> {
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
    for hint in value["hints"].as_array().into_iter().flatten() {
        if let Some(hint) = hint.as_str() {
            error = error.hint(hint);
        }
    }
    error
}

/// Whether `tock` was asked for machine-readable output.
///
/// A client parses its own arguments, but not the global flags: `tock` owns
/// those and forwards what a subcommand must honour.
pub fn json_output() -> bool {
    std::env::var("TOCK_FORMAT").is_ok_and(|v| v == "json")
}

/// Reduce both kinds of failure to one error: a dead connection, and a
/// response saying the operation failed. Only this needs to tell them apart.
pub fn answered(
    result: Result<(Response, Vec<u8>), ProtoError>,
) -> Result<(Response, Vec<u8>), CliError> {
    match result {
        Err(e) => Err(proto(e)),
        Ok((
            Response::Error {
                kind,
                message,
                suggestion,
            },
            _,
        )) => {
            let mut error = CliError::new(code_for(kind), message);
            // A transport owns the whole sentence, and may offer more than
            // one remedy: one hint per line.
            for hint in suggestion.iter().flat_map(|s| s.lines()) {
                error = error.hint(hint);
            }
            Err(error)
        }
        ok => ok.map_err(proto),
    }
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
        suggestion: Some(hint),
        ..
    } = &e
    {
        for line in hint.lines() {
            error = error.hint(line);
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers every read with fewer bytes than asked for.
    struct Stingy;
    impl Connection for Stingy {
        fn call(
            &mut self,
            _request: Request,
            _payload: Vec<u8>,
        ) -> Result<(Response, Vec<u8>), ProtoError> {
            Ok((Response::Data, vec![0xFF; 4]))
        }
    }

    #[test]
    fn a_short_read_is_an_error_not_a_silent_retry() {
        let mut session = Session {
            connection: Box::new(Stingy),
            transport: "stingy".into(),
            capabilities: vec![Capability::MemoryRead],
        };
        // `with_reads` would ask again forever if this came back Ok.
        let err = session.read_ranges(&[(0x1000, 64)]).unwrap_err();
        assert!(format!("{err:?}").contains('4'), "{err:?}");
    }
}
