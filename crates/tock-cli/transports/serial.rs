//! `tock-transport-serial`: a board's console, over a serial port.
//!
//! The console is not always reachable the same way the flash is. On a board
//! with a USB-to-UART bridge soldered on, the bridge is a console and nothing
//! else -- it appears as its own USB device, with no path to flash at all,
//! while the debug adapter next to it has no path to the console. So this
//! transport declares [`Capability::Console`] and nothing more, and a
//! command that wants the console asks for it by name:
//! `tock --transport serial console`.
//!
//! The port is a fact about a desk, not about a board: which `/dev/tty*` a
//! bridge landed on depends on what else is plugged in and in what order. It
//! belongs in a user config. The baud rate is the board's, and belongs in a
//! board file.
//!
//! Output is pushed: a `ConsoleAttach` puts the port's reads on a thread of
//! their own ([`device_loop`]) that writes [`ConsoleSink`] pushes until a
//! `ConsoleDetach` stops it.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tock_proto::{
    Capability, Connection, ConsoleSink, ErrorKind, OptionSpec, Options, Request, Response,
};

/// Baud when a board file does not say. What Tock's console uses.
const DEFAULT_BAUD: u64 = 115_200;

/// Gap between bytes sent to the device, unless a board says otherwise.
///
/// Not a guess at the wire -- 115200 carries a byte in 87us. It is a guess
/// at the far end. A UART with no flow control and a shallow receive buffer
/// drops whatever arrives while it is busy with the last byte, and a Tock
/// board servicing radio interrupts is busy often. Measured on an STM32WLE5
/// running a LoRaWAN app: bytes written one at a time arrive intact, and a
/// five-byte write loses all but the first.
///
/// It costs nothing for the case that matters. Console input is a person
/// typing, and 5ms a byte is 200 a second -- far faster than anyone types,
/// and a pasted command of forty characters lands in a fifth of a second.
const DEFAULT_PACE_MS: u64 = 5;

/// How often the device thread rechecks its stop flag while attached.
///
/// This timeout trades off how long a `detach` waits to join that thread
/// against how often an idle port wakes it for nothing.
const ATTACHED_POLL_MS: u64 = 100;

/// Every option this transport reads.
fn options() -> Vec<OptionSpec> {
    [
        ("port", "the serial device, e.g. /dev/ttyUSB0"),
        ("baud", "bits per second (default 115200)"),
        (
            "write_pace_ms",
            "gap between sent bytes, for a UART with no flow control (default 5)",
        ),
    ]
    .into_iter()
    .map(|(name, description)| OptionSpec {
        name: name.into(),
        description: description.into(),
    })
    .collect()
}

/// The termios constant for a baud rate. Only the rates a Tock board is
/// plausibly set to; an unlisted one is refused rather than silently
/// becoming something else, which is the failure that looks like a broken
/// cable.
fn baud_constant(baud: u64) -> Option<libc::speed_t> {
    Some(match baud {
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        921_600 => libc::B921600,
        _ => return None,
    })
}

