//! Advanced option (Windows, one admin prompt): installs or removes the
//! Virtual Display Driver that lets the tablet be an extra screen.
//!
//! The driver is a third-party, MIT-licensed Indirect Display driver
//! (<https://github.com/VirtualDrivers/Virtual-Display-Driver>), signed by
//! SignPath Foundation. The installer:
//! 1. downloads one pinned release and checks its SHA-256;
//! 2. checks the driver catalog's signature and trusts its signer for
//!    drivers (`TrustedPublisher`), so Windows installs it without asking;
//! 3. writes the driver's settings with **no monitor turned on** into a
//!    folder the user may write (the host adds tablet shapes later), and
//!    points the driver at it;
//! 4. creates the driver's device and installs the driver on it.
//!
//! The host then turns a monitor on only while the tablet is an extra
//! screen. `--remove-virtual-display` undoes all four steps.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};

/// The pinned driver release (x64).
const PACKAGE_URL: &str = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip";
const PACKAGE_SHA256: &str = "e24210692b442b39af763536330ce78b423f19342b7a7792c26de3944e418b3a";
/// Who must have signed the driver catalog.
const SIGNER: &str = "SignPath Foundation";
/// The driver's hardware id (from its INF).
const HARDWARE_ID: &str = r"Root\MttVDD";
const REGISTRY_KEY: &str = r"HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver";

/// Where Inkbridge keeps the driver package and its settings.
fn home() -> PathBuf {
    let data = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    data.join("Inkbridge").join("VirtualDisplay")
}

/// Runs a program, failing with its output if it fails.
fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("couldn't run {program}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(out.status.success(), "{program} failed: {}", text.trim());
    Ok(text)
}

fn powershell(script: &str) -> anyhow::Result<String> {
    run(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|f| f.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

/// Whether this process runs with admin rights.
fn elevated() -> bool {
    // `net session` needs admin rights and changes nothing.
    Command::new("net")
        .arg("session")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs this program again with admin rights (Windows asks the user) and
/// the same argument; returns its exit code.
fn run_elevated(arg: &str) -> anyhow::Result<i32> {
    let exe = std::env::current_exe()?;
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList '{arg}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        exe.display().to_string().replace('\'', "''")
    );
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()?;
    Ok(status.code().unwrap_or(1))
}

/// `--install-virtual-display`.
pub fn install() -> anyhow::Result<()> {
    if !elevated() {
        let code = run_elevated("--install-virtual-display")?;
        ensure!(
            code == 0,
            "the installation didn't finish (the admin prompt was declined, or see {})",
            home().join("install.log").display()
        );
        return Ok(());
    }
    let result = install_elevated();
    let _ = std::fs::write(home().join("install.log"), format!("{result:?}\n"));
    result
}

fn install_elevated() -> anyhow::Result<()> {
    let dir = home();
    std::fs::create_dir_all(&dir)?;
    // 1. The pinned package.
    let zip = dir.join("driver.zip");
    println!("Downloading the Virtual Display Driver…");
    run(
        "curl.exe",
        &["-fsSL", "-o", &zip.display().to_string(), PACKAGE_URL],
    )?;
    let hash = sha256_hex(&std::fs::read(&zip)?);
    ensure!(
        hash == PACKAGE_SHA256,
        "the download doesn't match the expected driver (SHA-256 {hash})"
    );
    let pkg = dir.join("package");
    let _ = std::fs::remove_dir_all(&pkg);
    std::fs::create_dir_all(&pkg)?;
    run(
        "tar.exe",
        &[
            "-xf",
            &zip.display().to_string(),
            "-C",
            &pkg.display().to_string(),
        ],
    )?;
    let inf = find_file(&pkg, "MttVDD.inf").context("MttVDD.inf missing from the package")?;
    let cat = find_file(&pkg, "mttvdd.cat").context("mttvdd.cat missing from the package")?;

    // 2. Check the signature, then trust its signer for drivers.
    let cat_ps = cat.display().to_string().replace('\'', "''");
    let signer = powershell(&format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{cat_ps}'; \
         if ($s.Status -ne 'Valid') {{ throw \"signature: $($s.Status)\" }}; \
         $s.SignerCertificate.Subject"
    ))?;
    ensure!(
        signer.contains(SIGNER),
        "the driver is signed by someone else: {}",
        signer.trim()
    );
    powershell(&format!(
        "$c = (Get-AuthenticodeSignature -LiteralPath '{cat_ps}').SignerCertificate; \
         $st = New-Object System.Security.Cryptography.X509Certificates.X509Store('TrustedPublisher','LocalMachine'); \
         $st.Open('ReadWrite'); $st.Add($c); $st.Close()"
    ))?;

    // 3. Settings: no monitor until a tablet asks for one.
    let modes = ib_capture::COMMON_MODES.to_vec();
    std::fs::write(
        dir.join("vdd_settings.xml"),
        ib_capture::vdd_settings_xml(0, &modes),
    )?;
    let dir_s = dir.display().to_string();
    // The driver (Local Service) and the host (the user) update the settings.
    run(
        "icacls.exe",
        &[
            &dir_s,
            "/grant",
            "*S-1-5-19:(OI)(CI)M",
            "/grant",
            "*S-1-5-32-545:(OI)(CI)M",
        ],
    )?;
    run(
        "reg.exe",
        &[
            "add",
            REGISTRY_KEY,
            "/v",
            "VDDPATH",
            "/t",
            "REG_SZ",
            "/d",
            &dir_s,
            "/f",
        ],
    )?;

    // 4. The device and its driver.
    println!("Installing the driver…");
    crate::vdd_device::install(HARDWARE_ID, &inf)?;
    println!("Done. The tablet's Screen menu now offers \"extra screen\".");
    Ok(())
}

