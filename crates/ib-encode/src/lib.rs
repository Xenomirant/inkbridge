//! Video encoding.
//!
//! Backends (per milestone, `cfg`-gated): Media Foundation H.264 with D3D11
//! zero-copy (M2), VideoToolbox (M4), VA-API (M3), openh264 software
//! fallback (M2), optional direct NVENC if measurements justify it.
//!
//! Every backend must be configured for **latency over quality**: no
//! B-frames, zero lookahead, no frame queue (one frame in flight),
//! intra-refresh instead of periodic large keyframes. See ADR-0005.

mod openh264;
mod scale;

pub use crate::openh264::OpenH264Encoder;
pub use scale::{fit_within, scale_bgra};

use ib_capture::Frame;
use ib_protocol::{FrameKind, VideoCodec};

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder unavailable: {0}")]
    Unavailable(String),
    #[error("frame format not supported by this encoder: {0}")]
    UnsupportedFrame(String),
    #[error("encoding failed: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub kind: FrameKind,
    pub capture_t_us: u64,
    /// Annex-B bitstream.
    pub data: Vec<u8>,
}

pub trait VideoEncoder: Send {
    fn name(&self) -> &'static str;

    fn config(&self) -> EncoderConfig;

    /// Encodes one frame synchronously. Returning the output for the same
    /// frame (no internal queue) is part of the latency contract.
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError>;

    /// Applies a new target bitrate from the adaptive controller, effective
    /// from the next frame.
    fn set_bitrate(&mut self, kbps: u32);
}

/// Emits a tiny fake bitstream: a keyframe first or when forced, deltas otherwise.
#[derive(Debug)]
pub struct FakeEncoder {
    config: EncoderConfig,
    frames: u64,
}

impl FakeEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        Self { config, frames: 0 }
    }
}

impl VideoEncoder for FakeEncoder {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError> {
        let kind = if self.frames == 0 || force_keyframe {
            FrameKind::Key
        } else {
            FrameKind::Delta
        };
        self.frames += 1;
        let mut data = vec![0, 0, 0, 1]; // Annex-B start code
        data.extend_from_slice(&self.frames.to_be_bytes());
        Ok(EncodedFrame {
            kind,
            capture_t_us: frame.capture_t_us,
            data,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_capture::{FakeScreenSource, ScreenSource};
    use ib_protocol::Region;
    use std::time::Duration;

    #[test]
    fn fake_capture_to_fake_encoder() {
        let mut src = FakeScreenSource::default();
        src.start(Region {
            x: 0,
            y: 0,
            width: 64,
            height: 40,
        })
        .unwrap();
        let mut enc = FakeEncoder::new(EncoderConfig {
            codec: VideoCodec::H264,
            width: 64,
            height: 40,
            fps: 60,
            bitrate_kbps: 8_000,
        });
        let kinds: Vec<FrameKind> = (0..3)
            .map(|i| {
                let f = src.next_frame(Duration::ZERO).unwrap().unwrap();
                enc.encode(&f, i == 2).unwrap().kind
            })
            .collect();
        assert_eq!(kinds, [FrameKind::Key, FrameKind::Delta, FrameKind::Key]);
    }
}
