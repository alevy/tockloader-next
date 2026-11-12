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
//!
//!    The set is deliberately small, and is meant to grow only alongside
//!    something that uses it. A message nobody sends and nobody implements
//!    is not a head start -- it is a design decision taken in advance of the
//!    thing that would have informed it, which is how `apps_install` came to
//!    assume one TBF at a time before any app-speaking transport existed.
//! 4. [`Request::Close`] -- the responder **releases the device** and stops.
//!    Releasing it means leaving it able to run: a transport that stopped a
//!    core to do its work has to start it again, and one that changed flash
//!    should reset so the device runs what is now on it. Hanging up with the
//!    target halted is not releasing it, however tidy the shutdown.
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
        /// What the caller intends to do, so a responder that routes can
        /// decide where to send it before anything is opened.
        ///
        /// A caller declares this once rather than discovering it a refusal
        /// at a time: `tock broker` may have several transports available --
        /// flash over a probe, the console over a bridge soldered beside it
        /// -- and nothing but the caller knows which of them this command
        /// will need. Empty asks for nothing in particular.
        #[serde(default)]
        needs: Vec<Capability>,
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
    /// Read whatever the device has said, waiting up to `timeout_ms` for
    /// something to arrive. An empty payload means it stayed quiet, which is
    /// an answer and not a failure. Payload in the response.
    ///
    /// Polled rather than pushed because the carrier is strictly
    /// synchronous: one response per request, so a responder can never speak
    /// unprompted. Blocking in the responder for a short while is what keeps
    /// that from becoming a busy loop -- the waiting happens where the file
    /// descriptor is, and the caller is free in between to send what the
    /// user has typed.
    ConsoleRead {
        max: u64,
        timeout_ms: u64,
    },
    /// Payload: bytes to send to the device, as if typed at it.
    ConsoleWrite,
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
        version: String,
        capabilities: Vec<Capability>,
        /// Every option this transport reads. See [`OptionSpec`].
        #[serde(default)]
        options: Vec<OptionSpec>,
    },
    Ok,
    /// Payload carries the data.
    Data,
    Vendor {
        value: serde_json::Value,
    },
    Error {
        kind: ErrorKind,
        message: String,
        /// A command the user can run next, if the transport knows one.
        suggestion: Option<String>,
        /// Context the user did not supply and could not have known -- which
        /// file set the option that failed, say. Distinct from `suggestion`,
        /// which is a thing to go and do: rendering provenance as "try ..."
        /// reads as advice to do what is already the case.
        #[serde(default)]
        details: Vec<String>,
        /// Which resolved options this is about, if any. More than one when
        /// it is their combination that is wrong.
        ///
        /// The transport names the key because only it knows what a good
        /// value is; the caller resolved that key and knows which file wrote
        /// it, so the caller is what says where it came from. Neither end
        /// can produce that sentence alone.
        #[serde(default)]
        keys: Vec<String>,
    },
}

impl Response {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Response {
        Response::Error {
            kind,
            message: message.into(),
            suggestion: None,
            details: Vec::new(),
            keys: Vec::new(),
        }
    }

    /// Add context lines, which print above any suggestion.
    pub fn detailed(self, details: impl IntoIterator<Item = impl Into<String>>) -> Response {
        match self {
            Response::Error {
                kind,
                message,
                suggestion,
                keys,
                ..
            } => Response::Error {
                kind,
                message,
                suggestion,
                details: details.into_iter().map(Into::into).collect(),
                keys,
            },
            other => other,
        }
    }

    /// Name the options this error is about, so the caller can say where
    /// each was written. A no-op on any response that is not an error.
    pub fn about(self, keys: impl IntoIterator<Item = impl Into<String>>) -> Response {
        match self {
            Response::Error {
                kind,
                message,
                suggestion,
                details,
                ..
            } => Response::Error {
                kind,
                message,
                suggestion,
                details,
                keys: keys.into_iter().map(Into::into).collect(),
            },
            other => other,
        }
    }

