use serde::{Deserialize, Serialize};

use crate::{Capabilities, PenSample, ProtocolVersion, TransportKind, VideoCodec};

/// A rectangle in host desktop coordinates (physical pixels; may be negative
/// on multi-monitor setups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// First message from the client once the encrypted connection is up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: ProtocolVersion,
    /// Human-readable device name shown on the host ("Xiaoxin Pad 2024").
    pub device_name: String,
    pub caps: Capabilities,
    /// Physical screen size in pixels, used to pick the stream resolution.
    pub screen_width: u32,
    pub screen_height: u32,
}

/// Host reply when the session is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHello {
    pub version: ProtocolVersion,
    pub host_name: String,
    pub caps: Capabilities,
    /// Negotiated codec, or `None` for pen-only sessions.
    pub codec: Option<VideoCodec>,
    /// Desktop area the tablet maps onto.
    pub region: Region,
    pub transport: TransportKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectReason {
    IncompatibleVersion,
    NotPaired,
    /// Pairing attempted with a wrong or expired one-time secret.
    BadPairingSecret,
    Busy,
}

/// Length of the one-time pairing secret carried in the QR code.
pub const PAIRING_SECRET_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub reason: RejectReason,
    pub host_version: ProtocolVersion,
}

/// Messages on the reliable control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// First message from an unpaired client: proves it scanned the host's QR
    /// code. On success the host remembers the client's certificate.
    PairRequest {
        secret: [u8; PAIRING_SECRET_LEN],
        device_name: String,
    },
    PairAccepted {
        host_name: String,
    },
    ClientHello(ClientHello),
    HostHello(HostHello),
    Reject(Reject),
    /// Clock-sync probe (NTP-style); `t0_us` on the sender's clock.
    Ping {
        t0_us: u64,
    },
    /// Reply carrying the receiver's receive (`t1`) and send (`t2`) times.
    Pong {
        t0_us: u64,
        t1_us: u64,
        t2_us: u64,
    },
    /// Change the mapped/streamed desktop region.
    SetRegion(Region),
    /// Pen contact/tool/button transitions, mirrored reliably so state can
    /// never get stuck if the corresponding datagram is lost.
    PenTransition(PenSample),
    /// Ask the host for an immediate keyframe (decoder reset, packet loss).
    RequestKeyframe,
    /// Bitrate hint from the client's congestion estimate, kbit/s.
    BitrateHint {
        kbps: u32,
    },
    Goodbye,
}
