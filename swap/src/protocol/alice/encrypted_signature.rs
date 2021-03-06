use crate::network::request_response::{CborCodec, EncryptedSignatureProtocol, TIMEOUT};
use crate::protocol::bob::EncryptedSignature;
use anyhow::{anyhow, Error, Result};
use libp2p::request_response::{
    ProtocolSupport, RequestResponse, RequestResponseConfig, RequestResponseEvent,
    RequestResponseMessage, ResponseChannel,
};
use libp2p::{NetworkBehaviour, PeerId};
use std::time::Duration;
use tracing::debug;

#[derive(Debug)]
pub enum OutEvent {
    MsgReceived {
        msg: EncryptedSignature,
        channel: ResponseChannel<()>,
        peer: PeerId,
    },
    AckSent,
    Failure {
        peer: PeerId,
        error: Error,
    },
}

/// A `NetworkBehaviour` that represents receiving the Bitcoin encrypted
/// signature from Bob.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "OutEvent", event_process = false)]
#[allow(missing_debug_implementations)]
pub struct Behaviour {
    rr: RequestResponse<CborCodec<EncryptedSignatureProtocol, EncryptedSignature, ()>>,
}

impl Behaviour {
    pub fn send_ack(&mut self, channel: ResponseChannel<()>) -> Result<()> {
        self.rr
            .send_response(channel, ())
            .map_err(|err| anyhow!("Failed to ack encrypted signature: {:?}", err))
    }
}

impl Default for Behaviour {
    fn default() -> Self {
        let timeout = Duration::from_secs(TIMEOUT);
        let mut config = RequestResponseConfig::default();
        config.set_request_timeout(timeout);

        Self {
            rr: RequestResponse::new(
                CborCodec::default(),
                vec![(EncryptedSignatureProtocol, ProtocolSupport::Inbound)],
                config,
            ),
        }
    }
}

impl From<RequestResponseEvent<EncryptedSignature, ()>> for OutEvent {
    fn from(event: RequestResponseEvent<EncryptedSignature, ()>) -> Self {
        match event {
            RequestResponseEvent::Message {
                peer,
                message:
                    RequestResponseMessage::Request {
                        request, channel, ..
                    },
                ..
            } => {
                debug!("Received encrypted signature from {}", peer);
                OutEvent::MsgReceived {
                    msg: request,
                    channel,
                    peer,
                }
            }
            RequestResponseEvent::Message {
                message: RequestResponseMessage::Response { .. },
                peer,
            } => OutEvent::Failure {
                peer,
                error: anyhow!("Alice should not get a Response"),
            },
            RequestResponseEvent::InboundFailure { error, peer, .. } => OutEvent::Failure {
                peer,
                error: anyhow!("Inbound failure: {:?}", error),
            },
            RequestResponseEvent::OutboundFailure { error, peer, .. } => OutEvent::Failure {
                peer,
                error: anyhow!("Outbound failure: {:?}", error),
            },
            RequestResponseEvent::ResponseSent { .. } => OutEvent::AckSent,
        }
    }
}
