//! Pairing offers: the content of the host's QR code.
//!
//! Format (a URI so phone camera apps recognise it):
//! `inkbridge:pair?v=1&a=<addr>,<addr>&fp=<64 hex>&s=<32 hex>`
//!
//! - `a`: host socket addresses to try, in order (USB link first, then LAN);
//! - `fp`: SHA-256 of the host certificate, pinned by the client;
//! - `s`: one-time 128-bit secret proving the client saw the QR code.

use std::net::SocketAddr;

use ib_protocol::PAIRING_SECRET_LEN;
use subtle::ConstantTimeEq;

use crate::identity::{Fingerprint, hex_decode, hex_encode};

/// One-time pairing secret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PairingSecret(pub [u8; PAIRING_SECRET_LEN]);

impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingSecret(…)")
    }
}

impl PairingSecret {
    /// Fresh secret from the OS CSPRNG.
    ///
    /// # Panics
    /// If the OS random source fails, which `ring` treats as unrecoverable.
    pub fn random() -> Self {
        use ring::rand::SecureRandom as _;
        let mut b = [0u8; PAIRING_SECRET_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut b)
            .expect("OS random number generator failed");
        Self(b)
    }

    /// Constant-time comparison, so response timing reveals nothing.
    pub fn matches(&self, candidate: &[u8; PAIRING_SECRET_LEN]) -> bool {
        self.0.ct_eq(candidate).into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    pub addrs: Vec<SocketAddr>,
    pub fingerprint: Fingerprint,
    pub secret: PairingSecret,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingOfferError {
    #[error("not an Inkbridge pairing code")]
    NotInkbridge,
    #[error("unsupported pairing code version")]
    UnsupportedVersion,
    #[error("pairing code is missing or has an invalid `{0}` field")]
    BadField(&'static str),
}

impl PairingOffer {
    const PREFIX: &'static str = "inkbridge:pair?";

    pub fn to_uri(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(ToString::to_string).collect();
        format!(
            "{}v=1&a={}&fp={}&s={}",
            Self::PREFIX,
            addrs.join(","),
            self.fingerprint.to_hex(),
            hex_encode(&self.secret.0)
        )
    }

    pub fn parse(uri: &str) -> Result<Self, PairingOfferError> {
        let query = uri
            .trim()
            .strip_prefix(Self::PREFIX)
            .ok_or(PairingOfferError::NotInkbridge)?;
        let (mut v, mut a, mut fp, mut s) = (None, None, None, None);
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("v", x)) => v = Some(x),
                Some(("a", x)) => a = Some(x),
                Some(("fp", x)) => fp = Some(x),
                Some(("s", x)) => s = Some(x),
                _ => {} // ignore unknown fields: forward compatible
            }
        }
        if v != Some("1") {
            return Err(PairingOfferError::UnsupportedVersion);
        }
        let addrs: Vec<SocketAddr> = a
            .ok_or(PairingOfferError::BadField("a"))?
            .split(',')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| PairingOfferError::BadField("a"))?;
        if addrs.is_empty() {
            return Err(PairingOfferError::BadField("a"));
        }
        let fingerprint = fp
            .and_then(Fingerprint::from_hex)
            .ok_or(PairingOfferError::BadField("fp"))?;
        let secret = s
            .and_then(hex_decode)
            .and_then(|b| <[u8; PAIRING_SECRET_LEN]>::try_from(b).ok())
            .map(PairingSecret)
            .ok_or(PairingOfferError::BadField("s"))?;
        Ok(Self {
            addrs,
            fingerprint,
            secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> PairingOffer {
        PairingOffer {
            addrs: vec![
                "192.168.42.129:47800".parse().unwrap(),
                "[fe80::1]:47800".parse().unwrap(),
            ],
            fingerprint: Fingerprint([7; 32]),
            secret: PairingSecret([9; PAIRING_SECRET_LEN]),
        }
    }

    #[test]
    fn uri_round_trip() {
        let o = offer();
        assert_eq!(PairingOffer::parse(&o.to_uri()), Ok(o));
    }

    #[test]
    fn rejects_bad_codes() {
        assert_eq!(
            PairingOffer::parse("https://example.com"),
            Err(PairingOfferError::NotInkbridge)
        );
        let uri = offer().to_uri();
        assert_eq!(
            PairingOffer::parse(&uri.replace("v=1", "v=9")),
            Err(PairingOfferError::UnsupportedVersion)
        );
        assert_eq!(
            PairingOffer::parse(&uri.replace("&s=", "&s=00")),
            Err(PairingOfferError::BadField("s"))
        );
    }

    #[test]
    fn secret_comparison() {
        let s = PairingSecret::random();
        assert!(s.matches(&s.0));
        assert!(!s.matches(&[0; PAIRING_SECRET_LEN]) || s.0 == [0; PAIRING_SECRET_LEN]);
        assert_ne!(PairingSecret::random(), s);
    }
}
