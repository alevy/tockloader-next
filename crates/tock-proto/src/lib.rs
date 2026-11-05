//! The protocol `tock` speaks to a transport.
//!
//! # Who talks
//!
//! Two roles, and every participant is one or the other.
//!
//! A **responder** implements [`Connection`]: it receives a [`Request`] and
//! answers with a [`Response`]. Transports are responders, and so is
//! `tock broker`. This is the extension point -- writing a transport means
//! implementing this trait and nothing else.
//!
//! A **caller** holds a [`Connection`] and sends requests over it. `tock` is a
//! caller when it drives a transport; an external `tock-<name>` subcommand is
//! a caller when it drives `tock broker`. Note that the broker is both at
//! once: it answers its own client while calling the transport beneath it,
//! which is how a third-party subcommand reaches a device it never opened.
//!
//! # What they say, and in what order
//!
//! 1. [`Request::Hello`] -- version and capability check, before anything
//!    else. A responder that disagrees about [`PROTOCOL_VERSION`] says so
//!    here.
//! 2. [`Request::Open`] -- the resolved board, flash geometry and transport
//!    options. Nothing may touch the device before this.
//! 3. Operations -- [`Request::Read`], [`Request::Write`] and the rest, in
//!    any order, as many times as the caller likes. A responder may refuse
//!    any it did not advertise in its [`Capability`] list.
//! 4. [`Request::Close`] -- the responder releases the device and stops.
//!
//! # How it travels
//!
//! The message layer above is independent of the carrier. [`StreamChannel`]
//! and [`serve`] are the two ends of the one carrier that exists today: a
//! framed byte stream, which in practice is a transport child process's
//! stdio. See [`StreamChannel`] for the frame layout.
//!
//! Control fields are JSON; bulk flash contents are not, and travel beside
//! the JSON as opaque bytes. That split is the point -- a megabyte of flash
//! would be unreadable and enormous base64-encoded into a message.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. 0 = draft, no stability promised.
pub const PROTOCOL_VERSION: u32 = 0;

// ---------------------------------------------------------------------------
// Messages

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
        client: String,
    },
    /// Connect to the device. `options` are the resolved transport options.
    Open {
        board: String,
        arch: String,
        flash: FlashInfo,
        options: serde_json::Map<String, serde_json::Value>,
        /// For each key in `options`, the absolute directory its value was
        /// written in -- the one thing a transport needs to resolve a
        /// relative path. Defaulted, so a transport that ignores paths can
        /// omit it entirely.
        #[serde(default)]
        option_bases: BTreeMap<String, String>,
    },
    /// Payload in the response.
    Read {
        address: u64,
        len: u64,
    },
    /// Payload: the bytes to write. Any address, any length.
    Write {
        address: u64,
    },
    Hash {
        address: u64,
        len: u64,
    },
    AppsList,
    /// Payload: one TBF.
    AppsInstall,
    AppsRemove {
        name: String,
    },
    Reset,
    Vendor {
        name: String,
        args: serde_json::Value,
    },
    Close,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u32,
        transport: String,
        version: String,
        capabilities: Vec<Capability>,
    },
    Ok,
    /// Payload carries the data.
    Data,
    /// SHA-256, hex.
    Hash {
        sha256: String,
    },
    Apps {
        apps: Vec<AppInfo>,
    },
    Vendor {
        value: serde_json::Value,
    },
    Error {
        kind: ErrorKind,
        message: String,
        /// A command the user can run next, if the transport knows one.
        suggestion: Option<String>,
    },
}

impl Response {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Response {
        Response::Error {
            kind,
            message: message.into(),
            suggestion: None,
        }
    }

    /// Attach the line the CLI prints as `try: ...`. A no-op on any response
    /// that is not an error.
    pub fn with_suggestion(self, hint: impl Into<String>) -> Response {
        match self {
            Response::Error { kind, message, .. } => Response::Error {
                kind,
                message,
                suggestion: Some(hint.into()),
            },
            other => other,
        }
    }
}

