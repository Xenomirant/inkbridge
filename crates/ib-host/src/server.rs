//! Accept loop and per-tablet session.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use ib_protocol::{
    Capabilities, ControlMessage, PenFeatures, Region, TransportKind, VideoChunk, VideoCodec,
};
use ib_session::{HostSettings, PairingStore, host_handshake};
use ib_transport::{Connection, HostEndpoint, Identity, PairingOffer, PairingSecret};
use tokio::sync::Mutex;

use crate::clock::now_us;
use crate::pen_thread::PenInput;
use crate::touch::TouchMapping;
use crate::video::VideoPipeline;

/// Encoded frames waiting for the network. Small on purpose: a deep queue is
/// latency; when it's full the pipeline skips to a keyframe instead.
const VIDEO_QUEUE: usize = 3;

/// Per-session resources, released when the session ends.
#[derive(Debug)]
struct SessionState {
    video: Option<VideoPipeline>,
    /// Where encoded frames go and the tablet's decoder limit, to start
    /// the pipeline again on another display. `None` without video.
    video_out: Option<(tokio::sync::mpsc::Sender<VideoChunk>, (u32, u32))>,
    /// Aspect (width / height) of the tablet's drawing area.
    aspect: f64,
    /// The tablet's drawing area in its pixels.
    viewport: (u32, u32),
    /// Whether the tablet's app understands the display list (protocol 0.2).
    display_choice: bool,
}

/// Logs what the network did to the video every couple of seconds (debug
/// level): how long frames waited to be handed to QUIC, and its loss and
/// window counters. A long wait means the link, not the encoder, stalled.
struct LinkLog {
    since: std::time::Instant,
    last: ib_transport::LinkStats,
    frames: u32,
    slowest: std::time::Duration,
}

impl LinkLog {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(2);

    fn new(conn: &Connection) -> Self {
        Self {
            since: std::time::Instant::now(),
            last: conn.link_stats(),
            frames: 0,
            slowest: std::time::Duration::ZERO,
        }
    }

    fn sent(&mut self, conn: &Connection, wait: std::time::Duration) {
        self.frames += 1;
        self.slowest = self.slowest.max(wait);
        if self.since.elapsed() < Self::EVERY {
            return;
        }
        let now = conn.link_stats();
        tracing::debug!(
            frames = self.frames,
            slowest_send_ms = self.slowest.as_millis(),
            packets = now.sent_packets - self.last.sent_packets,
            lost = now.lost_packets - self.last.lost_packets,
            slowdowns = now.congestion_events - self.last.congestion_events,
            window_kb = now.window_bytes / 1024,
            rtt_ms = now.rtt.as_millis(),
            "video link"
        );
        *self = Self::new(conn);
        self.last = now;
    }
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub host_name: String,
    pub bind: SocketAddr,
    pub region: Region,
    pub pen_features: PenFeatures,
    /// Whether screen streaming is available on this host.
    pub video: bool,
}

/// A running host: endpoint, paired devices and the current pairing secret.
#[derive(Debug)]
pub struct Host {
    endpoint: HostEndpoint,
    identity: Identity,
    config: HostConfig,
    store: Mutex<PairingStore>,
    /// Secret shown in the current QR code; `None` = pairing closed.
    pairing: Mutex<Option<PairingSecret>>,
    pen: Sender<PenInput>,
    /// One tablet at a time (there is one pen): held for a session's life.
    session: Mutex<()>,
    /// Bounds of the display being shown; the tablet can change it.
    display: std::sync::Mutex<Region>,
    usb_tip_shown: std::sync::atomic::AtomicBool,
}

impl Host {
    pub fn new(
        identity: Identity,
        config: HostConfig,
        store: PairingStore,
        pen: Sender<PenInput>,
    ) -> anyhow::Result<Arc<Self>> {
        let endpoint = match HostEndpoint::bind(&identity, config.bind) {
            // The preferred port can be reserved by other software (e.g. WSL's
            // mirrored networking reserves a block of Windows ports). Any port
            // works because the QR code carries the actual one.
            Err(ib_transport::TransportError::Io(e))
                if e.kind() == std::io::ErrorKind::AddrInUse && config.bind.port() != 0 =>
            {
                tracing::warn!(
                    port = config.bind.port(),
                    "port unavailable, using a free one"
                );
                HostEndpoint::bind(&identity, SocketAddr::new(config.bind.ip(), 0))?
            }
            other => other?,
        };
        let display = std::sync::Mutex::new(config.region);
        Ok(Arc::new(Self {
            endpoint,
            identity,
            config,
            store: Mutex::new(store),
            pairing: Mutex::new(None),
            session: Mutex::new(()),
            display,
            usb_tip_shown: std::sync::atomic::AtomicBool::new(false),
            pen,
        }))
    }

    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Opens pairing with a fresh one-time secret and returns the QR offer.
    pub async fn open_pairing(&self) -> anyhow::Result<PairingOffer> {
        let secret = PairingSecret::random();
        *self.pairing.lock().await = Some(secret);
        let port = self.local_addr()?.port();
        Ok(PairingOffer {
            addrs: advertised_addrs(port),
            fingerprint: self.identity.fingerprint(),
            secret,
        })
    }

