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

use std::collections::BTreeMap;

use tock_proto::{Capability, Client, Connection, ProtoError, Request, Response};

use tock_client::{CliError, Code};

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
}

/// The capability a request needs, and so which transport serves it.
///
/// The one place a request is mapped to a capability. `Hello`, `Open` and
/// `Close` are answered here and need nothing.
fn needed_for(request: &Request) -> Option<Capability> {
    Some(match request {
        Request::Read { .. } => Capability::MemoryRead,
        Request::Write { .. } => Capability::MemoryWrite,
        Request::ConsoleRead { .. } | Request::ConsoleWrite => Capability::Console,
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
        }
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
            let opened = connection::open(&self.settings, &name, self.trace).map_err(fatal)?;
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
            // Answered here: the client did not open this transport and does
            // not get to say when it closes.
            // Answered here: the client did not open any of this and does
            // not get to say when it closes. What it declared it needs is
            // checked now, against the routing, so "nothing here can do
            // that" arrives before the work rather than during it.
            Request::Hello { needs, .. } => {
                for capability in &needs {
                    self.link_for(capability)?;
                }
                let mut capabilities: Vec<Capability> = self
                    .links
                    .values()
                    .flat_map(|link| link.capabilities.iter().cloned())
                    .collect();
                capabilities.dedup();
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
}

/// `tock broker`: serve on stdio, for a subcommand that was not launched by
/// `tock` and so has no inherited connection.
pub(crate) fn broker(g: &Global) -> Result<(), CliError> {
    let mut broker = watched(
        Broker::new(load_settings(g)?, g.trace_protocol),
        g.trace_protocol,
    );
    tock_proto::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut broker,
    )
    .map_err(tock_client::proto)
}

/// Wrap a broker so `--trace-protocol` shows what the client asked, not only
/// what we went on to ask the transport. Without this the interesting half of
/// the conversation is invisible once clients are separate processes.
pub(crate) fn watched(broker: Broker, trace: bool) -> Box<dyn Connection> {
    if trace {
        Box::new(tock_proto::Traced {
            peer: "client",
            inner: Box::new(broker),
        })
    } else {
        Box::new(broker)
    }
}
