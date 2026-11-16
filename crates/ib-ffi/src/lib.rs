//! Inkbridge client core for mobile apps (Kotlin now, Swift later).
//!
//! The FFI surface is deliberately small and coarse (ADR-0001):
//! - free functions for identities and pairing codes;
//! - one [`Session`] object: `connect` (blocking; call it off the UI thread),
//!   then non-blocking `send_pen` / `send_transition` from the input thread.
//!
//! The session owns a small Tokio runtime that drives QUIC in the background,
//! so the app never deals with async Rust.

// FFI boundary: UniFFI requires exported functions to take owned values, and
// flat boolean records map most directly onto Kotlin/Swift data classes.
#![allow(clippy::needless_pass_by_value, clippy::struct_excessive_bools)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ib_protocol::{
    Buttons, Capabilities, ClientHello, ControlMessage, PROTOCOL_VERSION, PenBatch, PenFeatures,
    PenSample, PenState, Tool, TouchContact, TouchPhase, VideoChunk, VideoCodec,
};
use ib_session::{SessionError, client_handshake};
use ib_transport::{ClientEndpoint, Connection, Identity, PairingOffer, PenChannel, UsbListener};
use tokio::sync::mpsc;

uniffi::setup_scaffolding!();

// ----- Errors ---------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum InkbridgeError {
    #[error("not a valid Inkbridge pairing code: {0}")]
    InvalidCode(String),
    #[error("could not reach the computer: {0}")]
    Unreachable(String),
    #[error("the computer refused the connection: {0}")]
    Rejected(String),
    #[error("connection problem: {0}")]
    Transport(String),
}

impl From<SessionError> for InkbridgeError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Rejected(r) => Self::Rejected(format!("{r:?}")),
            other => Self::Transport(other.to_string()),
        }
    }
}

// ----- Records --------------------------------------------------------------

/// The tablet's long-lived identity; the app stores both blobs privately.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PairingInfo {
    pub addresses: Vec<String>,
    pub host_fingerprint: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HostInfo {
    pub name: String,
    pub address: String,
    /// Connected over the USB cable (otherwise Wi-Fi/LAN).
    pub usb: bool,
    pub region_width: u32,
    pub region_height: u32,
    /// Current region's position on the desktop (for the zoomable view).
    pub region_x: i32,
    pub region_y: i32,
    /// The whole display (the zoomable view's bounds).
    pub display_x: i32,
    pub display_y: i32,
    pub display_width: u32,
    pub display_height: u32,
    pub pressure: bool,
    pub tilt: bool,
    pub hover: bool,
    pub eraser: bool,
}

/// Live latency figures for the on-screen overlay (milliseconds, medians
/// over the last few seconds unless noted).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct LatencyReport {
    /// Capture → encoded, on the computer.
    pub encode_ms: f32,
    /// Encoded → received by the tablet (network, one way).
    pub network_ms: f32,
    /// Capture on the computer → shown on the tablet (end to end).
    pub total_ms: f32,
    pub total_p95_ms: f32,
    pub fps: f32,
    /// Clock-sync round trip (lowest recent).
    pub rtt_ms: f32,
    /// Bitrate the tablet currently asks for (adaptive), Mbit/s.
    pub bitrate_mbps: f32,
}

/// Clock sync and per-stage latency statistics for one session.
#[derive(Debug)]
struct Telemetry {
    rate: ib_clock::BitrateController,
    sync: ib_clock::ClockSync,
    encode: ib_clock::LatencyStats,
    network: ib_clock::LatencyStats,
    total: ib_clock::LatencyStats,
    presented: std::collections::VecDeque<u64>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            rate: ib_clock::BitrateController::new(ib_clock::RateConfig::default()),
            sync: ib_clock::ClockSync::default(),
            encode: ib_clock::LatencyStats::default(),
            network: ib_clock::LatencyStats::default(),
            total: ib_clock::LatencyStats::default(),
            presented: std::collections::VecDeque::new(),
        }
    }
}

impl Telemetry {
    /// Records a received frame; returns a new bitrate target if the
    /// adaptive controller changed it.
    fn on_video(&mut self, chunk: &VideoChunk, received_local_us: u64) -> Option<u32> {
        self.encode
            .record_us(chunk.encode_done_t_us.saturating_sub(chunk.capture_t_us));
        let sent = self.sync.remote_to_local(chunk.encode_done_t_us)?;
        let delay = received_local_us.saturating_sub(sent);
        self.network.record_us(delay);
        self.rate.on_delay(received_local_us, delay)
    }

