//! The tablet as an extra screen on Windows, through the Virtual Display
//! Driver (`MttVDD`, MIT licence, <https://github.com/VirtualDrivers/Virtual-Display-Driver>).
//!
//! Windows has no API for a virtual monitor; it takes an Indirect Display
//! driver. Installing it is an Advanced option of the host (one admin
//! prompt, `--install-virtual-display`). Everything here runs without
//! admin rights:
//!
//! - The driver reads its modes from `vdd_settings.xml` in the folder that
//!   `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH` names. The
//!   installer points it at a folder the user may write, so the host adds
//!   the tablet's shape there when it is missing.
//! - The driver always has one monitor (it treats a count of 0 as invalid).
//!   The host **attaches** it to the desktop with the tablet's mode while
//!   the tablet is an extra screen and **detaches** it otherwise, with the
//!   normal display settings API, as the Settings app does. No driver
//!   restart is involved.
//! - Only a new tablet shape needs the driver to reread its settings:
//!   `RELOAD_DRIVER` on its control pipe (`\\.\pipe\MTTVirtualDisplayPipe`,
//!   UTF-16 commands). This driver version's process restarts then (Windows
//!   logs a user-mode driver crash and starts it again), so the installer
//!   writes the common tablet shapes up front.
//!
//! Note: the driver's pipe accepts commands from any local program.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE,
    DISPLAY_DEVICEW, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{PIPE_READMODE_MESSAGE, SetNamedPipeHandleState};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, PCWSTR, w};

use crate::CaptureError;

/// The driver's control pipe.
const PIPE: &str = r"\\.\pipe\MTTVirtualDisplayPipe";
/// The adapter name the driver's INF gives its device.
const ADAPTER_NAME: &str = "Virtual Display Driver";
/// Refresh rate of the modes we add.
const REFRESH: u32 = 60;

fn failed(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(e.to_string())
}

