//! Paired tablets, persisted as a small text file:
//! one `<fingerprint hex> <device name>` per line.

use std::path::{Path, PathBuf};

use ib_transport::Fingerprint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub fingerprint: Fingerprint,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct PairingStore {
    devices: Vec<PairedDevice>,
    path: Option<PathBuf>,
}

impl PairingStore {
    /// In-memory store (tests, or "don't remember" mode).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads from `path`; a missing file means no paired devices yet.
    /// Malformed lines are skipped rather than failing the whole host.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let devices = text
            .lines()
            .filter_map(|line| {
                let (fp, name) = line.split_once(' ')?;
                Some(PairedDevice {
                    fingerprint: Fingerprint::from_hex(fp)?,
                    name: name.to_owned(),
                })
            })
            .collect();
        Ok(Self {
            devices,
            path: Some(path.to_owned()),
        })
    }

    pub fn devices(&self) -> &[PairedDevice] {
        &self.devices
    }

    pub fn is_paired(&self, fp: Fingerprint) -> bool {
        self.devices.iter().any(|d| d.fingerprint == fp)
    }

    /// Adds or renames a device and persists the store.
    pub fn add(&mut self, fingerprint: Fingerprint, name: &str) -> std::io::Result<()> {
        // Names come from the network: keep them single-line and bounded.
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        match self
            .devices
            .iter_mut()
            .find(|d| d.fingerprint == fingerprint)
        {
            Some(d) => d.name = name,
            None => self.devices.push(PairedDevice { fingerprint, name }),
        }
        self.save()
    }

    pub fn remove(&mut self, fingerprint: Fingerprint) -> std::io::Result<()> {
        self.devices.retain(|d| d.fingerprint != fingerprint);
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = String::new();
        for d in &self.devices {
            text.push_str(&d.fingerprint.to_hex());
            text.push(' ');
            text.push_str(&d.name);
            text.push('\n');
        }
        // Write-then-rename so a crash never leaves a truncated file.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_sanitises_names() {
        let dir = std::env::temp_dir().join(format!("ib-pairing-{}", std::process::id()));
        let path = dir.join("paired.txt");
        let _ = std::fs::remove_file(&path);

        let mut s = PairingStore::load(&path).unwrap();
        s.add(Fingerprint([1; 32]), "Tab\nlet").unwrap();
        s.add(Fingerprint([2; 32]), "Other").unwrap();
        s.remove(Fingerprint([2; 32])).unwrap();

        let back = PairingStore::load(&path).unwrap();
        assert_eq!(
            back.devices(),
            &[PairedDevice {
                fingerprint: Fingerprint([1; 32]),
                name: "Tablet".into()
            }]
        );
        assert!(back.is_paired(Fingerprint([1; 32])));
        assert!(!back.is_paired(Fingerprint([2; 32])));
        let _ = std::fs::remove_dir_all(dir);
    }
}
