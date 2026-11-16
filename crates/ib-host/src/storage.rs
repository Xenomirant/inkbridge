//! Where the host keeps its identity and paired devices.

use std::path::{Path, PathBuf};

use ib_transport::Identity;

/// Per-user config directory (no admin rights needed):
/// Windows `%APPDATA%\Inkbridge\Inkbridge\config`, Linux `~/.config/inkbridge`,
/// macOS `~/Library/Application Support/org.Inkbridge.Inkbridge`.
pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("org", "Inkbridge", "Inkbridge").map_or_else(
        || PathBuf::from(".inkbridge"),
        |d| d.config_dir().to_owned(),
    )
}

/// Loads the host identity, creating and saving it on first run.
pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<Identity> {
    let cert = dir.join("host-cert.der");
    let key = dir.join("host-key.der");
    if let (Ok(c), Ok(k)) = (std::fs::read(&cert), std::fs::read(&key)) {
        return Ok(Identity::from_der(c, k));
    }
    let id = Identity::generate()?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert, id.cert_der())?;
    std::fs::write(&key, id.to_pkcs8_der())?;
    tracing::info!(fingerprint = %id.fingerprint(), "created new host identity");
    Ok(id)
}
