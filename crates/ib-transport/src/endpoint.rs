use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ib_protocol::{PenBatch, decode, encode};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::CertificateDer;

use crate::framing::Control;
use crate::stream::{RecvHalf, SendHalf};
use crate::tcp::{TcpAcceptor, TcpSession};
use crate::tls::{AnyClientCert, PinnedServer, provider};
use crate::{Fingerprint, Identity, TransportError};

/// ALPN protocol id; bumped with the protocol's major version.
pub const ALPN: &[u8] = b"inkbridge/0";

/// Binds a UDP socket. IPv6 sockets are made **dual-stack** explicitly:
/// Windows defaults to IPv6-only (unlike Linux), which would make a host
/// bound to `[::]` unreachable for IPv4 tablets and a client unable to reach
/// IPv4 hosts.
fn bind_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// Stream bytes that may be buffered or in flight, unacknowledged: about
/// 40 ms of video at the highest bitrate.
const SEND_WINDOW: u64 = 128 * 1024;

/// QUIC settings tuned for an interactive LAN/USB link.
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // Detect a vanished tablet quickly so the pen gets released promptly.
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(5)).expect("valid timeout"),
    ));
    t.keep_alive_interval(Some(Duration::from_millis(1000)));
    // Don't let stream data pile up unsent when the link stalls: quinn's
    // default takes 10 MB (seconds of video) without the sender noticing.
    // With a small window the video writer blocks instead, and the pipeline
    // skips frames rather than delivering old ones late.
    t.send_window(SEND_WINDOW);
    // Small datagram buffers: stale pen samples are worse than lost ones.
    t.datagram_receive_buffer_size(Some(64 * 1024));
    t.datagram_send_buffer_size(64 * 1024);
    Arc::new(t)
}

fn tls_server_config(
    identity: &Identity,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<rustls::ServerConfig, TransportError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.cert_chain(), identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}

