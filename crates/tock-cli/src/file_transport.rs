//! The `file` transport: a flash image on disk standing in for a board.
//!
//! Replaces Python tockloader's "local board": the file covers the board's
//! whole flash, starting at `layout.flash.start`.
//!
//! Options: `path` (required), `create = true` to create an erased image if
//! the file does not exist.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use tock_proto::{Capability, ErrorKind, FlashInfo, Handler, Request, Response};

#[derive(Default)]
pub struct FileTransport {
    open: Option<(std::fs::File, FlashInfo)>,
}

impl FileTransport {
    fn open(
        &mut self,
        flash: FlashInfo,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Response {
        let Some(path) = options.get("path").and_then(|v| v.as_str()) else {
            return Response::Error {
                kind: ErrorKind::Selection,
                message: "the file transport needs a flash image path".into(),
                suggestion: Some("--opt path=flash.bin --opt create=true".into()),
            };
        };
        let create = options
            .get("create")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let exists = std::path::Path::new(path).exists();
        if !exists && !create {
            return Response::Error {
                kind: ErrorKind::Selection,
                message: format!("flash image `{path}` does not exist"),
                suggestion: Some(format!("--opt path={path} --opt create=true")),
            };
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(path)
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
        file.seek(SeekFrom::Start(address - flash.start))
            .map_err(io)?;
        let mut filled = 0;
        while filled < out.len() {
            match file.read(&mut out[filled..]).map_err(io)? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(out)
    }

    fn write(&mut self, address: u64, data: &[u8]) -> Result<(), Response> {
        let (file, flash) = self.require_open()?;
        check_range(flash, address, data.len() as u64)?;
        if !address.is_multiple_of(flash.page_size)
            || !(data.len() as u64).is_multiple_of(flash.page_size)
        {
            return Err(Response::error(
                ErrorKind::InvalidRequest,
                "writes must be whole pages",
            ));
        }
        file.seek(SeekFrom::Start(address - flash.start))
            .map_err(io)?;
        file.write_all(data).map_err(io)?;
        Ok(())
    }

    fn require_open(&mut self) -> Result<(&mut std::fs::File, &FlashInfo), Response> {
        match &mut self.open {
            Some((f, i)) => Ok((f, i)),
            None => Err(Response::error(ErrorKind::InvalidRequest, "not open")),
        }
    }
}

fn check_range(flash: &FlashInfo, address: u64, len: u64) -> Result<(), Response> {
    if address < flash.start || address + len > flash.start + flash.size {
        return Err(Response::error(
            ErrorKind::InvalidRequest,
            format!("{address:#x}+{len} is outside flash"),
        ));
    }
    Ok(())
}

fn io(e: std::io::Error) -> Response {
    Response::error(ErrorKind::Device, e.to_string())
}

impl Handler for FileTransport {
    fn handle(&mut self, request: Request, payload: Vec<u8>) -> (Response, Vec<u8>) {
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
            Request::Open { flash, options, .. } => Ok((self.open(flash, &options), vec![])),
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
        result.unwrap_or_else(|e| (e, vec![]))
    }
}
