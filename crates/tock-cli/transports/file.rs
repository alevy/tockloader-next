//! `tock-transport-file`: a flash image on disk standing in for a board.
//!
//! Replaces Python tockloader's "local board": the file covers the board's
//! whole flash, starting at `layout.flash.start`.
//!
//! Options: `path` (required), `create = true` to create an erased image if
//! the file does not exist. A relative `path` is resolved against the file it
//! was written in -- see `tock_proto::Options::get_path`.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use tock_proto::{Capability, Connection, ErrorKind, FlashInfo, Options, Request, Response};

#[derive(Default)]
pub struct FileTransport {
    open: Option<(std::fs::File, FlashInfo)>,
}

impl FileTransport {
    fn open(&mut self, flash: FlashInfo, options: Options<'_>) -> Response {
        // `get_path`, not `get_str`: a relative path belongs to the file it was
        // written in, not to whatever directory the command was run from.
        let Some(path) = options.get_path("path") else {
            return Response::Error {
                kind: ErrorKind::Selection,
                message: "the file transport needs a flash image path".into(),
                suggestion: Some(
                    "add --opt path=flash.bin --opt create=true\n\
                     or add `[transports.file]` with `path = \"flash.bin\"` to a Tock.toml"
                        .into(),
                ),
            };
        };
        let path = path.display().to_string();
        let create = options
            .get("create")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let exists = std::path::Path::new(&path).exists();
        if !exists && !create {
            return Response::Error {
                kind: ErrorKind::Selection,
                message: format!("flash image `{path}` does not exist"),
                suggestion: Some(
                    "add --opt create=true to create it\n\
                     or point `path` at an image that exists, in Tock.toml's \
                     `[transports.file]` or with --opt path=..."
                        .into(),
                ),
            };
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => return Response::error(ErrorKind::Device, format!("{path}: {e}")),
        };
        if !exists {
            let mut f = &file;
            if let Err(e) = f.write_all(&vec![0xFF; flash.size as usize]) {
                return Response::error(ErrorKind::Device, format!("{path}: {e}"));
            }
        }
        // Files shorter than flash read as erased; writes extend them.
        self.open = Some((file, flash));
        Response::Ok
    }

    fn read(&mut self, address: u64, len: u64) -> Result<Vec<u8>, Response> {
        let (file, flash) = self.require_open()?;
        check_range(flash, address, len)?;
        let mut out = vec![0xFF; len as usize];
        file.seek(SeekFrom::Start(address - flash.start))?;
        let mut filled = 0;
        while filled < out.len() {
            match file.read(&mut out[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(out)
    }

    fn write(&mut self, address: u64, data: &[u8]) -> Result<(), Response> {
        let (file, flash) = self.require_open()?;
        check_range(flash, address, data.len() as u64)?;
        file.seek(SeekFrom::Start(address - flash.start))?;
        file.write_all(data)?;
        Ok(())
    }

    fn require_open(&mut self) -> Result<(&mut std::fs::File, &FlashInfo), Response> {
        match &mut self.open {
            Some((f, i)) => Ok((f, i)),
            None => Err(Response::error(ErrorKind::InvalidRequest, "not open")),
        }
    }
}

/// Keep an access inside the image. The broker checks this too, with a better
/// message, but a transport is a separate executable: anyone who can spawn it
/// can talk to it, with no broker in front.
fn check_range(flash: &FlashInfo, address: u64, len: u64) -> Result<(), Response> {
    if address < flash.start || address + len > flash.start + flash.size {
        return Err(Response::error(
            ErrorKind::InvalidRequest,
            format!("{address:#x}+{len} is outside flash"),
        ));
    }
    Ok(())
}

impl Connection for FileTransport {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), tock_proto::ProtoError> {
        let result = match request {
            Request::Hello { .. } => Ok((
                Response::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    transport: "file".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: vec![Capability::MemoryRead, Capability::MemoryWrite],
                },
                vec![],
            )),
            Request::Open {
                flash,
                options,
                option_bases,
                ..
            } => Ok((
                self.open(flash, Options::new(&options, &option_bases)),
                vec![],
            )),
            Request::Read { address, len } => self.read(address, len).map(|d| (Response::Data, d)),
            Request::Write { address } => self
                .write(address, &payload)
                .map(|_| (Response::Ok, vec![])),
            Request::Close => {
                self.open = None;
                Ok((Response::Ok, vec![]))
            }
            other => Err(Response::error(
                ErrorKind::Unsupported,
                format!("file transport does not support {other:?}"),
            )),
        };
        Ok(result.unwrap_or_else(|e| (e, vec![])))
    }
}

fn main() {
    let mut transport = FileTransport::default();
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(e) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-file: {e}");
        std::process::exit(1);
    }
}
