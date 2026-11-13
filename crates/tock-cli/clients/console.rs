//! `tock-console`: the board's console, on your terminal.
//!
//! Two streams over one synchronous connection. The protocol answers exactly
//! one response per request, so neither end can speak unprompted, and this
//! takes turns: ask the device what it has said, waiting briefly; send
//! whatever has been typed; repeat. The waiting happens in the transport,
//! where the descriptor is, so a quiet board costs one request per poll
//! rather than a spin.
//!
//! "Briefly" adapts to the link rather than sitting at one fixed number. A
//! responder returns the instant something arrives, so a longer wait never
//! delays real output -- it only cuts down on how often this side asks into
//! the void over a link where a round trip is not free. See
//! [`next_timeout_ms`].
//!
//! The local terminal goes raw for the duration. A console is a byte stream
//! and the device is the one deciding what a character means -- Tock's
//! process console reads keys as it pleases -- so every convenience a
//! terminal would add on the way past is a byte the device does not get.
//! That includes `^C`, which is why there is an escape key instead.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use tock_client::{CliError, connect, finish, proto};
use tock_proto::{Capability, Request, Response};

/// Where the poll starts, and the least it is ever allowed to be. A link
/// with nothing worth measuring yet -- the very first request -- should
/// behave exactly as it always has: short enough that typing feels
/// immediate, long enough that a silent board on a desk is not a busy loop.
const POLL_MS: u64 = 50;

/// The most the poll is ever allowed to grow to, however bad the link.
///
/// The poll timeout is also the longest a keystroke can sit before the
/// device gets a turn to receive it -- the loop reads the keyboard between
/// requests, and a request already in flight cannot be interrupted to hand
/// one over early. Two seconds is long past where a link merely feels slow;
/// past it the tool reads as stuck, so growth stops there regardless of how
/// much worse the measured round trip gets.
const CEILING_MS: u64 = 2_000;

/// Most a single read may bring back. The device is asked again immediately,
/// so this bounds one frame rather than the conversation.
const READ_MAX: u64 = 4096;

/// `^]`, as telnet has used for forty years.
const ESCAPE: u8 = 0x1D;

/// How long to wait on the next poll, given how long the round trip to the
/// device just turned out to take.
///
/// Scaled rather than copied from the measurement: a single round trip is
/// one sample, not an average, and a poll timed to match it exactly would
/// come back early as often as late on any link with real jitter. A factor
/// of four leaves enough room that the answer is usually already on its way
/// back before this side would have given up and asked again -- which is
/// the whole saving, since asking again is a request the link has to carry
/// for nothing. Pure and separate from the loop so the curve can be checked
/// without a device on the other end.
fn next_timeout_ms(observed_rtt_ms: u64) -> u64 {
    observed_rtt_ms.saturating_mul(4).clamp(POLL_MS, CEILING_MS)
}

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
    // Grows away from the floor once a round trip is seen to cost more than
    // it; see `next_timeout_ms`.
    let mut poll_ms = POLL_MS;
    loop {
        let sent = Instant::now();
        let (_, said) = device
            .call(
                Request::ConsoleRead {
                    max: READ_MAX,
                    timeout_ms: poll_ms,
                },
                vec![],
            )
            .map_err(proto)?;
        // A quiet board holds the request for the full timeout before
        // answering empty, so what the call took beyond the timeout it was
        // given is time spent carried by the link rather than waited out on
        // purpose -- the round trip worth reacting to. A board with
        // something to say answers before the timeout is reached at all, in
        // which case this underestimates the round trip rather than
        // overstating it, which is the safe direction to be wrong in.
        // Saturating covers a clock that disagrees rather than trusting
        // that "beyond the timeout" can never read as negative.
        let elapsed_ms = sent.elapsed().as_millis() as u64;
        poll_ms = next_timeout_ms(elapsed_ms.saturating_sub(poll_ms));
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

#[cfg(test)]
mod console_tests {
    use super::*;

    #[test]
    fn a_fast_link_keeps_the_short_poll() {
        // A round trip of a millisecond is a desk, not a wire. The poll
        // should stay at the floor rather than drift upward on noise.
        assert_eq!(next_timeout_ms(1), 50);
    }

    #[test]
    fn a_slow_link_lengthens_the_poll() {
        // Once the round trip itself is three times the floor, holding the
        // poll at the floor would mean asking again before the previous ask
        // could possibly have come back.
        assert!(next_timeout_ms(150) > 50);
    }

    #[test]
    fn the_timeout_is_bounded() {
        // However bad the link, the poll stops growing at the ceiling
        // documented on `CEILING_MS`.
        assert!(next_timeout_ms(100_000) <= CEILING_MS);
    }
}
