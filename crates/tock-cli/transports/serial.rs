//! `tock-transport-serial`: a board's console, over a serial port.
//!
//! The console is not always reachable the same way the flash is. On a board
//! with a USB-to-UART bridge soldered on, the bridge is a console and nothing
//! else -- it appears as its own USB device, with no path to flash at all,
//! while the debug adapter next to it has no path to the console. So this
//! transport declares [`Capability::Console`] and nothing more, and a command
//! that wants the console asks for it by name: `tock --transport serial
//! console`.
//!
//! The port is a fact about a desk, not about a board: which `/dev/tty*` a
//! bridge landed on depends on what else is plugged in and in what order. It
//! belongs in a user config. The baud rate is the board's, and belongs in a
//! board file.
//!
//! Reads are polled, because the protocol is strictly synchronous and a
//! responder can never speak unprompted. The waiting is done here, where the
//! descriptor is, so a caller asking for a short wait gets a quiet answer
//! rather than a spin.

use std::os::fd::{AsRawFd, RawFd};

use tock_proto::{Capability, Connection, ErrorKind, OptionSpec, Options, Request, Response};

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

#[derive(Default)]
pub struct SerialTransport {
    port: Option<std::fs::File>,
    pace: std::time::Duration,
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
        self.port = Some(file);
        Response::Ok
    }

    fn port(&mut self) -> Result<&mut std::fs::File, Response> {
        self.port
            .as_mut()
            .ok_or_else(|| Response::error(ErrorKind::InvalidRequest, "not open"))
    }

    /// Whatever has arrived within `timeout_ms`. Empty is a normal answer.
    fn read(&mut self, max: u64, timeout_ms: u64) -> Result<Vec<u8>, Response> {
        use std::io::Read as _;
        let port = self.port()?;
        let fd = port.as_raw_fd();
        if !wait_readable(fd, timeout_ms) {
            return Ok(Vec::new());
        }
        // Capped so one request cannot be answered with an unbounded frame;
        // the caller asks again, which it was going to do anyway.
        let mut buffer = vec![0u8; max.min(64 * 1024) as usize];
        match port.read(&mut buffer) {
            Ok(n) => {
                buffer.truncate(n);
                Ok(buffer)
            }
            // The port said nothing after all, which the wait allows.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(Response::error(ErrorKind::Device, e.to_string())),
        }
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
            Request::ConsoleRead { max, timeout_ms } => self
                .read(max, timeout_ms)
                .map(|data| (Response::Data, data)),
            Request::ConsoleWrite => self.write(&payload).map(|()| (Response::Ok, vec![])),
            Request::Close => {
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
}

fn main() {
    let mut transport = SerialTransport::default();
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(e) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-serial: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "serial/tests.rs"]
mod tests;
