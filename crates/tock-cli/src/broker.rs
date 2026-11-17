//! Serving the protocol instead of speaking it.
//!
//! The same broker answers two carriers. `tock broker` serves it on stdio,
//! for a subcommand nobody launched. [`external`](crate::external) serves it
//! on a socket the subcommand inherits as fd 3, which is the usual case and
//! the one where `tock` genuinely owns the connection: it opened the
//! transport, and a subcommand reaches the device only by asking.
//!
//! The transport is opened on the first request that needs it, so a
//! subcommand that never touches a device -- building a TAB, say -- does not
//! spawn one, and does not fail on a machine with no board attached.
//!
//! Upstream the broker is a responder, served by
//! [`serve_streaming`](tock_proto::serve_streaming), which hands it a
//! [`ConsoleSink`]; downstream it is a caller, and every link it opens is a
//! pump (`tock_client::pumped`) pushing into that sink.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::Child;

use tock_proto::{Capability, Client, Connection, ConsoleSink, ProtoError, Request, Response};

use tock_client::{CliError, Code, Sink, pumped};

use crate::cli::Global;
use crate::config::load_settings;
use crate::connection;

pub(crate) struct Broker {
    settings: tock_board::Settings,
    trace: bool,
    /// One link per transport we have had to open, by name.
    ///
    /// A map rather than one connection because a board need not be
    /// reachable one way: flash may arrive over a debug probe while the
    /// console arrives over a bridge soldered beside it, and a client that
    /// asks for both gets both, each request going wherever its capability
    /// is routed.
    links: BTreeMap<String, Client>,
    /// How a transport is reached. See [`Spawn`].
    spawn: Spawn,
    /// Where our own pushed console bytes go. `None` until
    /// `serve_streaming` hands one over, and for good under plain `serve`.
    upstream: Option<ConsoleSink>,
}

/// The two halves of a connection to a transport, and the process they
/// belong to if one was started for it.
type Halves = (Box<dyn Read + Send>, Box<dyn Write>, Option<Child>);

/// How the broker reaches a transport.
type Spawn = Box<dyn FnMut(&str) -> Result<Halves, CliError>>;

/// The capability a request needs, and so which transport serves it.
///
/// The one place a request is mapped to a capability. `Hello`, `Open` and
/// `Close` are answered here and need nothing.
fn needed_for(request: &Request) -> Option<Capability> {
    Some(match request {
        Request::Read { .. } => Capability::MemoryRead,
        Request::Write { .. } => Capability::MemoryWrite,
        Request::ConsoleWrite | Request::ConsoleAttach | Request::ConsoleDetach => {
            Capability::Console
        }
        Request::Reset => Capability::Reset,
        Request::Vendor { name, .. } => Capability::Vendor(name.clone()),
        Request::Hello { .. } | Request::Open { .. } | Request::Close => return None,
    })
}

impl Broker {
    pub(crate) fn new(settings: tock_board::Settings, trace: bool) -> Broker {
        Broker {
            settings,
            trace,
            links: BTreeMap::new(),
            spawn: Box::new(|name| {
                connection::spawn_transport(name).map(|(r, w, child)| (r, w, Some(child)))
            }),
            upstream: None,
        }
    }

    /// A broker that reaches its transports through `spawn` rather than by
    /// starting one. See [`Spawn`].
    #[cfg(test)]
    fn over(settings: tock_board::Settings, spawn: Spawn) -> Broker {
        Broker {
            settings,
            trace: false,
            links: BTreeMap::new(),
            spawn,
            upstream: None,
        }
    }

    /// Open the transport `name`, with its reader on a pump.
    ///
    /// Every link gets one, not only the console's: which link that is
    /// depends on the board.
    fn open(&mut self, name: &str) -> Result<Client, CliError> {
        let (reader, writer, child) = (self.spawn)(name)?;
        let mut pump = pumped(reader, writer, self.relay());
        if let Some(child) = child {
            pump = pump.owning(child);
        }
        connection::open(
            connection::traced(Box::new(pump), self.trace),
            &self.settings,
            name,
        )
    }

