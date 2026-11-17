//! `tock-console`: the board's console, on your terminal.
//!
//! Two streams over one connection. A pump (built by
//! [`tock_client::connect_pumped`]) owns the reader for the whole session
//! and hands device output to [`console_sink`] as it arrives. All that is
//! left here is the other direction: wait for a keystroke, send it as a
//! `ConsoleWrite`. That wait is bounded rather than a flat blocking read, so
//! a dead connection -- or a stdout that can no longer be written -- is
//! noticed even while the keyboard stays silent; see [`paired_sink`].
//!
//! The local terminal goes raw for the duration. A console is a byte stream
//! and the device is the one deciding what a character means -- Tock's
//! process console reads keys as it pleases -- so every convenience a
//! terminal would add on the way past is a byte the device does not get.
//! That includes `^C`, which is why there is an escape key instead.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;
use std::sync::mpsc;

use clap::Parser;
use tock_client::{CliError, connect_pumped, finish, proto};
use tock_proto::{Capability, Client, ProtoError, Request, Response};

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
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let interactive = !args.cooked && is_tty(fd);

    let (sink, trouble) = console_sink();
    let mut device = connect_pumped(&[Capability::Console], sink)?;
    device
        .require(Capability::Console, "reading the console")
        .map_err(proto)?;

    // Held for the loop; restores the terminal however this ends. Declared
    // *after* `device` so it drops first: raw mode has to come off before
    // `device`'s teardown, which blocks on `Close` and may reap a spawned
    // `tock broker`. Raw mode means `^C` is not a signal, so a user stuck
    // behind either has no way out but `stty sane` in another terminal.
    let _raw = interactive.then(|| Raw::enter(fd)).flatten();

    if interactive {
        eprintln!("connected; ^] to quit");
    }

    console_loop(&mut device, fd, interactive, trouble, |buf| {
        stdin.lock().read(buf)
    })
}

/// The sink pushed console bytes land through: stdout, a chunk at a time,
/// flushed immediately, no `BufWriter`. Hands a failed write back to
/// [`paired_sink`] rather than swallowing it, which is what makes
/// `tock console | head -n 5` end when `head` does.
fn console_sink() -> (tock_client::Sink, mpsc::Receiver<CliError>) {
    paired_sink(|bytes: &[u8]| {
        let mut out = std::io::stdout();
        out.write_all(bytes).and_then(|()| out.flush()).map_err(io)
    })
}

/// Wraps a byte-writing closure with a channel telling [`console_loop_inner`]
/// the session is over: a value if a write failed, a disconnect once the pump
/// drops the closure. Only the first failure is sent.
fn paired_sink(
    mut write: impl FnMut(&[u8]) -> Result<(), CliError> + Send + 'static,
) -> (tock_client::Sink, mpsc::Receiver<CliError>) {
    let (sink_alive, trouble) = mpsc::channel::<CliError>();
    let mut report = Some(sink_alive);
    let sink: tock_client::Sink = Box::new(move |bytes: &[u8]| {
        if report.is_none() {
            return;
        }
        if let Err(e) = write(bytes) {
            if let Some(report) = report.take() {
                let _ = report.send(e);
            }
        }
    });
    (sink, trouble)
}

