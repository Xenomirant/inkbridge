//! Small OS integrations for the host process.

use ib_geometry::Region;

/// Makes desktop coordinates physical pixels on every monitor. Must run
/// before any window or input API is used.
pub fn init_process() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // SAFETY: plain FFI call; failure (already set) is harmless.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!("DPI awareness already set: {e}");
        }
    }
}

/// Advanced option (one admin approval): with USB tethering, Windows gives
/// the tablet's network the same priority as Ethernet/Wi-Fi and starts
/// sending part of the PC's internet traffic through the tablet. `fix`
/// raises the interface metric of tethering adapters so internet traffic
/// stays on the normal network (the direct link to the tablet keeps
/// working); `!fix` restores automatic metrics. Persists per adapter.
pub fn set_usb_routing(fix: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let filter = "Get-NetAdapter | Where-Object { $_.InterfaceDescription -match 'Remote NDIS|RNDIS|NCM' }";
        let action = if fix {
            "Set-NetIPInterface -InterfaceMetric 500 -AddressFamily IPv4"
        } else {
            "Set-NetIPInterface -AutomaticMetric Enabled -AddressFamily IPv4"
        };
        let script = format!("{filter} | ForEach-Object {{ $_ | {action} }}");
        // Elevated child PowerShell: Windows shows its admin prompt.
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-Command','{}'",
                script.replace('\'', "''")
            ))
            .status()?;
        anyhow::ensure!(
            status.success(),
            "the admin prompt was declined or PowerShell failed"
        );
        if crate::usb::tether_links().is_empty() {
            println!(
                "Note: no tablet is tethered right now; plug it in with USB tethering on and run this again."
            );
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = fix;
        anyhow::bail!("only needed on Windows")
    }
}

/// The udev rule behind `--setup-uinput`: gives the logged-in desktop user
/// (systemd-logind's `uaccess`, as for webcams) access to `/dev/uinput`.
pub const UINPUT_RULE_PATH: &str = "/etc/udev/rules.d/60-inkbridge-uinput.rules";
pub const UINPUT_RULE: &str = "# Inkbridge: let the logged-in user create a virtual pen (uinput).\n\
KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"\n";
pub const UINPUT_MODULE_PATH: &str = "/etc/modules-load.d/inkbridge-uinput.conf";

/// Advanced option (Linux, one admin prompt): installs or removes the uinput
/// access rule. Uses `pkexec` (the desktop's own admin prompt) when present.
pub fn set_uinput_access(enable: bool) -> anyhow::Result<()> {
    let script = if enable {
        format!(
            "printf '%s' '{UINPUT_RULE}' > {UINPUT_RULE_PATH} && echo uinput > {UINPUT_MODULE_PATH} && \
             modprobe uinput && udevadm control --reload-rules && udevadm trigger --name-match=uinput"
        )
    } else {
        format!("rm -f {UINPUT_RULE_PATH} {UINPUT_MODULE_PATH} && udevadm control --reload-rules")
    };
    let has_pkexec = std::process::Command::new("pkexec")
        .arg("--version")
        .output()
        .is_ok();
    if !has_pkexec {
        anyhow::bail!(
            "no graphical admin prompt (pkexec) found. Run instead:\n  sudo sh -c \"{script}\""
        );
    }
    let status = std::process::Command::new("pkexec")
        .args(["sh", "-c", &script])
        .status()?;
    anyhow::ensure!(
        status.success(),
        "the admin prompt was declined or the commands failed"
    );
    Ok(())
}

/// Gives the pen thread priority over normal work so injection isn't
/// delayed by background load.
pub fn raise_thread_priority() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        // SAFETY: GetCurrentThread returns a pseudo-handle valid for this call.
        if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
            tracing::warn!("could not raise pen thread priority: {e}");
        }
    }
}

/// Bounds of the primary display in physical pixels.
pub fn primary_display() -> Region {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        // SAFETY: plain FFI calls without pointers.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if let (Ok(width), Ok(height)) = (u32::try_from(w), u32::try_from(h))
            && width > 0
            && height > 0
        {
            return Region {
                x: 0,
                y: 0,
                width,
                height,
            };
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(d) = ib_capture::platform_source().and_then(|s| s.displays())
            && let Some(primary) = d.into_iter().find(|d| d.primary)
        {
            return primary.bounds;
        }
    }
    Region {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }
}
