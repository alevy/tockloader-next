//! The reading half of a connection that may carry a pushed console.
//!
//! The pump is one thread that owns the reader for the connection's whole
//! life, delivers every `Response::Console` it sees to a sink, and hands
//! anything else back as the answer to the one outstanding request.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use tock_proto::{Connection, ProtoError, Request, Response, read_frame, write_frame};

/// A frame that is not a push: the answer to the one request that may be
/// outstanding at a time.
type Answer = (Response, Vec<u8>);

/// Where pushed console bytes go.
pub type Sink = Box<dyn FnMut(&[u8]) + Send>;

/// A [`Connection`] built from a reader and a writer, with a background
/// thread -- the pump -- that owns the reader for as long as `Pumped` lives.
///
/// Built by [`pumped`]. There is no way to get the reader back out.
pub struct Pumped<W: Write> {
    /// `Option` only so [`Drop`] can close it before waiting on the child.
    writer: Option<W>,
    answers: mpsc::Receiver<Answer>,
    /// The process whose stdio these halves are, if we spawned it. `None`
    /// for an inherited socket, which has nothing to reap.
    child: Option<std::process::Child>,
}

/// Build a pump from the two halves of a connection.
///
/// End of stream or a read error drops the sender, turning a waiting
/// [`Pumped::call`] into [`ProtoError::Closed`] rather than a hang.
pub fn pumped<R: Read + Send + 'static, W: Write>(
    mut reader: R,
    writer: W,
    mut sink: Sink,
) -> Pumped<W> {
    let (answered, answers) = mpsc::sync_channel::<Answer>(1);
    thread::spawn(move || {
        loop {
            match read_frame::<_, Response>(&mut reader) {
                Ok(Some((Response::Console, bytes))) => sink(&bytes),
                Ok(Some(answer)) => {
                    if answered.send(answer).is_err() {
                        // Pumped was dropped; nobody is left to wait for this.
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });
    Pumped {
        writer: Some(writer),
        answers,
        child: None,
    }
}

impl<W: Write> Pumped<W> {
    /// Give this `Pumped` ownership of the child process whose stdio its
    /// halves are, so dropping it reaps that child rather than leaking it.
    pub fn owning(mut self, child: std::process::Child) -> Pumped<W> {
        self.child = Some(child);
        self
    }
}

impl<W: Write> Connection for Pumped<W> {
    /// Write the request, then wait for the pump to hand back the first
    /// frame that is not a push.
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        let writer = self.writer.as_mut().ok_or(ProtoError::Closed)?;
        write_frame(writer, &request, &payload)?;
        self.answers.recv().map_err(|_| ProtoError::Closed)
    }
}

impl<W: Write> Drop for Pumped<W> {
    /// Close our end, *then* reap: a child still blocked reading a pipe we
    /// hold open would turn `wait()` into a deadlock.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.writer = None;
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// Encode `responses` as the frames a device would send, so a test can
    /// hand them to [`pumped`] as its reader.
    fn frames(responses: &[(Response, &[u8])]) -> Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        for (response, payload) in responses {
            write_frame(&mut buf, response, payload).unwrap();
        }
        Cursor::new(buf)
    }

    /// A sink forwarding every push onto a channel a test can drain.
    fn collecting_sink() -> (Sink, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        (
            Box::new(move |bytes: &[u8]| tx.send(bytes.to_vec()).unwrap()),
            rx,
        )
    }

    #[test]
    fn pushes_reach_the_sink_while_idle() {
        let reader = frames(&[(Response::Console, b"hello"), (Response::Console, b"world")]);
        let (sink, pushes) = collecting_sink();
        let _pump = pumped(reader, Vec::new(), sink);

        // No request is ever made: the pushes must still arrive.
        assert_eq!(
            pushes.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"hello".to_vec()
        );
        assert_eq!(
            pushes.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"world".to_vec()
        );
    }

    #[test]
    fn a_push_during_a_reset_is_not_mistaken_for_its_answer() {
        let reader = frames(&[
            (Response::Console, b"one"),
            (Response::Console, b"two"),
            (Response::Ok, b""),
        ]);
        let (sink, pushes) = collecting_sink();
        let mut pump = pumped(reader, Vec::new(), sink);

        let (response, payload) = pump.call(Request::Reset, vec![]).unwrap();
        assert_eq!((response, payload), (Response::Ok, vec![]));

        assert_eq!(
            pushes.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"one".to_vec(),
            "a push made while the reset was outstanding must still reach the sink"
        );
        assert_eq!(
            pushes.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"two".to_vec()
        );
    }

    #[test]
    fn pushes_before_the_detach_ok_are_still_delivered() {
        let reader = frames(&[(Response::Console, b"echo"), (Response::Ok, b"")]);
        let (sink, pushes) = collecting_sink();
        let mut pump = pumped(reader, Vec::new(), sink);

        let (response, _) = pump.call(Request::ConsoleDetach, vec![]).unwrap();
        assert_eq!(response, Response::Ok);
        assert_eq!(
            pushes.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"echo".to_vec(),
            "a push made before the detach's Ok must still reach the sink"
        );
    }

    #[test]
    fn a_closed_connection_ends_the_pump_and_unblocks_a_waiter() {
        // End of stream before any answer, as if the peer vanished while a
        // request was outstanding.
        let reader = frames(&[]);
        let mut pump = pumped(reader, Vec::new(), Box::new(|_: &[u8]| {}));

        let (done, waited) = mpsc::channel();
        thread::spawn(move || {
            let result = pump.call(Request::Reset, vec![]);
            // The Pumped must outlive the call it just made.
            let _ = done.send(result);
        });

        // A bounded wait, so a deadlock fails the test rather than hangs it.
        match waited.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(ProtoError::Closed)) => {}
            Ok(other) => panic!("expected Err(ProtoError::Closed), got {other:?}"),
            Err(_) => panic!("call() never returned: the pump did not unblock its waiter"),
        }
    }
}
