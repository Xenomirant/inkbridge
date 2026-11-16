//! Software H.264 encoder (Cisco openh264, BSD-2-Clause), compiled from
//! source. The universal fallback when no hardware encoder is usable, and
//! the first end-to-end encoder (M2a). Configured for screen content and
//! real-time use: no frame skipping, no B-frames (Constrained Baseline).

use ib_capture::{Frame, FrameData};
use ib_protocol::{FrameKind, VideoCodec};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};

use crate::{EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};

/// Periodic safety keyframe (~5 min at 60 fps); otherwise keyframes are
/// only produced on demand. `0` would mean "automatic" in openh264.
const KEYFRAME_INTERVAL: u32 = 18_000;

pub struct OpenH264Encoder {
    encoder: Encoder,
    config: EncoderConfig,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenH264Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        if config.codec != VideoCodec::H264 {
            return Err(EncodeError::Unavailable(
                "openh264 only encodes H.264".into(),
            ));
        }
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), Self::oh_config(&config))
                .map_err(|e| EncodeError::Unavailable(e.to_string()))?;
        Ok(Self { encoder, config })
    }

    fn oh_config(c: &EncoderConfig) -> OhConfig {
        OhConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(c.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(f32::from(c.fps)))
            // Never drop frames on our behalf: dropped deltas would desync the
            // reliable stream; the host decides when to skip.
            .skip_frames(false)
            // Keyframes only on demand (start, resize, client request): they
            // are large, and large frames cost latency on the wire.
            .intra_frame_period(IntraFramePeriod::from_num_frames(KEYFRAME_INTERVAL))
            .scene_change_detect(false)
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<EncodedFrame, EncodeError> {
        let FrameData::Cpu(bgra) = &frame.data else {
            return Err(EncodeError::UnsupportedFrame(
                "openh264 needs CPU (BGRA) frames".into(),
            ));
        };
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(EncodeError::UnsupportedFrame(format!(
                "frame {}x{} doesn't match encoder {}x{}",
                frame.width, frame.height, self.config.width, self.config.height
            )));
        }
        let yuv = YUVBuffer::from_rgb_source(BgraSliceU8::new(
            bgra,
            (frame.width as usize, frame.height as usize),
        ));
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let bitstream = self
            .encoder
            .encode(&yuv)
            .map_err(|e| EncodeError::Failed(e.to_string()))?;
        let kind = match bitstream.frame_type() {
            FrameType::IDR | FrameType::I => FrameKind::Key,
            _ => FrameKind::Delta,
        };
        Ok(EncodedFrame {
            kind,
            capture_t_us: frame.capture_t_us,
            data: bitstream.to_vec(),
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps;
        // openh264 adjusts on re-creation; bitrate changes are rare (adaptive
        // control arrives in M2b with the hardware encoder).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, shade: u8) -> Frame {
        Frame {
            capture_t_us: 1,
            width: w,
            height: h,
            data: FrameData::Cpu([shade, shade, shade, 255].repeat((w * h) as usize)),
        }
    }

    /// A white page with a short dark "stroke" whose length grows with `n`,
    /// like screen content while writing.
    fn page_with_stroke(width: u32, height: u32, len: u32) -> Frame {
        let mut page = frame(width, height, 255);
        if let FrameData::Cpu(px) = &mut page.data {
            for x in 10..(10 + len * 4).min(width - 1) {
                let at = ((height / 2 * width + x) * 4) as usize;
                px[at..at + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        page
    }

    #[test]
    fn encodes_annex_b_with_keyframe_first_and_on_demand() {
        let cfg = EncoderConfig {
            codec: VideoCodec::H264,
            width: 320,
            height: 192,
            fps: 60,
            bitrate_kbps: 2000,
        };
        let mut enc = OpenH264Encoder::new(cfg).unwrap();
        let first = enc.encode(&page_with_stroke(320, 192, 0), false).unwrap();
        assert_eq!(first.kind, FrameKind::Key);
        assert!(
            first.data.starts_with(&[0, 0, 0, 1]),
            "Annex-B start code expected"
        );
        for len in 1..5 {
            let delta = enc.encode(&page_with_stroke(320, 192, len), false).unwrap();
            assert_eq!(delta.kind, FrameKind::Delta, "frame {len}");
            assert!(
                delta.data.len() < first.data.len(),
                "a small change must be cheap"
            );
        }
        let forced = enc.encode(&page_with_stroke(320, 192, 6), true).unwrap();
        assert_eq!(forced.kind, FrameKind::Key);
    }

    #[test]
    fn rejects_mismatched_frames() {
        let cfg = EncoderConfig {
            codec: VideoCodec::H264,
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 500,
        };
        let mut enc = OpenH264Encoder::new(cfg).unwrap();
        assert!(enc.encode(&frame(32, 32, 0), false).is_err());
    }
}
