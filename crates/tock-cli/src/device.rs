//! The core's side of the device connection: spawning a `tock-transport-<name>`
//! executable and memory operations on top of the protocol. The core has no
//! transports of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tock_board::Context;
use tock_image::{COALESCE_GAP, WriteRun, coalesce};
use tock_proto::{Capability, Channel, FlashInfo, ProtoError, Request, Response, StreamChannel};

use crate::error::{CliError, Code};

pub struct Device {
    channel: Box<dyn Channel>,
    child: Option<Child>,
    pub transport: String,
    pub capabilities: Vec<Capability>,
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
        };
        let options = serde_json::to_value(ctx.transport_options()).unwrap();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let open = Request::Open {
            board: ctx.board.board.name.clone(),
            arch: ctx.board.board.arch.clone(),
            flash,
            options: options.as_object().cloned().unwrap_or_default(),
            option_bases: option_bases(ctx, &cwd),
        };
        channel.call_ok(&open, &[]).map_err(proto)?;
        Ok(Device {
            channel,
            child,
            transport,
            capabilities,
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

    /// Read byte ranges, merging nearby ones into one request. Returns the
    /// bytes for each range as issued, which may be wider than asked for.
    pub fn read_ranges(&mut self, ranges: &[(u64, u64)]) -> Result<Vec<(u64, Vec<u8>)>, CliError> {
        let mut out = Vec::new();
        for (address, len) in coalesce(ranges, COALESCE_GAP) {
            let (_, data) = self
                .channel
                .call_ok(&Request::Read { address, len }, &[])
                .map_err(proto)?;
            // A short reply would leave the caller's read loop asking for the
            // same bytes forever. Say so instead of hanging.
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

    /// Write byte runs. `render` has already coalesced them, so each is sent
    /// as it stands; erase granularity is the transport's business.
    pub fn write_runs(&mut self, runs: &[WriteRun]) -> Result<(), CliError> {
        for run in runs {
            self.channel
                .call_ok(
                    &Request::Write {
                        address: run.address,
                    },
                    &run.data,
                )
                .map_err(proto)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers every read with fewer bytes than asked for.
    struct Stingy;
    impl Channel for Stingy {
        fn call(
            &mut self,
            _request: &Request,
            _payload: &[u8],
        ) -> Result<(Response, Vec<u8>), ProtoError> {
            Ok((Response::Data, vec![0xFF; 4]))
        }
    }

    #[test]
    fn a_short_read_is_an_error_not_a_silent_retry() {
        let mut device = Device {
            channel: Box::new(Stingy),
            child: None,
            transport: "stingy".into(),
            capabilities: vec![Capability::MemoryRead],
        };
        // `with_reads` would ask again forever if this came back Ok.
        let err = device.read_ranges(&[(0x1000, 64)]).unwrap_err();
        assert!(format!("{err:?}").contains("4"), "{err:?}");
    }
}