/// Attaches to a remote console and forwards stdin. Console output is handled by [`console_sink`].
fn console_loop(
    device: &mut Client,
    stdin_fd: RawFd,
    interactive: bool,
    trouble: mpsc::Receiver<CliError>,
    mut read_stdin: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> Result<(), CliError> {
    expect_ok(
        "console_attach",
        device.call(Request::ConsoleAttach, vec![]),
    )?;

    let result = console_loop_inner(device, stdin_fd, interactive, &trouble, &mut read_stdin);

    // However this ended, say we are done listening before `Client`'s own
    // `Drop` sends `Close` behind it. Best-effort: the loop's result is what
    // matters, not whether the far end heard the goodbye.
    let _ = device.call(Request::ConsoleDetach, vec![]);
    result
}

/// How long [`console_loop_inner`] waits on a single `poll` of stdin before
/// coming up for air to check `trouble`.
///
/// This timeout trades off how long a dead connection goes unnoticed
/// against how often an idle session wakes for nothing.
const STDIN_POLL_MS: i32 = 100;

fn console_loop_inner(
    device: &mut Client,
    stdin_fd: RawFd,
    interactive: bool,
    trouble: &mpsc::Receiver<CliError>,
    read_stdin: &mut impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> Result<(), CliError> {
    let mut keys = [0u8; 256];
    loop {
        // Wait for a keystroke, but never past `STDIN_POLL_MS` without
        // asking whether there is any point going on: a far end that dies
        // while nobody is typing would otherwise never be noticed.
        while !readable_within(stdin_fd, STDIN_POLL_MS) {
            match trouble.try_recv() {
                // A push could not be written out -- what
                // `tock console | head -n 5` has to notice, since nothing
                // else in this process will.
                Ok(e) => return Err(e),
                Err(mpsc::TryRecvError::Disconnected) => return Err(proto(ProtoError::Closed)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let typed = read_stdin(&mut keys).map_err(io)?;
        if typed == 0 {
            // Stdin is exhausted, not momentarily empty: a blocking read
            // only returns 0 when nothing is left to come. That alone is no
            // reason to stop listening -- `tock console < /dev/null` is a
            // reader -- so wait for the events that are: the pump dying, or
            // a push that could not be written out.
            return match trouble.recv() {
                Ok(e) => Err(e),
                Err(_) => Ok(()),
            };
        }
        if interactive && keys[..typed].contains(&ESCAPE) {
            return Ok(());
        }
        expect_ok(
            "console_write",
            device.call(Request::ConsoleWrite, keys[..typed].to_vec()),
        )?;
    }
}

/// `Ok` is the only acceptable answer to a request nobody expects data
/// back from; anything else -- a response that is not even an error -- is
/// this connection's problem, not the operation's, which is why it is not
/// a [`Response::Error`] already by the time this sees it.
fn expect_ok(op: &str, result: Result<(Response, Vec<u8>), ProtoError>) -> Result<(), CliError> {
    match result {
        Ok((Response::Ok, _)) => Ok(()),
        Ok((other, _)) => Err(unexpected(op, other)),
        Err(e) => Err(proto(e)),
    }
}

fn unexpected(op: &str, response: Response) -> CliError {
    CliError::new(
        tock_client::Code::Device,
        format!("unexpected answer to {op}: {response:?}"),
    )
}

fn io(e: std::io::Error) -> CliError {
    CliError::new(tock_client::Code::Failed, e.to_string())
}

fn is_tty(fd: RawFd) -> bool {
    // SAFETY: a plain query on a descriptor we hold.
    unsafe { libc::isatty(fd) == 1 }
}

/// Whether a read on `fd` would return without blocking, waiting up to
/// `timeout_ms` to find out.
///
/// Any revent counts as ready, not just `POLLIN`: the kernel reports
/// `POLLHUP` on a pipe or pty whose far end has closed whatever `events`
/// asks for, and that is what makes the following `read` return `Ok(0)`
/// rather than block. Checking only `POLLIN` would poll a stdin that has run
/// out forever. (`wait_readable` in `transports/serial.rs` does filter to
/// `POLLIN`, because its fd is always a tty, where a hangup sets it too.)
fn readable_within(fd: RawFd, timeout_ms: i32) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one descriptor we hold.
    unsafe { libc::poll(&mut poll, 1, timeout_ms) > 0 }
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

    use std::os::fd::FromRawFd;
    use std::thread;
    use std::time::Duration;

    use tock_client::pumped;
    use tock_proto::{read_frame, write_frame};

    /// Serializes every test below that opens a pty.
    ///
    /// Not for the reason the guard of the same name in
    /// `transports/serial/tests.rs` exists -- the `pty()` below never calls
    /// [`libc::ptsname`], so that race cannot happen here. Kept anyway:
    /// each of these tests holds two ptys, a pump thread and a responder
    /// thread at once, and serialising a handful of short tests costs the
    /// suite nothing.
    static PTY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A pseudo-terminal pair, standing in for the fake keyboard and the
    /// fake connection.
    ///
    /// Put in raw mode before it is handed back, as [`Raw::enter`] would: a
    /// fresh pty is canonical with echo on, which strands a keyboard byte in
    /// the line discipline awaiting a newline that never comes, and echoes
    /// and newline-translates the framed binary protocol. Nothing here goes
    /// through `run`, so the test asks for raw mode itself.
    fn pty() -> (std::fs::File, std::fs::File) {
        let mut a = 0;
        let mut b = 0;
        // SAFETY: two out-parameters we own; null for the optional settings.
        let made = unsafe {
            libc::openpty(
                &mut a,
                &mut b,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(made, 0, "openpty");
        // SAFETY: `a` is open, freshly returned by `openpty` above, and a
        // pty's termios is one shared state -- setting it from either end
        // affects both.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(a, &mut termios), 0, "tcgetattr");
            libc::cfmakeraw(&mut termios);
            assert_eq!(libc::tcsetattr(a, libc::TCSANOW, &termios), 0, "tcsetattr");
        }
        // SAFETY: both are ours, freshly opened, and closed nowhere else.
        unsafe { (std::fs::File::from_raw_fd(a), std::fs::File::from_raw_fd(b)) }
    }

    /// An anonymous pipe, used where a test wants unambiguous end-of-file:
    /// closing the write end gives a clean `Ok(0)` on the read side, where
    /// closing a pty's manager side hangs up the device side instead -- the
    /// wrong thing for `< /dev/null`.
    fn pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        // SAFETY: freshly created above, not owned elsewhere.
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }

    /// [`console_sink`] without the writing: the same [`paired_sink`], silent
    /// and never failing. Built through `paired_sink` rather than re-pairing
    /// the channel here, so every test using it exercises the mechanism
    /// `trouble` actually depends on.
    fn silent_sink() -> (tock_client::Sink, mpsc::Receiver<CliError>) {
        paired_sink(|_: &[u8]| Ok(()))
    }

    /// A fake device on `conn`: answers `Hello` with `capabilities`, `Close`
    /// with `Ok` and ends, and everything else however `answer` says.
    fn fake_device(
        conn: std::fs::File,
        capabilities: Vec<Capability>,
        mut answer: impl FnMut(&Request, &[u8]) -> (Response, Vec<u8>) + Send + 'static,
    ) -> thread::JoinHandle<()> {
        let mut writer = conn.try_clone().expect("clone the pty for the writer");
        let mut reader = conn;
        thread::spawn(move || {
            while let Ok(Some((request, payload))) = read_frame::<_, Request>(&mut reader) {
                let (response, out) = match &request {
                    Request::Hello { .. } => (
                        Response::Hello {
                            protocol: tock_proto::PROTOCOL_VERSION,
                            version: "test".into(),
                            capabilities: capabilities.clone(),
                            options: vec![],
                        },
                        vec![],
                    ),
                    Request::Close => (Response::Ok, vec![]),
                    _ => answer(&request, &payload),
                };
                let closing = request == Request::Close;
                if write_frame(&mut writer, &response, &out).is_err() {
                    break;
                }
                if closing {
                    break;
                }
            }
        })
    }

    /// Answers `Hello` then `ConsoleAttach`, then -- once told to, via
    /// `disconnect` -- vanishes: no `Close`, no answer, just its end of the
    /// connection closing, the way an unplugged board looks from here.
    fn fake_device_until_disconnected(
        conn: std::fs::File,
        capabilities: Vec<Capability>,
        disconnect: mpsc::Receiver<()>,
    ) -> thread::JoinHandle<()> {
        let mut writer = conn.try_clone().expect("clone the pty for the writer");
        let mut reader = conn;
        thread::spawn(move || {
            let (hello, _) = read_frame::<_, Request>(&mut reader)
                .expect("read")
                .expect("hello");
            assert!(matches!(hello, Request::Hello { .. }), "{hello:?}");
            write_frame(
                &mut writer,
                &Response::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    version: "test".into(),
                    capabilities,
                    options: vec![],
                },
                &[],
            )
            .unwrap();

            let (attach, _) = read_frame::<_, Request>(&mut reader)
                .expect("read")
                .expect("attach");
            assert_eq!(attach, Request::ConsoleAttach);
            write_frame(&mut writer, &Response::Ok, &[]).unwrap();

            // Wait to be told to vanish: dropping `reader` and `writer`
            // closes this side's only descriptors, with nothing said.
            let _ = disconnect.recv();
        })
    }

    /// A device that speaks unprompted: answers `Hello` with the streaming
    /// capabilities and everything else with `Ok`, and after answering a
    /// `ConsoleAttach` pushes `says` once. The push is written on the same
    /// thread as the answers, after the attach's own `Ok`, so the frames
    /// cannot interleave.
    ///
    /// Unlike [`fake_device_until_disconnected`] it stays alive and
    /// answering until `Close`, so a test cannot pass merely because the far
    /// end went away.
    fn fake_device_that_pushes(conn: std::fs::File, says: &'static [u8]) -> thread::JoinHandle<()> {
        let mut writer = conn.try_clone().expect("clone the pty for the writer");
        let mut reader = conn;
        thread::spawn(move || {
            while let Ok(Some((request, _))) = read_frame::<_, Request>(&mut reader) {
                let response = match &request {
                    Request::Hello { .. } => Response::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        version: "test".into(),
                        capabilities: vec![Capability::Console],
                        options: vec![],
                    },
                    _ => Response::Ok,
                };
                if write_frame(&mut writer, &response, &[]).is_err() {
                    break;
                }
                if request == Request::ConsoleAttach
                    && write_frame(&mut writer, &Response::Console, says).is_err()
                {
                    break;
                }
                if request == Request::Close {
                    break;
                }
            }
        })
    }

    /// Opens a `Client` over `conn`, through the same kind of pump
    /// `connect_pumped` would build, with `sink` wired up exactly as
    /// [`console_sink`] would be.
    fn client_over(conn: std::fs::File, sink: tock_client::Sink) -> Client {
        let reader = conn.try_clone().expect("clone the pty for the reader");
        Client::open(
            Box::new(pumped(reader, conn, sink)),
            "test",
            &[Capability::Console],
        )
        .expect("handshake")
    }

    /// Joins `handle`, failing the test instead of hanging the suite if it
    /// never returns within `timeout`. Every wait this module puts on the
    /// thread libtest runs the test on goes through here: an unbounded one
    /// there turns a bug into a wedged `cargo test` rather than a red one.
    fn join_within<T: Send + 'static>(handle: thread::JoinHandle<T>, timeout: Duration) -> T {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(handle.join());
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result.expect("a test thread panicked"),
            Err(_) => panic!("a test thread did not finish in time"),
        }
    }

    /// A keystroke reaches the device as soon as it is typed, with nothing
    /// to wait for.
    ///
    /// By construction rather than by a clock: the responder `panic!`s on
    /// any request a session has no business sending, and the keystroke
    /// still has to arrive inside `recv_timeout`.
    #[test]
    fn a_keystroke_is_sent_as_soon_as_it_is_typed() {
        let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (client_conn, device_conn) = pty();
        let (mut keyboard_write, mut keyboard_read) = pty();
        let keyboard_fd = keyboard_read.as_raw_fd();

        let (writes, got_write) = mpsc::channel();
        let responder = fake_device(
            device_conn,
            vec![Capability::Console],
            move |request, payload| match request {
                Request::ConsoleAttach | Request::ConsoleDetach => (Response::Ok, vec![]),
                Request::ConsoleWrite => {
                    let _ = writes.send(payload.to_vec());
                    (Response::Ok, vec![])
                }
                other => panic!("unexpected request reached the device: {other:?}"),
            },
        );

        // `console_loop` blocks for the session's whole life, so it gets its
        // own thread while this one drives the keyboard. `device` is built
        // inside the worker rather than handed to it: `Client` is not `Send`.
        let worker = thread::spawn(move || {
            let (sink, trouble) = silent_sink();
            let mut device = client_over(client_conn, sink);
            let result = console_loop(&mut device, keyboard_fd, true, trouble, move |buf| {
                keyboard_read.read(buf)
            });
            // `Client::drop` sends `Close`; the responder's loop ends on it.
            drop(device);
            result
        });

        keyboard_write.write_all(b"x").unwrap();
        let payload = got_write
            .recv_timeout(Duration::from_secs(5))
            .expect("no ConsoleWrite observed");
        assert_eq!(payload, b"x");
        keyboard_write.write_all(&[ESCAPE]).unwrap();

        join_within(worker, Duration::from_secs(5)).expect("console_loop");
        join_within(responder, Duration::from_secs(5));
    }

    #[test]
    fn a_dead_connection_ends_the_session_even_while_blocked_on_stdin() {
        let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (client_conn, device_conn) = pty();
        // A pipe whose write end is kept open, unlike the EOF test below:
        // this test is about a keyboard that stays silent, not one that goes
        // away, so a plain blocking read on it would hang forever.
        let (mut stdin_read, _stdin_write) = pipe();
        let stdin_fd = stdin_read.as_raw_fd();

        let (disconnect, go_away) = mpsc::channel();
        let responder =
            fake_device_until_disconnected(device_conn, vec![Capability::Console], go_away);

        let worker = thread::spawn(move || {
            let (sink, trouble) = silent_sink();
            let mut device = client_over(client_conn, sink);
            let result = console_loop(&mut device, stdin_fd, true, trouble, move |buf| {
                stdin_read.read(buf)
            });
            drop(device);
            result
        });

        // So the check after disconnecting means something: the session must
        // still be alive *before* the far end goes away, not finished for
        // some unrelated reason. `join_within` bounds the real assertion.
        thread::sleep(Duration::from_millis(100));
        assert!(
            !worker.is_finished(),
            "a live far end and a silent keyboard must not end the session on their own"
        );

        // Take the far end away: no `Close`, no answer, just gone.
        drop(disconnect);

        let result = join_within(worker, Duration::from_secs(5));
        assert!(
            result.is_err(),
            "a dead connection must end the session with an error, not hang \
             forever waiting on a keyboard that was never going to type \
             again, and not silently report Ok"
        );

        join_within(responder, Duration::from_secs(5));
    }

    #[test]
    fn stdin_eof_keeps_listening_until_the_far_end_disconnects() {
        let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (client_conn, device_conn) = pty();
        // A pipe, not a pty: closing its write end gives a clean `Ok(0)` on
        // the read side -- exactly `tock console < /dev/null`.
        let (mut stdin_read, stdin_write) = pipe();
        let stdin_fd = stdin_read.as_raw_fd();
        drop(stdin_write);

        let (disconnect, go_away) = mpsc::channel();
        let responder =
            fake_device_until_disconnected(device_conn, vec![Capability::Console], go_away);

        let worker = thread::spawn(move || {
            let (sink, trouble) = silent_sink();
            let mut device = client_over(client_conn, sink);
            let result = console_loop(&mut device, stdin_fd, true, trouble, move |buf| {
                stdin_read.read(buf)
            });
            drop(device);
            result
        });

        // So `is_finished` checks "still listening" rather than racing a
        // worker that has not reached the blocking wait yet. `join_within`
        // bounds the real assertions.
        thread::sleep(Duration::from_millis(100));
        assert!(
            !worker.is_finished(),
            "a live far end with nothing more to say must not end the \
             session just because stdin ran out"
        );

        // Take the far end away: no `Close`, no answer, just gone.
        drop(disconnect);

        join_within(worker, Duration::from_secs(5)).expect("console_loop");
        join_within(responder, Duration::from_secs(5));
    }

    /// `tock console | head -n 5`: the reader of our output goes away while
    /// the device is still talking and the keyboard is still silent.
    ///
    /// Nothing else in the process would notice. Rust ignores SIGPIPE, so the
    /// write fails rather than killing us; the far end is alive, so the pump
    /// never dies; and with stdin a pipe there is no `^]` to press.
    #[test]
    fn output_that_cannot_be_written_ends_the_session_rather_than_hanging() {
        let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (client_conn, device_conn) = pty();
        // A keyboard that neither types nor ends: the write end is held
        // open, so only the failure on the output side can finish this
        // session.
        let (mut stdin_read, _stdin_write) = pipe();
        let stdin_fd = stdin_read.as_raw_fd();

        // Our output with its reader already gone, as `| head -n 5` leaves
        // it once `head` has what it wanted.
        let (out_read, mut out_write) = pipe();
        drop(out_read);
        let (sink, trouble) = paired_sink(move |bytes: &[u8]| {
            out_write
                .write_all(bytes)
                .and_then(|()| out_write.flush())
                .map_err(io)
        });

        // Stays alive and answering throughout, including the detach and
        // the `Close`: if this passes it is because the write failed, not
        // because the far end vanished.
        let responder = fake_device_that_pushes(device_conn, b"the board is still talking");

        let worker = thread::spawn(move || {
            let mut device = client_over(client_conn, sink);
            let result = console_loop(&mut device, stdin_fd, true, trouble, move |buf| {
                stdin_read.read(buf)
            });
            drop(device);
            result
        });

        let error = join_within(worker, Duration::from_secs(5))
            .expect_err("a console whose output cannot be written must not keep listening");
        // The write's own error, not `ProtoError::Closed` (which would be
        // `Code::Device`): this has to end for the right reason and say so
        // to whoever is reading stderr.
        assert_eq!(error.code, tock_client::Code::Failed, "{error:?}");
        assert!(
            error.message.contains("Broken pipe"),
            "the failure the user sees must be the write's own: {error:?}"
        );

        join_within(responder, Duration::from_secs(5));
    }
}