/// Sends one command to the driver; returns its reply, if any. The driver
/// serves one client at a time and has no pipe for a moment while its
/// adapter restarts (after each count change), so this retries briefly.
fn command(cmd: &str) -> Result<String, CaptureError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match command_once(cmd) {
            Ok(reply) => return Ok(reply),
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn command_once(cmd: &str) -> Result<String, CaptureError> {
    let name = HSTRING::from(PIPE);
    // SAFETY: valid NUL-terminated path; the handle is closed below.
    let pipe = unsafe {
        CreateFileW(
            &name,
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| {
        CaptureError::Unavailable(format!(
            "the virtual display driver isn't running ({e}); install it with \
             `inkbridge-host --install-virtual-display`"
        ))
    })?;
    let result = exchange(pipe, cmd);
    // SAFETY: `pipe` was opened above and is closed once.
    let _ = unsafe { CloseHandle(pipe) };
    result
}

fn exchange(pipe: HANDLE, cmd: &str) -> Result<String, CaptureError> {
    let mode = PIPE_READMODE_MESSAGE;
    // SAFETY: valid pipe handle and a pointer to a local value.
    unsafe { SetNamedPipeHandleState(pipe, Some(&raw const mode), None, None) }.map_err(failed)?;
    let bytes: Vec<u8> = cmd.encode_utf16().flat_map(u16::to_le_bytes).collect();
    // SAFETY: valid handle and buffer.
    unsafe { WriteFile(pipe, Some(&bytes), None, None) }.map_err(failed)?;
    if !cmd.starts_with("PING") {
        // The other commands log to the pipe while the driver restarts;
        // nothing in it is needed.
        return Ok(String::new());
    }
    let mut reply = [0u8; 512];
    let mut read = 0u32;
    // SAFETY: valid handle, buffer and count.
    unsafe { ReadFile(pipe, Some(&mut reply), Some(&raw mut read), None) }.map_err(failed)?;
    Ok(String::from_utf8_lossy(&reply[..read as usize]).into_owned())
}

/// The folder holding the driver's settings, from the installer's
/// registry value.
fn settings_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 512];
    let mut size = u32::try_from(std::mem::size_of_val(&buf)).ok()?;
    // SAFETY: valid key/value names, buffer and size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\MikeTheTech\VirtualDisplayDriver"),
            w!("VDDPATH"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// The driver's settings file with `count` monitors and `modes`.
pub fn settings_xml(count: u32, modes: &[(u32, u32)], gpu: &str) -> String {
    use std::fmt::Write as _;
    let mut xml = String::from(
        "<?xml version='1.0' encoding='utf-8'?>\n<!-- Written by Inkbridge. -->\n<vdd_settings>\n",
    );
    let _ = writeln!(
        xml,
        "    <monitors>\n        <count>{count}</count>\n    </monitors>"
    );
    let _ = writeln!(
        xml,
        "    <gpu>\n        <friendlyname>{gpu}</friendlyname>\n    </gpu>"
    );
    let _ = writeln!(
        xml,
        "    <global>\n        <g_refresh_rate>{REFRESH}</g_refresh_rate>\n    </global>"
    );
    xml.push_str("    <resolutions>\n");
    for (w, h) in modes {
        let _ = writeln!(
            xml,
            "        <resolution>\n            <width>{w}</width>\n            <height>{h}</height>\n            <refresh_rate>{REFRESH}</refresh_rate>\n        </resolution>"
        );
    }
    xml.push_str(
        "    </resolutions>\n    <options>\n        <CustomEdid>false</CustomEdid>\n        \
         <PreventSpoof>false</PreventSpoof>\n        <EdidCeaOverride>false</EdidCeaOverride>\n        \
         <HardwareCursor>true</HardwareCursor>\n        <SDR10bit>false</SDR10bit>\n        \
         <HDRPlus>false</HDRPlus>\n        <logging>false</logging>\n        \
         <debuglogging>false</debuglogging>\n    </options>\n</vdd_settings>\n",
    );
    xml
}

/// Modes listed in a settings file (ours, or the driver's rewrite of it).
pub fn settings_modes(xml: &str) -> Vec<(u32, u32)> {
    let value = |block: &str, tag: &str| -> Option<u32> {
        let start = block.find(&format!("<{tag}>"))? + tag.len() + 2;
        let end = block[start..].find(&format!("</{tag}>"))? + start;
        block[start..end].trim().parse().ok()
    };
    xml.split("<resolution>")
        .skip(1)
        .filter_map(|block| Some((value(block, "width")?, value(block, "height")?)))
        .collect()
}

/// Modes a tablet commonly needs, written at install time; others are
/// added when a tablet asks for them.
pub const COMMON_MODES: &[(u32, u32)] = &[
    (1920, 1080),
    (1920, 1200),
    (1200, 1920),
    (1920, 1088),
    (1088, 1920),
    (2560, 1600),
    (1600, 2560),
    (2000, 1200),
    (1200, 2000),
    (2560, 1440),
];

/// Makes sure the driver offers `mode`. Returns whether the settings file
/// changed (the driver must restart to see it).
fn ensure_mode(mode: (u32, u32)) -> Result<bool, CaptureError> {
    let dir = settings_dir().ok_or_else(|| {
        CaptureError::Unavailable("the virtual display driver isn't set up by Inkbridge".into())
    })?;
    let path = dir.join("vdd_settings.xml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut modes = settings_modes(&text);
    if modes.contains(&mode) {
        return Ok(false);
    }
    if modes.is_empty() {
        modes.extend_from_slice(COMMON_MODES);
    }
    modes.push(mode);
    std::fs::write(&path, settings_xml(1, &modes, &render_gpu())).map_err(|e| {
        CaptureError::Failed(format!(
            "can't add the tablet's shape to {}: {e}",
            path.display()
        ))
    })?;
    Ok(true)
}

/// The graphics adapter that drives the main monitor, by name. The driver
/// renders on it, so capturing the virtual monitor stays on that GPU (on
/// a PC with two, "default" may pick one without a monitor).
pub fn render_gpu() -> String {
    for i in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `device` has its size set; enumeration stops at false.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut device, 0) }.as_bool() {
            break;
        }
        if (device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE).0 != 0 {
            return wide(&device.DeviceString);
        }
    }
    "default".into()
}