fn server_config(
    identity: &Identity,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> Result<quinn::ServerConfig, TransportError> {
    let tls = tls_server_config(identity, verifier)?;
    let quic = QuicServerConfig::try_from(tls)
        .map_err(|e| TransportError::Tls(rustls::Error::General(e.to_string())))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn tls_client_config(
    identity: &Identity,
    peers: Vec<Fingerprint>,
) -> Result<rustls::ClientConfig, TransportError> {
    let p = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer {
            expected: peers,
            provider: p,
        }))
        .with_client_auth_cert(identity.cert_chain(), identity.private_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    Ok(tls)
}

fn client_config(
    identity: &Identity,
    peers: Vec<Fingerprint>,
) -> Result<quinn::ClientConfig, TransportError> {
    let tls = tls_client_config(identity, peers)?;
    let quic = QuicClientConfig::try_from(tls)
        .map_err(|e| TransportError::Tls(rustls::Error::General(e.to_string())))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

/// Host side: listens for tablets, over QUIC and, on the same port number,
/// over TLS/TCP (the fallback).
#[derive(Debug)]
pub struct HostEndpoint {
    endpoint: quinn::Endpoint,
    tcp: Option<TcpAcceptor>,
    identity: Identity,
}

impl HostEndpoint {
    /// Binds QUIC on `addr`, and TCP on the same port if it is free (the
    /// host works without the fallback otherwise).
    pub fn bind(identity: &Identity, addr: SocketAddr) -> Result<Self, TransportError> {
        let verifier = Arc::new(AnyClientCert {
            provider: provider(),
        });
        let cfg = server_config(identity, verifier.clone())?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(cfg),
            bind_udp(addr)?,
            Arc::new(quinn::TokioRuntime),
        )?;
        let port = endpoint.local_addr()?.port();
        let tcp = match TcpAcceptor::bind(
            SocketAddr::new(addr.ip(), port),
            Arc::new(tls_server_config(identity, verifier)?),
        ) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("TCP fallback unavailable on port {port}: {e}");
                None
            }
        };
        Ok(Self {
            endpoint,
            tcp,
            identity: identity.clone(),
        })
    }

    /// The TCP fallback's address, if it is listening.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp.as_ref().map(TcpAcceptor::local_addr)
    }

    /// Dials a tablet that listens (USB: the tablet is our gateway, so we
    /// know its address, and outgoing connections need no firewall rule).
    /// Only tablets whose fingerprint is in `paired` are accepted.
    pub async fn dial(
        &self,
        addr: SocketAddr,
        paired: Vec<Fingerprint>,
    ) -> Result<Connection, TransportError> {
        let cfg = client_config(&self.identity, paired)?;
        let conn = self.endpoint.connect_with(cfg, addr, "inkbridge")?.await?;
        let peer = peer_fingerprint(&conn)?;
        Ok(Connection::quic(conn, peer))
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for the next tablet, over either transport. Returns `None`
    /// once the endpoint is closed.
    pub async fn accept(&self) -> Option<Result<Connection, TransportError>> {
        let tcp = async {
            match &self.tcp {
                Some(t) => t.accept().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            incoming = self.endpoint.accept() => {
                let incoming = incoming?;
                Some(
                    async move {
                        let conn = incoming.await?;
                        let peer = peer_fingerprint(&conn)?;
                        Ok(Connection::quic(conn, peer))
                    }
                    .await,
                )
            }
            Some((session, peer)) = tcp => Some(Ok(Connection {
                inner: Inner::Tcp(session),
                peer,
            })),
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"host shutting down");
    }
}

/// How long QUIC may try alone before TCP is tried too. A LAN handshake
/// takes a few milliseconds; where UDP is blocked QUIC never answers.
const QUIC_HEAD_START: Duration = Duration::from_millis(500);

/// Client (tablet) side.
#[derive(Debug)]
pub struct ClientEndpoint {
    endpoint: quinn::Endpoint,
    identity: Identity,
}

impl ClientEndpoint {
    pub fn new(identity: Identity) -> Result<Self, TransportError> {
        let socket = bind_udp((std::net::Ipv6Addr::UNSPECIFIED, 0).into())
            .or_else(|_| bind_udp((std::net::Ipv4Addr::UNSPECIFIED, 0).into()))?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self { endpoint, identity })
    }

    /// Connects to a host whose certificate must match `host_fp`.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        host_fp: Fingerprint,
    ) -> Result<Connection, TransportError> {
        let cfg = client_config(&self.identity, vec![host_fp])?;
        // The server name is irrelevant (we pin by fingerprint) but must be valid.
        let conn = self.endpoint.connect_with(cfg, addr, "inkbridge")?.await?;
        Ok(Connection::quic(conn, host_fp))
    }

    /// Connects over TLS/TCP only: the fallback, and the only way over
    /// links that carry just TCP (`adb reverse` to `127.0.0.1`).
    pub async fn connect_tcp(
        &self,
        addr: SocketAddr,
        host_fp: Fingerprint,
    ) -> Result<Connection, TransportError> {
        let cfg = Arc::new(tls_client_config(&self.identity, vec![host_fp])?);
        let session = crate::tcp::connect(addr, cfg).await?;
        Ok(Connection {
            inner: Inner::Tcp(session),
            peer: host_fp,
        })
    }

    /// Connects over QUIC, or over TCP if QUIC hasn't succeeded within a
    /// head start (UDP blocked) or failed. The first to succeed is used.
    pub async fn connect_any(
        &self,
        addr: SocketAddr,
        host_fp: Fingerprint,
    ) -> Result<Connection, TransportError> {
        let quic = self.connect(addr, host_fp);
        let tcp = async {
            tokio::time::sleep(QUIC_HEAD_START).await;
            self.connect_tcp(addr, host_fp).await
        };
        tokio::pin!(quic, tcp);
        tokio::select! {
            r = &mut quic => match r {
                Ok(c) => Ok(c),
                // QUIC's error explains more (wrong host, refused) unless TCP works.
                Err(e) => tcp.await.map_err(|_| e),
            },
            r = &mut tcp => match r {
                Ok(c) => Ok(c),
                Err(_) => quic.await,
            },
        }
    }
}

/// Tablet side, USB: waits for the paired computer to dial in over the
/// tethering link. Bound to the USB interface's address only, and accepts
/// only the paired computer's certificate.
#[derive(Debug)]
pub struct UsbListener {
    endpoint: quinn::Endpoint,
    host: Fingerprint,
}