/// What `ConsoleAttach` spawned, and how to stop it.
struct Attached {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
pub struct SerialTransport {
    port: Option<std::fs::File>,
    pace: std::time::Duration,
    /// Where to push console bytes, once
    /// [`serve_streaming`](tock_proto::serve_streaming) hands one over.
    console_sink: Option<ConsoleSink>,
    /// The device thread, while one is running.
    attached: Option<Attached>,
}

impl SerialTransport {
    fn open(&mut self, options: Options<'_>) -> Response {
        // `get_path`, not `get_str`: a port could reasonably be written
        // relative to a config, and this costs nothing when it is not.
        let Some(path) = options.get_path("port") else {
            return Response::error(ErrorKind::Selection, "the serial transport needs a port")
                .with_suggestion(
                    "add --opt port=/dev/ttyUSB0\n\
                     or put it in your user config: [transports.serial] port = \"/dev/ttyUSB0\"",
                )
                .about(["port"]);
        };
        let baud = options
            .get("baud")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_BAUD);
        let Some(speed) = baud_constant(baud) else {
            return Response::error(
                ErrorKind::Selection,
                format!("{baud} is not a baud rate this can set"),
            )
            .with_suggestion("try one of 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600")
            .about(["baud"]);
        };

        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => {
                let shown = path.display();
                return Response::error(ErrorKind::Selection, format!("{shown}: {e}"))
                    .with_suggestion(
                        "check the device exists and that you can use it \
                         (on Linux that usually means the `dialout` group)",
                    )
                    .about(["port"]);
            }
        };
        if let Err(e) = configure(file.as_raw_fd(), speed) {
            return Response::error(ErrorKind::Device, format!("{}: {e}", path.display()));
        }
        self.pace = std::time::Duration::from_millis(
            options
                .get("write_pace_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PACE_MS),
        );
        // A prior `ConsoleAttach`'s thread reads a clone of the old port, so
        // it has to stop before the port is replaced -- and only now the new
        // one is ready, so a failed re-`Open` never tears down a live one.
        let _ = self.detach();
        self.port = Some(file);
        Response::Ok
    }

    fn port(&mut self) -> Result<&mut std::fs::File, Response> {
        self.port
            .as_mut()
            .ok_or_else(|| Response::error(ErrorKind::InvalidRequest, "not open"))
    }

    /// Send bytes, one at a time, with [`DEFAULT_PACE_MS`] between them.
    ///
    /// A whole-buffer write would be correct on any link with flow control
    /// and is what the wire can carry, but the device on the other end of a
    /// bare UART cannot keep up with itself: see `write_pace_ms`. Pacing
    /// here rather than in the client because this is the end that knows
    /// what it is talking to.
    fn write(&mut self, data: &[u8]) -> Result<(), Response> {
        use std::io::Write as _;
        let pace = self.pace;
        let port = self.port()?;
        let device = |e: std::io::Error| Response::error(ErrorKind::Device, e.to_string());
        if pace.is_zero() {
            return port
                .write_all(data)
                .and_then(|()| port.flush())
                .map_err(device);
        }
        for (i, byte) in data.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(pace);
            }
            port.write_all(&[*byte])
                .and_then(|()| port.flush())
                .map_err(device)?;
        }
        Ok(())
    }

    /// Start pushing whatever the port says, until [`Self::detach`].
    ///
    /// Reads a [`try_clone`](std::fs::File::try_clone) of the port, not a
    /// second `open()`: opening again runs [`configure`], whose `tcflush`
    /// would discard what the board said before anyone attached.
    fn attach(&mut self) -> Result<(), Response> {
        if self.attached.is_some() {
            return Err(Response::error(
                ErrorKind::InvalidRequest,
                "the console is already attached",
            ));
        }
        let Some(sink) = self.console_sink.clone() else {
            // Only reachable on a connection not served by `serve_streaming`.
            return Err(Response::error(
                ErrorKind::InvalidRequest,
                "console streaming is not available on this connection",
            ));
        };
        let reader = self
            .port()?
            .try_clone()
            .map_err(|e| Response::error(ErrorKind::Device, e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn({
            let stop = stop.clone();
            move || device_loop(reader, sink, stop)
        });
        self.attached = Some(Attached { stop, handle });
        Ok(())
    }

    /// Stop pushing, and wait for the device thread to actually stop before
    /// returning, so no push can follow the `Ok`.
    ///
    /// Detaching when nothing is attached is not an error.
    fn detach(&mut self) -> Result<(), Response> {
        if let Some(attached) = self.attached.take() {
            attached.stop.store(true, Ordering::Relaxed);
            // A panicked device thread has already stopped pushing.
            let _ = attached.handle.join();
        }
        Ok(())
    }
}

/// Raw mode at `speed`: no echo, no line discipline, no translation. A
/// console is a byte stream, and every convenience a terminal would add --
/// mapping carriage returns, buffering by line, interpreting control
/// characters -- corrupts it.
fn configure(fd: RawFd, speed: libc::speed_t) -> std::io::Result<()> {
    // SAFETY: `fd` is an open descriptor for the life of this call, and
    // `termios` is written by the kernel before it is read.
    unsafe {
        let mut settings: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut settings) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut settings);
        if libc::cfsetispeed(&mut settings, speed) != 0
            || libc::cfsetospeed(&mut settings, speed) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // Read returns as soon as anything is there. The waiting is done by
        // `poll`, which can be given a deadline; VTIME cannot express one
        // shorter than a decisecond.
        settings.c_cc[libc::VMIN] = 0;
        settings.c_cc[libc::VTIME] = 0;
        // Do not let opening the port assert modem control lines in a way
        // that resets the board, and do not block on carrier detect.
        settings.c_cflag |= libc::CLOCAL | libc::CREAD;
        if libc::tcsetattr(fd, libc::TCSANOW, &settings) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Throw away anything buffered from before we were looking, so the
        // first read is the device talking now and not a stale tail.
        libc::tcflush(fd, libc::TCIFLUSH);
    }
    Ok(())
}

