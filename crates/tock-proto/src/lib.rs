//! Draft device protocol, spoken between the core, subcommands and
//! transports.
//!
//! A message is a JSON control part plus an optional binary payload, so bulk
//! flash data never goes through JSON. The message layer ([`Request`],
//! [`Response`]) is separate from the carrier: [`StreamChannel`] frames
//! messages over any byte stream, which today is always a transport child
//! process's stdio.
//!
//! Frame: `u32 LE json_len | json | u32 LE payload_len | payload`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. 0 = draft, no stability promised.
pub const PROTOCOL_VERSION: u32 = 0;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashInfo {
    pub start: u64,
    pub size: u64,
}

/// Resolved options, as a transport sees them, each with the directory its
/// value was written in so a relative path can be made sense of.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed the connection")]
    Closed,
    #[error("{message}")]
    Remote {
        kind: ErrorKind,
        message: String,
        suggestion: Option<String>,
    },
    #[error("unexpected response: {0:?}")]
    Unexpected(Box<Response>),
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

/// Something that answers requests: a transport, or the broker.
pub trait Handler {
    fn handle(&mut self, request: Request, payload: Vec<u8>) -> (Response, Vec<u8>);
}

/// The client side of a connection, independent of the carrier.
pub trait Channel {
    fn call(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError>;

    /// Like `call`, but a `Response::Error` becomes `Err`.
    fn call_ok(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match self.call(request, payload)? {
            (
                Response::Error {
                    kind,
                    message,
                    suggestion,
                },
                _,
            ) => Err(ProtoError::Remote {
                kind,
                message,
                suggestion,
            }),
            ok => Ok(ok),
        }
    }
}

/// Framed carrier over a byte stream.
pub struct StreamChannel<R: Read, W: Write> {
    pub reader: R,
    pub writer: W,
}

impl<R: Read, W: Write> Channel for StreamChannel<R, W> {
    fn call(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        write_frame(&mut self.writer, request, payload)?;
        read_frame(&mut self.reader)?.ok_or(ProtoError::Closed)
    }
}

pub fn write_frame<W: Write, T: Serialize>(
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

/// `Ok(None)` on a clean end of stream.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
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

/// Serve requests from a stream until `Close` or end of stream. Used by
/// transport plugins (`tock-transport-*`) and `tock broker`.
pub fn serve<R: Read, W: Write, H: Handler>(
    mut reader: R,
    mut writer: W,
    handler: &mut H,
) -> Result<(), ProtoError> {
    while let Some((request, payload)) = read_frame::<_, Request>(&mut reader)? {
        let close = request == Request::Close;
        let (response, out) = handler.handle(request, payload);
        write_frame(&mut writer, &response, &out)?;
        if close {
            break;
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Handler for Echo {
        fn handle(&mut self, request: Request, payload: Vec<u8>) -> (Response, Vec<u8>) {
            match request {
                Request::Read { len, .. } => (Response::Data, vec![0xAB; len as usize]),
                Request::Write { .. } => (Response::Ok, vec![]),
                Request::Close => (Response::Ok, vec![]),
                _ => (Response::error(ErrorKind::Unsupported, "nope"), payload),
            }
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
}
