use std::fmt;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::TransportError;

/// SHA-256 of a DER certificate: how peers recognise each other.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert_der: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        Self(out)
    }

    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex_decode(s)?;
        Some(Self(bytes.try_into().ok()?))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form is enough to tell devices apart in logs.
        write!(f, "Fingerprint({}…)", &self.to_hex()[..16])
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A device's long-lived certificate and private key (ECDSA P-256).
///
/// Persisted by the application (host: config dir; Android: app-private
/// storage) via [`Identity::to_pkcs8_der`] / [`Identity::from_der`].
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    fingerprint: Fingerprint,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key: self.key.clone_key(),
            fingerprint: self.fingerprint,
        }
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print key material.
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn generate() -> Result<Self, TransportError> {
        let ck = rcgen::generate_simple_self_signed(vec!["inkbridge".to_owned()])?;
        let cert = ck.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
        Ok(Self::from_parts(cert, key))
    }

    pub fn from_der(cert_der: Vec<u8>, pkcs8_key_der: Vec<u8>) -> Self {
        Self::from_parts(
            CertificateDer::from(cert_der),
            PrivatePkcs8KeyDer::from(pkcs8_key_der),
        )
    }

    fn from_parts(cert: CertificateDer<'static>, key: PrivatePkcs8KeyDer<'static>) -> Self {
        let fingerprint = Fingerprint::of(&cert);
        Self {
            cert,
            key,
            fingerprint,
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert
    }

    pub fn to_pkcs8_der(&self) -> &[u8] {
        self.key.secret_pkcs8_der()
    }

    pub(crate) fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.cert.clone()]
    }

    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_through_der() {
        let id = Identity::generate().unwrap();
        let back = Identity::from_der(id.cert_der().to_vec(), id.to_pkcs8_der().to_vec());
        assert_eq!(back.fingerprint(), id.fingerprint());
        assert_ne!(
            Identity::generate().unwrap().fingerprint(),
            id.fingerprint()
        );
    }

    #[test]
    fn fingerprint_hex_round_trip() {
        let fp = Fingerprint([0xAB; 32]);
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(Fingerprint::from_hex("zz"), None);
        assert_eq!(Fingerprint::from_hex("abcd"), None); // wrong length
    }

    #[test]
    fn debug_never_prints_keys() {
        let id = Identity::generate().unwrap();
        let dbg = format!("{id:?}");
        assert!(dbg.contains("fingerprint") && !dbg.contains("key"));
    }
}
