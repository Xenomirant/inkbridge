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
    Region {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }
}
