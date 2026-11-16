use serde::{Deserialize, Serialize};

/// Kind of encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameKind {
    /// Self-contained; decoders can start here.
    Key,
    /// Depends on previous frames (intra-refresh frames are also `Delta`).
    Delta,
}

/// Header and payload of one encoded video access unit.
///
/// `capture_t_us` and `encode_done_t_us` (host clock) let the client compute
/// per-stage latency once clocks are synchronised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoChunk {
    pub frame_no: u64,
    pub kind: FrameKind,
    pub capture_t_us: u64,
    pub encode_done_t_us: u64,
    /// Annex-B H.264/HEVC bitstream.
    pub payload: Vec<u8>,
}