    fn on_presented(&mut self, capture_host_us: u64, now_local_us: u64) {
        if let Some(captured) = self.sync.remote_to_local(capture_host_us) {
            self.total.record_us(now_local_us.saturating_sub(captured));
        }
        self.presented.push_back(now_local_us);
        while self
            .presented
            .front()
            .is_some_and(|t| now_local_us.saturating_sub(*t) > 1_000_000)
        {
            self.presented.pop_front();
        }
    }

    fn report(&self) -> LatencyReport {
        let ms = |s: &ib_clock::LatencyStats, p: f64| {
            s.percentile_us(p).map_or(0.0, |us| us as f32 / 1000.0)
        };
        LatencyReport {
            encode_ms: ms(&self.encode, 50.0),
            network_ms: ms(&self.network, 50.0),
            total_ms: ms(&self.total, 50.0),
            total_p95_ms: ms(&self.total, 95.0),
            fps: self.presented.len() as f32,
            rtt_ms: self.sync.min_rtt_us().map_or(0.0, |us| us as f32 / 1000.0),
            bitrate_mbps: self.rate.target_kbps() as f32 / 1000.0,
        }
    }
}

/// One encoded video frame (Annex-B H.264 access unit).
#[derive(Debug, Clone, uniffi::Record)]
pub struct EncodedVideoFrame {
    pub frame_no: u64,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub capture_t_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FingerPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One finger of a gesture; `x`/`y` normalised to the drawing area.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct Finger {
    pub id: u8,
    pub x: f32,
    pub y: f32,
    pub phase: FingerPhase,
}

impl From<Finger> for TouchContact {
    fn from(f: Finger) -> Self {
        Self {
            id: f.id,
            x: f.x,
            y: f.y,
            phase: match f.phase {
                FingerPhase::Down => TouchPhase::Down,
                FingerPhase::Move => TouchPhase::Move,
                FingerPhase::Up => TouchPhase::Up,
                FingerPhase::Cancel => TouchPhase::Cancel,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PenContact {
    Hover,
    Contact,
    OutOfRange,
}

/// One stylus sample. `x`/`y` are normalised to the drawing area (0..1).
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct StylusSample {
    pub t_us: u64,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub rotation: f32,
    pub contact: PenContact,
    pub eraser: bool,
    pub barrel_button: bool,
}

impl From<StylusSample> for PenSample {
    fn from(s: StylusSample) -> Self {
        let mut buttons = Buttons::default();
        if s.barrel_button {
            buttons = Buttons::BARREL;
        }
        Self {
            t_us: s.t_us,
            x: s.x,
            y: s.y,
            pressure: s.pressure,
            tilt_x: s.tilt_x,
            tilt_y: s.tilt_y,
            rotation: s.rotation,
            state: match s.contact {
                PenContact::Hover => PenState::Hover,
                PenContact::Contact => PenState::Contact,
                PenContact::OutOfRange => PenState::OutOfRange,
            },
            tool: if s.eraser { Tool::Eraser } else { Tool::Pen },
            buttons,
        }
    }
}

// ----- Free functions ---------------------------------------------------------

#[uniffi::export]
pub fn generate_identity() -> Result<DeviceIdentity, InkbridgeError> {
    let id = Identity::generate().map_err(|e| InkbridgeError::Transport(e.to_string()))?;
    Ok(DeviceIdentity {
        cert_der: id.cert_der().to_vec(),
        key_der: id.to_pkcs8_der().to_vec(),
    })
}

#[uniffi::export]
pub fn parse_pairing_code(code: String) -> Result<PairingInfo, InkbridgeError> {
    let offer =
        PairingOffer::parse(&code).map_err(|e| InkbridgeError::InvalidCode(e.to_string()))?;
    Ok(PairingInfo {
        addresses: offer.addrs.iter().map(ToString::to_string).collect(),
        host_fingerprint: offer.fingerprint.to_hex(),
    })
}

#[uniffi::export]
pub fn core_version() -> String {
    format!(
        "{} (protocol {PROTOCOL_VERSION})",
        env!("CARGO_PKG_VERSION")
    )
}

// ----- Session -----------------------------------------------------------------

/// A live connection to one computer.
#[derive(uniffi::Object)]
pub struct Session {
    runtime: tokio::runtime::Runtime,
    /// Kept alive for as long as the session; dropping it would stop QUIC.
    _keep: KeepAlive,
    connection: Connection,
    pen: PenChannel,
    control_tx: mpsc::UnboundedSender<ControlMessage>,
    seq: AtomicU32,
    closed: Arc<AtomicBool>,
    host: HostInfo,
    /// Current desktop region size, updated by the computer.
    region: Arc<Mutex<ib_protocol::Region>>,
    /// Incoming encoded video, if the session streams the screen.
    video: Option<Mutex<mpsc::Receiver<VideoChunk>>>,
    telemetry: Arc<Mutex<Telemetry>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("host", &self.host.name)
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl Session {
    /// Connects (and pairs, if `pair` is true) using a pairing code. Blocks
    /// for up to a few seconds per address; call from a background thread.
    #[uniffi::constructor]
    pub fn connect(
        code: String,
        identity: DeviceIdentity,
        device_name: String,
        pair: bool,
        screen_width: u32,
        screen_height: u32,
    ) -> Result<Arc<Self>, InkbridgeError> {
        let offer =
            PairingOffer::parse(&code).map_err(|e| InkbridgeError::InvalidCode(e.to_string()))?;
        let runtime = new_runtime()?;
        let identity = Identity::from_der(identity.cert_der, identity.key_der);
        let (keep, connection) = runtime.block_on(async {
            let endpoint = ClientEndpoint::new(identity)
                .map_err(|e| InkbridgeError::Transport(e.to_string()))?;
            let mut last_error = String::from("no addresses in pairing code");
            for addr in &offer.addrs {
                match tokio::time::timeout(
                    Duration::from_secs(3),
                    endpoint.connect(*addr, offer.fingerprint),
                )
                .await
                {
                    Ok(Ok(c)) => return Ok((KeepAlive::Client(endpoint), c)),
                    Ok(Err(e)) => last_error = format!("{addr}: {e}"),
                    Err(_) => last_error = format!("{addr}: timed out"),
                }
            }
            Err(InkbridgeError::Unreachable(last_error))
        })?;
        let pairing = pair.then(|| (offer.secret.0, device_name.clone()));
        Self::establish(
            runtime,
            keep,
            connection,
            device_name,
            pairing,
            screen_width,
            screen_height,
        )
    }

    /// USB: waits up to `timeout_ms` for the paired computer (from `code`)
    /// to dial in over the tethering link. Listens on `bind_ip` (the tablet's
    /// address on the USB interface) only. Blocks; call from a background
    /// thread. Returns `Unreachable` on timeout.
    #[uniffi::constructor]
    pub fn accept_usb(
        code: String,
        identity: DeviceIdentity,
        device_name: String,
        bind_ip: String,
        timeout_ms: u32,
        screen_width: u32,
        screen_height: u32,
    ) -> Result<Arc<Self>, InkbridgeError> {
        let offer =
            PairingOffer::parse(&code).map_err(|e| InkbridgeError::InvalidCode(e.to_string()))?;
        let ip: std::net::IpAddr = bind_ip
            .parse()
            .map_err(|_| InkbridgeError::Transport(format!("not an IP address: {bind_ip}")))?;
        let runtime = new_runtime()?;
        let identity = Identity::from_der(identity.cert_der, identity.key_der);
        let (keep, connection) = runtime.block_on(async {
            let addr = std::net::SocketAddr::new(ip, ib_protocol::DEFAULT_PORT);
            let listener = UsbListener::bind(&identity, addr, offer.fingerprint)
                .map_err(|e| InkbridgeError::Transport(e.to_string()))?;
            let wait = Duration::from_millis(u64::from(timeout_ms));
            match tokio::time::timeout(wait, listener.accept()).await {
                Ok(Some(Ok(c))) => Ok((KeepAlive::Usb(listener), c)),
                Ok(Some(Err(e))) => Err(InkbridgeError::Transport(e.to_string())),
                Ok(None) | Err(_) => {
                    Err(InkbridgeError::Unreachable("no computer on USB yet".into()))
                }
            }
        })?;
        Self::establish(
            runtime,
            keep,
            connection,
            device_name,
            None,
            screen_width,
            screen_height,
        )
    }

    /// Host details; `region_*` reflect the current mapped desktop area.
    pub fn host(&self) -> HostInfo {
        let mut h = self.host.clone();
        if let Ok(g) = self.region.lock() {
            h.region_x = g.x;
            h.region_y = g.y;
            h.region_width = g.width;
            h.region_height = g.height;
        }
        h
    }

    /// Asks the computer to show this part of its display (zoomable view),
    /// in desktop pixels. It adjusts the request (aspect, bounds, minimum
    /// size) and the new region arrives via `host()`. Never blocks.
    pub fn request_view(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<(), InkbridgeError> {
        self.control_tx
            .send(ControlMessage::RequestView(ib_protocol::Region {
                x,
                y,
                width,
                height,
            }))
            .map_err(|_| InkbridgeError::Transport("session closed".into()))
    }

    /// Reports the drawing area size (after rotation or resizing). The
    /// computer answers with the desktop region it maps there. Never blocks.
    pub fn send_viewport(&self, width: u32, height: u32) -> Result<(), InkbridgeError> {
        self.control_tx
            .send(ControlMessage::Viewport { width, height })
            .map_err(|_| InkbridgeError::Transport("session closed".into()))
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    /// Sends samples as unreliable datagrams (split into protocol-sized
    /// batches). Never blocks.
    pub fn send_pen(&self, samples: Vec<StylusSample>) -> Result<(), InkbridgeError> {
        let _guard = self.runtime.enter();
        for chunk in samples.chunks(PenBatch::MAX_SAMPLES) {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let batch = PenBatch {
                seq,
                samples: chunk.iter().map(|s| PenSample::from(*s)).collect(),
            };
            self.pen
                .send(&batch)
                .map_err(|e| InkbridgeError::Transport(e.to_string()))?;
        }
        Ok(())
    }

    /// Sends a contact/tool/button change reliably (in addition to the
    /// datagram), so the computer can never miss a pen-up. Never blocks.
    pub fn send_transition(&self, sample: StylusSample) -> Result<(), InkbridgeError> {
        self.control_tx
            .send(ControlMessage::PenTransition(sample.into()))
            .map_err(|_| InkbridgeError::Transport("session closed".into()))
    }

    /// Two-finger scroll, touchpad style: centroid (`x`, `y`) and movement
    /// since the last call (`dx`, `dy`), normalised to the drawing area;
    /// `t_us` is the event time; `end` when the fingers lift. The computer
    /// applies acceleration and inertia. Never blocks.
    pub fn send_scroll(
        &self,
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        t_us: u64,
        end: bool,
    ) -> Result<(), InkbridgeError> {
        self.control_tx
            .send(ControlMessage::Scroll {
                x,
                y,
                dx,
                dy,
                t_us,
                end,
            })
            .map_err(|_| InkbridgeError::Transport("session closed".into()))
    }

    /// Sends one multi-finger gesture frame (all fingers of the gesture).
    /// The computer injects it as native touch (used for pinch-zoom).
    /// Never blocks.
    pub fn send_touch(&self, fingers: Vec<Finger>) -> Result<(), InkbridgeError> {
        let contacts = fingers
            .into_iter()
            .take(TouchContact::MAX)
            .map(Into::into)
            .collect();
        self.control_tx
            .send(ControlMessage::Touch(contacts))
            .map_err(|_| InkbridgeError::Transport("session closed".into()))
    }

    /// Whether the computer streams its screen in this session.
    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }

    /// Next encoded video frame, waiting up to `timeout_ms`. Call from the
    /// decoder thread (blocks); `None` on timeout or end of stream.
    pub fn next_video_frame(&self, timeout_ms: u32) -> Option<EncodedVideoFrame> {
        let video = self.video.as_ref()?;
        let mut rx = video.lock().ok()?;
        let chunk = self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), rx.recv())
                .await
                .ok()
                .flatten()
        })?;
        Some(EncodedVideoFrame {
            frame_no: chunk.frame_no,
            keyframe: chunk.kind == ib_protocol::FrameKind::Key,
            width: chunk.width,
            height: chunk.height,
            capture_t_us: chunk.capture_t_us,
            data: chunk.payload,
        })
    }

    /// Tells the core a frame captured at `capture_t_us` (computer clock, as
    /// given in the frame) reached the screen, for end-to-end latency.
    pub fn frame_presented(&self, capture_t_us: u64) {
        if let Ok(mut t) = self.telemetry.lock() {
            t.on_presented(capture_t_us, ib_clock::now_us());
        }
    }

    /// Current latency figures for the overlay.
    pub fn latency(&self) -> LatencyReport {
        self.telemetry
            .lock()
            .map_or_else(|_| LatencyReport::default(), |t| t.report())
    }

    /// Asks the computer for a keyframe (decoder start or error). Never blocks.
    pub fn request_keyframe(&self) {
        let _ = self.control_tx.send(ControlMessage::RequestKeyframe);
    }

    /// Smoothed network round-trip time in microseconds.
    pub fn rtt_us(&self) -> u64 {
        u64::try_from(self.connection.rtt().as_micros()).unwrap_or(u64::MAX)
    }

    /// Ends the session politely. (Named `disconnect` because UniFFI
    /// objects already have `close()`, which frees the native object.)
    ///
    /// Blocks for up to ~0.3 s while the goodbye is delivered, so the
    /// computer releases the pen at once instead of after its idle timeout.
    /// Call off the UI thread.
    pub fn disconnect(&self) {
        let _ = self.control_tx.send(ControlMessage::Goodbye);
        let closed = self.closed.clone();
        let conn = self.connection.clone();
        self.runtime.block_on(async move {
            // The control task sets `closed` once the goodbye is flushed.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
            while !closed.load(Ordering::Acquire) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            conn.close("tablet closed the session");
        });
    }
}

/// Whatever must stay alive with a session for QUIC to keep running.
#[expect(
    dead_code,
    reason = "held, never read: dropping the endpoint would stop QUIC"
)]
enum KeepAlive {
    Client(ClientEndpoint),
    Usb(UsbListener),
}

impl std::fmt::Debug for KeepAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Client(_) => "KeepAlive::Client",
            Self::Usb(_) => "KeepAlive::Usb",
        })
    }
}

