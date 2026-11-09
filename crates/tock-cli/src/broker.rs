//! Serving the protocol instead of speaking it.
//!
//! The same broker answers two carriers. `tock broker` serves it on stdio,
//! for a subcommand nobody launched. [`external`](crate::external) serves it
//! on a socket the subcommand inherits as fd 3, which is the usual case and
//! the one where `tock` genuinely owns the connection the PRD says it does.
//!
//! The transport is opened on the first request that needs it, so a
//! subcommand that never touches a device -- building a TAB, say -- does not
//! spawn one, and does not fail on a machine with no board attached.

use tock_proto::{Client, Connection, ProtoError, Request, Response};

use tock_client::{CliError, Code};

use crate::cli::Global;
use crate::config::load_settings;
use crate::connection;

pub(crate) struct Broker {
    settings: tock_board::Settings,
    trace: bool,
    /// The connection to the transport, opened on demand. Not the device
    /// itself -- this is the wire to it.
    link: Option<Client>,
}

impl Broker {
    pub(crate) fn new(settings: tock_board::Settings, trace: bool) -> Broker {
        Broker {
            settings,
            trace,
            link: None,
        }
    }

    /// Open on demand. A failure to open is fatal to the connection -- there
    /// will never be a device -- so it travels as an error rather than a
    /// response, carrying whatever the CLI would have told the user.
    fn link(&mut self) -> Result<&mut Client, ProtoError> {
        if self.link.is_none() {
            let opened = connection::open(&self.settings, self.trace).map_err(fatal)?;
            self.link = Some(opened);
        }
        Ok(self.link.as_mut().expect("just opened"))
    }
}

fn fatal(e: CliError) -> ProtoError {
    let kind = match e.code {
        Code::Config => tock_proto::ErrorKind::Selection,
        Code::Capability => tock_proto::ErrorKind::Unsupported,
        _ => tock_proto::ErrorKind::Device,
    };
    let error = ProtoError::failed(e.message).with_kind(kind);
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
            Request::Hello { .. } => {
                let link = self.link()?;
                Ok((
                    Response::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        transport: link.transport.clone(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        capabilities: link.capabilities.clone(),
                    },
                    vec![],
                ))
            }
            Request::Open { .. } | Request::Close => Ok((Response::Ok, vec![])),
            // The raw connection, not `Client::call`: a relay must hand a
            // refusal back as the refusal it is, rather than adopt it as its
            // own failure and hang up.
            other => self.link()?.connection.call(other, payload),
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
