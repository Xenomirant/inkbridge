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
    #[cfg(windows)]
    if let Some(id) = chosen_display() {
        let mut sink = ib_inject::platform_sink(desktop)?;
        let bounds = displays()
            .into_iter()
            .find(|d| d.id == id)
            .map_or_else(primary_display, |d| d.bounds);
        sink.set_display(id, bounds);
        return Ok(sink);
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
            cfg!(any(target_os = "macos", windows)),
            "choosing a display works on macOS and Windows only so far"
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
        let extra = extra_display_id();
        let mut all = ib_capture::mac_displays();
        for d in &mut all {
            if Some(d.id) == extra {
                EXTRA_NAME.clone_into(&mut d.name);
            }
        }
        all
    }
    #[cfg(windows)]
    match ib_capture::windows_displays() {
        Ok(mut all) if !all.is_empty() => {
            let extra = extra_display_id();
            for d in &mut all {
                if Some(d.id) == extra {
                    EXTRA_NAME.clone_into(&mut d.name);
                }
            }
            return all;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("could not list the displays: {e}"),
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

/// Name of the tablet's own display in the display list.
pub const EXTRA_NAME: &str = "This tablet (extra screen)";

/// How long the extra display outlives a session, so a tablet that
/// reconnects finds its windows where they were.
const EXTRA_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// The OS's virtual display: `CGVirtualDisplay` on macOS, the Virtual
/// Display Driver on Windows (an Advanced option).
#[cfg(target_os = "macos")]
type VirtualDisplay = ib_capture::MacVirtualDisplay;
#[cfg(windows)]
type VirtualDisplay = ib_capture::WindowsVirtualDisplay;

/// The extra display the tablet is, while it is one.
#[cfg(any(target_os = "macos", windows))]
static EXTRA: std::sync::Mutex<Option<(VirtualDisplay, ExtraScreen)>> = std::sync::Mutex::new(None);
/// Whether new sessions should make the tablet an extra display.
static EXTEND_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether that display should be the pixel-for-pixel kind.
static SHARP_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Counts sessions, so an old session's clean-up can't undo a newer one.
static SESSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn extra_display_id() -> Option<u64> {
    #[cfg(any(target_os = "macos", windows))]
    {
        EXTRA
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(d, _)| u64::from(d.id()))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// What the extra display is made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtraScreen {
    /// The tablet's drawing area in its pixels.
    pub viewport: (u32, u32),
    /// The part of it the display covers: all of it, or (`sharp`) as much
    /// as the tablet can show pixel for pixel.
    pub area: (u32, u32),
    /// The size of the picture the tablet is sent.
    pub picture: (u32, u32),
    /// Pixel for pixel on the tablet, with bars where the area is smaller
    /// than the drawing area; otherwise the picture is stretched to fill.
    pub sharp: bool,
}

/// Whether this computer can make the tablet an extra display.
pub fn extend_supported() -> bool {
    #[cfg(any(target_os = "macos", windows))]
    {
        VirtualDisplay::supported()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

/// Whether the tablet is an extra display right now.
pub fn extending() -> bool {
    extra_display_id().is_some()
}

/// Whether new sessions start with the tablet as an extra display.
pub fn extend_wanted() -> bool {
    EXTEND_WANTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `--extend`, or the tablet's choice in its display menu.
pub fn set_extend_wanted(wanted: bool) {
    EXTEND_WANTED.store(wanted, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the extra display for new sessions is the pixel-for-pixel kind.
pub fn sharp_wanted() -> bool {
    SHARP_WANTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `--extend sharp`, or the tablet's choice in its display menu.
pub fn set_sharp_wanted(wanted: bool) {
    SHARP_WANTED.store(wanted, std::sync::atomic::Ordering::Relaxed);
}

/// What the extra display was made for, while there is one.
pub fn extra_screen() -> Option<ExtraScreen> {
    #[cfg(any(target_os = "macos", windows))]
    {
        EXTRA
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(_, made_for)| *made_for)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// Makes the tablet an extra display as `wanted` describes, or changes
/// the existing one to that (the tablet was rotated, or the other kind
/// was chosen). Returns its position in [`displays`]; the caller switches
/// to it.
pub fn extend(wanted: ExtraScreen) -> anyhow::Result<usize> {
    #[cfg(any(target_os = "macos", windows))]
    {
        let (width, height) = wanted.area;
        anyhow::ensure!(width > 0 && height > 0, "the tablet's size isn't known yet");
        // The two kinds are different displays to the system.
        if extra_screen().is_some_and(|e| e.sharp != wanted.sharp) {
            stop_extending();
        }
        let id = {
            let mut extra = EXTRA
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((display, made_for)) = extra.as_mut() {
                display.resize(width, height, wanted.picture)?;
                *made_for = wanted;
            } else {
                let display = VirtualDisplay::new(
                    "Inkbridge tablet",
                    width,
                    height,
                    wanted.picture,
                    wanted.sharp,
                )?;
                *extra = Some((display, wanted));
            }
            extra.as_ref().map(|(d, _)| u64::from(d.id()))
        };
        // The system adds the display, and changes its shape, a moment later.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let found = displays().into_iter().position(|d| {
                Some(d.id) == id && (d.bounds.width >= d.bounds.height) == (width >= height)
            });
            if let Some(index) = found {
                return Ok(index);
            }
            if std::time::Instant::now() > deadline {
                stop_extending();
                anyhow::bail!("the extra display didn't appear");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = wanted;
        anyhow::bail!("using the tablet as an extra screen works on macOS and Windows only so far")
    }
}

/// Removes the extra display. Windows on it move to the other displays.
pub fn stop_extending() {
    #[cfg(any(target_os = "macos", windows))]
    {
        let gone = EXTRA
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((display, _)) = gone {
            let mut chosen = DISPLAY
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *chosen == Some(u64::from(display.id())) {
                *chosen = None;
            }
        }
    }
}

/// A session begins. Returns the main display if the display that was
/// being shown is gone (unplugged, or the extra display was removed), so
/// the caller shows that one instead.
pub fn session_started() -> Option<ib_capture::Display> {
    SESSIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut chosen = DISPLAY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = (*chosen)?;
    let all = displays();
    if all.iter().any(|d| d.id == id) {
        return None;
    }
    *chosen = None;
    all.into_iter().next()
}

/// A session ended: the extra display stays for a short while, then goes
/// unless a session started in between.
pub fn session_ended() {
    if !extending() {
        return;
    }
    let session = SESSIONS.load(std::sync::atomic::Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("inkbridge-extra-display".into())
        .spawn(move || {
            std::thread::sleep(EXTRA_GRACE);
            if SESSIONS.load(std::sync::atomic::Ordering::SeqCst) == session {
                tracing::info!("no tablet came back, removing the extra display");
                stop_extending();
            }
        });
    if spawned.is_err() {
        stop_extending();
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

/// Windows screen capture on the chosen display: GPU frames for the
/// hardware encoder, CPU frames for the software one.
#[cfg(windows)]
pub fn windows_source(gpu: bool) -> ib_capture::WindowsScreenSource {
    let source = if gpu {
        ib_capture::WindowsScreenSource::new_gpu()
    } else {
        ib_capture::WindowsScreenSource::new()
    };
    match chosen_display() {
        Some(id) => source.on_display(id),
        None => source,
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
