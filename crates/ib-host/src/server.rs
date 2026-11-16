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
#[derive(Debug, Default)]
struct SessionState {
    video: Option<VideoPipeline>,
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
        Ok(Arc::new(Self {
            endpoint,
            identity,
            config,
            store: Mutex::new(store),
            pairing: Mutex::new(None),
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
    pub async fn run(self: Arc<Self>, on_paired: impl Fn(&str) + Send + Sync) {
        while let Some(incoming) = self.endpoint.accept().await {
            match incoming {
                Ok(conn) => {
                    tracing::info!(peer = %conn.remote_addr(), "tablet connected");
                    match self.serve(&conn).await {
                        Ok(Some(name)) => on_paired(&name),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("session ended: {e}"),
                    }
                    let _ = self.pen.send(PenInput::Disconnected);
                    conn.close("session over");
                    tracing::info!("tablet disconnected");
                }
                Err(e) => tracing::warn!("connection attempt failed: {e}"),
            }
        }
    }

    /// One session. Returns the device name if it paired during this session.
    async fn serve(&self, conn: &Connection) -> anyhow::Result<Option<String>> {
        let mut ctl = conn.accept_control().await?;
        let peer = conn.peer_fingerprint();
        let settings = HostSettings {
            host_name: self.config.host_name.clone(),
            caps: Capabilities {
                video_codecs: if self.config.video {
                    vec![VideoCodec::H264]
                } else {
                    vec![]
                },
                max_width: self.config.region.width,
                max_height: self.config.region.height,
                max_fps: 60,
                pen: self.config.pen_features,
            },
            region: self.config.region,
            transport: TransportKind::Wifi,
        };

        let pending = *self.pairing.lock().await;
        let (hello, newly_paired) = {
            let mut store = self.store.lock().await;
            let was_paired = store.is_paired(peer);
            let hello = host_handshake(&mut ctl, peer, &settings, &mut store, pending).await?;
            (hello, !was_paired)
        };
        if newly_paired {
            // One-time secret: never accept it twice.
            *self.pairing.lock().await = None;
        }
        tracing::info!(device = %hello.device_name, "session started");
        let region = self
            .apply_viewport(&mut ctl, hello.screen_width, hello.screen_height)
            .await?;

        // Video: only if both sides support it (codec negotiated).
        let mut session = SessionState::default();
        let mut video_task = None;
        if settings.caps.negotiate_codec(&hello.caps).is_some() {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<VideoChunk>(VIDEO_QUEUE);
            match VideoPipeline::start(region, (hello.caps.max_width, hello.caps.max_height), tx) {
                Ok(p) => {
                    session.video = Some(p);
                    let conn = conn.clone();
                    video_task = Some(tokio::spawn(async move {
                        let mut stream = conn.open_video().await?;
                        while let Some(chunk) = rx.recv().await {
                            stream.send(&chunk).await?;
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

    /// Picks the desktop region for the tablet's current viewport and tells
    /// both the pen thread and the tablet.
    async fn apply_viewport(
        &self,
        ctl: &mut ib_transport::Control,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Region> {
        let region = ib_geometry::region_for_viewport(self.config.region, width, height);
        tracing::info!(width, height, ?region, "tablet viewport");
        let _ = self.pen.send(PenInput::SetRegion(region));
        ctl.send(&ControlMessage::SetRegion(region)).await?;
        Ok(region)
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
                    let region = self.apply_viewport(ctl, width, height).await?;
                    if let Some(v) = &session.video {
                        v.set_region(region);
                    }
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

    #[test]
    fn tether_addresses_sort_first() {
        assert!(is_probably_tether("192.168.42.129".parse().unwrap()));
        assert!(!is_probably_tether("10.200.111.151".parse().unwrap()));
    }
}