/// `--remove-virtual-display`.
pub fn remove() -> anyhow::Result<()> {
    if !elevated() {
        let code = run_elevated("--remove-virtual-display")?;
        ensure!(
            code == 0,
            "removal didn't finish (the admin prompt was declined?)"
        );
        return Ok(());
    }
    let dir = home();
    // The signer to forget, read before the package goes.
    let thumbprint = find_file(&dir, "mttvdd.cat").and_then(|cat| {
        powershell(&format!(
            "(Get-AuthenticodeSignature -LiteralPath '{}').SignerCertificate.Thumbprint",
            cat.display().to_string().replace('\'', "''")
        ))
        .ok()
    });
    crate::vdd_device::remove(HARDWARE_ID)?;
    // The driver package in Windows' store.
    let drivers = run("pnputil.exe", &["/enum-drivers"]).unwrap_or_default();
    for published in published_names(&drivers, "mttvdd.inf") {
        let _ = run(
            "pnputil.exe",
            &["/delete-driver", &published, "/uninstall", "/force"],
        );
    }
    if let Some(t) = thumbprint
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
    {
        let _ = powershell(&format!(
            "$st = New-Object System.Security.Cryptography.X509Certificates.X509Store('TrustedPublisher','LocalMachine'); \
             $st.Open('ReadWrite'); $st.Certificates | Where-Object Thumbprint -eq '{t}' | ForEach-Object {{ $st.Remove($_) }}; $st.Close()"
        ));
    }
    let _ = run("reg.exe", &["delete", REGISTRY_KEY, "/f"]);
    let _ = std::fs::remove_dir_all(&dir);
    println!("Removed the virtual display driver.");
    Ok(())
}

/// Published names (`oem12.inf`) of driver packages whose original INF is
/// `original`, from `pnputil /enum-drivers` output.
fn published_names(listing: &str, original: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut published = None;
    for line in listing.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("Published Name") {
            published = Some(value.to_owned());
        } else if key.eq_ignore_ascii_case("Original Name")
            && value.eq_ignore_ascii_case(original)
            && let Some(p) = published.take()
        {
            out.push(p);
        }
    }
    out
}

/// Fails unless this is a 64-bit x86 Windows (the pinned package's).
pub fn check_platform() -> anyhow::Result<()> {
    if cfg!(target_arch = "x86_64") {
        Ok(())
    } else {
        bail!("the virtual display driver is set up on x64 Windows only so far")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_driver_packages_are_found_in_pnputil_output() {
        let listing = "Microsoft PnP Utility\n\n\
            Published Name:     oem3.inf\nOriginal Name:      nvlddmkm.inf\nProvider Name: NVIDIA\n\n\
            Published Name:     oem41.inf\nOriginal Name:      mttvdd.inf\nProvider Name: MikeTheTech\n";
        assert_eq!(published_names(listing, "MttVDD.inf"), ["oem41.inf"]);
        assert_eq!(published_names("", "mttvdd.inf"), Vec::<String>::new());
    }

    #[test]
    fn hashes_are_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