    /// Attach the line the CLI prints as `try: ...`. A no-op on any response
    /// that is not an error.
    pub fn with_suggestion(self, hint: impl Into<String>) -> Response {
        match self {
            Response::Error {
                kind,
                message,
                details,
                keys,
                ..
            } => Response::Error {
                kind,
                message,
                suggestion: Some(hint.into()),
                details,
                keys,
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
    Reset,
    /// `console_read` and `console_write`: a byte stream to and from the
    /// running device. Not every transport that can reach a board's flash
    /// can reach its console, and the reverse holds too -- a USB-to-UART
    /// bridge is a console and nothing else.
    Console,
    /// Namespaced vendor operations, passed through untouched.
    Vendor(String),
}

impl Client {
    /// The keys among `given` that the far end never declared.
    ///
    /// Sorted, so an error lists them the same way twice.
    pub fn undeclared<'a>(&self, given: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut unknown: Vec<String> = given
            .into_iter()
            .filter(|key| !self.options.iter().any(|o| o.name == *key))
            .map(str::to_string)
            .collect();
        unknown.sort();
        unknown
    }
}

impl Capability {
    /// The name this goes by on the wire, for a message or a listing.
    pub fn name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Object(map) => map.keys().next().cloned(),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// One option a transport reads, as it declares itself in [`Response::Hello`].
///
/// **A transport must declare every option it reads.** The caller refuses a
/// key that is not here, so an option a transport forgets to declare is an
/// option it will never be given. An empty list means it takes none.
///
/// Names and prose, not types. A declared type would only catch a subset of
/// wrong -- `page_size = 5000` is a perfectly good integer and still useless
/// -- so the value is the transport's to judge, at [`Request::Open`], where
/// it answers with the offending key in [`Response::Error::keys`]. What is
/// declared here is the part that is genuinely static: which keys exist. The
/// caller is the only end that can check those, because it is the end that
/// resolved them, and it can do so before the transport has touched a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionSpec {
    /// The key, exactly as it appears in `[transports.<name>]` and in
    /// `--opt <key>=<value>`.
    pub name: String,
    /// One line, for a listing and for an error that says what is available.
    pub description: String,
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
        /// Context, as on [`Response::Error::details`].
        details: Vec<String>,
    },
}

impl ProtoError {
    pub fn failed(message: impl Into<String>) -> ProtoError {
        ProtoError::Failed {
            kind: ErrorKind::Device,
            message: message.into(),
            suggestion: None,
            details: Vec::new(),
        }
    }

    /// Add context lines, which print above any suggestion.
    pub fn detailed(self, details: impl IntoIterator<Item = impl Into<String>>) -> ProtoError {
        match self {
            ProtoError::Failed {
                kind,
                message,
                suggestion,
                ..
            } => ProtoError::Failed {
                kind,
                message,
                suggestion,
                details: details.into_iter().map(Into::into).collect(),
            },
            other => other,
        }
    }

    /// Classify it as something other than a device failure.
    pub fn with_kind(self, kind: ErrorKind) -> ProtoError {
        match self {
            ProtoError::Failed {
                message,
                suggestion,
                details,
                ..
            } => ProtoError::Failed {
                kind,
                message,
                suggestion,
                details,
            },
            other => other,
        }
    }