    /// The sink a pumped link pushes into: our own, upstream, so a byte the
    /// device said travels client-wards without anyone asking for it.
    fn relay(&self) -> Sink {
        let upstream = self.upstream.clone();
        Box::new(move |bytes: &[u8]| {
            if let Some(sink) = &upstream {
                // A client that has gone away is the serve loop's news to
                // hear, at the end of its own stream.
                let _ = sink.push(bytes);
            }
        })
    }

    /// The transport serving `capability`, opened if it is not already.
    ///
    /// A failure to open is fatal to the connection -- there will never be a
    /// device -- so it travels as an error rather than a response, carrying
    /// whatever the CLI would have told the user.
    ///
    /// Opened on demand rather than up front: a client that declares two
    /// capabilities and only ever exercises one should only ever cost one
    /// transport.
    fn link_for(&mut self, capability: &Capability) -> Result<&mut Client, ProtoError> {
        let name = self.route(capability).map_err(fatal)?;
        if !self.links.contains_key(&name) {
            let opened = self.open(&name).map_err(fatal)?;
            // A route is a claim about a transport, and this is where it is
            // checked against what the transport says of itself.
            if !opened.capabilities.contains(capability) {
                return Err(fatal(
                    CliError::new(
                        Code::Capability,
                        format!(
                            "`{}` is routed to transport `{name}`, which does not provide it",
                            capability.name()
                        ),
                    )
                    .detail(format!("from {}", self.route_source(capability)))
                    .hint("tock transports -- to see what each one can do"),
                ));
            }
            self.links.insert(name.clone(), opened);
        }
        Ok(self.links.get_mut(&name).expect("just opened"))
    }

    /// Which transport a capability is routed to.
    fn route(&self, capability: &Capability) -> Result<String, CliError> {
        self.settings
            .transport
            .route(&capability.name())
            .map(|sourced| sourced.value.clone())
            .ok_or_else(|| {
                CliError::new(
                    Code::Config,
                    format!(
                        "nothing is routed to `{}` for this board",
                        capability.name()
                    ),
                )
                .hint(format!(
                    "set it in your user config: [transport] {} = \"<name>\"",
                    capability.name()
                ))
                .hint("tock transports -- to see what is installed")
            })
    }

    fn route_source(&self, capability: &Capability) -> String {
        self.settings
            .transport
            .route(&capability.name())
            .map(|sourced| sourced.source.to_string())
            .unwrap_or_default()
    }
}

fn fatal(e: CliError) -> ProtoError {
    let kind = match e.code {
        Code::Config => tock_proto::ErrorKind::Selection,
        Code::Capability => tock_proto::ErrorKind::Unsupported,
        _ => tock_proto::ErrorKind::Device,
    };
    let error = ProtoError::failed(e.message)
        .with_kind(kind)
        .detailed(e.details);
    if e.hints.is_empty() {
        error
    } else {
        error.with_suggestion(e.hints.join("\n"))
    }
}

impl Connection for Broker {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match request {
            // Answered here: the client did not open any of this and does
            // not get to say when it closes. What it declared it needs is
            // checked now, against the routing, so "nothing here can do
            // that" arrives before the work rather than during it.
            Request::Hello { needs, .. } => {
                for capability in &needs {
                    self.link_for(capability)?;
                }
                // A set, not `Vec::dedup`: two links can share a capability,
                // and collected link by link the duplicates are not adjacent.
                let mut seen = std::collections::HashSet::new();
                let capabilities: Vec<Capability> = self
                    .links
                    .values()
                    .flat_map(|link| link.capabilities.iter().cloned())
                    .filter(|capability| seen.insert(capability.clone()))
                    .collect();
                let options = self
                    .links
                    .values()
                    .flat_map(|link| link.options.iter().cloned())
                    .collect();
                Ok((
                    Response::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").into(),
                        capabilities,
                        options,
                    },
                    vec![],
                ))
            }
            Request::Open { .. } | Request::Close => Ok((Response::Ok, vec![])),
            // The raw connection, not `Client::call`: a relay must hand a
            // refusal back as the refusal it is, rather than adopt it as its
            // own failure and hang up.
            other => {
                let capability = needed_for(&other).expect("the rest are answered above");
                self.link_for(&capability)?.connection.call(other, payload)
            }
        }
    }

    fn set_console_sink(&mut self, sink: ConsoleSink) {
        self.upstream = Some(sink);
    }
}

