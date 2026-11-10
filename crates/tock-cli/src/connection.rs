//! The core's side of the connection to a device: spawning a
//! `tock-transport-<name>` executable and memory operations on top of the
//! protocol. The core has no transports of its own.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use tock_board::Settings;
use tock_proto::{Client, Connection, FlashInfo, Request, Traced};

use tock_client::{CliError, Code, Spawned, proto};

/// Open the transport named in the context. Every transport is an executable
/// `tock-transport-<name>`, found next to `tock` or on `PATH` -- including the
/// `file` transport, which ships alongside `tock` but is not special.
pub fn open_connection(settings: &Settings, trace: bool) -> Result<Box<dyn Connection>, CliError> {
    let name = &settings.transport.name.value;
    let exe = format!("tock-transport-{name}");
    let path = crate::external::find_executable(&exe).ok_or_else(|| {
        CliError::new(
            Code::Config,
            format!(
                "transport `{name}` (from {}) needs `{exe}`, which was not found \
                 next to `tock` or on PATH",
                settings.transport.name.source
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
    let reader = child.stdout.take().expect("piped");
    let writer = child.stdin.take().expect("piped");
    let spawned = Spawned::new(reader, writer, child);
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
/// The result is an ordinary [`Client`] -- the same thing a subcommand gets
/// from the connection it inherits. The difference is only that we started
/// the transport and it did not.
pub fn open(settings: &Settings, trace: bool) -> Result<Client, CliError> {
    let connection = open_connection(settings, trace)?;
    let layout = &settings.board.layout;
    let flash = FlashInfo {
        start: layout.flash.start.0,
        size: layout.flash.size.0,
    };
    let mut device =
        Client::open(connection, concat!("tock ", env!("CARGO_PKG_VERSION"))).map_err(proto)?;

    let options = serde_json::to_value(settings.transport_options()).unwrap();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    device
        .call(
            Request::Open {
                board: settings.board.info.name.clone(),
                arch: settings.board.info.arch.clone(),
                flash,
                options: options.as_object().cloned().unwrap_or_default(),
                option_bases: settings.option_bases(&cwd),
            },
            vec![],
        )
        .map_err(proto)?;
    Ok(device)
}