/// Whether the descriptor has something to read within `timeout_ms`.
///
/// `POLLIN` only: a tty hangup sets it too, so the read that follows
/// returns `Ok(0)` and [`device_loop`] ends on it.
fn wait_readable(fd: RawFd, timeout_ms: u64) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one descriptor we own, and a timeout that fits an int.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms.min(i32::MAX as u64) as i32) };
    ready > 0 && poll.revents & libc::POLLIN != 0
}

/// What `attach` spawns: push whatever `port` says until `stop` is set.
fn device_loop(mut port: std::fs::File, sink: ConsoleSink, stop: Arc<AtomicBool>) {
    use std::io::Read as _;
    let fd = port.as_raw_fd();
    let mut buffer = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        if !wait_readable(fd, ATTACHED_POLL_MS) {
            continue;
        }
        match port.read(&mut buffer) {
            // The port itself is gone: nothing more will ever arrive.
            Ok(0) => break,
            Ok(n) => {
                // The peer is gone; nothing left to push to.
                if sink.push(&buffer[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
}

impl Connection for SerialTransport {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), tock_proto::ProtoError> {
        let answer = match request {
            Request::Hello { .. } => Ok((
                Response::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: vec![Capability::Console],
                    options: options(),
                },
                vec![],
            )),
            // `Open` carries the board, the architecture and the flash
            // geometry, and a console has no use for any of them. It is
            // still how options arrive, so this takes what it needs and
            // ignores the rest.
            Request::Open {
                options,
                option_bases,
                ..
            } => Ok((self.open(Options::new(&options, &option_bases)), vec![])),
            Request::ConsoleWrite => self.write(&payload).map(|()| (Response::Ok, vec![])),
            Request::ConsoleAttach => self.attach().map(|()| (Response::Ok, vec![])),
            Request::ConsoleDetach => self.detach().map(|()| (Response::Ok, vec![])),
            Request::Close => {
                // Closing detaches too: the device thread holds its own
                // clone of the descriptor and would outlive the port.
                let _ = self.detach();
                self.port = None;
                Ok((Response::Ok, vec![]))
            }
            other => Err(Response::error(
                ErrorKind::Unsupported,
                format!("serial transport does not support {other:?}"),
            )),
        };
        Ok(answer.unwrap_or_else(|e| (e, vec![])))
    }

    fn set_console_sink(&mut self, sink: ConsoleSink) {
        self.console_sink = Some(sink);
    }
}

fn main() {
    let mut transport = SerialTransport::default();
    let stdin = std::io::stdin().lock();
    // Not `.lock()`: `serve_streaming` keeps the writer past this call, so
    // it must be `'static`, which `StdoutLock` is not.
    if let Err(e) = tock_proto::serve_streaming(stdin, std::io::stdout(), &mut transport) {
        eprintln!("tock-transport-serial: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "serial/tests.rs"]
mod tests;
