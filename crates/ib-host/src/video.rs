//! Screen streaming pipeline: capture → scale → encode on a dedicated
//! thread, handing encoded chunks to the async sender.
//!
//! The video stream is reliable and ordered, so an encoded delta frame may
//! never be dropped on its own. If the network falls behind (the outgoing
//! queue is full), frames are skipped *before* they are encoded, so the
//! decoder stays in sync and no keyframe is needed; the bitrate is lowered
//! and the newest picture is sent as soon as there is room.

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

/// Creates a replacement capture source (e.g. CPU capture for the software
/// encoder when the hardware path fails).
pub type SourceFactory = Box<dyn FnOnce() -> Box<dyn ScreenSource> + Send>;

#[derive(Debug)]
pub enum VideoCommand {
    SetRegion(Region),
    Keyframe,
    /// Bitrate the tablet asks for (adaptive control), kbit/s.
    Bitrate(u32),
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
        // Windows: GPU capture for the hardware encoder, CPU capture as the
        // fallback if no hardware encoder can be used.
        #[cfg(windows)]
        {
            let fallback: SourceFactory =
                Box::new(|| Box::new(ib_capture::WindowsScreenSource::new()));
            Self::start_with(
                Box::new(ib_capture::WindowsScreenSource::new_gpu()),
                Some(fallback),
                region,
                client_max,
                out,
            )
        }
        // macOS: the system's pixel buffers go to VideoToolbox as they are.
        #[cfg(target_os = "macos")]
        {
            let fallback: SourceFactory = Box::new(|| match ib_capture::MacScreenSource::new() {
                Ok(source) => Box::new(source),
                Err(e) => {
                    tracing::error!("screen capture: {e}");
                    Box::new(ib_capture::FakeScreenSource::default())
                }
            });
            Self::start_with(
                Box::new(ib_capture::MacScreenSource::new_gpu()?),
                Some(fallback),
                region,
                client_max,
                out,
            )
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Self::start_with(
                crate::platform::capture_source()?,
                None,
                region,
                client_max,
                out,
            )
        }
    }

    /// Like [`start`](Self::start) with an explicit capture source (tests).
    pub fn start_with(
        source: Box<dyn ScreenSource>,
        fallback: Option<SourceFactory>,
        region: Region,
        client_max: (u32, u32),
        out: tokio::sync::mpsc::Sender<VideoChunk>,
    ) -> anyhow::Result<Self> {
        let (commands, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("inkbridge-video".into())
            .spawn(move || {
                if let Err(e) = run(source, fallback, region, client_max, &rx, &out) {
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

    pub fn set_bitrate(&self, kbps: u32) {
        let _ = self.commands.send(VideoCommand::Bitrate(kbps));
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

/// The client's size limit (long side, short side) as (max width, max
/// height) for a picture shaped like `region`.
fn oriented_max(region: Region, client_max: (u32, u32)) -> (u32, u32) {
    let (long, short) = (
        client_max.0.max(client_max.1),
        client_max.0.min(client_max.1),
    );
    if region.width >= region.height {
        (long, short)
    } else {
        (short, long)
    }
}

/// Encoder for the given capture region.
fn encoder_for(region: Region, client_max: (u32, u32)) -> anyhow::Result<OpenH264Encoder> {
    let client_max = oriented_max(region, client_max);
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

/// Picture size and bitrate for a hardware encoder: the tablet's full
/// resolution and a generous bitrate for sharp text (≈10 bits/pixel/s,
/// capped for Wi-Fi).
#[cfg(any(windows, target_os = "macos"))]
fn hardware_config(region: Region, client_max: (u32, u32)) -> EncoderConfig {
    let client_max = oriented_max(region, client_max);
    let (width, height) = fit_within(region.width, region.height, client_max.0, client_max.1);
    let bitrate_kbps =
        ((u64::from(width) * u64::from(height) * 10 / 1000) as u32).min(HARDWARE_MAX_KBPS);
    EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: FPS,
        bitrate_kbps,
    }
}

/// Builds an encoder for `frame`: hardware (Media Foundation, VideoToolbox)
/// for GPU frames, software (openh264) for CPU frames.
fn make_encoder(
    frame: &Frame,
    region: Region,
    client_max: (u32, u32),
) -> anyhow::Result<Box<dyn VideoEncoder>> {
    match &frame.data {
        #[cfg(windows)]
        FrameData::Gpu(gpu) => {
            let config = hardware_config(region, client_max);
            let (width, height, bitrate_kbps) = (config.width, config.height, config.bitrate_kbps);
            let enc = ib_encode::MfEncoder::new(&gpu.device, (frame.width, frame.height), config)?;
            tracing::info!(
                encoder = enc.encoder_name(),
                ?region,
                width,
                height,
                bitrate_kbps,
                "hardware video encoder"
            );
            Ok(Box::new(enc))
        }
        #[cfg(target_os = "macos")]
        FrameData::Gpu(_) => {
            let config = hardware_config(region, client_max);
            let enc = ib_encode::VtEncoder::new(config)?;
            tracing::info!(
                ?region,
                width = config.width,
                height = config.height,
                bitrate_kbps = config.bitrate_kbps,
                "hardware video encoder (VideoToolbox)"
            );
            Ok(Box::new(enc))
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        FrameData::Gpu(_) => anyhow::bail!("no hardware encoder on this platform"),
        FrameData::Cpu(_) => {
            #[cfg(all(target_os = "linux", feature = "vaapi"))]
            if let Some(enc) = vaapi_encoder(region, client_max) {
                return Ok(enc);
            }
            Ok(Box::new(encoder_for(region, client_max)?))
        }
    }
}

/// Linux hardware encoder for CPU frames, at the tablet's full resolution.
/// `None` (logged once) when the GPU or driver can't encode H.264.
#[cfg(all(target_os = "linux", feature = "vaapi"))]
static VAAPI_UNAVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(target_os = "linux", feature = "vaapi"))]
fn vaapi_encoder(region: Region, client_max: (u32, u32)) -> Option<Box<dyn VideoEncoder>> {
    if VAAPI_UNAVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let client_max = oriented_max(region, client_max);
    let (width, height) = fit_within(region.width, region.height, client_max.0, client_max.1);
    let bitrate_kbps =
        ((u64::from(width) * u64::from(height) * 10 / 1000) as u32).min(HARDWARE_MAX_KBPS);
    let config = EncoderConfig {
        codec: VideoCodec::H264,
        width,
        height,
        fps: FPS,
        bitrate_kbps,
    };
    match ib_encode::VaapiEncoder::new(config) {
        Ok(enc) => {
            tracing::info!(
                ?region,
                width,
                height,
                bitrate_kbps,
                "hardware video encoder (VA-API)"
            );
            Some(Box::new(enc))
        }
        Err(e) => {
            VAAPI_UNAVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("no VA-API encoder, using software: {e}");
            None
        }
    }
}

/// Host side of adaptive bitrate: the tablet's request, capped by a local
/// safety limit that drops at once when frames must be skipped (the queue to
/// the network is full) and recovers slowly.
#[derive(Debug)]
struct Rate {
    requested: u32,
    local_cap: u32,
    last_change: std::time::Instant,
    applied: u32,
}

impl Rate {
    const MIN_KBPS: u32 = 1_500;
    const REQUEST_FLOOR_DIVISOR: u32 = 3;

    fn new() -> Self {
        Self {
            requested: u32::MAX,
            local_cap: u32::MAX,
            last_change: std::time::Instant::now(),
            applied: u32::MAX,
        }
    }

    fn on_skip(&mut self, current: u32) {
        self.local_cap = (current.min(self.local_cap) * 4 / 5).max(Self::MIN_KBPS);
        self.last_change = std::time::Instant::now();
    }

    /// Lets the local cap recover by 5% every 2 s without skips.
    fn tick(&mut self) {
        if self.local_cap != u32::MAX && self.last_change.elapsed() >= Duration::from_secs(2) {
            self.local_cap = self.local_cap.saturating_add(self.local_cap / 20);
            self.last_change = std::time::Instant::now();
        }
    }

    /// The bitrate to use for an encoder whose own maximum is `encoder_max`.
    ///
    /// The tablet's request comes from delay measurements, which on a
    /// jittery Wi-Fi fall even when the link has room, so it can lower the
    /// bitrate only to a third of the maximum. Below that the picture gets
    /// worse (or frames are skipped) faster than the latency improves. The
    /// local cap reacts to frames that really could not be sent and has no
    /// such floor.
    fn target(&self, encoder_max: u32) -> u32 {
        self.requested
            .max(encoder_max / Self::REQUEST_FLOOR_DIVISOR)
            .min(self.local_cap)
            .min(encoder_max)
            .max(Self::MIN_KBPS.min(encoder_max))
    }
}

/// The bitrate is lowered at most this often while the network is behind,
/// so one stall doesn't take it straight to the minimum.
const BACKOFF_INTERVAL: Duration = Duration::from_millis(250);

/// Consecutive hardware failures before falling back to software for good.
const HARDWARE_RETRIES: u32 = 3;
/// Bitrate ceiling for the hardware path (kbit/s).
#[cfg(any(
    windows,
    target_os = "macos",
    all(target_os = "linux", feature = "vaapi")
))]
const HARDWARE_MAX_KBPS: u32 = 25_000;

fn run(
    source: Box<dyn ScreenSource>,
    fallback: Option<SourceFactory>,
    region: Region,
    client_max: (u32, u32),
    commands: &Receiver<VideoCommand>,
    out: &tokio::sync::mpsc::Sender<VideoChunk>,
) -> anyhow::Result<()> {
    let mut p = Pipeline::new(source, fallback, region, client_max)?;
    loop {
        if p.apply_commands(commands)? == Flow::Stop {
            return Ok(());
        }
        let Some(frame) = p.next_input()? else {
            continue;
        };
        if !p.ensure_encoder(&frame)? {
            continue;
        }
        if p.encode_and_send(frame, out)? == Flow::Stop {
            return Ok(());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// State of one running pipeline (owned by the video thread).
struct Pipeline {
    source: Box<dyn ScreenSource>,
    fallback: Option<SourceFactory>,
    region: Region,
    client_max: (u32, u32),
    encoder: Option<Box<dyn VideoEncoder>>,
    /// The bitrate the current encoder started with (its maximum).
    encoder_max: u32,
    rate: Rate,
    failures: u32,
    force_key: bool,
    frame_no: u64,
    /// Last captured frame, re-encoded when a keyframe is needed on a static screen.
    last: Option<Frame>,
    /// The encoder skipped the last frame: encode it again when the screen
    /// stays still, so the tablet ends up with the current picture.
    resend: bool,
    /// When the bitrate was last lowered because the network was behind.
    last_cut: Option<std::time::Instant>,
}

impl Pipeline {
    fn new(
        mut source: Box<dyn ScreenSource>,
        fallback: Option<SourceFactory>,
        region: Region,
        client_max: (u32, u32),
    ) -> anyhow::Result<Self> {
        start_capture(source.as_mut(), region, client_max)?;
        Ok(Self {
            source,
            fallback,
            region,
            client_max,
            encoder: None,
            encoder_max: 0,
            rate: Rate::new(),
            failures: 0,
            force_key: true,
            frame_no: 0,
            last: None,
            resend: false,
            last_cut: None,
        })
    }

    fn apply_commands(&mut self, commands: &Receiver<VideoCommand>) -> anyhow::Result<Flow> {
        loop {
            match commands.try_recv() {
                Ok(VideoCommand::SetRegion(r)) if r != self.region => {
                    self.region = r;
                    start_capture(self.source.as_mut(), r, self.client_max)?;
                    self.encoder = None; // rebuilt for the new size on the next frame
                    self.force_key = true;
                    self.last = None;
                }
                Ok(VideoCommand::SetRegion(_)) => {}
                Ok(VideoCommand::Keyframe) => self.force_key = true,
                Ok(VideoCommand::Bitrate(kbps)) => self.rate.requested = kbps,
                Ok(VideoCommand::Stop) | Err(TryRecvError::Disconnected) => return Ok(Flow::Stop),
                Err(TryRecvError::Empty) => return Ok(Flow::Continue),
            }
        }
    }

    /// The next frame to encode: a fresh capture, or the last one again when
    /// a keyframe is due on a static screen.
    fn next_input(&mut self) -> anyhow::Result<Option<Frame>> {
        Ok(match self.source.next_frame(Duration::from_millis(50))? {
            Some(f) => Some(f),
            None if self.force_key || self.resend => self.last.clone(),
            None => None,
        })
    }

    /// Makes sure an encoder exists for `frame`; `false` means "skip this
    /// frame" (e.g. after switching to the software path).
    fn ensure_encoder(&mut self, frame: &Frame) -> anyhow::Result<bool> {
        if self.encoder.is_some() {
            return Ok(true);
        }
        match make_encoder(frame, self.region, self.client_max) {
            Ok(e) => {
                self.encoder_max = e.config().bitrate_kbps;
                self.rate.applied = self.encoder_max;
                self.encoder = Some(e);
                self.force_key = true;
                Ok(true)
            }
            Err(e) => {
                self.fall_back(&e)?;
                Ok(false)
            }
        }
    }

    /// Switches to CPU capture + software encoding (no usable hardware path).
    fn fall_back(&mut self, why: &anyhow::Error) -> anyhow::Result<()> {
        let make = self
            .fallback
            .take()
            .ok_or_else(|| anyhow::anyhow!("video encoding failed: {why:#}"))?;
        tracing::warn!("hardware video path unavailable ({why:#}); using the software encoder");
        self.source = make();
        start_capture(self.source.as_mut(), self.region, self.client_max)?;
        self.encoder = None;
        self.last = None;
        Ok(())
    }

    fn encode_and_send(
        &mut self,
        frame: Frame,
        out: &tokio::sync::mpsc::Sender<VideoChunk>,
    ) -> anyhow::Result<Flow> {
        // Network behind: don't encode a frame that can't be sent. The
        // decoder stays in sync, so the next frame can be a normal one.
        if out.capacity() == 0 {
            if out.is_closed() {
                return Ok(Flow::Stop);
            }
            if self
                .last_cut
                .is_none_or(|t| t.elapsed() >= BACKOFF_INTERVAL)
            {
                self.rate.on_skip(self.rate.applied);
                self.last_cut = Some(std::time::Instant::now());
            }
            self.last = Some(frame);
            self.resend = true;
            return Ok(Flow::Continue);
        }
        let enc = self.encoder.as_mut().expect("ensure_encoder ran");
        #[cfg(all(target_os = "linux", feature = "vaapi"))]
        let was_vaapi = enc.name() == "vaapi";
        self.rate.tick();
        let target = self.rate.target(self.encoder_max);
        if target != self.rate.applied {
            enc.set_bitrate(target);
            self.rate.applied = target;
            tracing::debug!(kbps = target, "video bitrate");
        }
        let cfg = enc.config();
        let input = scale_to(&frame, cfg.width, cfg.height);
        let encoded = match enc.encode(&input, self.force_key) {
            Ok(e) => {
                self.failures = 0;
                self.resend = false;
                e
            }
            Err(ib_encode::EncodeError::Dropped) => {
                tracing::trace!("the encoder skipped a frame to hold the bitrate");
                self.last = Some(frame);
                self.resend = true;
                return Ok(Flow::Continue);
            }
            Err(e) => {
                // Rebuild the encoder; after repeated failures, use software.
                self.failures += 1;
                tracing::warn!(failures = self.failures, "video encode failed: {e}");
                self.encoder = None;
                self.force_key = true;
                if self.failures >= HARDWARE_RETRIES {
                    #[cfg(all(target_os = "linux", feature = "vaapi"))]
                    if was_vaapi {
                        // Same CPU frames, software encoder from now on.
                        VAAPI_UNAVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!("VA-API encoding keeps failing; using the software encoder");
                        self.failures = 0;
                        return Ok(Flow::Continue);
                    }
                    self.fall_back(&anyhow::Error::new(e))?;
                }
                return Ok(Flow::Continue);
            }
        };
        self.frame_no += 1;
        let chunk = VideoChunk {
            frame_no: self.frame_no,
            kind: encoded.kind,
            width: cfg.width,
            height: cfg.height,
            capture_t_us: encoded.capture_t_us,
            encode_done_t_us: ib_clock::now_us(),
            payload: encoded.data,
        };
        tracing::trace!(
            frame_no = chunk.frame_no,
            kind = ?chunk.kind,
            bytes = chunk.payload.len(),
            "video chunk"
        );
        match out.try_send(chunk) {
            Ok(()) => self.force_key = false,
            // Can't happen after the check above (this thread is the only
            // sender); if it does, the lost frame needs a keyframe after it.
            Err(TrySendError::Full(_)) => {
                self.force_key = true;
                self.rate.on_skip(self.rate.applied);
            }
            Err(TrySendError::Closed(_)) => return Ok(Flow::Stop),
        }
        self.last = Some(frame);
        Ok(Flow::Continue)
    }
}

/// Starts capturing `region`. On macOS the system scales the frames to the
/// hardware encoder's size, which is faster than scaling them afterwards.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn start_capture(
    source: &mut dyn ScreenSource,
    region: Region,
    client_max: (u32, u32),
) -> Result<(), ib_capture::CaptureError> {
    #[cfg(target_os = "macos")]
    {
        let config = hardware_config(region, client_max);
        source.set_output_size(Some((config.width, config.height)));
    }
    source.start(region)
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
            None,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_network_skips_frames_before_encoding() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let region = Region {
            x: 0,
            y: 0,
            width: 128,
            height: 80,
        };
        let p = VideoPipeline::start_with(
            Box::new(FakeScreenSource::default()),
            None,
            region,
            (1920, 1080),
            tx,
        )
        .unwrap();
        // Read slowly: the queue is full most of the time.
        let mut chunks = Vec::new();
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            chunks.push(rx.recv().await.unwrap());
        }
        drop(p);
        assert_eq!(chunks[0].kind, FrameKind::Key);
        // Every encoded frame reaches the tablet: frames are skipped before
        // encoding, so the stream has no gap that would need a keyframe.
        // (The software encoder may still choose a keyframe by itself when
        // the fake screen changes completely.)
        for (i, c) in chunks.iter().enumerate().skip(1) {
            assert_eq!(c.frame_no, chunks[i - 1].frame_no + 1, "chunk {i}");
        }
    }

    #[test]
    fn decoder_limit_applies_to_the_long_and_short_side() {
        let limit = (1920, 1088);
        let region = |width, height| Region {
            x: 0,
            y: 0,
            width,
            height,
        };
        // A portrait strip and the full screen of a 3420x2214 display.
        let portrait = oriented_max(region(1384, 2214), limit);
        assert_eq!(portrait, (1088, 1920));
        assert_eq!(fit_within(1384, 2214, portrait.0, portrait.1), (1088, 1740));
        let landscape = oriented_max(region(3420, 2214), limit);
        assert_eq!(landscape, (1920, 1088));
        assert_eq!(
            fit_within(3420, 2214, landscape.0, landscape.1),
            (1680, 1088)
        );
        // Clients that send one number for both sides are unaffected.
        assert_eq!(oriented_max(region(1384, 2214), (1920, 1920)), (1920, 1920));
    }

    #[test]
    fn rate_follows_tablet_and_backs_off_on_skips() {
        let mut r = Rate::new();
        assert_eq!(r.target(20_000), 20_000); // no request yet: encoder maximum
        r.requested = 8_000;
        assert_eq!(r.target(20_000), 8_000);
        r.requested = 2_000;
        assert_eq!(r.target(20_000), 6_666, "requests stop at a third");
        r.requested = 8_000;
        r.on_skip(8_000);
        assert_eq!(r.target(20_000), 6_400); // immediate local cut
        r.requested = 30_000;
        assert_eq!(r.target(20_000), 6_400, "local cap still applies");
        for _ in 0..10 {
            r.on_skip(Rate::MIN_KBPS);
        }
        assert_eq!(r.target(20_000), Rate::MIN_KBPS);
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
            None,
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
