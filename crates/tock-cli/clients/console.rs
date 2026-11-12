//! `tock-console`: the board's console, on your terminal.
//!
//! Two streams over one synchronous connection. The protocol answers exactly
//! one response per request, so neither end can speak unprompted, and this
//! takes turns: ask the device what it has said, waiting briefly; send
//! whatever has been typed; repeat. The waiting happens in the transport,
//! where the descriptor is, so a quiet board costs one request every
//! [`POLL_MS`] rather than a spin.
//!
//! The local terminal goes raw for the duration. A console is a byte stream
//! and the device is the one deciding what a character means -- Tock's
//! process console reads keys as it pleases -- so every convenience a
//! terminal would add on the way past is a byte the device does not get.
//! That includes `^C`, which is why there is an escape key instead.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, connect, finish, proto};
use tock_proto::{Capability, Request, Response};

/// How long the device is given to say something before we look at the
/// keyboard. Short enough that typing feels immediate, long enough that a
/// silent board is not a busy loop.
const POLL_MS: u64 = 50;

/// Most a single read may bring back. The device is asked again immediately,
/// so this bounds one frame rather than the conversation.
const READ_MAX: u64 = 4096;

/// `^]`, as telnet has used for forty years.
const ESCAPE: u8 = 0x1D;

#[derive(Parser)]
#[command(name = "tock-console", about = "Talk to the board's console")]
struct Args {
    /// Do not put the terminal in raw mode. Keystrokes then reach the board
    /// a line at a time, and `^C` stays local.
    #[arg(long)]
    cooked: bool,
}

fn main() -> ExitCode {
    finish(run(&Args::parse()))
}

fn run(args: &Args) -> Result<(), CliError> {
    let mut device = connect(&[Capability::Console])?;
    device
        .require(Capability::Console, "reading the console")
        .map_err(proto)?;

    let stdin = std::io::stdin();
    let interactive = !args.cooked && is_tty(stdin.as_raw_fd());
    // Held for the loop; restores the terminal however this ends.
    let _raw = interactive.then(|| Raw::enter(stdin.as_raw_fd())).flatten();
    if interactive {
        eprintln!("connected; ^] to quit");
    }

    let mut out = std::io::stdout();
    let mut keys = [0u8; 256];
    // Cleared when input runs out. The device keeps talking either way:
    // `tock console < /dev/null` is a reader, and ending its input is not a
    // reason to stop listening.
    let mut sending = true;
    loop {
        let (_, said) = device
            .call(
                Request::ConsoleRead {
                    max: READ_MAX,
                    timeout_ms: POLL_MS,
                },
                vec![],
            )
            .map_err(proto)?;
        if !said.is_empty() {
            out.write_all(&said)
                .and_then(|()| out.flush())
                .map_err(io)?;
        }

        // Non-blocking, because the device gets the next turn either way.
        if !sending || !readable(stdin.as_raw_fd()) {
            continue;
        }
        let typed = stdin.lock().read(&mut keys).map_err(io)?;
        if typed == 0 {
            // Input has run out: a pipe finished feeding us, or a terminal
            // went away. Stop looking at it -- a descriptor at end of file
            // is readable forever, so continuing to poll would spin -- and
            // go on listening to the device.
            sending = false;
            continue;
        }
        if interactive && keys[..typed].contains(&ESCAPE) {
            return Ok(());
        }
        match device.call(Request::ConsoleWrite, keys[..typed].to_vec()) {
            Ok((Response::Ok, _)) => {}
            Ok((other, _)) => {
                return Err(CliError::new(
                    tock_client::Code::Device,
                    format!("unexpected answer to console_write: {other:?}"),
                ));
            }
            Err(e) => return Err(proto(e)),
        }
    }
}

fn io(e: std::io::Error) -> CliError {
    CliError::new(tock_client::Code::Failed, e.to_string())
}

fn is_tty(fd: RawFd) -> bool {
    // SAFETY: a plain query on a descriptor we hold.
    unsafe { libc::isatty(fd) == 1 }
}

/// Whether there is input waiting, right now.
fn readable(fd: RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one descriptor we hold, and a zero timeout.
    unsafe { libc::poll(&mut poll, 1, 0) > 0 && poll.revents & libc::POLLIN != 0 }
}

/// The terminal in raw mode, put back when this goes.
///
/// A guard rather than a pair of calls because the restore has to happen on
/// every path out, including the error ones. A tool that leaves a terminal
/// without an echo is a tool you have to close the window on.
struct Raw {
    fd: RawFd,
    was: libc::termios,
}

impl Raw {
    fn enter(fd: RawFd) -> Option<Raw> {
        // SAFETY: `fd` is open for the life of the call, and the kernel
        // fills `was` before it is read.
        unsafe {
            let mut was: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut was) != 0 {
                return None;
            }
            let mut raw = was;
            libc::cfmakeraw(&mut raw);
            (libc::tcsetattr(fd, libc::TCSANOW, &raw) == 0).then_some(Raw { fd, was })
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: restoring settings we read from this same descriptor.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.was) };
        // The device left the cursor wherever it liked.
        let _ = std::io::stdout().write_all(b"\r\n");
        let _ = std::io::stdout().flush();
    }
}
