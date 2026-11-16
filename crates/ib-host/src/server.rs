//! Accept loop and per-tablet session.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use ib_protocol::{Capabilities, ControlMessage, PenFeatures, Region, TransportKind};
use ib_session::{HostSettings, PairingStore, host_handshake};
use ib_transport::{Connection, HostEndpoint, Identity, PairingOffer, PairingSecret};
use tokio::sync::Mutex;

use crate::clock::now_us;
use crate::pen_thread::PenInput;

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub host_name: String,
    pub bind: SocketAddr,
    pub region: Region,
    pub pen_features: PenFeatures,
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
                video_codecs: vec![], // M1: pen-only sessions
                max_width: self.config.region.width,
                max_height: self.config.region.height,
                max_fps: 0,
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

        let result = self.control_loop(&mut ctl).await;
        datagrams.abort();
        result?;
        Ok(newly_paired.then_some(hello.device_name))
    }

    async fn control_loop(&self, ctl: &mut ib_transport::Control) -> anyhow::Result<()> {
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
