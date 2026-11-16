//! Screen streaming pipeline: capture → scale → encode on a dedicated
//! thread, handing encoded chunks to the async sender.
//!
//! The video stream is reliable and ordered, so a delta frame may never be
//! dropped on its own: if the network falls behind (the outgoing queue is
//! full), frames are skipped *and the next one is forced to be a keyframe*,
//! which resynchronises the decoder instead of corrupting the picture.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use ib_capture::{Frame, FrameData, ScreenSource};
use ib_encode::{EncoderConfig, OpenH264Encoder, VideoEncoder, fit_within, scale_bgra};
use ib_protocol::{Region, VideoChunk, VideoCodec};
use tokio::sync::mpsc::error::TrySendError;

/// The software path is CPU-bound: cap its pixel count (≈1080p). A budget
/// rather than per-side limits, so a portrait strip (e.g. 900×1440) keeps
/// its full resolution. The hardware path (M2b) streams at full size.
const SOFTWARE_MAX_PIXELS: u64 = 1920 * 1088;
const FPS: u16 = 60;

#[derive(Debug)]
pub enum VideoCommand {
    SetRegion(Region),
    Keyframe,
    Stop,
}

/// Handle to a running pipeline; stops it when dropped.
#[derive(Debug)]
pub struct VideoPipeline {
    commands: Sender<VideoCommand>,
    thread: Option<JoinHandle<()>>,
}

impl VideoPipeline {
    /// Starts capturing `region`, encoding for a client that can show at most
    /// `client_max` pixels, and sending chunks to `out`.
    pub fn start(
        region: Region,
        client_max: (u32, u32),
        out: tokio::sync::mpsc::Sender<VideoChunk>,
    ) -> anyhow::Result<Self> {
        Self::start_with(ib_capture::platform_source()?, region, client_max, out)
    }