    pub async fn paired_count(&self) -> usize {
        self.store.lock().await.devices().len()
    }

    /// Serves tablets one at a time (there's one pen) until the endpoint closes.
    /// `on_paired` runs after a successful pairing, e.g. to show a new QR code.
    /// Serves tablets until the endpoint closes: tablets connecting over
    /// Wi-Fi, and tablets on USB tethering, which this host dials.
    pub async fn run(self: Arc<Self>, on_paired: impl Fn(&str) + Send + Sync) {
        tokio::join!(self.accept_loop(&on_paired), self.usb_loop(&on_paired));
    }

    async fn accept_loop(&self, on_paired: &(impl Fn(&str) + Send + Sync)) {
        // One accept is kept across sessions: dropping it part-way would
        // drop the tablet whose handshake it is carrying.
        let mut accept = Box::pin(self.endpoint.accept());
        // A tablet that connected while another session was running.
        let mut waiting: Option<Connection> = None;
        loop {
            let conn = if let Some(conn) = waiting.take() {
                conn
            } else {
                let Some(incoming) = accept.as_mut().await else {
                    return;
                };
                accept.set(self.endpoint.accept());
                match incoming {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::warn!("connection attempt failed: {e}");
                        continue;
                    }
                }
            };
            let _slot = self.session.lock().await;
            let session = self.run_session(&conn, TransportKind::Wifi, on_paired);
            tokio::pin!(session);
            loop {
                let incoming = tokio::select! {
                    () = &mut session => break,
                    incoming = accept.as_mut() => incoming,
                };
                let Some(incoming) = incoming else {
                    session.await;
                    return;
                };
                accept.set(self.endpoint.accept());
                match incoming {
                    Ok(new) => {
                        // The same tablet again: its old session is dead
                        // (app restarted, network changed) even if the idle
                        // timeout hasn't noticed yet. Don't make it wait.
                        if new.peer_fingerprint() == conn.peer_fingerprint() {
                            tracing::info!("the tablet reconnected, ending its previous session");
                            conn.close("replaced by a new connection");
                        }
                        if let Some(old) = waiting.replace(new) {
                            old.close("another tablet is waiting");
                        }
                    }
                    Err(e) => tracing::warn!("connection attempt failed: {e}"),
                }
            }
        }
    }

    /// Dials tablets on USB tethering links (the tablet is our gateway
    /// there; outgoing connections need no firewall rule). Paired tablets
    /// only; the session itself is the same as over Wi-Fi.
    async fn usb_loop(&self, on_paired: &(impl Fn(&str) + Send + Sync)) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Ok(_slot) = self.session.try_lock() else {
                continue;
            }; // a tablet is connected
            let paired: Vec<_> = self
                .store
                .lock()
                .await
                .devices()
                .iter()
                .map(|d| d.fingerprint)
                .collect();
            if paired.is_empty() {
                continue;
            }
            for link in crate::usb::tether_links() {
                if !self
                    .usb_tip_shown
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::info!(
                        tablet = %link.tablet,
                        "USB tethering link found. Tip: run `inkbridge-host --fix-usb-routing` once so this PC's \
                         internet traffic doesn't go through the tablet."
                    );
                }
                let addr = SocketAddr::new(IpAddr::V4(link.tablet), ib_protocol::DEFAULT_PORT);
                let dial = self.endpoint.dial(addr, paired.clone());
                match tokio::time::timeout(std::time::Duration::from_millis(1500), dial).await {
                    Ok(Ok(conn)) => {
                        tracing::info!(%addr, "tablet reached over USB");
                        self.run_session(&conn, TransportKind::Usb, on_paired).await;
                        break;
                    }
                    // The tablet isn't listening (app closed) or refused us.
                    Ok(Err(e)) => tracing::debug!(%addr, "USB dial failed: {e}"),
                    Err(_) => tracing::debug!(%addr, "USB dial timed out"),
                }
            }
        }
    }

    async fn run_session(
        &self,
        conn: &Connection,
        transport: TransportKind,
        on_paired: &(impl Fn(&str) + Send + Sync),
    ) {
        tracing::info!(peer = %conn.remote_addr(), ?transport, "tablet connected");
        match self.serve(conn, transport).await {
            Ok(Some(name)) => on_paired(&name),
            Ok(None) => {}
            Err(e) => tracing::warn!("session ended: {e}"),
        }
        let _ = self.pen.send(PenInput::Disconnected);
        crate::platform::session_ended();
        conn.close("session over");
        tracing::info!("tablet disconnected");
    }

    /// One session. Returns the device name if it paired during this session.
    async fn serve(
        &self,
        conn: &Connection,
        transport: TransportKind,
    ) -> anyhow::Result<Option<String>> {
        let mut ctl = conn.accept_control().await?;
        let peer = conn.peer_fingerprint();
        if let Some(main) = crate::platform::session_started() {
            tracing::info!("the display shown last is gone, showing the main one");
            self.show(&main);
        }
        let settings = HostSettings {
            host_name: self.config.host_name.clone(),
            caps: Capabilities {
                video_codecs: if self.config.video {
                    vec![VideoCodec::H264]
                } else {
                    vec![]
                },
                max_width: self.display().width,
                max_height: self.display().height,
                max_fps: 60,
                pen: self.config.pen_features,
            },
            region: self.display(),
            transport,
        };

        let pending = *self.pairing.lock().await;
        let (hello, newly_paired) = {
            let mut store = self.store.lock().await;
            host_handshake(&mut ctl, peer, &settings, &mut store, pending).await?
        };
        if newly_paired {
            // One-time secret: never accept it twice, not even from a
            // tablet that was paired before.
            *self.pairing.lock().await = None;
        }
        tracing::info!(device = %hello.device_name, "session started");
        self.restore_extra_display(hello.screen_width, hello.screen_height);
        let region = self
            .apply_viewport(&mut ctl, hello.screen_width, hello.screen_height)
            .await?;

        // Video: only if both sides support it (codec negotiated).
        let mut session = SessionState {
            video: None,
            video_out: None,
            aspect: f64::from(hello.screen_width.max(1)) / f64::from(hello.screen_height.max(1)),
            viewport: (hello.screen_width, hello.screen_height),
            display_choice: false,
        };
        session.display_choice = hello.version.minor >= ib_protocol::DISPLAY_CHOICE_MINOR;
        self.send_displays(&mut ctl, &session).await?;
        let mut video_task = None;
        if settings.caps.negotiate_codec(&hello.caps).is_some() {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<VideoChunk>(VIDEO_QUEUE);
            let client_max = (hello.caps.max_width, hello.caps.max_height);
            match VideoPipeline::start(region, client_max, tx.clone()) {
                Ok(p) => {
                    session.video = Some(p);
                    session.video_out = Some((tx, client_max));
                    let conn = conn.clone();
                    video_task = Some(tokio::spawn(async move {
                        let mut stream = conn.open_video().await?;
                        let mut link = LinkLog::new(&conn);
                        while let Some(chunk) = rx.recv().await {
                            let started = std::time::Instant::now();
                            stream.send(&chunk).await?;
                            link.sent(&conn, started.elapsed());
                        }
                        Ok::<_, ib_transport::TransportError>(())
                    }));
                }
                Err(e) => tracing::warn!("screen streaming unavailable: {e:#}"),
            }
        }

        // Fingers act where they touch the picture when the tablet shows it;
        // otherwise (pen-only) gestures start at the cursor.
        let mapping = if session.video.is_some() {
            TouchMapping::Absolute
        } else {
            TouchMapping::AnchoredAtPointer
        };
        let _ = self.pen.send(PenInput::SetTouchMapping(mapping));

        // Datagrams → pen thread, on their own task.
        let pen_rx = conn.pen();
        let pen_tx = self.pen.clone();
        let datagrams = tokio::spawn(async move {
            while let Ok(batch) = pen_rx.recv().await {
                if pen_tx.send(PenInput::Batch(batch)).is_err() {
                    break;
                }
            }
        });

        let result = self.control_loop(&mut ctl, &mut session).await;
        datagrams.abort();
        drop(session); // stops capture and encoding
        if let Some(t) = video_task {
            t.abort();
        }
        result?;
        Ok(newly_paired.then_some(hello.device_name))
    }

    fn display(&self) -> Region {
        *self
            .display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The displays the tablet can choose from. When the computer can make
    /// the tablet an extra screen and hasn't yet, that choice comes last.
    fn display_choices() -> Vec<ib_protocol::DisplayInfo> {
        let mut choices: Vec<ib_protocol::DisplayInfo> = crate::platform::displays()
            .into_iter()
            .map(|d| ib_protocol::DisplayInfo {
                name: d.name,
                bounds: d.bounds,
            })
            .collect();
        if crate::platform::extend_supported() && !crate::platform::extending() {
            // No size: the display doesn't exist until it is chosen.
            choices.push(ib_protocol::DisplayInfo {
                name: "Use this tablet as an extra screen".into(),
                bounds: Region {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            });
        }
        choices
    }

    /// Tells the tablet which displays it can choose from, if there is a
    /// choice and its app understands the message (older ones stop reading
    /// at an unknown message).
    async fn send_displays(
        &self,
        ctl: &mut ib_transport::Control,
        session: &SessionState,
    ) -> anyhow::Result<()> {
        let displays = Self::display_choices();
        if !session.display_choice || displays.len() < 2 {
            return Ok(());
        }
        let current = u8::try_from(crate::platform::current_display()).unwrap_or(0);
        ctl.send(&ControlMessage::Displays { displays, current })
            .await?;
        Ok(())
    }

    /// A session starts while the tablet should be an extra display
    /// (`--extend`, or it was one in its last session): make it one again.
    fn restore_extra_display(&self, screen_width: u32, screen_height: u32) {
        if !crate::platform::extend_wanted() {
            return;
        }
        // One left from the last session keeps its shape: the drawing
        // area (sent next) is smaller than the screen.
        let (width, height) =
            crate::platform::extra_display_size().unwrap_or((screen_width, screen_height));
        match crate::platform::extend(width, height).and_then(crate::platform::switch_display) {
            Ok(extra) => self.show(&extra),
            Err(e) => tracing::warn!("the tablet is not an extra screen: {e:#}"),
        }
    }

    /// Makes `display` the one this host maps the tablet to.
    fn show(&self, display: &ib_capture::Display) {
        *self
            .display
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = display.bounds;
        let _ = self
            .pen
            .send(PenInput::SetDisplay(display.id, display.bounds));
    }

    /// The tablet chose another display, or to be an extra one: move the
    /// pen and the picture there.
    async fn select_display(
        &self,
        ctl: &mut ib_transport::Control,
        session: &mut SessionState,
        index: usize,
    ) -> anyhow::Result<()> {
        let extend = !crate::platform::extending() && index == crate::platform::displays().len();
        let (width, height) = session.viewport;
        let switched = if extend {
            crate::platform::extend(width, height)
        } else {
            Ok(index)
        }
        .and_then(crate::platform::switch_display);
        let chosen = match switched {
            Ok(display) => display,
            Err(e) => {
                tracing::warn!("display not changed: {e:#}");
                return self.send_displays(ctl, session).await;
            }
        };
        tracing::info!(name = %chosen.name, bounds = ?chosen.bounds, "tablet chose a display");
        crate::platform::set_extend_wanted(chosen.name == crate::platform::EXTRA_NAME);
        if !crate::platform::extend_wanted() {
            // Capture stops first: it may be running on the extra display.
            session.video = None;
            crate::platform::stop_extending();
        }
        self.show(&chosen);
        self.remap(ctl, session).await
    }

    /// The shown display changed (another one, or the extra display was
    /// reshaped): tell the tablet and start capture again on it.
    async fn remap(
        &self,
        ctl: &mut ib_transport::Control,
        session: &mut SessionState,
    ) -> anyhow::Result<()> {
        // The tablet learns the new bounds before the region inside them.
        self.send_displays(ctl, session).await?;
        // Capture is bound to a display: stop it before mapping the new
        // region, then start again there.
        session.video = None;
        let (width, height) = session.viewport;
        let region = self.apply_viewport(ctl, width, height).await?;
        if let Some((out, client_max)) = &session.video_out {
            match VideoPipeline::start(region, *client_max, out.clone()) {
                Ok(p) => session.video = Some(p),
                Err(e) => tracing::warn!("screen streaming unavailable: {e:#}"),
            }
        }
        Ok(())
    }

    /// The drawing area of a tablet that is an extra display changed (it
    /// was rotated, or reported its real area after connecting): give the
    /// display that shape. Returns whether it changed.
    fn reshape_extra_display(&self, width: u32, height: u32) -> bool {
        let shaped_for = crate::platform::extra_display_size();
        if !crate::platform::extend_wanted()
            || shaped_for.is_none()
            || shaped_for == Some((width, height))
            || width == 0
            || height == 0
        {
            return false;
        }
        match crate::platform::extend(width, height).and_then(crate::platform::switch_display) {
            Ok(extra) => {
                tracing::info!(bounds = ?extra.bounds, "the extra display follows the tablet's shape");
                self.show(&extra);
                true
            }
            Err(e) => {
                tracing::warn!("the extra display kept its shape: {e:#}");
                false
            }
        }
    }

    /// Picks the desktop region for the tablet's current viewport and tells
    /// both the pen thread and the tablet.
    async fn apply_viewport(
        &self,
        ctl: &mut ib_transport::Control,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Region> {
        let region = ib_geometry::region_for_viewport(self.display(), width, height);
        tracing::info!(width, height, ?region, "tablet viewport");
        self.set_region(ctl, None, region).await?;
        Ok(region)
    }

    /// Maps `region` everywhere at once: pen, video capture, and the tablet.
    async fn set_region(
        &self,
        ctl: &mut ib_transport::Control,
        session: Option<&SessionState>,
        region: Region,
    ) -> anyhow::Result<()> {
        let _ = self.pen.send(PenInput::SetRegion(region));
        if let Some(v) = session.and_then(|s| s.video.as_ref()) {
            v.set_region(region);
        }
        ctl.send(&ControlMessage::SetRegion(region)).await?;
        Ok(())
    }

    async fn control_loop(
        &self,
        ctl: &mut ib_transport::Control,
        session: &mut SessionState,
    ) -> anyhow::Result<()> {
        loop {
            match ctl.recv().await? {
                ControlMessage::PenTransition(s) => {
                    let _ = self.pen.send(PenInput::Transition(s));
                }
                ControlMessage::Ping { t0_us } => {
                    let t1_us = now_us();
                    ctl.send(&ControlMessage::Pong {
                        t0_us,
                        t1_us,
                        t2_us: now_us(),
                    })
                    .await?;
                }
                ControlMessage::Touch(contacts) => {
                    let _ = self.pen.send(PenInput::Touch(contacts));
                }
                ControlMessage::Scroll {
                    x,
                    y,
                    dx,
                    dy,
                    t_us,
                    end,
                } => {
                    let _ = self.pen.send(PenInput::Scroll {
                        x,
                        y,
                        dx,
                        dy,
                        t_us,
                        end,
                    });
                }
                ControlMessage::Viewport { width, height } => {
                    session.aspect = f64::from(width.max(1)) / f64::from(height.max(1));
                    session.viewport = (width, height);
                    if self.reshape_extra_display(width, height) {
                        self.remap(ctl, session).await?;
                    } else {
                        let region = self.apply_viewport(ctl, width, height).await?;
                        if let Some(v) = &session.video {
                            v.set_region(region);
                        }
                    }
                }
                ControlMessage::RequestView(requested) => {
                    let region = ib_geometry::clamp_view(self.display(), requested, session.aspect);
                    tracing::info!(?requested, ?region, "tablet view");
                    self.set_region(ctl, Some(session), region).await?;
                }
                ControlMessage::RequestKeyframe => {
                    if let Some(v) = &session.video {
                        v.request_keyframe();
                    }
                }
                ControlMessage::BitrateHint { kbps } => {
                    if let Some(v) = &session.video {
                        v.set_bitrate(kbps);
                    }
                }
                ControlMessage::SelectDisplay { index } => {
                    self.select_display(ctl, session, usize::from(index))
                        .await?;
                }
                ControlMessage::Goodbye => return Ok(()),
                other => tracing::debug!(?other, "ignored control message"),
            }
        }
    }
}

/// Addresses to put in the QR code: every non-loopback IPv4 address, with
/// likely USB-tethering links first (Android's tether ranges), so a tablet
/// on a cable connects over it.
pub fn advertised_addrs(port: u16) -> Vec<SocketAddr> {
    let mut v4: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_link_local() => Some(ip),
            _ => None,
        })
        .collect();
    v4.sort_by_key(|ip| !is_probably_tether(*ip));
    v4.dedup();
    v4.into_iter()
        .take(4)
        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        .collect()
}

