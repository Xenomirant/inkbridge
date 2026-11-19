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
    /// The whole display the region lies on (bounds for the zoomable view).
    pub display: Region,
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

/// Phase of one finger in a [`ControlMessage::Touch`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    /// The gesture was aborted (e.g. a pen came in range): no click/tap.
    Cancel,
}

/// One finger. `x`/`y` are normalised to the streamed region, like pen
/// coordinates; `id` is stable for the life of the finger.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TouchContact {
    pub id: u8,
    pub x: f32,
    pub y: f32,
    pub phase: TouchPhase,
}

impl TouchContact {
    /// Largest number of simultaneous fingers the protocol carries.
    pub const MAX: usize = 5;
}

/// A display the host can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Name for the tablet's menu ("Built-in display").
    pub name: String,
    /// Its bounds, in the coordinates `SetRegion` uses while it is shown.
    pub bounds: Region,
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
    /// Change the mapped/streamed desktop region. Sent by the host after a
    /// session starts and whenever the region changes (e.g. on rotation).
    SetRegion(Region),
    /// Client → host: show this part of the display (zoomable view), in
    /// desktop pixels. The host adjusts it (tablet aspect, inside the
    /// display, minimum size) and answers with `SetRegion`.
    RequestView(Region),
    /// Client → host: the tablet's usable area changed (rotation, split
    /// screen), in physical pixels. The host answers with `SetRegion`.
    Viewport {
        width: u32,
        height: u32,
    },
    /// Pen contact/tool/button transitions, mirrored reliably so state can
    /// never get stuck if the corresponding datagram is lost.
    PenTransition(PenSample),
    /// Multi-finger pinch/zoom frame, injected as native touch so the host
    /// OS zooms around the fingers. Every frame lists all fingers.
    Touch(Vec<TouchContact>),
    /// Two-finger scroll, touchpad style. `x`/`y`: centroid of the fingers,
    /// `dx`/`dy`: movement since the previous message (fingers down = positive
    /// `dy`), all normalised to the region like pen coordinates. `t_us` is
    /// the tablet's timestamp (velocity for acceleration and inertia);
    /// `end` marks the fingers lifting.
    Scroll {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        t_us: u64,
        end: bool,
    },
    /// Ask the host for an immediate keyframe (decoder reset, packet loss).
    RequestKeyframe,
    /// Bitrate hint from the client's congestion estimate, kbit/s.
    BitrateHint {
        kbps: u32,
    },
    Goodbye,
    /// Host → client (protocol 0.2+): the displays the tablet can choose
    /// from and the index of the one being shown. Sent when there is more
    /// than one, after the session starts and after every switch.
    Displays {
        displays: Vec<DisplayInfo>,
        current: u8,
    },
    /// Client → host (protocol 0.2+): show and draw on this display (an
    /// index into the last `Displays`). The host answers with `SetRegion`
    /// and `Displays`.
    SelectDisplay {
        index: u8,
    },
    /// Host → client (protocol 0.3+): whether the host can type for the
    /// tablet. Sent after the session starts; without it the client shows
    /// no keyboard. Only sent to clients of 0.3 or later.
    Keyboard {
        available: bool,
    },
    /// Client → host (protocol 0.3+, only if the host said `Keyboard`):
    /// type this text, at most [`crate::MAX_TEXT_LEN`] bytes.
    Text(String),
    /// Client → host (protocol 0.3+): press and release `key` while
    /// holding `modifiers`.
    Key {
        key: crate::Key,
        modifiers: crate::Modifiers,
    },
}
