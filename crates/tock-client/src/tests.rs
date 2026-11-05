//! Tests for the handoff: spawning, and shutting down cleanly.

use super::*;

/// A `Spawned` dropped without a `Close` having been sent must still
/// exit. It is the pipe, not the message, that has to end the child --
/// `serve` stops on either, and only end of stream is ours to
/// guarantee. Waiting while still holding the pipe open is a deadlock:
/// the child waits for a request, we wait for the child.
///
/// `cat` stands in for a peer that reads until end of stream. This test
/// hangs rather than fails if the ordering is wrong, which is why it has
/// its own clock.
#[test]
fn dropping_a_spawned_connection_does_not_hang() {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let mut child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("cat");
    let reader = child.stdout.take().unwrap();
    let writer = child.stdin.take().unwrap();
    let spawned = Spawned::new(reader, writer, child);

    let (done, waited) = mpsc::channel();
    std::thread::spawn(move || {
        drop(spawned);
        let _ = done.send(());
    });
    assert!(
        waited.recv_timeout(Duration::from_secs(10)).is_ok(),
        "dropping a Spawned did not return: the child never saw end of stream"
    );
}