fn is_probably_tether(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    // Classic Android RNDIS tethering subnets.
    o[0] == 192 && o[1] == 168 && (o[2] == 42 || o[2] == 43 || o[2] == 44)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tablet's handshake with the host, as the app does it.
    async fn join(
        conn: &Connection,
        secret: Option<PairingSecret>,
    ) -> Result<ib_protocol::HostHello, ib_session::SessionError> {
        let mut ctl = conn.open_control().await.unwrap();
        let hello = ib_protocol::ClientHello {
            version: ib_protocol::PROTOCOL_VERSION,
            device_name: "Tablet".into(),
            caps: Capabilities {
                video_codecs: vec![],
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                pen: PenFeatures::default(),
            },
            screen_width: 1920,
            screen_height: 1200,
        };
        let r =
            ib_session::client_handshake(&mut ctl, hello, secret.map(|s| (s.0, "Tablet".into())))
                .await;
        // The control stream must outlive the handshake.
        std::mem::forget(ctl);
        r
    }

    /// A host on a loopback port with pairing open.
    async fn host() -> (Identity, SocketAddr, PairingOffer) {
        let identity = Identity::generate().unwrap();
        let config = HostConfig {
            host_name: "Host".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            region: Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            pen_features: PenFeatures::default(),
            video: false,
        };
        let (pen, pen_rx) = std::sync::mpsc::channel();
        std::mem::forget(pen_rx); // nobody reads the pen in these tests
        let host = Host::new(identity.clone(), config, PairingStore::in_memory(), pen).unwrap();
        let addr = host.local_addr().unwrap();
        let offer = host.open_pairing().await.unwrap();
        tokio::spawn(host.run(|_| {}));
        (identity, addr, offer)
    }

    /// An app that was killed leaves its session open on the host until the
    /// idle timeout. The same tablet connecting again must get in at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reconnecting_tablet_replaces_its_dead_session() {
        let (identity, addr, offer) = host().await;
        let tablet = Identity::generate().unwrap();
        let first = ib_transport::ClientEndpoint::new(tablet.clone()).unwrap();
        let old = first.connect(addr, identity.fingerprint()).await.unwrap();
        join(&old, Some(offer.secret)).await.unwrap();

        // The first session stays open; the tablet connects again.
        let second = ib_transport::ClientEndpoint::new(tablet).unwrap();
        let rejoined = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let conn = second.connect(addr, identity.fingerprint()).await.unwrap();
            join(&conn, None).await.map(|_| conn)
        })
        .await
        .expect("the new connection waited for the old session");
        rejoined.unwrap();
        old.closed().await;
    }

    /// Apps from before the display menu stop reading the control stream at
    /// a message they don't know, so they must not get the display list.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_older_app_is_not_sent_the_display_list() {
        let (identity, addr, offer) = host().await;
        let endpoint = ib_transport::ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let conn = endpoint
            .connect(addr, identity.fingerprint())
            .await
            .unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        let hello = ib_protocol::ClientHello {
            version: ib_protocol::ProtocolVersion {
                major: ib_protocol::PROTOCOL_VERSION.major,
                minor: ib_protocol::DISPLAY_CHOICE_MINOR - 1,
            },
            device_name: "Old tablet".into(),
            caps: Capabilities {
                video_codecs: vec![],
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                pen: PenFeatures::default(),
            },
            screen_width: 1920,
            screen_height: 1200,
        };
        ib_session::client_handshake(&mut ctl, hello, Some((offer.secret.0, "Old tablet".into())))
            .await
            .unwrap();
        let quiet = std::time::Duration::from_millis(300);
        while let Ok(msg) = tokio::time::timeout(quiet, ctl.recv()).await {
            assert!(
                !matches!(msg.unwrap(), ControlMessage::Displays { .. }),
                "display list sent to an app that can't read it"
            );
        }
    }

    /// Reads control messages until one matches.
    async fn wait_for<T>(
        ctl: &mut ib_transport::Control,
        mut pick: impl FnMut(ControlMessage) -> Option<T>,
    ) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(found) = pick(ctl.recv().await.unwrap()) {
                    return found;
                }
            }
        })
        .await
        .expect("the host did not answer")
    }

    /// The tablet becomes an extra screen from its display menu: a new
    /// display appears with the tablet's shape, the picture and the pen
    /// move to it, it follows a rotation, and it goes away again when the
    /// tablet chooses a real display.
    ///
    /// Adds a real extra display to this Mac for a few seconds (windows
    /// may shift). Needs the Screen Recording permission. Run with:
    /// `cargo test -p ib-host --lib the_tablet_becomes_an_extra_screen -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "creates a real virtual display; needs the Screen Recording permission"]
    async fn the_tablet_becomes_an_extra_screen() {
        assert!(crate::platform::extend_supported(), "macOS only");
        let real = crate::platform::displays();
        let identity = Identity::generate().unwrap();
        let config = HostConfig {
            host_name: "Host".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            region: real[0].bounds,
            pen_features: PenFeatures::default(),
            video: true,
        };
        let (pen, pen_rx) = std::sync::mpsc::channel();
        let host = Host::new(identity.clone(), config, PairingStore::in_memory(), pen).unwrap();
        let addr = host.local_addr().unwrap();
        let offer = host.open_pairing().await.unwrap();
        tokio::spawn(host.run(|_| {}));

        let endpoint = ib_transport::ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let conn = endpoint
            .connect(addr, identity.fingerprint())
            .await
            .unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        let hello = ib_protocol::ClientHello {
            version: ib_protocol::PROTOCOL_VERSION,
            device_name: "Tablet".into(),
            caps: Capabilities {
                video_codecs: vec![VideoCodec::H264],
                max_width: 1920,
                max_height: 1088,
                max_fps: 60,
                pen: PenFeatures::default(),
            },
            screen_width: 1920,
            screen_height: 1200,
        };
        ib_session::client_handshake(&mut ctl, hello, Some((offer.secret.0, "Tablet".into())))
            .await
            .unwrap();
        let displays = |m| match m {
            ControlMessage::Displays { displays, current } => Some((displays, current)),
            _ => None,
        };
        let (listed, _) = wait_for(&mut ctl, displays).await;
        assert_eq!(listed.len(), real.len() + 1, "the offer comes last");
        let mut video = conn.accept_video().await.unwrap();
        let main = video.recv().await.unwrap();

        // Become an extra screen.
        let offer_index = u8::try_from(real.len()).unwrap();
        ctl.send(&ControlMessage::SelectDisplay { index: offer_index })
            .await
            .unwrap();
        let (listed, current) = wait_for(&mut ctl, displays).await;
        let extra = &listed[usize::from(current)];
        println!("extra screen: {extra:?}");
        assert_eq!(extra.name, crate::platform::EXTRA_NAME);
        assert_eq!((extra.bounds.width, extra.bounds.height), (2560, 1600));
        assert_eq!(
            listed.len(),
            real.len() + 1,
            "no second offer while extending"
        );
        assert!(
            pen_rx.try_iter().any(
                |i| matches!(i, PenInput::SetDisplay(id, _) if !real.iter().any(|d| d.id == id))
            )
        );
        let frame = loop {
            let f = video.recv().await.unwrap();
            // The extra screen has another shape than the main display.
            if (f.width, f.height) != (main.width, main.height) {
                break f;
            }
        };
        println!("landscape picture {}x{}", frame.width, frame.height);
        assert!(frame.width > frame.height);

        // The tablet is rotated: the display takes its new shape.
        ctl.send(&ControlMessage::Viewport {
            width: 1200,
            height: 1920,
        })
        .await
        .unwrap();
        let (listed, current) = wait_for(&mut ctl, displays).await;
        let extra = &listed[usize::from(current)];
        assert_eq!((extra.bounds.width, extra.bounds.height), (1600, 2560));
        let frame = loop {
            let f = video.recv().await.unwrap();
            if f.height > f.width {
                break f;
            }
        };
        println!("portrait picture {}x{}", frame.width, frame.height);

        // Back to a real display: the extra one goes away.
        ctl.send(&ControlMessage::SelectDisplay { index: 0 })
            .await
            .unwrap();
        let (listed, current) = wait_for(&mut ctl, displays).await;
        assert_eq!(current, 0);
        assert_eq!(listed.len(), real.len() + 1);
        assert!(listed.iter().all(|d| d.name != crate::platform::EXTRA_NAME));
        assert_eq!(crate::platform::displays().len(), real.len());
    }

    /// The tablet's display menu, against the real screens: the picture and
    /// the mapped region move to the second display and back.
    ///
    /// Needs two displays and the Screen Recording permission (macOS only
    /// so far). Run with:
    /// `cargo test -p ib-host --lib the_tablet_switches_displays -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs two real displays and the Screen Recording permission"]
    async fn the_tablet_switches_displays() {
        let all = crate::platform::displays();
        assert!(all.len() >= 2, "connect a second display");
        let identity = Identity::generate().unwrap();
        let config = HostConfig {
            host_name: "Host".into(),
            bind: "127.0.0.1:0".parse().unwrap(),
            region: all[0].bounds,
            pen_features: PenFeatures::default(),
            video: true,
        };
        let (pen, pen_rx) = std::sync::mpsc::channel();
        let host = Host::new(identity.clone(), config, PairingStore::in_memory(), pen).unwrap();
        let addr = host.local_addr().unwrap();
        let offer = host.open_pairing().await.unwrap();
        tokio::spawn(host.run(|_| {}));

        let endpoint = ib_transport::ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let conn = endpoint
            .connect(addr, identity.fingerprint())
            .await
            .unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        let hello = ib_protocol::ClientHello {
            version: ib_protocol::PROTOCOL_VERSION,
            device_name: "Tablet".into(),
            caps: Capabilities {
                video_codecs: vec![VideoCodec::H264],
                max_width: 1920,
                max_height: 1088,
                max_fps: 60,
                pen: PenFeatures::default(),
            },
            screen_width: 1920,
            screen_height: 1200,
        };
        ib_session::client_handshake(&mut ctl, hello, Some((offer.secret.0, "Tablet".into())))
            .await
            .unwrap();
        let listed = wait_for(&mut ctl, |m| match m {
            ControlMessage::Displays { displays, current } => Some((displays, current)),
            _ => None,
        })
        .await;
        // On macOS the list ends with the offer to be an extra screen.
        let offers = all.len() + usize::from(crate::platform::extend_supported());
        assert_eq!((listed.0.len(), listed.1), (offers, 0));

        let mut video = conn.accept_video().await.unwrap();
        let first = video.recv().await.unwrap();
        println!("display 1: picture {}x{}", first.width, first.height);

        for (index, display) in [(1u8, &all[1]), (0u8, &all[0])] {
            ctl.send(&ControlMessage::SelectDisplay { index })
                .await
                .unwrap();
            let current = wait_for(&mut ctl, |m| match m {
                ControlMessage::Displays { current, .. } => Some(current),
                _ => None,
            })
            .await;
            assert_eq!(current, index);
            let region = wait_for(&mut ctl, |m| match m {
                ControlMessage::SetRegion(r) => Some(r),
                _ => None,
            })
            .await;
            assert!(
                region.width <= display.bounds.width && region.height <= display.bounds.height,
                "{region:?} is not on {:?}",
                display.bounds
            );
            // The pen follows, and the picture restarts with a keyframe.
            assert!(
                pen_rx
                    .try_iter()
                    .any(|i| matches!(i, PenInput::SetDisplay(id, _) if id == display.id))
            );
            let key = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let chunk = video.recv().await.unwrap();
                    if chunk.kind == ib_protocol::FrameKind::Key {
                        return chunk;
                    }
                }
            })
            .await
            .expect("no picture from the new display");
            println!(
                "{}: region {}x{}, picture {}x{}",
                display.name, region.width, region.height, key.width, key.height
            );
        }
    }

    /// The secret in the QR code works once, whoever uses it: a tablet that
    /// is already paired doesn't leave it open for the next scanner.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_pairing_secret_is_spent_by_a_known_tablet_too() {
        let (identity, addr, offer) = host().await;
        let tablet = Identity::generate().unwrap();
        let endpoint = ib_transport::ClientEndpoint::new(tablet).unwrap();
        for expect_ok in [true, false] {
            let conn = endpoint
                .connect(addr, identity.fingerprint())
                .await
                .unwrap();
            let joined = join(&conn, Some(offer.secret)).await;
            assert_eq!(joined.is_ok(), expect_ok, "{joined:?}");
            conn.close("done");
        }
        // Still paired: it gets in without the secret.
        let conn = endpoint
            .connect(addr, identity.fingerprint())
            .await
            .unwrap();
        join(&conn, None).await.unwrap();
    }

    #[test]
    fn tether_addresses_sort_first() {
        assert!(is_probably_tether("192.168.42.129".parse().unwrap()));
        assert!(!is_probably_tether("10.200.111.151".parse().unwrap()));
    }
}
