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
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain FFI call that only changes the calling thread's QoS class.
        let rc = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if rc != 0 {
            tracing::warn!("could not raise pen thread priority: error {rc}");
        }
    }
}

/// The screen capture backend for a new tablet session: on Wayland the
/// portal stream granted at startup, otherwise the OS default.
pub fn capture_source() -> Result<Box<dyn ib_capture::ScreenSource>, ib_capture::CaptureError> {
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    if let Some(d) = crate::wayland::desktop() {
        return Ok(d.capture());
    }
    ib_capture::platform_source()
}

/// Whether sessions can stream the screen. On macOS this doesn't touch the
/// capture backend, so the Screen Recording prompt appears when a tablet
/// first connects rather than when the host starts.
pub fn video_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        capture_source().is_ok()
    }
}

/// Tells the user, before macOS does, which permissions it will ask for
/// and why. Prints nothing once they are granted.
pub fn explain_permissions() {
    #[cfg(target_os = "macos")]
    {
        let pen = !ib_inject::pen_access_granted();
        let screen = !ib_capture::screen_access_granted();
        if pen || screen {
            println!("macOS will ask you to allow the following for the app that runs this host:");
        }
        if screen {
            println!(
                "  - Screen Recording, when your tablet connects: to show this screen on the \
                 tablet. No audio is captured and nothing is stored."
            );
        }
        if pen {
            println!(
                "  - Accessibility, when you first use the pen: to move the pointer and draw."
            );
        }
        if pen || screen {
            println!("Inkbridge sends the picture only to your paired tablet.\n");
        }
    }
}

/// The pen backend. Linux: the uinput virtual tablet (pressure; Advanced
/// option), else on Wayland the portal pointer (no setup, no pressure).
pub fn pen_sink(desktop: Region) -> Result<Box<dyn ib_inject::PenSink>, ib_inject::InjectError> {
    #[cfg(target_os = "macos")]
    if let Some(id) = chosen_display().and_then(|id| u32::try_from(id).ok()) {
        return Ok(Box::new(ib_inject::MacPenSink::on_display(id)));
    }
    let sink = ib_inject::platform_sink(desktop);
    #[cfg(all(target_os = "linux", feature = "wayland"))]
    if sink.is_err()
        && let Some(s) =
            crate::wayland::desktop().and_then(crate::wayland::WaylandDesktop::pointer_sink)
    {
        tracing::info!("pen works as a mouse (no pressure); run --setup-uinput for pressure");
        return Ok(s);
    }
    sink
}

/// The display being shown (its capture id), if it isn't the main one.
/// Set by `--display` and by the tablet's display menu.
static DISPLAY: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

fn chosen_display() -> Option<u64> {
    *DISPLAY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Position in [`displays`] of the display being shown.
pub fn current_display() -> usize {
    let chosen = chosen_display();
    displays()
        .iter()
        .position(|d| Some(d.id) == chosen)
        .unwrap_or(0)
}

/// Makes the display at `index` of [`displays`] the one new captures and
/// the pen use. The caller restarts capture and tells the pen thread.
pub fn switch_display(index: usize) -> anyhow::Result<ib_capture::Display> {
    let all = displays();
    let Some(display) = all.get(index) else {
        anyhow::bail!(
            "there is no display {}. Connected displays:\n{}",
            index + 1,
            describe_displays()
        );
    };
    if !display.primary {
        anyhow::ensure!(
            cfg!(target_os = "macos"),
            "choosing a display works on macOS only so far"
        );
    }
    *DISPLAY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        (!display.primary).then_some(display.id);
    Ok(display.clone())
}

/// The displays the host could show, main display first.
pub fn displays() -> Vec<ib_capture::Display> {
    #[cfg(target_os = "macos")]
    {
        ib_capture::mac_displays()
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![ib_capture::Display {
            id: 0,
            name: "Main display".into(),
            bounds: primary_display(),
            primary: true,
        }]
    }
}

/// One line per display for `--list-displays`, numbered as `--display` counts.
pub fn describe_displays() -> String {
    let lines: Vec<String> = displays()
        .iter()
        .enumerate()
        .map(|(i, d)| {
            format!(
                "{}: {}, {}x{}{}",
                i + 1,
                d.name,
                d.bounds.width,
                d.bounds.height,
                if d.primary { " (main)" } else { "" }
            )
        })
        .collect();
    lines.join("\n")
}

/// `--display N`: display `number` (1-based, as listed) is shown and drawn
/// on. Returns its size.
pub fn select_display(number: usize) -> anyhow::Result<Region> {
    let Some(index) = number.checked_sub(1) else {
        anyhow::bail!("displays are numbered from 1:\n{}", describe_displays());
    };
    Ok(switch_display(index)?.bounds)
}

/// macOS screen capture on the chosen display.
#[cfg(target_os = "macos")]
pub fn mac_source(gpu: bool) -> Result<ib_capture::MacScreenSource, ib_capture::CaptureError> {
    let source = if gpu {
        ib_capture::MacScreenSource::new_gpu()?
    } else {
        ib_capture::MacScreenSource::new()?
    };
    match chosen_display() {
        Some(id) => source.on_display(id),
        None => Ok(source),
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
        if let Ok(d) = capture_source().and_then(|s| s.displays())
            && let Some(primary) = d.into_iter().find(|d| d.primary)
        {
            return primary.bounds;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (region, _) = ib_inject::main_display();
        if region.width > 0 && region.height > 0 {
            return region;
        }
    }
    Region {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }
}

/// The computer's name as shown on the tablet.
pub fn host_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        && let Ok(name) = String::from_utf8(out.stdout)
        && !name.trim().is_empty()
    {
        return name.trim().to_owned();
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Inkbridge host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_number_that_is_not_listed_is_refused() {
        assert!(select_display(0).is_err());
        let e = select_display(99).unwrap_err().to_string();
        assert!(e.contains("there is no display 99"), "{e}");
    }

    #[test]
    fn displays_are_numbered_from_one_with_the_main_one_first() {
        let all = displays();
        if let Some(first) = all.first() {
            assert!(first.primary);
            assert!(describe_displays().starts_with("1: "));
        }
    }
}