    /// Attach the line the CLI prints as `try: ...`.
    pub fn with_suggestion(self, hint: impl Into<String>) -> ProtoError {
        match self {
            ProtoError::Failed {
                kind,
                message,
                details,
                ..
            } => ProtoError::Failed {
                kind,
                message,
                suggestion: Some(hint.into()),
                details,
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

/// The requesting end of a connection, with the rules it owes the other.
///
/// [`serve`] is the responding end's loop; this is its opposite number. Greet
/// before anything else, refuse to proceed on a version mismatch, read the
/// capabilities off the reply and check them before asking -- all of that is
/// the protocol, not a style, and a client in another language has to do the
/// same. [`Options`] is already here for the same reason on the other side.
///
/// Errors here are [`ProtoError`]. A caller that reports to a user maps them
/// to whatever vocabulary it has; `tock-client` turns them into exit codes.
pub struct Client {
    /// For a relay, which wants [`Connection::call`] and its two kinds of
    /// failure kept apart. Everything else should use [`Client::call`].
    pub connection: Box<dyn Connection>,
    /// The far end's own version, as it reported it.
    pub version: String,
    pub capabilities: Vec<Capability>,
    /// Every option the far end said it reads. See [`OptionSpec`].
    pub options: Vec<OptionSpec>,
}

impl Client {
    /// Greet whatever is on the other end and learn what it can do.
    ///
    /// `client` is who is talking, for the far end's logs -- the program,
    /// not this crate, which is why it is asked for rather than assumed.
    pub fn open(
        mut connection: Box<dyn Connection>,
        client: &str,
        needs: &[Capability],
    ) -> Result<Client, ProtoError> {
        let hello = Request::Hello {
            protocol: PROTOCOL_VERSION,
            client: client.to_string(),
            needs: needs.to_vec(),
        };
        match collapse(connection.call(hello, vec![]))? {
            (
                Response::Hello {
                    protocol,
                    version,
                    capabilities,
                    options,
                },
                _,
            ) => {
                if protocol != PROTOCOL_VERSION {
                    return Err(ProtoError::failed(format!(
                        "the far end speaks protocol {protocol}, this tool speaks \
                         {PROTOCOL_VERSION}"
                    ))
                    .with_kind(ErrorKind::Device));
                }
                Ok(Client {
                    connection,
                    version,
                    capabilities,
                    options,
                })
            }
            (other, _) => Err(ProtoError::Unexpected(Box::new(other))),
        }
    }

    /// Send a request and treat a refusal as this command's failure.
    ///
    /// The distinction [`Connection`] keeps is spent here, deliberately: a
    /// caller asking for bytes has no use for it. A **relay** must not do
    /// this -- a refusal is its peer's to see -- which is what
    /// [`Client::connection`] is for.
    pub fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        collapse(self.connection.call(request, payload))
    }

    /// Fail clearly if the device cannot do what this command needs.
    pub fn require(&self, capability: Capability, doing: &str) -> Result<(), ProtoError> {
        if self.capabilities.contains(&capability) {
            return Ok(());
        }
        let name = capability.name();
        Err(ProtoError::failed(format!(
            "`{doing}` needs the `{name}` capability, which this connection does not provide"
        ))
        .with_kind(ErrorKind::Unsupported)
        .with_suggestion("tock transports -- to see what each one can do"))
    }

    /// Read byte ranges, one request each, and insist on being answered in
    /// full.
    ///
    /// The short-read check is the point: a caller looping until it has the
    /// bytes it asked for would otherwise ask for the same ones forever.
    /// Merging nearby ranges is *not* here -- that is a workload decision,
    /// and a client that issues one read per range is slow, not wrong.
    pub fn read_ranges(
        &mut self,
        ranges: &[(u64, u64)],
    ) -> Result<Vec<(u64, Vec<u8>)>, ProtoError> {
        let mut out = Vec::new();
        for &(address, len) in ranges {
            let (_, data) = self.call(Request::Read { address, len }, vec![])?;
            if data.len() as u64 != len {
                return Err(ProtoError::failed(format!(
                    "{} of the {len} bytes asked for at {address:#x} came back",
                    data.len()
                ))
                .with_kind(ErrorKind::Device));
            }
            out.push((address, data));
        }
        Ok(out)
    }
}

impl Drop for Client {
    /// Say goodbye, so the far end stops for a stated reason rather than by
    /// noticing a closed pipe. [`Spawned`] handles the case where we cannot.
    fn drop(&mut self) {
        let _ = self.connection.call(Request::Close, vec![]);
    }
}

/// Turn "the operation failed" into "this failed", for a caller that has no
/// use for the difference.
fn collapse(
    result: Result<(Response, Vec<u8>), ProtoError>,
) -> Result<(Response, Vec<u8>), ProtoError> {
    match result? {
        (
            Response::Error {
                kind,
                message,
                suggestion,
                details,
                ..
            },
            _,
        ) => {
            let error = ProtoError::failed(message)
                .with_kind(kind)
                .detailed(details);
            Err(match suggestion {
                Some(hint) => error.with_suggestion(hint),
                None => error,
            })
        }
        ok => Ok(ok),
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
                details,
            } => Response::Error {
                kind,
                message,
                suggestion,
                details,
                keys: Vec::new(),
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
mod tests;