/// What a transport can do. Commands check these up front and fail with a
/// clear error if a capability is missing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `read` arbitrary flash.
    MemoryRead,
    /// `write` an arbitrary byte range. The transport is responsible for its
    /// device's erase granularity: if erasing destroys more than was sent, it
    /// is the transport's job to preserve the rest.
    MemoryWrite,
    /// `hash` a range without transferring it.
    MemoryHash,
    /// App-level `apps_list` / `apps_install` / `apps_remove`, e.g. dynamic
    /// loading into a running kernel.
    Apps,
    Reset,
    Console,
    /// Namespaced vendor operations, passed through untouched.
    Vendor(String),
}

/// The flash a transport is opened for, from [`Request::Open`].
///
/// **A transport must refuse a `Read` or `Write` outside it**, with
/// [`ErrorKind::InvalidRequest`]. Nothing in front can do this for it: a
/// transport is an executable, so anyone who can spawn it can talk to it
/// with no `tock` in the way. A device's address map is the transport's
/// subject, and programming an address the client got wrong is how a kernel
/// or a bootloader gets overwritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashInfo {
    pub start: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Unsupported,
    /// No device, or more than one matching device.
    Selection,
    Device,
    InvalidRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub address: Option<u64>,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// The two roles

/// Something you send a [`Request`] to and get a [`Response`] back from.
///
/// One trait for both ends, because it is one operation. A transport
/// implements it by doing the work; [`StreamChannel`] implements it by
/// forwarding over a byte stream; `tock broker` implements it by delegating
/// to another `Connection`. `serve` is the inverse adapter: it puts a local
/// implementation behind a stream, so the thing on the other side can hold a
/// `StreamChannel` and not care which it is talking to.
///
/// # The two kinds of failure
///
/// They are different and must stay so:
///
/// - **The operation failed.** `Ok(Response::Error { .. })`. The device said
///   no, the address was out of range, the image is missing. The connection
///   is fine and the caller may send more.
/// - **The connection failed.** `Err(..)`. The pipe broke, a frame was
///   malformed, the peer went away. Nothing further can be sent.
///
/// An implementation that cannot fail at the second level simply never
/// returns `Err`, the way an in-memory writer never fails.
pub trait Connection {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError>;
}

/// So a boxed connection is still a connection, and decorators can be
/// stacked without each layer caring what it wraps.
impl Connection for Box<dyn Connection> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        (**self).call(request, payload)
    }
}

/// A failure of the connection itself, never of the operation -- an operation
/// that failed is a [`Response::Error`], which is an `Ok`.
///
/// [`ProtoError::Failed`] carries the same detail a [`Response::Error`] would,
/// because the commonest fatal cases are the user's to fix -- openocd is not
/// installed, a config file is missing -- and telling them so is worth as much
/// here as anywhere.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
    #[error("unexpected response: {0:?}")]
    Unexpected(Box<Response>),
    /// The connection could not be established, or has failed for good.
    #[error("{message}")]
    Failed {
        /// What sort of failure, so a caller can classify it the way it would
        /// classify a [`Response::Error`].
        kind: ErrorKind,
        message: String,
        /// What the user might do about it, in whole sentences. One per line.
        suggestion: Option<String>,
    },
}

impl ProtoError {
    pub fn failed(message: impl Into<String>) -> ProtoError {
        ProtoError::Failed {
            kind: ErrorKind::Device,
            message: message.into(),
            suggestion: None,
        }
    }

    /// Classify it as something other than a device failure.
    pub fn with_kind(self, kind: ErrorKind) -> ProtoError {
        match self {
            ProtoError::Failed {
                message,
                suggestion,
                ..
            } => ProtoError::Failed {
                kind,
                message,
                suggestion,
            },
            other => other,
        }
    }

