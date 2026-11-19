//! Creates and removes a root-enumerated device with Windows' SetupAPI (what
//! Microsoft's `devcon install` / `devcon remove` do). Needs admin rights.

use std::path::Path;

use anyhow::Context;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIF_REMOVE, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    INSTALLFLAG_FORCE, SETUP_DI_GET_CLASS_DEVS_FLAGS, SP_DEVINFO_DATA, SPDRP_HARDWAREID,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiSetDeviceRegistryPropertyW,
    UpdateDriverForPlugAndPlayDevicesW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

/// The display device class (a constant, so its address is stable).
static DISPLAY_CLASS: windows::core::GUID = GUID_DEVCLASS_DISPLAY;

/// Closes a device information set when dropped.
struct DeviceSet(HDEVINFO);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: the set was created by SetupAPI and is destroyed once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn info_data() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).unwrap_or(0),
        ..Default::default()
    }
}

/// A REG_MULTI_SZ with one string, as bytes.
fn multi_sz(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0, 0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// The strings of a REG_MULTI_SZ.
fn parse_multi_sz(bytes: &[u8]) -> Vec<String> {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    units
        .split(|&u| u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// Display-class devices whose hardware ids include `hardware_id`.
fn existing(hardware_id: &str) -> anyhow::Result<(DeviceSet, Vec<SP_DEVINFO_DATA>)> {
    // SAFETY: valid class GUID; no enumerator or window.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&raw const DISPLAY_CLASS),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )
    }
    .context("listing display devices")?;
    let set = DeviceSet(set);
    let mut found = Vec::new();
    for index in 0.. {
        let mut data = info_data();
        // SAFETY: valid set; `data` has its size set. Fails past the end.
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &raw mut data) }.is_err() {
            break;
        }
        let mut buf = vec![0u8; 1024];
        // SAFETY: valid set and element; the buffer is writable.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &raw const data,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buf),
                None,
            )
        };
        if ok.is_ok()
            && parse_multi_sz(&buf)
                .iter()
                .any(|id| id.eq_ignore_ascii_case(hardware_id))
        {
            found.push(data);
        }
    }
    Ok((set, found))
}

/// Creates a device with `hardware_id` (unless one exists) and installs the
/// driver in `inf` on it.
pub fn install(hardware_id: &str, inf: &Path) -> anyhow::Result<()> {
    let (_, present) = existing(hardware_id)?;
    if present.is_empty() {
        // SAFETY: valid class GUID.
        let set = DeviceSet(
            unsafe { SetupDiCreateDeviceInfoList(Some(&raw const DISPLAY_CLASS), None) }
                .context("creating a device list")?,
        );
        let mut data = info_data();
        // SAFETY: valid set, class name and GUID; `data` has its size set.
        unsafe {
            SetupDiCreateDeviceInfoW(
                set.0,
                w!("Display"),
                &raw const DISPLAY_CLASS,
                PCWSTR::null(),
                None,
                DICD_GENERATE_ID,
                Some(&raw mut data),
            )
        }
        .context("creating the device")?;
        let ids = multi_sz(hardware_id);
        // SAFETY: valid set and element; the buffer is a REG_MULTI_SZ.
        unsafe {
            SetupDiSetDeviceRegistryPropertyW(set.0, &raw mut data, SPDRP_HARDWAREID, Some(&ids))
        }
        .context("setting the hardware id")?;
        // SAFETY: valid set and element.
        unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&raw const data)) }
            .context("registering the device")?;
    }
    let mut reboot = BOOL(0);
    // SAFETY: valid strings; `reboot` is writable.
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            &HSTRING::from(hardware_id),
            &HSTRING::from(inf.as_os_str()),
            INSTALLFLAG_FORCE,
            Some(&raw mut reboot),
        )
    }
    .context("installing the driver")?;
    if reboot.as_bool() {
        println!("Windows asks for a restart before the driver works.");
    }
    Ok(())
}

/// Removes every device with `hardware_id`.
pub fn remove(hardware_id: &str) -> anyhow::Result<()> {
    let (set, found) = existing(hardware_id)?;
    for data in found {
        // SAFETY: the element belongs to `set`, which is still open.
        unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, Some(&raw const data)) }
            .context("removing the device")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_strings_round_trip() {
        let bytes = multi_sz(r"Root\MttVDD");
        assert_eq!(parse_multi_sz(&bytes), [r"Root\MttVDD"]);
        assert_eq!(parse_multi_sz(&[]), Vec::<String>::new());
    }
}