/// `tock broker`: serve on stdio, for a subcommand that was not launched by
/// `tock` and so has no inherited connection.
pub(crate) fn broker(g: &Global) -> Result<(), CliError> {
    let mut broker = watched(
        Broker::new(load_settings(g)?, g.trace_protocol),
        g.trace_protocol,
    );
    // Not `.lock()` on stdout: `serve_streaming` keeps the writer past this
    // call, so it must be `Send + 'static`, which `StdoutLock` is not.
    tock_proto::serve_streaming(std::io::stdin().lock(), std::io::stdout(), &mut broker)
        .map_err(tock_client::proto)
}

/// Wrap a broker so `--trace-protocol` shows what the client asked, not only
/// what we went on to ask the transport. Without this the interesting half of
/// the conversation is invisible once clients are separate processes.
///
/// [`tock_proto::Traced`] forwards [`Connection::set_console_sink`] to what
/// it wraps, so the broker gets its sink either way.
pub(crate) fn watched(broker: Broker, trace: bool) -> Box<dyn Connection> {
    if !trace {
        return Box::new(broker);
    }
    Box::new(tock_proto::Traced {
        peer: "client",
        inner: Box::new(broker),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use tock_board::{BoardFile, Settings, Source, Sourced, TransportContext};
    use tock_proto::{read_frame, write_frame};

    use super::*;

    /// How long anything here may take before it is a failure. Every hop is
    /// a socket pair inside this process, so a wait this long is a wedge.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// Settings that route every capability to `transport`, which is all
    /// the broker reads out of them.
    fn settings(transport: &str) -> Settings {
        settings_routing(&[(tock_board::DEFAULT_ROUTE, transport)])
    }

    /// Settings with a route per `(capability, transport)` pair, for the
    /// board that is reached more than one way.
    fn settings_routing(routes: &[(&str, &str)]) -> Settings {
        let board = BoardFile::parse(
            "test board",
            r#"
            [board]
            name = "test"
            arch = "cortex-m4"
            [layout]
            flash = { start = 0, size = "1M" }
            kernel = { start = 0, size = "256K" }
            apps = { start = "256K", size = "768K" }
            "#,
        )
        .expect("the test board file parses");
        Settings {
            board_file: sourced("test board"),
            board,
            transport: TransportContext {
                routes: routes
                    .iter()
                    .map(|(capability, transport)| (capability.to_string(), sourced(transport)))
                    .collect(),
                options: BTreeMap::new(),
            },
            device: None,
            project: None,
            user: None,
        }
    }

    fn sourced(value: &str) -> Sourced<String> {
        Sourced {
            value: value.into(),
            source: Source::Flag("--transport".into()),
        }
    }

    /// A fake `tock-transport-*` on `conn`: answers `Hello` with
    /// `capabilities`, `Open` and `Close` as any transport does, and
    /// everything else however `answer` says. Whatever is sent down the
    /// returned channel is pushed as a `Response::Console` frame by a thread
    /// of its own.
    fn fake_transport(
        conn: UnixStream,
        capabilities: Vec<Capability>,
        mut answer: impl FnMut(&Request, &[u8]) -> (Response, Vec<u8>) + Send + 'static,
    ) -> (mpsc::Sender<Vec<u8>>, thread::JoinHandle<()>) {
        let writer = Arc::new(Mutex::new(conn.try_clone().expect("clone the socket")));
        let mut reader = conn;
        let (push, pushes) = mpsc::channel::<Vec<u8>>();
        thread::spawn({
            let writer = writer.clone();
            move || {
                while let Ok(bytes) = pushes.recv() {
                    let mut writer = writer.lock().expect("the fake transport's writer");
                    if write_frame(&mut *writer, &Response::Console, &bytes).is_err() {
                        break;
                    }
                }
            }
        });
        let responder = thread::spawn(move || {
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
                    Request::Open { .. } | Request::Close => (Response::Ok, vec![]),
                    other => answer(other, &payload),
                };
                let closing = request == Request::Close;
                let mut writer = writer.lock().expect("the fake transport's writer");
                if write_frame(&mut *writer, &response, &out).is_err() {
                    break;
                }
                drop(writer);
                if closing {
                    break;
                }
            }
        });
        (push, responder)
    }

    /// A broker served by `serve_streaming` on its own thread, reaching
    /// `transport` instead of starting one. Hands back the client's end of
    /// its connection and the thread to join at the end. Built inside that
    /// thread because neither a `Client` nor a `Broker` holding one is
    /// `Send`.
    fn broker_over(
        settings: Settings,
        transport: UnixStream,
        trace: bool,
    ) -> (UnixStream, thread::JoinHandle<Result<(), ProtoError>>) {
        broker_over_all(settings, vec![("fake", transport)], trace)
    }

    /// [`broker_over`], for the board reached by more than one transport:
    /// each is found by the name a route gave it.
    fn broker_over_all(
        settings: Settings,
        transports: Vec<(&'static str, UnixStream)>,
        trace: bool,
    ) -> (UnixStream, thread::JoinHandle<Result<(), ProtoError>>) {
        let (client, ours) = UnixStream::pair().expect("a socket pair");
        client
            .set_read_timeout(Some(PATIENCE))
            .expect("a read timeout, so no read here can hang the suite");
        let served = thread::spawn(move || {
            let reader = ours.try_clone().expect("clone the socket");
            let mut transports: BTreeMap<&str, UnixStream> = transports.into_iter().collect();
            let broker = Broker::over(
                settings,
                Box::new(move |name| {
                    let conn = transports
                        .remove(name)
                        .expect("a fake transport, opened once");
                    let reader = conn.try_clone().expect("clone the socket");
                    Ok((Box::new(reader), Box::new(conn), None))
                }),
            );
            let mut broker = watched(broker, trace);
            tock_proto::serve_streaming(reader, ours, &mut broker)
        });
        (client, served)
    }

    /// Greet the broker as a client does, and hand back what it says it can
    /// do.
    fn hello(client: &mut UnixStream, needs: Vec<Capability>) -> Vec<Capability> {
        write_frame(
            client,
            &Request::Hello {
                protocol: tock_proto::PROTOCOL_VERSION,
                client: "test".into(),
                needs,
            },
            &[],
        )
        .expect("greet the broker");
        match next_frame(client) {
            (Response::Hello { capabilities, .. }, _) => capabilities,
            (other, _) => panic!("not a greeting: {other:?}"),
        }
    }

    /// Send `request` and read whatever comes back.
    fn ask(client: &mut UnixStream, request: Request) -> (Response, Vec<u8>) {
        write_frame(client, &request, &[]).expect("ask the broker");
        next_frame(client)
    }

    /// The next frame the broker wrote. The read timeout set in
    /// [`broker_over`] turns "never" into a failure rather than a hang.
    fn next_frame(client: &mut UnixStream) -> (Response, Vec<u8>) {
        read_frame::<_, Response>(client)
            .expect("the broker said nothing in time")
            .expect("the broker closed the connection")
    }

    /// End the session the way a client does, and let everything wind down.
    fn close(mut client: UnixStream) {
        assert_eq!(ask(&mut client, Request::Close).0, Response::Ok);
    }

    /// Join `handle`, failing instead of hanging if it never returns.
    fn join_within<T: Send + 'static>(handle: thread::JoinHandle<T>) -> T {
        let (done, joined) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(handle.join());
        });
        match joined.recv_timeout(PATIENCE) {
            Ok(result) => result.expect("a test thread panicked"),
            Err(_) => panic!("a test thread did not finish in time"),
        }
    }

    #[test]
    fn a_push_from_the_transport_reaches_the_client() {
        let (transport, device) = UnixStream::pair().expect("a socket pair");
        let (push, responder) =
            fake_transport(
                device,
                vec![Capability::Console],
                |request, _| match request {
                    Request::ConsoleAttach | Request::ConsoleDetach => (Response::Ok, vec![]),
                    other => panic!("unexpected request reached the transport: {other:?}"),
                },
            );
        let (mut client, served) = broker_over(settings("fake"), transport, false);

        let can = hello(&mut client, vec![Capability::Console]);
        assert!(
            can.contains(&Capability::Console),
            "a broker relaying a console transport must say so: {can:?}"
        );
        assert_eq!(
            ask(&mut client, Request::ConsoleAttach).0,
            Response::Ok,
            "the attach must be relayed and its answer brought back"
        );

        // Unprompted, with no request outstanding: these must still arrive.
        push.send(b"one".to_vec()).expect("push");
        assert_eq!(
            next_frame(&mut client),
            (Response::Console, b"one".to_vec())
        );
        push.send(b"two".to_vec()).expect("push");
        assert_eq!(
            next_frame(&mut client),
            (Response::Console, b"two".to_vec())
        );

        assert_eq!(ask(&mut client, Request::ConsoleDetach).0, Response::Ok);
        close(client);
        join_within(served).expect("serve_streaming");
        join_within(responder);
    }

    #[test]
    fn a_push_reaches_the_client_through_the_tracer() {
        let (transport, device) = UnixStream::pair().expect("a socket pair");
        let (push, responder) = fake_transport(device, vec![Capability::Console], |_, _| {
            (Response::Ok, vec![])
        });
        // `--trace-protocol` puts a decorator between `serve_streaming` and
        // the broker. The sink must still arrive.
        let (mut client, served) = broker_over(settings("fake"), transport, true);

        assert!(hello(&mut client, vec![Capability::Console]).contains(&Capability::Console));
        assert_eq!(ask(&mut client, Request::ConsoleAttach).0, Response::Ok);

        push.send(b"traced".to_vec()).expect("push");
        assert_eq!(
            next_frame(&mut client),
            (Response::Console, b"traced".to_vec()),
            "tracing the conversation must not swallow the console sink"
        );

        close(client);
        join_within(served).expect("serve_streaming");
        join_within(responder);
    }

    /// A board reached two ways, as the nrf52840dk is: flash over a probe,
    /// the console over a bridge beside it. Both claim `console`, and the
    /// greeting offers every capability its links have, each exactly once.
    #[test]
    fn a_capability_two_links_share_is_offered_once() {
        let (probe, probe_device) = UnixStream::pair().expect("a socket pair");
        let (bridge, bridge_device) = UnixStream::pair().expect("a socket pair");
        let ok = |_: &Request, _: &[u8]| (Response::Ok, vec![]);
        let (_, probe_responder) = fake_transport(
            probe_device,
            vec![Capability::MemoryRead, Capability::Console],
            ok,
        );
        let (_, bridge_responder) = fake_transport(bridge_device, vec![Capability::Console], ok);
        let (mut client, served) = broker_over_all(
            settings_routing(&[(tock_board::DEFAULT_ROUTE, "probe"), ("console", "bridge")]),
            vec![("probe", probe), ("bridge", bridge)],
            false,
        );

        let mut can = hello(
            &mut client,
            vec![Capability::MemoryRead, Capability::Console],
        );
        can.sort_by_key(|capability| capability.name());
        assert_eq!(
            can,
            vec![Capability::Console, Capability::MemoryRead],
            "every capability the links have, each offered once"
        );

        close(client);
        join_within(served).expect("serve_streaming");
        join_within(probe_responder);
        join_within(bridge_responder);
    }

    #[test]
    fn a_reset_is_answered_while_the_console_is_attached() {
        let (transport, device) = UnixStream::pair().expect("a socket pair");
        let (push, responder) = fake_transport(
            device,
            vec![Capability::Console, Capability::Reset],
            |_, _| (Response::Ok, vec![]),
        );
        let (mut client, served) = broker_over(settings("fake"), transport, false);

        hello(&mut client, vec![Capability::Console]);
        assert_eq!(ask(&mut client, Request::ConsoleAttach).0, Response::Ok);
        push.send(b"before".to_vec()).expect("push");
        assert_eq!(
            next_frame(&mut client),
            (Response::Console, b"before".to_vec())
        );

        // A request still has to be answered while the console is pushing
        // over the same link, and the next push still has to arrive.
        assert_eq!(ask(&mut client, Request::Reset).0, Response::Ok);
        push.send(b"after".to_vec()).expect("push");
        assert_eq!(
            next_frame(&mut client),
            (Response::Console, b"after".to_vec())
        );

        close(client);
        join_within(served).expect("serve_streaming");
        join_within(responder);
    }
}