    /// Attach the line the CLI prints as `try: ...`.
    pub fn with_suggestion(self, hint: impl Into<String>) -> ProtoError {
        match self {
            ProtoError::Failed { kind, message, .. } => ProtoError::Failed {
                kind,
                message,
                suggestion: Some(hint.into()),
            },
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// The carrier

/// The caller's end of a framed byte stream. [`serve`] is the other end.
///
/// Each message is one frame, control part first:
///
/// ```text
/// u32 LE json_len | json | u32 LE payload_len | payload
/// ```
///
/// Both lengths are always present; either may be zero. The payload is the
/// request's or response's bulk bytes -- flash contents, a TBF -- and is
/// never JSON.
pub struct StreamChannel<R: Read, W: Write> {
    pub reader: R,
    pub writer: W,
}

impl<R: Read, W: Write> Connection for StreamChannel<R, W> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        write_frame(&mut self.writer, &request, &payload)?;
        read_frame(&mut self.reader)?.ok_or(ProtoError::Closed)
    }
}

/// A [`Connection`] to a process we started, reaped when the connection goes.
///
/// Both ends of the tool need this and had written it twice: `tock` spawns a
/// `tock-transport-<name>`, and a subcommand run on its own spawns
/// `tock broker`. Neither cares which, which is the point of the trait.
pub struct Spawned<R: Read, W: Write> {
    /// An `Option` only so [`Drop`] can close it before waiting.
    channel: Option<StreamChannel<R, W>>,
    child: std::process::Child,
}

impl<R: Read, W: Write> Spawned<R, W> {
    pub fn new(reader: R, writer: W, child: std::process::Child) -> Spawned<R, W> {
        Spawned {
            channel: Some(StreamChannel { reader, writer }),
            child,
        }
    }
}

impl<R: Read, W: Write> Connection for Spawned<R, W> {
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

impl<R: Read, W: Write> Drop for Spawned<R, W> {
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

/// A [`Connection`] that logs every message to stderr and forwards it.
///
/// One trait for both ends is what makes this ordinary -- and what lets the
/// same decorator watch either conversation. Wrapping a transport shows what
/// we ask a device; wrapping a broker shows what a subcommand asks us. `peer`
/// says which, since with both on they interleave.
///
/// `->` is always the request and `<-` always the answer, whichever side of
/// it we are.
pub struct Traced {
    pub peer: &'static str,
    pub inner: Box<dyn Connection>,
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

/// Put a [`Connection`] behind a byte stream: read requests, answer them,
/// until `Close` or end of stream. The inverse of [`StreamChannel`], and what
/// every `tock-transport-*` executable runs in `main`.
///
/// A connection-level failure is reported to the peer as a [`Response::Error`]
/// -- it is waiting for a frame and would otherwise hang -- and then ends the
/// loop, since by definition nothing further can be sent. That counts as
/// having done the job, so it is an `Ok`: the peer has the message, including
/// any suggestion, and decides what the user's command exits with. Returning
/// an error here as well would only print the same sentence twice.
pub fn serve<R: Read, W: Write, C: Connection>(
    mut reader: R,
    mut writer: W,
    connection: &mut C,
) -> Result<(), ProtoError> {
    while let Some((request, payload)) = read_frame::<_, Request>(&mut reader)? {
        let close = request == Request::Close;
        match connection.call(request, payload) {
            Ok((response, out)) => {
                write_frame(&mut writer, &response, &out)?;
                if close {
                    break;
                }
            }
            Err(e) => {
                write_frame(&mut writer, &Response::from(e), &[])?;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One frame out. See [`StreamChannel`] for the layout.
fn write_frame<W: Write, T: Serialize>(
    w: &mut W,
    message: &T,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let json = serde_json::to_vec(message)?;
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(&json)?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// One frame in, or `Ok(None)` at a clean end of stream. See
/// [`StreamChannel`] for the layout.
fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<Option<(T, Vec<u8>)>, ProtoError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut json = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut json)?;
    r.read_exact(&mut len)?;
    let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut payload)?;
    Ok(Some((serde_json::from_slice(&json)?, payload)))
}

// ---------------------------------------------------------------------------
// For responders

/// The options from [`Request::Open`], as a responder reads them.
///
/// Not a wire type -- a borrowed view over what arrived, pairing each value
/// with the directory it was written in so a relative path can be resolved
/// against the file that declared it rather than the working directory.
pub struct Options<'a> {
    values: &'a serde_json::Map<String, serde_json::Value>,
    bases: &'a BTreeMap<String, String>,
}

impl<'a> Options<'a> {
    pub fn new(
        values: &'a serde_json::Map<String, serde_json::Value>,
        bases: &'a BTreeMap<String, String>,
    ) -> Options<'a> {
        Options { values, bases }
    }

    /// The raw value, for option shapes the typed accessors do not cover --
    /// a list, say. Prefer `get_str` / `get_bool` / `get_path`.
    pub fn get(&self, key: &str) -> Option<&'a serde_json::Value> {
        self.values.get(key)
    }

