//! Pairing + hello exchange on the control stream (see `docs/protocol.md`).

use ib_protocol::{
    Capabilities, ClientHello, ControlMessage, HostHello, PAIRING_SECRET_LEN, PROTOCOL_VERSION,
    Region, Reject, RejectReason, TransportKind,
};
use ib_transport::{Control, Fingerprint, PairingSecret, TransportError};

use crate::PairingStore;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("peer rejected the session: {0:?}")]
    Rejected(RejectReason),
    #[error("unexpected message during handshake: {0}")]
    Unexpected(&'static str),
    #[error("could not save paired device: {0}")]
    Store(#[from] std::io::Error),
}

/// What the host offers to every session.
#[derive(Debug, Clone)]
pub struct HostSettings {
    pub host_name: String,
    pub caps: Capabilities,
    pub region: Region,
    pub transport: TransportKind,
}

/// Host side. `pending_secret` is the secret currently shown in the QR code,
/// if pairing mode is open. Returns the client's hello and whether it used
/// that secret (the caller must then retire it); on failure the client has
/// already been told why.
pub async fn host_handshake(
    ctl: &mut Control,
    peer: Fingerprint,
    settings: &HostSettings,
    store: &mut PairingStore,
    pending_secret: Option<PairingSecret>,
) -> Result<(ClientHello, bool), SessionError> {
    let mut msg = ctl.recv().await?;
    let mut used_secret = false;

    if let ControlMessage::PairRequest {
        secret,
        device_name,
    } = &msg
    {
        if !pending_secret.is_some_and(|s| s.matches(secret)) {
            reject(ctl, RejectReason::BadPairingSecret).await?;
            return Err(SessionError::Rejected(RejectReason::BadPairingSecret));
        }
        let known = store.is_paired(peer);
        store.add(peer, device_name)?;
        used_secret = true;
        if known {
            tracing::info!(device = %device_name, fingerprint = ?peer, "known device paired again");
        } else {
            tracing::info!(device = %device_name, fingerprint = ?peer, "paired new device");
        }
        ctl.send(&ControlMessage::PairAccepted {
            host_name: settings.host_name.clone(),
        })
        .await?;
        msg = ctl.recv().await?;
    }

    let ControlMessage::ClientHello(hello) = msg else {
        return Err(SessionError::Unexpected("expected ClientHello"));
    };
    if !store.is_paired(peer) {
        reject(ctl, RejectReason::NotPaired).await?;
        return Err(SessionError::Rejected(RejectReason::NotPaired));
    }
    if !hello.version.is_compatible_with(PROTOCOL_VERSION) {
        reject(ctl, RejectReason::IncompatibleVersion).await?;
        return Err(SessionError::Rejected(RejectReason::IncompatibleVersion));
    }

    let mut caps = settings.caps.clone();
    caps.pen = caps.pen.intersect(hello.caps.pen);
    ctl.send(&ControlMessage::HostHello(HostHello {
        version: PROTOCOL_VERSION,
        host_name: settings.host_name.clone(),
        codec: settings.caps.negotiate_codec(&hello.caps),
        caps,
        region: settings.region,
        display: settings.region,
        transport: settings.transport,
    }))
    .await?;
    Ok((hello, used_secret))
}

async fn reject(ctl: &mut Control, reason: RejectReason) -> Result<(), TransportError> {
    ctl.send(&ControlMessage::Reject(Reject {
        reason,
        host_version: PROTOCOL_VERSION,
    }))
    .await?;
    // The caller drops the connection next; make sure the reason arrives.
    ctl.close_send().await;
    Ok(())
}

/// Client side. Pass `pair_secret` (from the QR code) on first contact.
pub async fn client_handshake(
    ctl: &mut Control,
    hello: ClientHello,
    pair_secret: Option<([u8; PAIRING_SECRET_LEN], String)>,
) -> Result<HostHello, SessionError> {
    if let Some((secret, device_name)) = pair_secret {
        ctl.send(&ControlMessage::PairRequest {
            secret,
            device_name,
        })
        .await?;
        match ctl.recv().await? {
            ControlMessage::PairAccepted { .. } => {}
            ControlMessage::Reject(r) => return Err(SessionError::Rejected(r.reason)),
            _ => return Err(SessionError::Unexpected("expected PairAccepted")),
        }
    }
    ctl.send(&ControlMessage::ClientHello(hello)).await?;
    match ctl.recv().await? {
        ControlMessage::HostHello(h) => Ok(h),
        ControlMessage::Reject(r) => Err(SessionError::Rejected(r.reason)),
        _ => Err(SessionError::Unexpected("expected HostHello")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_protocol::{PenFeatures, VideoCodec};
    use ib_transport::{ClientEndpoint, HostEndpoint, Identity};

    fn caps(pen: PenFeatures) -> Capabilities {
        Capabilities {
            video_codecs: vec![VideoCodec::H264],
            max_width: 2560,
            max_height: 1600,
            max_fps: 60,
            pen,
        }
    }

    fn settings() -> HostSettings {
        HostSettings {
            host_name: "PC".into(),
            caps: caps(PenFeatures {
                pressure: true,
                tilt: true,
                eraser: true,
                ..Default::default()
            }),
            region: Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            transport: TransportKind::Wifi,
        }
    }

    fn hello() -> ClientHello {
        ClientHello {
            version: PROTOCOL_VERSION,
            device_name: "Tablet".into(),
            caps: caps(PenFeatures {
                pressure: true,
                hover: true,
                eraser: true,
                ..Default::default()
            }),
            screen_width: 2560,
            screen_height: 1600,
        }
    }

    /// Runs one host handshake against one client handshake over real QUIC.
    async fn run(
        store: PairingStore,
        pending: Option<PairingSecret>,
        client_secret: Option<[u8; PAIRING_SECRET_LEN]>,
    ) -> (
        Result<ClientHello, SessionError>,
        Result<HostHello, SessionError>,
        PairingStore,
    ) {
        let host_id = Identity::generate().unwrap();
        let host = HostEndpoint::bind(&host_id, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = host.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut store = store;
            let conn = host.accept().await.unwrap().unwrap();
            let mut ctl = conn.accept_control().await.unwrap();
            let r = host_handshake(
                &mut ctl,
                conn.peer_fingerprint(),
                &settings(),
                &mut store,
                pending,
            )
            .await
            .map(|(hello, _)| hello);
            // Keep the connection open until the client is done reading.
            conn.closed().await;
            (r, store)
        });
        let client = ClientEndpoint::new(Identity::generate().unwrap()).unwrap();
        let conn = client.connect(addr, host_id.fingerprint()).await.unwrap();
        let mut ctl = conn.open_control().await.unwrap();
        let c = client_handshake(
            &mut ctl,
            hello(),
            client_secret.map(|s| (s, "Tablet".into())),
        )
        .await;
        conn.close("test done");
        let (h, store) = server.await.unwrap();
        (h, c, store)
    }

    #[tokio::test]
    async fn pairing_then_hello_succeeds_and_negotiates() {
        let secret = PairingSecret::random();
        let (h, c, store) = run(PairingStore::in_memory(), Some(secret), Some(secret.0)).await;
        assert_eq!(h.unwrap().device_name, "Tablet");
        let hh = c.unwrap();
        assert_eq!(hh.codec, Some(VideoCodec::H264));
        assert_eq!(
            hh.caps.pen,
            PenFeatures {
                pressure: true,
                eraser: true,
                ..Default::default()
            }
        );
        assert_eq!(store.devices().len(), 1);
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected_and_nothing_is_stored() {
        let (h, c, store) = run(
            PairingStore::in_memory(),
            Some(PairingSecret::random()),
            Some([0; 16]),
        )
        .await;
        assert!(matches!(
            h,
            Err(SessionError::Rejected(RejectReason::BadPairingSecret))
        ));
        assert!(matches!(
            c,
            Err(SessionError::Rejected(RejectReason::BadPairingSecret))
        ));
        assert_eq!(store.devices(), &[]);
    }

    #[tokio::test]
    async fn pairing_closed_rejects_any_secret() {
        let (_, c, _) = run(PairingStore::in_memory(), None, Some([0; 16])).await;
        assert!(matches!(
            c,
            Err(SessionError::Rejected(RejectReason::BadPairingSecret))
        ));
    }

    #[tokio::test]
    async fn unpaired_client_without_secret_is_rejected() {
        let (_, c, _) = run(PairingStore::in_memory(), None, None).await;
        assert!(matches!(
            c,
            Err(SessionError::Rejected(RejectReason::NotPaired))
        ));
    }
}
