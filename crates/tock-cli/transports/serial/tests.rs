//! Tests that need no serial port, plus the attach/detach push behaviour,
//! which is driven over a pty standing in for one.

use super::*;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tock_proto::{FlashInfo, write_frame};

#[test]
fn a_baud_rate_that_cannot_be_set_is_refused_rather_than_approximated() {
    assert_eq!(baud_constant(115_200), Some(libc::B115200));
    assert_eq!(baud_constant(9_600), Some(libc::B9600));
    // Silently becoming the nearest rate is the failure that looks like a
    // broken cable: the port opens, and every byte is garbage.
    assert_eq!(baud_constant(115_201), None);
    assert_eq!(baud_constant(0), None);
}

#[test]
fn operations_before_open_are_refused_not_ignored() {
    let mut t = SerialTransport::default();
    match t.write(b"x") {
        Err(Response::Error { message, kind, .. }) => {
            assert_eq!(kind, ErrorKind::InvalidRequest);
            assert!(message.contains("not open"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_missing_port_says_what_to_set() {
    let values = serde_json::Map::new();
    let bases = std::collections::BTreeMap::new();
    let mut t = SerialTransport::default();
    match t.open(Options::new(&values, &bases)) {
        Response::Error {
            keys, suggestion, ..
        } => {
            assert_eq!(keys, vec!["port"]);
            assert!(suggestion.unwrap().contains("/dev/ttyUSB0"));
        }
        other => panic!("{other:?}"),
    }
}

/// The device thread waits on the port with a deadline, so a port with
/// nothing to say has to come back not-ready rather than block or fail.
#[test]
fn waiting_on_a_descriptor_with_nothing_to_say_returns_false() {
    // A pipe with no writer data pending stands in for a quiet device.
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    assert!(!wait_readable(fds[0], 0));
    // And becomes readable once there is something.
    assert_eq!(unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) }, 1);
    assert!(wait_readable(fds[0], 0));
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

// ---------------------------------------------------------------------------
// Attach/detach, driven over a pty standing in for a real port.

/// Serializes every test below that opens a pty.
///
/// [`libc::ptsname`] returns a pointer into a buffer static to the process,
/// so two threads inside it race and the loser reads the winner's path.
static PTY_TESTS: Mutex<()> = Mutex::new(());

/// A pseudo-terminal pair, which is a serial port for every purpose this
/// transport has: `termios` applies, `poll` works, and bytes written to one
/// end arrive at the other. Returns the manager end (written to, to act as
/// "the device"), a device-side handle kept open only so the manager side
/// does not see EOF, and the path `SerialTransport::open` should be given.
fn pty() -> (std::fs::File, std::fs::File, PathBuf) {
    let mut manager = 0;
    let mut device = 0;
    // SAFETY: two out-parameters we own, and null for the optional settings.
    let made = unsafe {
        libc::openpty(
            &mut manager,
            &mut device,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(made, 0, "openpty");
    // SAFETY: freshly opened by `openpty` and not owned elsewhere.
    let name = unsafe {
        let raw = libc::ptsname(manager);
        assert!(!raw.is_null(), "ptsname");
        std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned()
    };
    // SAFETY: both are ours, freshly opened, and closed nowhere else.
    unsafe {
        (
            std::fs::File::from_raw_fd(manager),
            std::fs::File::from_raw_fd(device),
            PathBuf::from(name),
        )
    }
}

/// An anonymous pipe, as the two `File`s either end of it.
fn pipe() -> (std::fs::File, std::fs::File) {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: freshly created by `pipe`, not owned elsewhere.
    unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    }
}

/// A `Write` that appends every call to a shared buffer, so a test can read
/// back what `serve_streaming` wrote after handing the writer away by value.
#[derive(Clone)]
struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for RecordingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// However many complete frames are currently in `buf`. A trailing partial
/// frame is not counted.
fn frames_so_far(buf: &[u8]) -> Vec<(Response, Vec<u8>)> {
    let mut rest = buf;
    let mut out = Vec::new();
    while let Ok(Some(frame)) = tock_proto::read_frame::<_, Response>(&mut rest) {
        out.push(frame);
    }
    out
}

/// Blocks until at least `count` frames have landed in `buf`, returning all
/// of them. Bounded, so a responder that stops pushing fails the test
/// rather than hangs it.
fn wait_for_frames(buf: &Arc<Mutex<Vec<u8>>>, count: usize) -> Vec<(Response, Vec<u8>)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frames = frames_so_far(&buf.lock().unwrap());
        if frames.len() >= count {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} frames; got {frames:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Joins `handle`, failing the test instead of hanging the suite if it never
/// returns.
fn join_within(handle: std::thread::JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.join());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result.expect("serve_streaming panicked"),
        Err(_) => panic!("serve_streaming did not return in time"),
    }
}

/// Runs a fresh `SerialTransport` behind `serve_streaming` on its own
/// thread. Returns the write end of the request pipe, the buffer its
/// responses land in, and the serving thread's handle.
fn start_streaming() -> (
    std::fs::File,
    Arc<Mutex<Vec<u8>>>,
    std::thread::JoinHandle<()>,
) {
    let (read_end, write_end) = pipe();
    let said = Arc::new(Mutex::new(Vec::new()));
    let writer = RecordingWriter(said.clone());
    let join = std::thread::spawn(move || {
        let mut transport = SerialTransport::default();
        let _ = tock_proto::serve_streaming(read_end, writer, &mut transport);
    });
    (write_end, said, join)
}

fn send_hello(requests: &mut impl std::io::Write) {
    write_frame(
        requests,
        &Request::Hello {
            protocol: tock_proto::PROTOCOL_VERSION,
            client: "test".into(),
            needs: vec![],
        },
        &[],
    )
    .unwrap();
}

fn send_open(requests: &mut impl std::io::Write, port: &Path) {
    let mut options = serde_json::Map::new();
    options.insert(
        "port".into(),
        serde_json::Value::String(port.display().to_string()),
    );
    // No pacing: a pty keeps up, and the test should not wait on it.
    options.insert("write_pace_ms".into(), serde_json::Value::from(0));
    write_frame(
        requests,
        &Request::Open {
            board: "nrf52840dk".into(),
            arch: "cortex-m4".into(),
            flash: FlashInfo { start: 0, size: 0 },
            options,
            option_bases: BTreeMap::new(),
        },
        &[],
    )
    .unwrap();
}

#[test]
fn hello_advertises_the_console() {
    let mut t = SerialTransport::default();
    let (response, _) = t
        .call(
            Request::Hello {
                protocol: tock_proto::PROTOCOL_VERSION,
                client: "test".into(),
                needs: vec![],
            },
            vec![],
        )
        .unwrap();
    match response {
        Response::Hello { capabilities, .. } => assert!(
            capabilities.contains(&Capability::Console),
            "{capabilities:?}"
        ),
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn bytes_written_to_the_port_arrive_as_pushes() {
    let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (mut manager, _keepalive, device) = pty();
    let (mut requests, said, join) = start_streaming();

    send_hello(&mut requests);
    send_open(&mut requests, &device);
    write_frame(&mut requests, &Request::ConsoleAttach, &[]).unwrap();

    // Hello, Open and Attach each answer before any push can happen.
    wait_for_frames(&said, 3);

    manager.write_all(b"hello").unwrap();

    let frames = wait_for_frames(&said, 4);
    assert_eq!(
        frames[3],
        (Response::Console, b"hello".to_vec()),
        "the pushed frame must carry exactly what was written to the port"
    );

    write_frame(&mut requests, &Request::ConsoleDetach, &[]).unwrap();
    wait_for_frames(&said, 5);
    drop(requests);
    join_within(join, Duration::from_secs(5));
}

#[test]
fn no_push_arrives_after_the_detach_is_answered() {
    let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (mut manager, _keepalive, device) = pty();
    let (mut requests, said, join) = start_streaming();

    send_hello(&mut requests);
    send_open(&mut requests, &device);
    write_frame(&mut requests, &Request::ConsoleAttach, &[]).unwrap();
    wait_for_frames(&said, 3);

    manager.write_all(b"first").unwrap();
    wait_for_frames(&said, 4);

    write_frame(&mut requests, &Request::ConsoleDetach, &[]).unwrap();
    let frames = wait_for_frames(&said, 5);
    assert_eq!(
        frames[4].0,
        Response::Ok,
        "the fifth frame must be the detach's answer"
    );

    // Write more bytes once the detach's Ok has been observed: the sink
    // must never see them.
    manager.write_all(b"second").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let final_frames = frames_so_far(&said.lock().unwrap());
    assert_eq!(
        final_frames.len(),
        5,
        "no push may arrive after the Ok to ConsoleDetach: {final_frames:?}"
    );

    drop(requests);
    join_within(join, Duration::from_secs(5));
}

/// A second `ConsoleAttach` while attached is an error, not a no-op.
#[test]
fn attaching_twice_is_an_error() {
    let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (_manager, _keepalive, device) = pty();
    let (mut requests, said, join) = start_streaming();

    send_hello(&mut requests);
    send_open(&mut requests, &device);
    write_frame(&mut requests, &Request::ConsoleAttach, &[]).unwrap();
    write_frame(&mut requests, &Request::ConsoleAttach, &[]).unwrap();

    let frames = wait_for_frames(&said, 4);
    assert_eq!(
        frames[2].0,
        Response::Ok,
        "the first attach must have been accepted for the second to mean \
         anything: {frames:?}"
    );
    match &frames[3].0 {
        Response::Error { kind, message, .. } => {
            assert_eq!(*kind, ErrorKind::InvalidRequest);
            assert!(
                message.contains("already attached"),
                "refused, but not for being a second attach: {message}"
            );
        }
        other => panic!("expected the second attach to be refused, got {other:?}"),
    }

    write_frame(&mut requests, &Request::ConsoleDetach, &[]).unwrap();
    wait_for_frames(&said, 5);
    drop(requests);
    join_within(join, Duration::from_secs(5));
}

/// Closing detaches too: the two sessions below run one after the other
/// against the same `SerialTransport`, whose state must not have desynced
/// between.
#[test]
fn close_tears_down_an_active_attachment() {
    let _guard = PTY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (_manager, _keepalive, device) = pty();
    let mut transport = SerialTransport::default();

    let mut asked = Vec::new();
    send_hello(&mut asked);
    send_open(&mut asked, &device);
    write_frame(&mut asked, &Request::ConsoleAttach, &[]).unwrap();
    write_frame(&mut asked, &Request::Close, &[]).unwrap();

    let said = Arc::new(Mutex::new(Vec::new()));
    tock_proto::serve_streaming(&asked[..], RecordingWriter(said.clone()), &mut transport).unwrap();
    let frames = frames_so_far(&said.lock().unwrap());
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[2].0, Response::Ok, "attach: {frames:?}");
    assert_eq!(frames[3].0, Response::Ok, "close: {frames:?}");

    // If `Close` left `self.attached` set, this second attach would hit a
    // stale "already attached" refusal instead of succeeding.
    let mut asked = Vec::new();
    send_open(&mut asked, &device);
    write_frame(&mut asked, &Request::ConsoleAttach, &[]).unwrap();
    write_frame(&mut asked, &Request::ConsoleDetach, &[]).unwrap();

    let said = Arc::new(Mutex::new(Vec::new()));
    tock_proto::serve_streaming(&asked[..], RecordingWriter(said.clone()), &mut transport).unwrap();
    let frames = frames_so_far(&said.lock().unwrap());
    assert_eq!(
        frames[1].0,
        Response::Ok,
        "a second attach after Close must succeed, not hit a stale \
         'already attached' refusal: {frames:?}"
    );
}
