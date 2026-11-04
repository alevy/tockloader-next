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
        board: String,
        flash: tock_proto::FlashInfo,
    }
    impl Broker {
        /// External subcommands reach the device through here, so this is
        /// where their requests are checked. Transports check their own
        /// bounds too -- they are separately spawnable -- but only the broker
        /// knows which board it is, so only the broker can say which one the
        /// address is outside of.
        fn check(&self, request: &tock_proto::Request, payload: &[u8]) -> Option<String> {
            let (address, len) = match *request {
                tock_proto::Request::Read { address, len } => (address, len),
                tock_proto::Request::Write { address } => (address, payload.len() as u64),
                _ => return None,
            };
            let (start, size) = (self.flash.start, self.flash.size);
            let board = &self.board;
            if address < start || address + len > start + size {
                return Some(format!(
                    "{address:#x}+{len} is outside flash on {board} ({start:#x} +{size:#x})"
                ));
            }
            None
        }
    }
    /// The broker is both roles at once: it answers its own client while
    /// calling the transport beneath it, which is how a third-party
    /// subcommand reaches a device it never opened. With one trait that is
    /// just delegation.
    impl tock_proto::Connection for Broker {
        fn call(
            &mut self,
            request: tock_proto::Request,
            payload: Vec<u8>,
        ) -> Result<(tock_proto::Response, Vec<u8>), tock_proto::ProtoError> {
            use tock_proto::{Request as Rq, Response as Rs};
            if let Some(problem) = self.check(&request, &payload) {
                return Ok((
                    Rs::error(tock_proto::ErrorKind::InvalidRequest, problem),
                    vec![],
                ));
            }
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

    let layout = &ctx.board.layout;
    let mut broker = Broker {
        board: ctx.board.board.name.clone(),
        flash: tock_proto::FlashInfo {
            start: layout.flash.start.0,
            size: layout.flash.size.0,
        },
        device: Device::connect(&ctx, g.trace_protocol)?,
    };
    tock_proto::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut broker,
    )
    .map_err(device::proto)
}