    /// The directory a value for `key` was written in, for options whose
    /// shape `get_path` cannot handle -- a list of paths, say.
    pub fn base_dir(&self, key: &str) -> Option<PathBuf> {
        self.bases.get(key).map(PathBuf::from)
    }

    /// A path option, resolved against the directory of the file it was
    /// written in. Absolute values, and values with no provenance, are used
    /// exactly as written.
    pub fn get_path(&self, key: &str) -> Option<PathBuf> {
        let value = PathBuf::from(self.get(key)?.as_str()?);
        if value.is_absolute() {
            return Some(value);
        }
        match self.base_dir(key) {
            Some(base) => Some(base.join(value)),
            None => Some(value),
        }
    }
}

/// Report a connection failure to a peer that is waiting for a frame. Keeps
/// the suggestion: a peer one hop further away needs it just as much, and
/// `serve` is the only place a `ProtoError` turns back into a `Response`.
impl From<ProtoError> for Response {
    fn from(e: ProtoError) -> Response {
        match e {
            ProtoError::Failed {
                kind,
                message,
                suggestion,
            } => Response::Error {
                kind,
                message,
                suggestion,
            },
            other => Response::error(ErrorKind::Device, other.to_string()),
        }
    }
}

/// An I/O failure inside a transport is a device error. Having the conversion
/// here means a transport can use `?` on ordinary `std::io` calls and return
/// `Result<_, Response>` throughout, instead of each one inventing its own
/// error plumbing.
impl From<std::io::Error> for Response {
    fn from(e: std::io::Error) -> Response {
        Response::error(ErrorKind::Device, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Connection for Echo {
        fn call(
            &mut self,
            request: Request,
            payload: Vec<u8>,
        ) -> Result<(Response, Vec<u8>), ProtoError> {
            Ok(match request {
                Request::Read { len, .. } => (Response::Data, vec![0xAB; len as usize]),
                Request::Write { .. } => (Response::Ok, vec![]),
                Request::Close => (Response::Ok, vec![]),
                _ => (Response::error(ErrorKind::Unsupported, "nope"), payload),
            })
        }
    }

    #[test]
    fn frames_round_trip_over_a_stream() {
        // Client writes requests into a buffer; server reads them and writes
        // responses into another buffer; client reads those.
        let mut to_server = Vec::new();
        write_frame(&mut to_server, &Request::Read { address: 0, len: 4 }, &[]).unwrap();
        write_frame(&mut to_server, &Request::Write { address: 0 }, &[1, 2, 3]).unwrap();
        write_frame(&mut to_server, &Request::Reset, &[]).unwrap();
        write_frame(&mut to_server, &Request::Close, &[]).unwrap();
        let mut to_client = Vec::new();
        serve(&to_server[..], &mut to_client, &mut Echo).unwrap();

        let mut r = &to_client[..];
        let (resp, data): (Response, _) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!((resp, data), (Response::Data, vec![0xAB; 4]));
        let (resp, _): (Response, Vec<u8>) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(resp, Response::Ok);
        let (resp, _): (Response, Vec<u8>) = read_frame(&mut r).unwrap().unwrap();
        assert!(matches!(
            resp,
            Response::Error {
                kind: ErrorKind::Unsupported,
                ..
            }
        ));
    }

    #[test]
    fn json_shape_is_readable() {
        let s = serde_json::to_string(&Request::Read {
            address: 16,
            len: 4,
        })
        .unwrap();
        assert_eq!(s, r#"{"op":"read","address":16,"len":4}"#);
        let s = serde_json::to_string(&Capability::Vendor("acme.provision".into())).unwrap();
        assert_eq!(s, r#"{"vendor":"acme.provision"}"#);
    }

    #[test]
    fn base_dir_exposes_the_directory_a_value_was_written_in() {
        let values = serde_json::json!({"config": ["a.cfg"], "chip": "X"});
        let mut bases = BTreeMap::new();
        bases.insert("config".to_string(), at("/repo"));
        let o = Options::new(values.as_object().unwrap(), &bases);
        // A list-valued option can resolve its own elements.
        assert_eq!(o.base_dir("config"), Some(PathBuf::from("/repo")));
        // No provenance recorded for this key.
        assert_eq!(o.base_dir("chip"), None);
    }

    #[test]
    fn io_errors_convert_into_device_responses() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let r: Response = e.into();
        match r {
            Response::Error { kind, message, .. } => {
                assert_eq!(kind, ErrorKind::Device);
                assert!(message.contains("nope"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn multi_word_wire_names_are_snake_case() {
        let s = serde_json::to_string(&Request::AppsList).unwrap();
        assert_eq!(s, r#"{"op":"apps_list"}"#);
        let s = serde_json::to_string(&Request::AppsRemove { name: "a".into() }).unwrap();
        assert_eq!(s, r#"{"op":"apps_remove","name":"a"}"#);
        let s = serde_json::to_string(&Capability::MemoryRead).unwrap();
        assert_eq!(s, r#""memory_read""#);
        let s = serde_json::to_string(&ErrorKind::InvalidRequest).unwrap();
        assert_eq!(s, r#""invalid_request""#);

        let open = Request::Open {
            board: "b".into(),
            arch: "cortex-m4".into(),
            flash: FlashInfo {
                start: 0,
                size: 1024,
            },
            options: serde_json::Map::new(),
            option_bases: BTreeMap::new(),
        };
        let s = serde_json::to_string(&open).unwrap();
        assert!(s.contains(r#""option_bases":{}"#), "{s}");
    }

    fn at(base: &str) -> String {
        base.to_string()
    }

    #[test]
    fn get_path_joins_relative_values_onto_the_base() {
        let values = serde_json::json!({"path": "flash.bin", "abs": "/tmp/x.bin", "bare": "y.bin"});
        let mut bases = BTreeMap::new();
        bases.insert("path".to_string(), at("/repo"));
        bases.insert("abs".to_string(), at("/repo"));
        let o = Options::new(values.as_object().unwrap(), &bases);
        assert_eq!(o.get_path("path"), Some(PathBuf::from("/repo/flash.bin")));
        // Absolute values pass through untouched.
        assert_eq!(o.get_path("abs"), Some(PathBuf::from("/tmp/x.bin")));
        // No provenance: the value is used as written.
        assert_eq!(o.get_path("bare"), Some(PathBuf::from("y.bin")));
        assert_eq!(o.get_path("missing"), None);
    }

    #[test]
    fn open_without_option_bases_still_parses() {
        let json = r#"{"op":"open","board":"b","arch":"cortex-m4",
            "flash":{"start":0,"size":1024},"options":{}}"#;
        let r: Request = serde_json::from_str(json).unwrap();
        match r {
            Request::Open { option_bases, .. } => assert!(option_bases.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    /// A `Spawned` dropped without a `Close` having been sent must still
    /// exit. It is the pipe, not the message, that has to end the child --
    /// `serve` stops on either, and only end of stream is ours to
    /// guarantee. Waiting while still holding the pipe open is a deadlock:
    /// the child waits for a request, we wait for the child.
    ///
    /// `cat` stands in for a peer that reads until end of stream. This test
    /// hangs rather than fails if the ordering is wrong, which is why it has
    /// its own clock.
    #[test]
    fn dropping_a_spawned_connection_does_not_hang() {
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::time::Duration;

        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("cat");
        let reader = child.stdout.take().unwrap();
        let writer = child.stdin.take().unwrap();
        let spawned = Spawned::new(reader, writer, child);

        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || {
            drop(spawned);
            let _ = done.send(());
        });
        assert!(
            waited.recv_timeout(Duration::from_secs(10)).is_ok(),
            "dropping a Spawned did not return: the child never saw end of stream"
        );
    }
}
