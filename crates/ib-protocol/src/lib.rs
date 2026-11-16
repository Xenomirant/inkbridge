//! Inkbridge wire protocol.
//!
//! This crate defines every message exchanged between a desktop **host** and a
//! tablet **client**. It performs no I/O: transports hand it bytes and get
//! typed messages back. That keeps it trivially testable and lets every
//! platform (Rust host, Android and iPad via `UniFFI`) share one definition.
//!
//! Channels (see `docs/protocol.md`):
//! - **control**: reliable, ordered stream carrying [`ControlMessage`];
//! - **pen**: unreliable datagrams carrying [`PenBatch`];
//! - **video**: unidirectional streams carrying [`VideoChunk`].

mod caps;
mod codec;
mod control;
mod pen;
mod version;
mod video;

pub use caps::{Capabilities, PenFeatures, TransportKind, VideoCodec};
pub use codec::{DecodeError, EncodeError, MAX_MESSAGE_LEN, decode, encode};
pub use control::{
    ClientHello, ControlMessage, HostHello, PAIRING_SECRET_LEN, Region, Reject, RejectReason,
    TouchContact, TouchPhase,
};
pub use pen::{Buttons, PenBatch, PenSample, PenState, Tool};
pub use version::{PROTOCOL_VERSION, ProtocolVersion};

/// Default UDP port: the host listens here (Wi-Fi), and a tablet listens here
/// on its USB tethering link for the host to dial in. Below 32768, outside
/// the ranges operating systems hand out automatically.
pub const DEFAULT_PORT: u16 = 27840;
pub use video::{FrameKind, VideoChunk};