    /// Like [`start`](Self::start) with an explicit capture source (tests).
    pub fn start_with(
        source: Box<dyn ScreenSource>,
        region: Region,
        client_max: (u32, u32),
        out: tokio::sync::mpsc::Sender<VideoChunk>,
    ) -> anyhow::Result<Self> {
        let (commands, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("inkbridge-video".into())
            .spawn(move || {
                if let Err(e) = run(source, region, client_max, &rx, &out) {
                    tracing::error!("video pipeline stopped: {e:#}");
                }
            })?;
        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    pub fn set_region(&self, region: Region) {
        let _ = self.commands.send(VideoCommand::SetRegion(region));
    }

    pub fn request_keyframe(&self) {
        let _ = self.commands.send(VideoCommand::Keyframe);
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        let _ = self.commands.send(VideoCommand::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Encoder for the given capture region.
fn encoder_for(region: Region, client_max: (u32, u32)) -> anyhow::Result<OpenH264Encoder> {
    let pixels = u64::from(region.width) * u64::from(region.height);
    let budget = if pixels > SOFTWARE_MAX_PIXELS {
        (SOFTWARE_MAX_PIXELS as f64 / pixels as f64).sqrt()
    } else {
        1.0
    };
    let (width, height) = fit_within(
        region.width,
        region.height,
        ((f64::from(region.width) * budget) as u32).min(client_max.0),
        ((f64::from(region.height) * budget) as u32).min(client_max.1),
    );
    // ~6 bits per pixel per second at 60 fps (≈12 Mbit/s at 1080p): generous,
    // because blurry text and strokes are worse than bandwidth on a LAN.
    let bitrate_kbps = (u64::from(width) * u64::from(height) * 6 / 1000) as u32;
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: FPS,
        bitrate_kbps,
    };
    tracing::info!(?region, width, height, bitrate_kbps, "video encoder");
    Ok(OpenH264Encoder::new(config)?)
}

fn run(
    mut source: Box<dyn ScreenSource>,
    mut region: Region,
    client_max: (u32, u32),
    commands: &Receiver<VideoCommand>,
    out: &tokio::sync::mpsc::Sender<VideoChunk>,
) -> anyhow::Result<()> {
    source.start(region)?;
    let mut encoder = encoder_for(region, client_max)?;
    let mut force_key = true;
    let mut frame_no = 0u64;
    let mut last: Option<Frame> = None;

    loop {
        loop {
            match commands.try_recv() {
                Ok(VideoCommand::SetRegion(r)) if r != region => {
                    region = r;
                    source.start(region)?;
                    encoder = encoder_for(region, client_max)?;
                    force_key = true;
                    last = None;
                }
                Ok(VideoCommand::SetRegion(_)) => {}
                // A keyframe needs a picture even if the screen is static:
                // re-encode the last captured frame.
                Ok(VideoCommand::Keyframe) => force_key = true,
                Ok(VideoCommand::Stop) | Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }

        let frame = match source.next_frame(Duration::from_millis(50))? {
            Some(f) => f,
            None if force_key => match last.clone() {
                Some(f) => f,
                None => continue,
            },
            None => continue,
        };
        let cfg = encoder.config();
        let scaled = scale_to(&frame, cfg.width, cfg.height);
        let encoded = encoder.encode(&scaled, force_key)?;
        frame_no += 1;
        let chunk = VideoChunk {
            frame_no,
            kind: encoded.kind,
            width: cfg.width,
            height: cfg.height,
            capture_t_us: encoded.capture_t_us,
            encode_done_t_us: ib_clock::now_us(),
            payload: encoded.data,
        };
        match out.try_send(chunk) {
            Ok(()) => force_key = false,
            // Network behind: skip, and resync with a keyframe next time.
            Err(TrySendError::Full(_)) => force_key = true,
            Err(TrySendError::Closed(_)) => return Ok(()),
        }
        last = Some(frame);
    }
}

fn scale_to(frame: &Frame, width: u32, height: u32) -> Frame {
    match &frame.data {
        FrameData::Cpu(px) if frame.width != width || frame.height != height => Frame {
            capture_t_us: frame.capture_t_us,
            width,
            height,
            data: FrameData::Cpu(scale_bgra(px, frame.width, frame.height, width, height)),
        },
        _ => frame.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_capture::FakeScreenSource;
    use ib_protocol::FrameKind;

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_keyframe_first_then_deltas_and_honours_requests() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let region = Region {
            x: 0,
            y: 0,
            width: 128,
            height: 80,
        };
        let p = VideoPipeline::start_with(
            Box::new(FakeScreenSource::default()),
            region,
            (1920, 1080),
            tx,
        )
        .unwrap();

        let first = rx.recv().await.unwrap();
        assert_eq!(first.kind, FrameKind::Key);
        assert_eq!((first.width, first.height), (128, 80));
        assert!(first.encode_done_t_us >= first.capture_t_us);
        // The fake changes every frame; later frames are deltas until asked.
        let mut saw_delta = false;
        for _ in 0..5 {
            saw_delta |= rx.recv().await.unwrap().kind == FrameKind::Delta;
        }
        assert!(saw_delta);
        p.request_keyframe();
        let mut got_key = false;
        for _ in 0..10 {
            if rx.recv().await.unwrap().kind == FrameKind::Key {
                got_key = true;
                break;
            }
        }
        assert!(got_key, "keyframe request must produce a keyframe");
        drop(p);
    }

    #[test]
    fn portrait_strip_keeps_full_resolution_landscape_is_capped() {
        let strip = encoder_for(
            Region {
                x: 830,
                y: 0,
                width: 900,
                height: 1440,
            },
            (1920, 1920),
        )
        .unwrap();
        assert_eq!((strip.config().width, strip.config().height), (900, 1440));
        let full = encoder_for(
            Region {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
            (1920, 1920),
        )
        .unwrap();
        let c = full.config();
        assert!(u64::from(c.width) * u64::from(c.height) <= SOFTWARE_MAX_PIXELS);
        assert!(
            c.width >= 1900,
            "landscape should stay ~1080p, got {}x{}",
            c.width,
            c.height
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_region_changes_the_picture_size() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let p = VideoPipeline::start_with(
            Box::new(FakeScreenSource::default()),
            Region {
                x: 0,
                y: 0,
                width: 128,
                height: 80,
            },
            (1920, 1080),
            tx,
        )
        .unwrap();
        rx.recv().await.unwrap();
        p.set_region(Region {
            x: 0,
            y: 0,
            width: 64,
            height: 96,
        });
        let mut resized = None;
        for _ in 0..20 {
            let c = rx.recv().await.unwrap();
            if (c.width, c.height) == (64, 96) {
                resized = Some(c);
                break;
            }
        }
        assert_eq!(resized.expect("resized frame").kind, FrameKind::Key);
    }
}