fn wide(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// The driver's monitor: its device name (`\\.\DISPLAYn`) and whether it is
/// part of the desktop.
fn find_monitor() -> Option<(String, bool)> {
    for i in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: u32::try_from(size_of::<DISPLAY_DEVICEW>()).ok()?,
            ..Default::default()
        };
        // SAFETY: `device` has its size set; index enumeration stops at false.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &raw mut device, 0) }.as_bool() {
            return None;
        }
        if wide(&device.DeviceString) == ADAPTER_NAME {
            let attached = (device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 != 0;
            return Some((wide(&device.DeviceName), attached));
        }
    }
    None
}

/// Width of the primary monitor, to place ours to its right.
fn primary_width() -> i32 {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: valid DEVMODEW with its size set; null name = primary.
    if unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) }
        .as_bool()
    {
        i32::try_from(mode.dmPelsWidth).unwrap_or(1920)
    } else {
        1920
    }
}

/// Gives monitor `device` the size `w × h` and puts it right of the primary.
fn set_mode(device: &str, (w, h): (u32, u32)) -> Result<(), CaptureError> {
    let name = HSTRING::from(device);
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0),
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION,
        dmPelsWidth: w,
        dmPelsHeight: h,
        ..Default::default()
    };
    mode.Anonymous1.Anonymous2.dmPosition.x = primary_width();
    mode.Anonymous1.Anonymous2.dmPosition.y = 0;
    // SAFETY: valid device name and DEVMODEW; stage, then apply all.
    let staged = unsafe {
        ChangeDisplaySettingsExW(
            &name,
            Some(&raw const mode),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        )
    };
    if staged != DISP_CHANGE_SUCCESSFUL {
        return Err(failed(format!(
            "Windows refused the mode {w}x{h} ({staged:?})"
        )));
    }
    // SAFETY: null device and mode apply the staged changes.
    let applied =
        unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE::default(), None) };
    if applied != DISP_CHANGE_SUCCESSFUL {
        return Err(failed(format!(
            "Windows didn't apply the display change ({applied:?})"
        )));
    }
    Ok(())
}

/// Takes monitor `device` off the desktop (it stays known to Windows, so
/// it can come back without restarting the driver).
fn detach(device: &str) -> Result<(), CaptureError> {
    let name = HSTRING::from(device);
    let mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0),
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION,
        ..Default::default()
    };
    // SAFETY: valid device name and DEVMODEW; stage, then apply all.
    let staged = unsafe {
        ChangeDisplaySettingsExW(
            &name,
            Some(&raw const mode),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        )
    };
    if staged != DISP_CHANGE_SUCCESSFUL {
        return Err(failed(format!(
            "Windows refused to detach {device} ({staged:?})"
        )));
    }
    // SAFETY: null device and mode apply the staged changes.
    let applied =
        unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE::default(), None) };
    if applied != DISP_CHANGE_SUCCESSFUL {
        return Err(failed(format!(
            "Windows didn't apply the display change ({applied:?})"
        )));
    }
    Ok(())
}

/// Takes the driver's monitor off the desktop if it is on it. For start-up
/// (after a crash or restart) and the installer: the monitor belongs on
/// the desktop only while a tablet is an extra screen.
pub fn detach_virtual_monitor() -> Result<(), CaptureError> {
    match find_monitor() {
        Some((device, true)) => detach(&device),
        _ => Ok(()),
    }
}

/// Whether the driver's monitor exists (on the desktop or not).
pub fn monitor_present() -> bool {
    find_monitor().is_some()
}

/// Windows' number for a device name (`\\.\DISPLAY3` → 3).
fn display_number(device: &str) -> Option<u32> {
    device.rsplit("DISPLAY").next()?.parse().ok()
}

