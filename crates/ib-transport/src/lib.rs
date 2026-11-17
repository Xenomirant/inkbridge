//! Inkbridge transport layer.
//!
//! - [`Identity`]: each device's long-lived self-signed certificate. Peers are
//!   identified by its SHA-256 [`Fingerprint`], not by any CA.
//! - [`PairingOffer`]: what the host's QR code encodes (addresses, host
//!   fingerprint, one-time secret).
//! - [`HostEndpoint`] / [`ClientEndpoint`]: QUIC endpoints. The client pins
//!   the host fingerprint from the QR code; the host requires a client
//!   certificate and exposes its fingerprint so the session layer can check it
//!   against the paired-device list.
//! - [`Control`]: length-prefixed `ControlMessage`s on the reliable stream;
//!   [`PenChannel`]: `PenBatch` datagrams.
//!
//! No session policy lives here (that's `ib-session`), so this crate stays
//! small and independently testable.

mod endpoint;
mod framing;
mod identity;
mod pairing;
mod tls;
mod video;

pub use endpoint::{
    ALPN, ClientEndpoint, Connection, HostEndpoint, LinkStats, PenChannel, UsbListener,
};
pub use framing::{Control, ControlReceiver, ControlSender};
pub use identity::{Fingerprint, Identity};
pub use pairing::{PairingOffer, PairingOfferError, PairingSecret};
pub use video::{VideoReceiver, VideoSender};

/// Errors from the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("QUIC connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("QUIC connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream read: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("datagram: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol decode: {0}")]
    Decode(#[from] ib_protocol::DecodeError),
    #[error("protocol encode: {0}")]
    Encode(#[from] ib_protocol::EncodeError),
    #[error("peer sent no client certificate")]
    NoPeerCertificate,
    #[error("message length {0} exceeds limit")]
    TooLong(usize),
}
