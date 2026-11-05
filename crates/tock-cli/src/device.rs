//! The core's side of the device connection: spawning a `tock-transport-<name>`
//! executable and memory operations on top of the protocol. The core has no
//! transports of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tock_board::Context;
use tock_proto::{Connection, ErrorKind, FlashInfo, ProtoError, Request, Response, StreamChannel};

use tock_client::{CliError, Code, Session};

/// A spawned transport, reaped when the connection goes.
struct Spawned<R: std::io::Read, W: std::io::Write> {
    channel: StreamChannel<R, W>,
    child: Child,
}

impl<R: std::io::Read, W: std::io::Write> Connection for Spawned<R, W> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        self.channel.call(request, payload)
    }
}

impl<R: std::io::Read, W: std::io::Write> Drop for Spawned<R, W> {
    fn drop(&mut self) {
        let _ = self.child.wait();
    }
}

/// The directory each resolved option was written in. The core never decides
/// what an option *means*, so this is all it can offer a transport that needs
/// to resolve a relative path. Absolute, because a plugin or a `tock broker`
/// client does not reliably share our working directory.
fn option_bases(ctx: &Context, cwd: &Path) -> BTreeMap<String, String> {
    let dir_of = |origin: Option<&String>| {
        // A board reference like `bundled:nrf52840dk` is not a file and has no
        // directory; values from one cannot be relative paths anyway.
        let Some(path) = origin.filter(|o| !o.starts_with("bundled:")).map(Path::new) else {
            return cwd.to_path_buf();
        };
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        absolute.parent().unwrap_or(cwd).to_path_buf()
    };

    ctx.transport
        .options
        .iter()
        .map(|(key, sourced)| {
            let origin = match &sourced.source {
                // Flags and the environment are written at a prompt, not in a
                // file, so their base is wherever the command was run.
                tock_board::Source::Flag(_) | tock_board::Source::Env(_) => None,
                // An alias records only its own name, so the file it was
                // written in is the user config.
                tock_board::Source::Device(_) => ctx.user.as_ref(),
                tock_board::Source::Project(o)
                | tock_board::Source::User(o)
                | tock_board::Source::Board(o) => Some(o),
            };
            (key.clone(), dir_of(origin).display().to_string())
        })
        .collect()
}

/// A [`Connection`] that logs every message to stderr (`--trace-protocol`)
/// and forwards it.
///
/// One trait for both ends is what makes this ordinary -- and what lets the
/// same decorator watch either conversation. Wrapping the transport shows
/// what we ask a device; wrapping the broker shows what a subcommand asks
/// us. `peer` says which, since with both on they interleave.
///
/// `->` is always the request and `<-` always the answer, whichever side of
/// it we are.
pub(crate) struct Traced {
    pub(crate) peer: &'static str,
    pub(crate) inner: Box<dyn Connection>,
}

impl Connection for Traced {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        let peer = self.peer;
        eprintln!(
            "{peer} -> {} (+{} bytes)",
            serde_json::to_string(&request).unwrap(),
            payload.len()
        );
        let answer = self.inner.call(request, payload);
        match &answer {
            Ok((response, data)) => eprintln!(
                "{peer} <- {} (+{} bytes)",
                serde_json::to_string(response).unwrap(),
                data.len()
            ),
            Err(e) => eprintln!("{peer} <- error: {e}"),
        }
        answer
    }
}

/// A [`Connection`] that refuses reads and writes outside the board's flash,
/// and forwards everything else.
///
/// A decorator rather than something the broker does, so every path to a
/// device has it: the built-in commands, and a third-party subcommand coming
/// through `tock broker`. Only this layer knows which board it is, which is
/// why it can say which one the address is outside of.
struct Bounded {
    inner: Box<dyn Connection>,
    board: String,
    flash: FlashInfo,
}

impl Bounded {
    fn problem(&self, request: &Request, payload: &[u8]) -> Option<String> {
        let (address, len) = match *request {
            Request::Read { address, len } => (address, len),
            Request::Write { address } => (address, payload.len() as u64),
            _ => return None,
        };
        let (start, size) = (self.flash.start, self.flash.size);
        let board = &self.board;
        (address < start || address + len > start + size).then(|| {
            format!("{address:#x}+{len} is outside flash on {board} ({start:#x} +{size:#x})")
        })
    }
}

impl Connection for Bounded {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match self.problem(&request, &payload) {
            Some(problem) => Ok((Response::error(ErrorKind::InvalidRequest, problem), vec![])),
            None => self.inner.call(request, payload),
        }
    }
}

/// Open the transport named in the context. Every transport is an executable
/// `tock-transport-<name>`, found next to `tock` or on `PATH` -- including the
/// `file` transport, which ships alongside `tock` but is not special.
pub fn open_connection(ctx: &Context, trace: bool) -> Result<Box<dyn Connection>, CliError> {
    let name = &ctx.transport.name.value;
    let exe = format!("tock-transport-{name}");
    let path = crate::external::find_executable(&exe).ok_or_else(|| {
        CliError::new(
            Code::Config,
            format!(
                "transport `{name}` (from {}) needs `{exe}`, which was not found \
                 next to `tock` or on PATH",
                ctx.transport.name.source
            ),
        )
        .hint(format!("install it, e.g. `cargo install {exe}`"))
    })?;
    let mut child = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            CliError::new(
                Code::Device,
                format!("cannot start {}: {e}", path.display()),
            )
        })?;
    let spawned = Spawned {
        channel: StreamChannel {
            reader: child.stdout.take().expect("piped"),
            writer: child.stdin.take().expect("piped"),
        },
        child,
    };
    Ok(if trace {
        Box::new(Traced {
            peer: "transport",
            inner: Box::new(spawned),
        })
    } else {
        Box::new(spawned)
    })
}

/// Spawn the transport the context names, greet it, and open it.
///
/// The result is an ordinary [`Session`] -- the same thing a subcommand gets
/// from the connection it inherits. The difference is only that we started
/// the transport and it did not.
pub fn open(ctx: &Context, trace: bool) -> Result<Session, CliError> {
    let connection = open_connection(ctx, trace)?;
    let layout = &ctx.board.layout;
    let flash = FlashInfo {
        start: layout.flash.start.0,
        size: layout.flash.size.0,
    };
    // Every request from here on is checked against the board's flash,
    // whoever sent it.
    let connection: Box<dyn Connection> = Box::new(Bounded {
        inner: connection,
        board: ctx.board.board.name.clone(),
        flash: flash.clone(),
    });
    let mut session = Session::open(connection)?;

    let options = serde_json::to_value(ctx.transport_options()).unwrap();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    session.call(
        Request::Open {
            board: ctx.board.board.name.clone(),
            arch: ctx.board.board.arch.clone(),
            flash,
            options: options.as_object().cloned().unwrap_or_default(),
            option_bases: option_bases(ctx, &cwd),
        },
        vec![],
    )?;
    Ok(session)
}
