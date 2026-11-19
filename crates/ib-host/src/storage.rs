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
        // Files from older versions may be readable by everyone.
        private(&key)?;
        return Ok(Identity::from_der(c, k));
    }
    let id = Identity::generate()?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert, id.cert_der())?;
    write_private(&key, id.to_pkcs8_der())?;
    tracing::info!(fingerprint = %id.fingerprint(), "created new host identity");
    Ok(id)
}

/// Makes `path` readable by its owner only. Windows: the config directory
/// is in the user's own profile, which other users can't read.
fn private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Writes a secret: created owner-only, so it is never readable by others,
/// not even for a moment.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)?;
    private(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_private_key_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ib-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = load_or_create_identity(&dir).unwrap();
        let mode = |p| std::fs::metadata(dir.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("host-key.der"), 0o600);
        // An old, world-readable key is tightened on load.
        std::fs::set_permissions(
            dir.join("host-key.der"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let again = load_or_create_identity(&dir).unwrap();
        assert_eq!(again.fingerprint(), first.fingerprint());
        assert_eq!(mode("host-key.der"), 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