fn new_runtime() -> Result<tokio::runtime::Runtime, InkbridgeError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("inkbridge-net")
        .enable_all()
        .build()
        .map_err(|e| InkbridgeError::Transport(e.to_string()))
}

impl Session {
    /// Hello/pairing exchange on a fresh connection (either direction), then
    /// starts the session's background tasks.
    fn establish(
        runtime: tokio::runtime::Runtime,
        keep: KeepAlive,
        connection: Connection,
        device_name: String,
        pairing: Option<([u8; ib_protocol::PAIRING_SECRET_LEN], String)>,
        screen_width: u32,
        screen_height: u32,
    ) -> Result<Arc<Self>, InkbridgeError> {
        let (control, hello) = runtime.block_on(async {
            let mut control = connection
                .open_control()
                .await
                .map_err(|e| InkbridgeError::Transport(e.to_string()))?;
            let hello = ClientHello {
                version: PROTOCOL_VERSION,
                device_name,
                caps: Capabilities {
                    // Every Android device decodes H.264 in hardware.
                    video_codecs: vec![VideoCodec::H264],
                    max_width: screen_width.max(screen_height),
                    max_height: screen_width.max(screen_height),
                    max_fps: 60,
                    pen: PenFeatures {
                        pressure: true,
                        tilt: true,
                        rotation: false,
                        hover: true,
                        eraser: true,
                        barrel_button: true,
                    },
                },
                screen_width,
                screen_height,
            };
            let host = client_handshake(&mut control, hello, pairing).await?;
            Ok::<_, InkbridgeError>((control, host))
        })?;

        let tasks = spawn_session_tasks(&runtime, control, &connection, hello.region);
        let video = hello.codec.map(|_| {
            spawn_video_receiver(
                &runtime,
                &connection,
                tasks.telemetry.clone(),
                tasks.control_tx.clone(),
            )
        });
        let host = HostInfo {
            name: hello.host_name,
            address: connection.remote_addr().to_string(),
            usb: hello.transport == ib_protocol::TransportKind::Usb,
            region_width: hello.region.width,
            region_height: hello.region.height,
            region_x: hello.region.x,
            region_y: hello.region.y,
            display_x: hello.display.x,
            display_y: hello.display.y,
            display_width: hello.display.width,
            display_height: hello.display.height,
            pressure: hello.caps.pen.pressure,
            tilt: hello.caps.pen.tilt,
            hover: hello.caps.pen.hover,
            eraser: hello.caps.pen.eraser,
        };
        Ok(Arc::new(Self {
            pen: connection.pen(),
            runtime,
            _keep: keep,
            connection,
            control_tx: tasks.control_tx,
            seq: AtomicU32::new(0),
            closed: tasks.closed,
            host,
            region: tasks.region,
            video,
            telemetry: tasks.telemetry,
        }))
    }
}

