use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ib_protocol::{PenBatch, decode, encode};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::pki_types::CertificateDer;

use crate::framing::Control;
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

/// QUIC settings tuned for an interactive LAN/USB link.
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // Detect a vanished tablet quickly so the pen gets released promptly.
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(5)).expect("valid timeout"),
    ));
    t.keep_alive_interval(Some(Duration::from_millis(1000)));
    // Small datagram buffers: stale pen samples are worse than lost ones.
    t.datagram_receive_buffer_size(Some(64 * 1024));
    t.datagram_send_buffer_size(64 * 1024);
    Arc::new(t)
}

/// Host side: listens for tablets.
#[derive(Debug)]
pub struct HostEndpoint {
    endpoint: quinn::Endpoint,
}

impl HostEndpoint {
    pub fn bind(identity: &Identity, addr: SocketAddr) -> Result<Self, TransportError> {
        let p = provider();
        let mut tls = rustls::ServerConfig::builder_with_provider(p.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(AnyClientCert { provider: p }))
            .with_single_cert(identity.cert_chain(), identity.private_key())?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let quic = QuicServerConfig::try_from(tls)
            .map_err(|e| TransportError::Tls(rustls::Error::General(e.to_string())))?;
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
        cfg.transport_config(transport_config());
        Ok(Self {
            endpoint: quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(cfg),
                bind_udp(addr)?,
                Arc::new(quinn::TokioRuntime),
            )?,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Waits for the next tablet. Returns `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<Result<Connection, TransportError>> {
        let incoming = self.endpoint.accept().await?;
        Some(
            async move {
                let conn = incoming.await?;
                let peer = peer_fingerprint(&conn)?;
                Ok(Connection { conn, peer })
            }
            .await,
        )
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"host shutting down");
    }
}

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
        let p = provider();
        let mut tls = rustls::ClientConfig::builder_with_provider(p.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServer {
                expected: host_fp,
                provider: p,
            }))
            .with_client_auth_cert(self.identity.cert_chain(), self.identity.private_key())?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let quic = QuicClientConfig::try_from(tls)
            .map_err(|e| TransportError::Tls(rustls::Error::General(e.to_string())))?;
        let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
        cfg.transport_config(transport_config());
        // The server name is irrelevant (we pin by fingerprint) but must be valid.
        let conn = self.endpoint.connect_with(cfg, addr, "inkbridge")?.await?;
        Ok(Connection {
            conn,
            peer: host_fp,
        })
    }
}

/// An authenticated QUIC connection to a peer with a known fingerprint.
#[derive(Debug, Clone)]
pub struct Connection {
    conn: quinn::Connection,
    peer: Fingerprint,
}

impl Connection {
    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Client: opens the control stream.
    pub async fn open_control(&self) -> Result<Control, TransportError> {
        let (send, recv) = self.conn.open_bi().await?;
        Ok(Control::new(send, recv))
    }

    /// Host: accepts the control stream opened by the client.
    pub async fn accept_control(&self) -> Result<Control, TransportError> {
        let (send, recv) = self.conn.accept_bi().await?;
        Ok(Control::new(send, recv))
    }

    pub fn pen(&self) -> PenChannel {
        PenChannel {
            conn: self.conn.clone(),
        }
    }

    /// Smoothed round-trip time estimate from QUIC.
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Resolves when the connection ends, for any reason.
    pub async fn closed(&self) {
        let _ = self.conn.closed().await;
    }
}

/// Unreliable pen-sample datagrams.
#[derive(Debug, Clone)]
pub struct PenChannel {
    conn: quinn::Connection,
}

impl PenChannel {
    pub fn send(&self, batch: &PenBatch) -> Result<(), TransportError> {
        let bytes = encode(batch)?;
        self.conn.send_datagram(Bytes::from(bytes))?;
        Ok(())
    }

    pub async fn recv(&self) -> Result<PenBatch, TransportError> {
        let bytes = self.conn.read_datagram().await?;
        Ok(decode(&bytes)?)
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
}