impl UsbListener {
    pub fn bind(
        identity: &Identity,
        addr: SocketAddr,
        host: Fingerprint,
    ) -> Result<Self, TransportError> {
        let verifier = Arc::new(crate::tls::PinnedClient {
            expected: host,
            provider: provider(),
        });
        let cfg = server_config(identity, verifier)?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(cfg),
            bind_udp(addr)?,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self { endpoint, host })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for the computer. `None` once closed.
    pub async fn accept(&self) -> Option<Result<Connection, TransportError>> {
        let incoming = self.endpoint.accept().await?;
        let host = self.host;
        Some(async move { Ok(Connection::quic(incoming.await?, host)) }.await)
    }
}

/// What QUIC knows about the path to the peer. Counters are totals since
/// the connection started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    pub sent_packets: u64,
    pub lost_packets: u64,
    /// Times the sender slowed down because it saw loss.
    pub congestion_events: u64,
    /// Bytes the sender may have in flight.
    pub window_bytes: u64,
    pub rtt: Duration,
}

/// An authenticated connection to a peer with a known fingerprint, over
/// QUIC or the TCP fallback.
#[derive(Debug, Clone)]
pub struct Connection {
    inner: Inner,
    peer: Fingerprint,
}

#[derive(Debug, Clone)]
enum Inner {
    Quic(quinn::Connection),
    Tcp(Arc<TcpSession>),
}

impl Connection {
    fn quic(conn: quinn::Connection, peer: Fingerprint) -> Self {
        Self {
            inner: Inner::Quic(conn),
            peer,
        }
    }

    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer
    }

    pub fn remote_addr(&self) -> SocketAddr {
        match &self.inner {
            Inner::Quic(c) => c.remote_address(),
            Inner::Tcp(s) => s.remote,
        }
    }

    /// Whether this is the TCP fallback rather than QUIC.
    pub fn is_tcp(&self) -> bool {
        matches!(self.inner, Inner::Tcp(_))
    }

    /// Client: opens the control stream.
    pub async fn open_control(&self) -> Result<Control, TransportError> {
        match &self.inner {
            Inner::Quic(c) => {
                let (send, recv) = c.open_bi().await?;
                Ok(Control::new(SendHalf::Quic(send), RecvHalf::Quic(recv)))
            }
            Inner::Tcp(s) => s.control(),
        }
    }

    /// Host: accepts the control stream opened by the client.
    pub async fn accept_control(&self) -> Result<Control, TransportError> {
        match &self.inner {
            Inner::Quic(c) => {
                let (send, recv) = c.accept_bi().await?;
                Ok(Control::new(SendHalf::Quic(send), RecvHalf::Quic(recv)))
            }
            Inner::Tcp(s) => s.control(),
        }
    }

    /// Host: opens the video stream to the tablet.
    pub async fn open_video(&self) -> Result<crate::VideoSender, TransportError> {
        match &self.inner {
            Inner::Quic(c) => Ok(crate::VideoSender::new(SendHalf::Quic(c.open_uni().await?))),
            Inner::Tcp(s) => s.video_sender(),
        }
    }

    /// Tablet: accepts the host's video stream.
    pub async fn accept_video(&self) -> Result<crate::VideoReceiver, TransportError> {
        match &self.inner {
            Inner::Quic(c) => Ok(crate::VideoReceiver::new(RecvHalf::Quic(
                c.accept_uni().await?,
            ))),
            Inner::Tcp(s) => s.video_receiver(),
        }
    }

    pub fn pen(&self) -> PenChannel {
        PenChannel {
            inner: self.inner.clone(),
        }
    }

    /// Smoothed round-trip time estimate.
    pub fn rtt(&self) -> Duration {
        match &self.inner {
            Inner::Quic(c) => c.rtt(),
            Inner::Tcp(s) => s.rtt(),
        }
    }

    /// Counters of the network path, for diagnosing stalls. Over TCP only
    /// the round trip is known (the kernel keeps the rest).
    pub fn link_stats(&self) -> LinkStats {
        match &self.inner {
            Inner::Quic(c) => {
                let path = c.stats().path;
                LinkStats {
                    sent_packets: path.sent_packets,
                    lost_packets: path.lost_packets,
                    congestion_events: path.congestion_events,
                    window_bytes: path.cwnd,
                    rtt: path.rtt,
                }
            }
            Inner::Tcp(s) => LinkStats {
                sent_packets: 0,
                lost_packets: 0,
                congestion_events: 0,
                window_bytes: 0,
                rtt: s.rtt(),
            },
        }
    }

    pub fn close(&self, reason: &str) {
        match &self.inner {
            Inner::Quic(c) => c.close(0u32.into(), reason.as_bytes()),
            Inner::Tcp(s) => s.close(),
        }
    }

    /// Resolves when the connection ends, for any reason.
    pub async fn closed(&self) {
        match &self.inner {
            Inner::Quic(c) => {
                let _ = c.closed().await;
            }
            Inner::Tcp(s) => s.closed.wait().await,
        }
    }
}

