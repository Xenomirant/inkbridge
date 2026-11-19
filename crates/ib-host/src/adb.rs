//! Advanced option `--adb`: the tablet over a USB cable without tethering.
//!
//! `adb reverse tcp:P tcp:P` makes port P on the tablet's loopback reach
//! port P on this computer through the cable. The app tries `127.0.0.1`
//! over TCP before anything else, so a tablet with USB debugging on
//! connects through the cable by itself. This is the USB path on macOS,
//! which can't use the tablet's RNDIS tethering. adb carries TCP only, so
//! the session runs on the TCP fallback.
//!
//! Needs Android's platform tools (adb) and USB debugging on the tablet.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Where adb usually is, after `PATH`.
fn adb_candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    let mut dirs: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|sdk| PathBuf::from(sdk).join("platform-tools"))
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local).join("Android");
        dirs.push(local.join("Sdk").join("platform-tools"));
        dirs.push(local.join("platform-tools"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("Library/Android/sdk/platform-tools"));
        dirs.push(home.join("Android/Sdk/platform-tools"));
    }
    let mut all = vec![PathBuf::from(exe)];
    all.extend(dirs.into_iter().map(|d| d.join(exe)));
    all
}

/// The first adb that runs.
pub fn find_adb() -> Option<PathBuf> {
    adb_candidates().into_iter().find(|adb| {
        Command::new(adb)
            .arg("version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Serial numbers of the devices `adb devices` lists as ready.
fn ready_devices(listing: &str) -> Vec<String> {
    listing
        .lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let serial = parts.next()?;
            (parts.next() == Some("device")).then(|| serial.to_owned())
        })
        .collect()
}

/// Keeps `adb reverse` set up for every tablet plugged in with USB
/// debugging, on a background thread, for as long as the host runs.
pub fn keep_reversed(adb: PathBuf, port: u16) {
    let spec = format!("tcp:{port}");
    let run = move || {
        let mut reversed: HashSet<String> = HashSet::new();
        loop {
            let listing = Command::new(&adb).arg("devices").output();
            let devices = listing.map_or_else(
                |_| Vec::new(),
                |o| ready_devices(&String::from_utf8_lossy(&o.stdout)),
            );
            // A tablet unplugged and plugged in again needs it again.
            reversed.retain(|s| devices.contains(s));
            for serial in devices {
                if reversed.contains(&serial) {
                    continue;
                }
                let done = Command::new(&adb)
                    .args(["-s", &serial, "reverse", &spec, &spec])
                    .output()
                    .is_ok_and(|o| o.status.success());
                if done {
                    tracing::info!(%serial, "tablet on USB through adb: it connects over the cable");
                    reversed.insert(serial);
                } else {
                    tracing::debug!(%serial, "adb reverse failed (USB debugging not allowed yet?)");
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("inkbridge-adb".into())
        .spawn(run)
    {
        tracing::warn!("--adb: could not start: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ready_devices_are_listed() {
        let listing = "* daemon started successfully\n\
                       List of devices attached\n\
                       HA1B2C3D\tdevice\n\
                       emulator-5554\toffline\n\
                       XYZ987\tunauthorized\n\
                       R5CT\tdevice usb:1-1 product:x model:TB331FC\n\n";
        assert_eq!(ready_devices(listing), ["HA1B2C3D", "R5CT"]);
        assert_eq!(ready_devices(""), Vec::<String>::new());
    }
}
