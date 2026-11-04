//! Serving the protocol instead of speaking it.
//!
//! `tock broker` resolves the context, opens the transport, and then answers
//! requests on stdio -- so a subcommand that is a separate process reaches the
//! same device, resolved the same way, without opening anything itself.

use crate::cli::Global;
use crate::config::load_context;
use crate::device::{self, Device};
use crate::error::CliError;

/// Answer Hello and Open ourselves (the broker has already opened the
/// transport with the resolved context) and forward everything else.
pub(crate) fn broker(g: &Global) -> Result<(), CliError> {
    let ctx = load_context(g)?;
    struct Broker {
        device: Device,
    }

    impl tock_proto::Connection for Broker {
        fn call(
            &mut self,
            request: tock_proto::Request,
            payload: Vec<u8>,
        ) -> Result<(tock_proto::Response, Vec<u8>), tock_proto::ProtoError> {
            use tock_proto::{Request as Rq, Response as Rs};
            match request {
                // Answered here: the transport was already opened with the
                // resolved context before this client connected.
                Rq::Hello { .. } => Ok((
                    Rs::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        transport: self.device.transport.clone(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        capabilities: self.device.capabilities.clone(),
                    },
                    vec![],
                )),
                Rq::Open { .. } | Rq::Close => Ok((Rs::Ok, vec![])),
                other => self.device.forward(other, payload),
            }
        }
    }

    let mut broker = Broker {
        device: Device::connect(&ctx, g.trace_protocol)?,
    };
    tock_proto::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut broker,
    )
    .map_err(device::proto)
}