/// Pen samples: unreliable QUIC datagrams, or the TCP session's pen
/// connection (where a stalled link drops new samples instead).
#[derive(Debug, Clone)]
pub struct PenChannel {
    inner: Inner,
}

impl PenChannel {
    pub fn send(&self, batch: &PenBatch) -> Result<(), TransportError> {
        match &self.inner {
            Inner::Quic(c) => {
                c.send_datagram(Bytes::from(encode(batch)?))?;
                Ok(())
            }
            Inner::Tcp(s) => s.send_pen(batch),
        }
    }

    pub async fn recv(&self) -> Result<PenBatch, TransportError> {
        match &self.inner {
            Inner::Quic(c) => Ok(decode(&c.read_datagram().await?)?),
            Inner::Tcp(s) => s.recv_pen().await,
        }
    }
}

fn peer_fingerprint(conn: &quinn::Connection) -> Result<Fingerprint, TransportError> {
    let identity = conn
        .peer_identity()
        .ok_or(TransportError::NoPeerCertificate)?;
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| TransportError::NoPeerCertificate)?;
    let leaf = certs.first().ok_or(TransportError::NoPeerCertificate)?;
    Ok(Fingerprint::of(leaf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_protocol::{ControlMessage, PenSample, PenState};

    fn pair() -> (HostEndpoint, Identity, ClientEndpoint, Identity) {
        let host_id = Identity::generate().unwrap();
        let host = HostEndpoint::bind(&host_id, "127.0.0.1:0".parse().unwrap()).unwrap();
        let client_id = Identity::generate().unwrap();
        let client = ClientEndpoint::new(client_id.clone()).unwrap();
        (host, host_id, client, client_id)
    }

    #[tokio::test]
    async fn control_and_pen_round_trip_with_mutual_fingerprints() {
        let (host, host_id, client, client_id) = pair();
        let addr = host.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let conn = host.accept().await.unwrap().unwrap();
            assert_eq!(conn.peer_fingerprint(), client_id.fingerprint());
            let mut ctl = conn.accept_control().await.unwrap();
            let msg = ctl.recv().await.unwrap();
            ctl.send(&ControlMessage::Pong {
                t0_us: 1,
                t1_us: 2,
                t2_us: 3,
            })
            .await
            .unwrap();
            let batch = conn.pen().recv().await.unwrap();
            (msg, batch)
        });

        let conn = client.connect(addr, host_id.fingerprint()).await.unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        ctl.send(&ControlMessage::Ping { t0_us: 1 }).await.unwrap();
        assert_eq!(
            ctl.recv().await.unwrap(),
            ControlMessage::Pong {
                t0_us: 1,
                t1_us: 2,
                t2_us: 3
            }
        );
        let batch = PenBatch {
            seq: 1,
            samples: vec![PenSample {
                x: 0.5,
                y: 0.25,
                pressure: 0.8,
                state: PenState::Contact,
                ..Default::default()
            }],
        };
        conn.pen().send(&batch).unwrap();

        let (msg, got) = server.await.unwrap();
        assert_eq!(msg, ControlMessage::Ping { t0_us: 1 });
        assert_eq!(got, batch);
    }

    /// Regression: Windows makes `[::]` sockets IPv6-only by default, which
    /// left IPv4 tablets unable to reach the host.
    #[tokio::test]
    async fn ipv4_client_reaches_dual_stack_host() {
        let host_id = Identity::generate().unwrap();
        let host = HostEndpoint::bind(&host_id, "[::]:0".parse().unwrap()).unwrap();
        let port = host.local_addr().unwrap().port();
        tokio::spawn(async move {
            let conn = host.accept().await.unwrap().unwrap();
            conn.closed().await;
        });
        let client = ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let conn = tokio::time::timeout(
            Duration::from_secs(5),
            client.connect(addr, host_id.fingerprint()),
        )
        .await
        .expect("IPv4 client must reach a host bound to [::]")
        .unwrap();
        conn.close("done");
    }

    /// USB: the host dials the tablet; roles inside the session are unchanged
    /// (the tablet opens the control stream).
    #[tokio::test]
    async fn host_dials_paired_tablet_over_usb() {
        let (host_id, tablet_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = UsbListener::bind(
            &tablet_id,
            "127.0.0.1:0".parse().unwrap(),
            host_id.fingerprint(),
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();
        let tablet = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().unwrap();
            let mut ctl = conn.open_control().await.unwrap();
            ctl.send(&ControlMessage::Ping { t0_us: 7 }).await.unwrap();
            ctl.recv().await.unwrap()
        });
        let host = HostEndpoint::bind(&host_id, "127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = host
            .dial(addr, vec![tablet_id.fingerprint()])
            .await
            .unwrap();
        assert_eq!(conn.peer_fingerprint(), tablet_id.fingerprint());
        let mut ctl = conn.accept_control().await.unwrap();
        assert_eq!(ctl.recv().await.unwrap(), ControlMessage::Ping { t0_us: 7 });
        ctl.send(&ControlMessage::Goodbye).await.unwrap();
        assert_eq!(tablet.await.unwrap(), ControlMessage::Goodbye);
    }

    #[tokio::test]
    async fn host_refuses_unpaired_tablet_and_tablet_refuses_other_host() {
        let (host_id, tablet_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        // Host doesn't know this tablet.
        let listener = UsbListener::bind(
            &tablet_id,
            "127.0.0.1:0".parse().unwrap(),
            host_id.fingerprint(),
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { while let Some(_c) = listener.accept().await {} });
        let host = HostEndpoint::bind(&host_id, "127.0.0.1:0".parse().unwrap()).unwrap();
        let err = host.dial(addr, vec![Fingerprint([1; 32])]).await;
        assert!(err.is_err(), "unpaired tablet must be refused");

        // Tablet is paired with a different computer. In TLS 1.3 the dialler
        // completes its side of the handshake before the listener has checked
        // the dialler's certificate, so `dial` may report success; what
        // matters is that the tablet rejects it and no session can run.
        let stranger = Identity::generate().unwrap();
        let listener = UsbListener::bind(
            &tablet_id,
            "127.0.0.1:0".parse().unwrap(),
            stranger.fingerprint(),
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.map(|r| r.is_ok()) });
        if let Ok(Ok(conn)) = tokio::time::timeout(
            Duration::from_secs(5),
            host.dial(addr, vec![tablet_id.fingerprint()]),
        )
        .await
        {
            let ctl = tokio::time::timeout(Duration::from_secs(5), conn.accept_control()).await;
            assert!(
                !matches!(ctl, Ok(Ok(_))),
                "no session may run with a rejected computer"
            );
        }
        let tablet_side = tokio::time::timeout(Duration::from_secs(5), accepted)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tablet_side,
            Some(false),
            "tablet must reject a computer it isn't paired with"
        );
    }

    #[tokio::test]
    async fn wrong_pinned_fingerprint_is_refused() {
        let (host, _host_id, client, _) = pair();
        let addr = host.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = host.accept().await;
        });
        let err = client
            .connect(addr, Fingerprint([0; 32]))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Connection(_)), "{err:?}");
    }

    fn sample_batch() -> PenBatch {
        PenBatch {
            seq: 9,
            samples: vec![PenSample {
                x: 0.25,
                y: 0.75,
                pressure: 0.5,
                state: PenState::Contact,
                ..Default::default()
            }],
        }
    }

    #[tokio::test]
    async fn the_tcp_fallback_carries_control_pen_and_video() {
        let (host, host_id, client, client_id) = pair();
        let addr = host.tcp_addr().expect("TCP listening");
        let server = tokio::spawn(async move {
            let conn = host.accept().await.unwrap().unwrap();
            assert!(conn.is_tcp());
            assert_eq!(conn.peer_fingerprint(), client_id.fingerprint());
            let mut ctl = conn.accept_control().await.unwrap();
            assert_eq!(ctl.recv().await.unwrap(), ControlMessage::Ping { t0_us: 5 });
            let mut video = conn.open_video().await.unwrap();
            video
                .send(&ib_protocol::VideoChunk {
                    frame_no: 1,
                    kind: ib_protocol::FrameKind::Key,
                    width: 64,
                    height: 64,
                    capture_t_us: 2,
                    encode_done_t_us: 3,
                    payload: vec![7; 300_000],
                })
                .await
                .unwrap();
            let batch = conn.pen().recv().await.unwrap();
            ctl.send(&ControlMessage::Goodbye).await.unwrap();
            (batch, conn)
        });

        let conn = client
            .connect_tcp(addr, host_id.fingerprint())
            .await
            .unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        ctl.send(&ControlMessage::Ping { t0_us: 5 }).await.unwrap();
        let mut video = conn.accept_video().await.unwrap();
        let chunk = video.recv().await.unwrap();
        assert_eq!((chunk.frame_no, chunk.payload.len()), (1, 300_000));
        conn.pen().send(&sample_batch()).unwrap();
        assert_eq!(ctl.recv().await.unwrap(), ControlMessage::Goodbye);
        let (batch, host_conn) = server.await.unwrap();
        assert_eq!(batch, sample_batch());

        // The pen connection's pings measure the round trip.
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(conn.rtt() > Duration::ZERO, "no round trip measured");

        // Closing one side ends the session on the other.
        conn.close("done");
        tokio::time::timeout(Duration::from_secs(3), host_conn.closed())
            .await
            .expect("the host notices the tablet left");
    }

    /// Where UDP is blocked QUIC never answers: after its head start the
    /// client gets through over TCP.
    #[tokio::test]
    async fn connect_any_falls_back_to_tcp_when_udp_is_blocked() {
        let host_id = Identity::generate().unwrap();
        // A UDP socket that never answers, and the TCP fallback on its port.
        let black_hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = black_hole.local_addr().unwrap().port();
        let verifier = Arc::new(AnyClientCert {
            provider: provider(),
        });
        let host = HostEndpoint {
            endpoint: quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(server_config(&host_id, verifier.clone()).unwrap()),
                bind_udp("127.0.0.1:0".parse().unwrap()).unwrap(),
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap(),
            tcp: Some(
                TcpAcceptor::bind(
                    SocketAddr::from(([127, 0, 0, 1], port)),
                    Arc::new(tls_server_config(&host_id, verifier).unwrap()),
                )
                .unwrap(),
            ),
            identity: host_id.clone(),
        };
        tokio::spawn(async move {
            let conn = host.accept().await.unwrap().unwrap();
            let mut ctl = conn.accept_control().await.unwrap();
            let msg = ctl.recv().await.unwrap();
            ctl.send(&msg).await.unwrap();
            conn.closed().await;
        });
        let client = ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let started = std::time::Instant::now();
        let conn = tokio::time::timeout(
            Duration::from_secs(4),
            client.connect_any(addr, host_id.fingerprint()),
        )
        .await
        .expect("TCP must take over well before QUIC gives up")
        .unwrap();
        assert!(conn.is_tcp());
        assert!(started.elapsed() >= QUIC_HEAD_START);
        let mut ctl = conn.open_control().await.unwrap();
        ctl.send(&ControlMessage::Ping { t0_us: 3 }).await.unwrap();
        assert_eq!(ctl.recv().await.unwrap(), ControlMessage::Ping { t0_us: 3 });
        drop(black_hole);
    }

    #[tokio::test]
    async fn connect_any_prefers_quic() {
        let (host, host_id, client, _) = pair();
        let addr = host.local_addr().unwrap();
        tokio::spawn(async move {
            let conn = host.accept().await.unwrap().unwrap();
            conn.closed().await;
        });
        let conn = client
            .connect_any(addr, host_id.fingerprint())
            .await
            .unwrap();
        assert!(!conn.is_tcp());
    }

    #[tokio::test]
    async fn the_tcp_fallback_refuses_the_wrong_host() {
        let (host, _host_id, client, _) = pair();
        let addr = host.tcp_addr().unwrap();
        tokio::spawn(async move {
            let _ = host.accept().await;
        });
        assert!(
            client
                .connect_tcp(addr, Fingerprint([0; 32]))
                .await
                .is_err()
        );
    }
}
