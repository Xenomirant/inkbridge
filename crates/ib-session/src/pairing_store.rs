//! Paired tablets, persisted as a small text file: one
//! `<fingerprint hex> @<last use, unix seconds> <device name>` per line.
//!
//! A paired tablet can inject input, so pairings don't last forever: one
//! that hasn't connected for [`FORGET_AFTER`] is dropped and has to scan
//! the QR code again. Lines from before last-use dates (`<fingerprint>
//! <name>`) count as used when they are loaded.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ib_transport::Fingerprint;

/// How long a pairing lasts without a connection (the tablet app forgets
/// computers after the same time).
pub const FORGET_AFTER: Duration = Duration::from_hours(90 * 24);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// Unix seconds of the last pairing or connection.
    pub last_used: u64,
}

impl PairedDevice {
    fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.last_used) > FORGET_AFTER.as_secs()
    }
}

#[derive(Debug, Default)]
pub struct PairingStore {
    devices: Vec<PairedDevice>,
    path: Option<PathBuf>,
}

/// Unix seconds now.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One line of the file, in either format.
fn parse_line(line: &str, loaded_at: u64) -> Option<PairedDevice> {
    let (fp, rest) = line.split_once(' ')?;
    let (last_used, name) = match rest.strip_prefix('@').and_then(|r| r.split_once(' ')) {
        Some((secs, name)) => (secs.parse().ok()?, name),
        None => (loaded_at, rest),
    };
    Some(PairedDevice {
        fingerprint: Fingerprint::from_hex(fp)?,
        name: name.to_owned(),
        last_used,
    })
}

impl PairingStore {
    /// In-memory store (tests, or "don't remember" mode).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads from `path`; a missing file means no paired devices yet.
    /// Malformed lines are skipped rather than failing the whole host, and
    /// expired pairings are dropped (the file is rewritten without them).
    pub fn load(path: &Path) -> std::io::Result<Self> {
        Self::load_at(path, now())
    }

    fn load_at(path: &Path, now: u64) -> std::io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let all: Vec<PairedDevice> = text.lines().filter_map(|l| parse_line(l, now)).collect();
        let count = all.len();
        // Lines from before last-use dates get today's date written down.
        let undated = text.lines().any(|l| {
            l.split_once(' ')
                .is_some_and(|(_, rest)| !rest.starts_with('@'))
        });
        let devices: Vec<PairedDevice> = all.into_iter().filter(|d| !d.expired(now)).collect();
        let store = Self {
            devices,
            path: Some(path.to_owned()),
        };
        if store.devices.len() < count {
            tracing::info!(
                forgotten = count - store.devices.len(),
                "forgot tablets that haven't connected for 90 days"
            );
        }
        if store.devices.len() < count || undated {
            store.save()?;
        }
        Ok(store)
    }

    pub fn devices(&self) -> &[PairedDevice] {
        &self.devices
    }

    /// Whether `fp` is paired and the pairing hasn't expired.
    pub fn is_paired(&self, fp: Fingerprint) -> bool {
        let now = now();
        self.devices
            .iter()
            .any(|d| d.fingerprint == fp && !d.expired(now))
    }

    /// Adds or renames a device, marks it used and persists the store.
    pub fn add(&mut self, fingerprint: Fingerprint, name: &str) -> std::io::Result<()> {
        // Names come from the network: keep them single-line and bounded.
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        let last_used = now();
        match self
            .devices
            .iter_mut()
            .find(|d| d.fingerprint == fingerprint)
        {
            Some(d) => {
                d.name = name;
                d.last_used = last_used;
            }
            None => self.devices.push(PairedDevice {
                fingerprint,
                name,
                last_used,
            }),
        }
        self.save()
    }

    /// A paired tablet connected: its pairing lasts from now. Written at
    /// most once a day, so reconnects don't rewrite the file each time.
    pub fn mark_used(&mut self, fingerprint: Fingerprint) -> std::io::Result<()> {
        let now = now();
        let Some(d) = self
            .devices
            .iter_mut()
            .find(|d| d.fingerprint == fingerprint)
        else {
            return Ok(());
        };
        if now.saturating_sub(d.last_used) < 24 * 60 * 60 {
            return Ok(());
        }
        d.last_used = now;
        self.save()
    }

    pub fn remove(&mut self, fingerprint: Fingerprint) -> std::io::Result<()> {
        self.devices.retain(|d| d.fingerprint != fingerprint);
        self.save()
    }

    /// Forgets every paired tablet.
    pub fn clear(&mut self) -> std::io::Result<()> {
        self.devices.clear();
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
            let _ = writeln!(
                text,
                "{} @{} {}",
                d.fingerprint.to_hex(),
                d.last_used,
                d.name
            );
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

    fn temp_file(test: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ib-pairing-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("paired.txt"), dir)
    }

    #[test]
    fn persists_and_sanitises_names() {
        let (path, dir) = temp_file("names");
        let mut s = PairingStore::load(&path).unwrap();
        s.add(Fingerprint([1; 32]), "Tab\nlet").unwrap();
        s.add(Fingerprint([2; 32]), "Other").unwrap();
        s.remove(Fingerprint([2; 32])).unwrap();

        let back = PairingStore::load(&path).unwrap();
        assert_eq!(back.devices().len(), 1);
        assert_eq!(back.devices()[0].fingerprint, Fingerprint([1; 32]));
        assert_eq!(back.devices()[0].name, "Tablet");
        assert!(back.is_paired(Fingerprint([1; 32])));
        assert!(!back.is_paired(Fingerprint([2; 32])));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tablet_unused_for_90_days_is_forgotten() {
        let (path, dir) = temp_file("expiry");
        let now = 1_900_000_000;
        let day = 24 * 60 * 60;
        let fresh = Fingerprint([1; 32]).to_hex();
        let stale = Fingerprint([2; 32]).to_hex();
        std::fs::write(
            &path,
            format!(
                "{fresh} @{} Recent tab\n{stale} @{} Old tab\n",
                now - 89 * day,
                now - 91 * day
            ),
        )
        .unwrap();

        let s = PairingStore::load_at(&path, now).unwrap();
        let names: Vec<&str> = s.devices().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Recent tab"]);
        // The file no longer lists the forgotten tablet.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&stale), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lines_without_a_date_count_as_used_when_loaded() {
        let (path, dir) = temp_file("old-format");
        let fp = Fingerprint([3; 32]);
        std::fs::write(&path, format!("{} Tab with spaces\n", fp.to_hex())).unwrap();

        let s = PairingStore::load_at(&path, 1_900_000_000).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(" @1900000000 Tab with spaces"), "{text}");
        assert_eq!(
            s.devices(),
            &[PairedDevice {
                fingerprint: fp,
                name: "Tab with spaces".into(),
                last_used: 1_900_000_000,
            }]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_connection_renews_the_pairing() {
        let (path, dir) = temp_file("renew");
        let fp = Fingerprint([4; 32]);
        let long_ago = now() - 30 * 24 * 60 * 60;
        std::fs::write(&path, format!("{} @{long_ago} Tab\n", fp.to_hex())).unwrap();

        let mut s = PairingStore::load(&path).unwrap();
        s.mark_used(fp).unwrap();
        let back = PairingStore::load(&path).unwrap();
        assert!(back.devices()[0].last_used >= now() - 5);
        let _ = std::fs::remove_dir_all(dir);
    }
}
