//! Tests that need no serial port. What a port does when written to needs
//! hardware; which baud rates are accepted and what is refused does not.

use super::*;

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
    let refusal = |r: Result<_, Response>| match r {
        Err(Response::Error { message, kind, .. }) => (kind, message),
        _ => panic!("expected a refusal"),
    };
    let (kind, message) = refusal(t.read(16, 0));
    assert_eq!(kind, ErrorKind::InvalidRequest);
    assert!(message.contains("not open"), "{message}");
    let (_, message) = refusal(t.write(b"x").map(|()| Vec::new()));
    assert!(message.contains("not open"), "{message}");
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

/// A console is a byte stream. `poll` with a zero timeout is how a caller
/// asks "anything right now?", and it has to be answerable without a port
/// having anything to say.
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