/// One virtual monitor shaped like the tablet, for as long as it lives.
#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    id: u32,
    size: (u32, u32),
    picture: (u32, u32),
    sharp: bool,
}

impl WindowsVirtualDisplay {
    /// Whether the driver is installed and answering.
    pub fn supported() -> bool {
        // Its reply can be preceded by log messages, so reaching the pipe
        // is the test.
        settings_dir().is_some() && command_once("PING").is_ok()
    }

    /// Puts the driver's monitor on the desktop with `width × height`
    /// pixels. `picture` and `sharp` are kept for the caller (the mode is
    /// the same either way here).
    pub fn new(
        _name: &str,
        width: u32,
        height: u32,
        picture: (u32, u32),
        sharp: bool,
    ) -> Result<Self, CaptureError> {
        let id = show((width, height))?;
        Ok(Self {
            id,
            size: (width, height),
            picture,
            sharp,
        })
    }

    /// Gives the monitor another shape (the tablet turned).
    pub fn resize(
        &mut self,
        width: u32,
        height: u32,
        picture: (u32, u32),
    ) -> Result<(), CaptureError> {
        if self.size != (width, height) {
            self.id = show((width, height))?;
            self.size = (width, height);
        }
        self.picture = picture;
        Ok(())
    }

    pub fn sharp(&self) -> bool {
        self.sharp
    }

    /// The tablet drawing area this display was last shaped for.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Windows' number for the monitor, as capture and the pen use it.
    pub fn id(&self) -> u32 {
        self.id
    }
}

/// Attaches the driver's monitor with `mode` (adding the mode to the
/// driver's settings first if it is new); returns its display number.
fn show(mode: (u32, u32)) -> Result<u32, CaptureError> {
    if ensure_mode(mode)? {
        command("RELOAD_DRIVER")?;
    }
    let device = wait_for(|_| true)?;
    set_mode(&device, mode)?;
    let device = wait_for(|attached| attached)?;
    display_number(&device).ok_or_else(|| failed(format!("unexpected display name {device}")))
}

/// Waits until the driver's monitor exists and `ready(attached)` holds.
fn wait_for(ready: impl Fn(bool) -> bool) -> Result<String, CaptureError> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some((device, attached)) = find_monitor()
            && ready(attached)
        {
            return Ok(device);
        }
        if Instant::now() > deadline {
            return Err(failed("the virtual monitor didn't appear"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for WindowsVirtualDisplay {
    fn drop(&mut self) {
        // Windows moves the windows on it to the other monitors.
        let _ = detach_virtual_monitor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_their_modes() {
        let xml = settings_xml(1, &[(1920, 1088), (2560, 1600)], "NVIDIA GeForce RTX 5060");
        assert!(xml.contains("<count>1</count>"));
        assert!(xml.contains("<friendlyname>NVIDIA GeForce RTX 5060</friendlyname>"));
        assert_eq!(settings_modes(&xml), [(1920, 1088), (2560, 1600)]);
        assert_eq!(settings_modes(""), Vec::<(u32, u32)>::new());
    }

    #[test]
    fn display_numbers_come_from_device_names() {
        assert_eq!(display_number(r"\\.\DISPLAY12"), Some(12));
        assert_eq!(display_number("nonsense"), None);
    }

    /// Needs the driver installed (`--install-virtual-display`).
    #[test]
    #[ignore = "needs the virtual display driver"]
    fn a_virtual_monitor_appears_and_goes_away() {
        assert!(WindowsVirtualDisplay::supported());
        let display = WindowsVirtualDisplay::new("test", 1920, 1088, (1920, 1088), true).unwrap();
        let listed = crate::windows_displays().unwrap();
        let ours = listed
            .iter()
            .find(|d| d.id == u64::from(display.id()))
            .unwrap();
        assert_eq!((ours.bounds.width, ours.bounds.height), (1920, 1088));
        drop(display);
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            find_monitor().is_none_or(|(_, attached)| !attached)
        });
        assert!(gone, "the virtual monitor stayed");
    }
}
