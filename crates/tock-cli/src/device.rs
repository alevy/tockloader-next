//! The core's side of the device connection: spawning a `tock-transport-<name>`
//! executable and memory operations on top of the protocol. The core has no
//! transports of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tock_board::Context;
use tock_image::PageWrite;
use tock_proto::{
    Capability, Channel, FlashInfo, OptionLayer, OptionSource, ProtoError, Request, Response,
    StreamChannel,
};

use crate::error::{CliError, Code};

pub struct Device {
    channel: Box<dyn Channel>,
    child: Option<Child>,
    pub transport: String,
    pub capabilities: Vec<Capability>,
    page_size: u64,
}

/// Where each resolved option was written, for transports that need to make
/// sense of a relative value. The core never decides what an option *means*,
/// so this is all it can offer: `base` is absolute, because a plugin or a
/// `tock broker` client does not reliably share our working directory.
fn option_sources(ctx: &Context, cwd: &Path) -> BTreeMap<String, OptionSource> {
    // A board reference like `bundled:nrf52840dk` is not a file and has no
    // directory; values from one cannot be machine-specific paths anyway.
    let as_file = |origin: &str| (!origin.starts_with("bundled:")).then(|| origin.to_string());
    let dir_of = |origin: Option<&String>| {
        let Some(path) = origin.map(Path::new) else {
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
            let (layer, file) = match &sourced.source {
                tock_board::Source::Flag(_) => (OptionLayer::Flag, None),
                tock_board::Source::Env(_) => (OptionLayer::Env, None),
                tock_board::Source::Device(_) => (OptionLayer::Device, ctx.user.clone()),
                tock_board::Source::Project(o) => (OptionLayer::Project, Some(o.clone())),
                tock_board::Source::User(o) => (OptionLayer::User, Some(o.clone())),
                tock_board::Source::Board(o) => (OptionLayer::Board, as_file(o)),
            };
            let base = dir_of(file.as_ref()).display().to_string();
            (key.clone(), OptionSource { layer, file, base })
        })
        .collect()
}

/// Logs every message to stderr (`--trace-protocol`).
struct Traced(Box<dyn Channel>);

impl Channel for Traced {
    fn call(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        eprintln!(
            "-> {} (+{} bytes)",
            serde_json::to_string(request).unwrap(),
            payload.len()
        );
        let r = self.0.call(request, payload);
        match &r {
            Ok((resp, data)) => eprintln!(
                "<- {} (+{} bytes)",
                serde_json::to_string(resp).unwrap(),
                data.len()
            ),
            Err(e) => eprintln!("<- error: {e}"),
        }
        r
    }
}

/// Open the transport named in the context. Every transport is an executable
/// `tock-transport-<name>`, found next to `tock` or on `PATH` -- including the
/// `file` transport, which ships alongside `tock` but is not special.
pub fn open_channel(
    ctx: &Context,
    trace: bool,
) -> Result<(Box<dyn Channel>, Option<Child>), CliError> {
    let name = &ctx.transport.name.value;
    let exe = format!("tock-transport-{name}");
    let path = crate::find_executable(&exe).ok_or_else(|| {
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
    let channel = StreamChannel {
        reader: child.stdout.take().unwrap(),
        writer: child.stdin.take().unwrap(),
    };
    let channel: Box<dyn Channel> = if trace {
        Box::new(Traced(Box::new(channel)))
    } else {
        Box::new(channel)
    };
    Ok((channel, Some(child)))
}

impl Device {
    pub fn connect(ctx: &Context, trace: bool) -> Result<Device, CliError> {
        let (mut channel, child) = open_channel(ctx, trace)?;
        let hello = Request::Hello {
            protocol: tock_proto::PROTOCOL_VERSION,
            client: format!("tock {}", env!("CARGO_PKG_VERSION")),
        };
        let (transport, capabilities) = match channel.call_ok(&hello, &[]).map_err(proto)? {
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
                            "transport `{transport}` speaks protocol {protocol}, tock speaks {}",
                            tock_proto::PROTOCOL_VERSION
                        ),
                    ));
                }
                (transport, capabilities)
            }
            (other, _) => return Err(proto(ProtoError::Unexpected(Box::new(other)))),
        };
        let layout = &ctx.board.layout;
        let flash = FlashInfo {
            start: layout.flash.start.0,
            size: layout.flash.size.0,
            page_size: layout.flash.page_size.0,
        };
        let options = serde_json::to_value(ctx.transport_options()).unwrap();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let open = Request::Open {
            board: ctx.board.board.name.clone(),
            arch: ctx.board.board.arch.clone(),
            flash,
            options: options.as_object().cloned().unwrap_or_default(),
            option_sources: option_sources(ctx, &cwd),
        };
        channel.call_ok(&open, &[]).map_err(proto)?;
        Ok(Device {
            channel,
            child,
            transport,
            capabilities,
            page_size: layout.flash.page_size.0,
        })
    }

    /// Fail clearly if the transport cannot do what a command needs.
    pub fn require(&self, cap: Capability, command: &str) -> Result<(), CliError> {
        if self.capabilities.contains(&cap) {
            return Ok(());
        }
        Err(CliError::new(
            Code::Capability,
            format!(
                "`{command}` needs the `{}` capability, which transport `{}` does not have",
                serde_json::to_value(&cap).unwrap().as_str().unwrap_or("?"),
                self.transport
            ),
        )
        .hint("pick a transport that has it with --transport <name>"))
    }

    /// Read whole pages, merging adjacent pages into one request.
    pub fn read_pages(&mut self, pages: &[u64]) -> Result<Vec<(u64, Vec<u8>)>, CliError> {
        let mut out = Vec::new();
        for (start, count) in runs(pages, self.page_size) {
            let (_, data) = self
                .channel
                .call_ok(
                    &Request::Read {
                        address: start,
                        len: count * self.page_size,
                    },
                    &[],
                )
                .map_err(proto)?;
            for (i, chunk) in data.chunks(self.page_size as usize).enumerate() {
                out.push((start + i as u64 * self.page_size, chunk.to_vec()));
            }
        }
        Ok(out)
    }

    pub fn write_pages(&mut self, writes: &[PageWrite]) -> Result<(), CliError> {
        let addresses: Vec<u64> = writes.iter().map(|w| w.address).collect();
        let mut i = 0;
        for (start, count) in runs(&addresses, self.page_size) {
            let data: Vec<u8> = writes[i..i + count as usize]
                .iter()
                .flat_map(|w| w.data.iter().copied())
                .collect();
            self.channel
                .call_ok(&Request::Write { address: start }, &data)
                .map_err(proto)?;
            i += count as usize;
        }
        Ok(())
    }

    /// Forward a raw request (used by `tock broker`).
    pub fn forward(
        &mut self,
        request: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        self.channel.call(request, payload)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.channel.call(&Request::Close, &[]);
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}

/// Group sorted page addresses into `(start, page_count)` runs.
fn runs(pages: &[u64], page_size: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &p in pages {
        match out.last_mut() {
            Some((s, n)) if *s + *n * page_size == p => *n += 1,
            _ => out.push((p, 1)),
        }
    }
    out
}

pub fn proto(e: ProtoError) -> CliError {
    match e {
        ProtoError::Remote {
            message,
            suggestion,
            ..
        } => {
            let mut err = CliError::new(Code::Device, message);
            // A transport owns the whole sentence, and may offer more than one
            // remedy: one hint per line.
            for hint in suggestion.iter().flat_map(|s| s.lines()) {
                err = err.hint(hint);
            }
            err
        }
        e => CliError::new(Code::Device, e.to_string()),
    }
}
