//! Draft device protocol, spoken between the core, subcommands and
//! transports.
//!
//! A message is a JSON control part plus an optional binary payload, so bulk
//! flash data never goes through JSON. The message layer ([`Request`],
//! [`Response`]) is separate from the carrier: [`StreamChannel`] frames
//! messages over any byte stream (a child process's stdio today), and
//! [`LocalChannel`] calls a handler in-process, for hosts that embed a
//! transport rather than spawning one (WASM, in-browser IDEs).
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
    /// `write` whole pages (the transport erases as needed).
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
    pub page_size: u64,
}

/// Which configuration layer an option came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionLayer {
    Flag,
    Env,
    Device,
    Project,
    User,
    Board,
}

/// Where one resolved option came from. The core never interprets an option's
/// meaning, so a transport that needs to resolve a relative path gets this
/// instead: `base` is an absolute directory, always set, and is the directory
/// of the file the value was written in (or the working directory, for values
/// that came from a flag or the environment).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OptionSource {
    pub layer: OptionLayer,
    pub file: Option<String>,
    pub base: String,
}

/// Resolved options plus their provenance, as a transport sees them.
pub struct Options<'a> {
    values: &'a serde_json::Map<String, serde_json::Value>,
    sources: &'a BTreeMap<String, OptionSource>,
}

impl<'a> Options<'a> {
    pub fn new(
        values: &'a serde_json::Map<String, serde_json::Value>,
        sources: &'a BTreeMap<String, OptionSource>,
    ) -> Options<'a> {
        Options { values, sources }
    }

    pub fn get_str(&self, key: &str) -> Option<&'a str> {
        self.values.get(key).and_then(|v| v.as_str())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.values.get(key).and_then(|v| v.as_bool())
    }

    /// A path option, resolved against the directory of the file it was
    /// written in. Absolute values, and values with no provenance, are used
    /// exactly as written.
    pub fn get_path(&self, key: &str) -> Option<PathBuf> {
        let value = PathBuf::from(self.get_str(key)?);
        if value.is_absolute() {
            return Some(value);
        }
        match self.sources.get(key) {
            Some(source) => Some(PathBuf::from(&source.base).join(value)),
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
        /// Provenance for each key in `options`. Defaulted so a transport
        /// built before this existed still opens.
        #[serde(default)]
        option_sources: BTreeMap<String, OptionSource>,
    },
    /// Payload in the response.
    Read {
        address: u64,
        len: u64,
    },
    /// Payload: page-aligned data, whole pages.
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

/// In-process carrier, for a host that embeds a [`Handler`] instead of
/// spawning `tock-transport-<name>`.
pub struct LocalChannel<H: Handler>(pub H);

impl<H: Handler> Channel for LocalChannel<H> {
    fn call(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        Ok(self.0.handle(request.clone(), payload.to_vec()))
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
    fn local_channel_calls_a_handler_in_process() {
        struct Echo;
        impl Handler for Echo {
            fn handle(&mut self, request: Request, payload: Vec<u8>) -> (Response, Vec<u8>) {
                match request {
                    Request::Read { .. } => (Response::Data, payload),
                    _ => (Response::Ok, vec![]),
                }
            }
        }
        let mut ch = LocalChannel(Echo);
        let (r, data) = ch
            .call_ok(&Request::Read { address: 0, len: 2 }, &[1, 2])
            .unwrap();
        assert_eq!(r, Response::Data);
        assert_eq!(data, vec![1, 2]);
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
                page_size: 512,
            },
            options: serde_json::Map::new(),
            option_sources: BTreeMap::new(),
        };
        let s = serde_json::to_string(&open).unwrap();
        assert!(s.contains(r#""option_sources":{}"#), "{s}");
        assert!(!s.contains("option-sources"), "{s}");
    }

    fn at(base: &str) -> OptionSource {
        OptionSource {
            layer: OptionLayer::Project,
            file: Some(format!("{base}/Tock.toml")),
            base: base.into(),
        }
    }

    #[test]
    fn get_path_joins_relative_values_onto_the_base() {
        let values = serde_json::json!({"path": "flash.bin", "abs": "/tmp/x.bin", "bare": "y.bin"});
        let mut sources = BTreeMap::new();
        sources.insert("path".to_string(), at("/repo"));
        sources.insert("abs".to_string(), at("/repo"));
        let o = Options::new(values.as_object().unwrap(), &sources);
        assert_eq!(o.get_path("path"), Some(PathBuf::from("/repo/flash.bin")));
        // Absolute values pass through untouched.
        assert_eq!(o.get_path("abs"), Some(PathBuf::from("/tmp/x.bin")));
        // No provenance: the value is used as written.
        assert_eq!(o.get_path("bare"), Some(PathBuf::from("y.bin")));
        assert_eq!(o.get_path("missing"), None);
    }

    #[test]
    fn open_without_option_sources_still_parses() {
        let json = r#"{"op":"open","board":"b","arch":"cortex-m4",
            "flash":{"start":0,"size":1024,"page_size":512},"options":{}}"#;
        let r: Request = serde_json::from_str(json).unwrap();
        match r {
            Request::Open { option_sources, .. } => assert!(option_sources.is_empty()),
            other => panic!("{other:?}"),
        }
    }
}