/// Background tasks of a session and the handles the app-facing side keeps.
struct SessionTasks {
    control_tx: mpsc::UnboundedSender<ControlMessage>,
    closed: Arc<AtomicBool>,
    region: Arc<Mutex<ib_protocol::Region>>,
    telemetry: Arc<Mutex<Telemetry>>,
}

/// Starts the control writer (so input handlers never block on the
/// network), the control reader (region updates from the computer) and a
/// connection watcher.
fn spawn_session_tasks(
    runtime: &tokio::runtime::Runtime,
    control: ib_transport::Control,
    connection: &Connection,
    initial_region: ib_protocol::Region,
) -> SessionTasks {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlMessage>();
    let closed = Arc::new(AtomicBool::new(false));
    let (mut sender, mut receiver) = control.split();

    let closed_writer = closed.clone();
    runtime.spawn(async move {
        while let Some(msg) = control_rx.recv().await {
            let goodbye = matches!(msg, ControlMessage::Goodbye);
            if sender.send(&msg).await.is_err() || goodbye {
                break;
            }
        }
        sender.close_send().await;
        closed_writer.store(true, Ordering::Release);
    });

    let region = Arc::new(Mutex::new(initial_region));
    let region_reader = region.clone();
    let telemetry = Arc::new(Mutex::new(Telemetry::default()));
    let telemetry_reader = telemetry.clone();
    runtime.spawn(async move {
        while let Ok(msg) = receiver.recv().await {
            match msg {
                ControlMessage::SetRegion(r) => {
                    if let Ok(mut g) = region_reader.lock() {
                        *g = r;
                    }
                }
                ControlMessage::Pong {
                    t0_us,
                    t1_us,
                    t2_us,
                } => {
                    let t3 = ib_clock::now_us();
                    if let Ok(mut t) = telemetry_reader.lock() {
                        t.sync.add(ib_clock::Exchange {
                            t0: t0_us,
                            t1: t1_us,
                            t2: t2_us,
                            t3,
                        });
                    }
                }
                _ => {}
            }
        }
    });

    // Clock sync: a ping per second (fast at first) keeps the offset fresh.
    let pinger = control_tx.clone();
    runtime.spawn(async move {
        for i in 0u32.. {
            if pinger
                .send(ControlMessage::Ping {
                    t0_us: ib_clock::now_us(),
                })
                .is_err()
            {
                break;
            }
            let pause = if i < 8 { 100 } else { 1000 };
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    });

    let conn = connection.clone();
    let closed_watch = closed.clone();
    runtime.spawn(async move {
        conn.closed().await;
        closed_watch.store(true, Ordering::Release);
    });

    SessionTasks {
        control_tx,
        closed,
        region,
        telemetry,
    }
}

/// Receives the host's video stream into a small queue for the decoder.
/// The queue applies back-pressure to QUIC rather than dropping frames
/// (a dropped delta frame would corrupt the picture).
fn spawn_video_receiver(
    runtime: &tokio::runtime::Runtime,
    connection: &Connection,
    telemetry: Arc<Mutex<Telemetry>>,
    control_tx: mpsc::UnboundedSender<ControlMessage>,
) -> Mutex<mpsc::Receiver<VideoChunk>> {
    let (tx, rx) = mpsc::channel::<VideoChunk>(8);
    let conn = connection.clone();
    runtime.spawn(async move {
        let Ok(mut stream) = conn.accept_video().await else {
            return;
        };
        while let Ok(chunk) = stream.recv().await {
            let hint = telemetry
                .lock()
                .ok()
                .and_then(|mut t| t.on_video(&chunk, ib_clock::now_us()));
            if let Some(kbps) = hint {
                let _ = control_tx.send(ControlMessage::BitrateHint { kbps });
            }
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
    });
    Mutex::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_round_trip_and_errors() {
        assert!(matches!(
            parse_pairing_code("hello".into()),
            Err(InkbridgeError::InvalidCode(_))
        ));
        let id = generate_identity().unwrap();
        assert!(!id.cert_der.is_empty() && !id.key_der.is_empty());
    }

    #[test]
    fn stylus_sample_maps_to_protocol() {
        let s = StylusSample {
            t_us: 5,
            x: 0.25,
            y: 0.75,
            pressure: 0.5,
            tilt_x: 10.0,
            tilt_y: -10.0,
            rotation: 0.0,
            contact: PenContact::Contact,
            eraser: true,
            barrel_button: true,
        };
        let p = PenSample::from(s);
        assert_eq!(p.state, PenState::Contact);
        assert_eq!(p.tool, Tool::Eraser);
        assert!(p.buttons.contains(Buttons::BARREL));
    }
}
