//! Linux Wayland: pen, touch and keyboard follow the display the tablet
//! shows. On the main display they go to the main backend (the uinput
//! virtual devices, or the portal session of the main screen). On the
//! extra screen (a virtual monitor with its own portal session):
//! - with uinput, everything stays on the uinput devices and GNOME's
//!   tablet and touchscreen mapping is pointed at the virtual monitor, so
//!   the pen keeps its pressure;
//! - without, the pen, touch and keys go to that portal session. Its
//!   absolute positions are refused by xdg-desktop-portal up to at least
//!   1.21 for virtual streams (they have no size; flatpak/xdg-desktop-portal
//!   #2077, fix in #2078), so only taps and keys work there until then.

use ib_geometry::{DesktopPoint, Region};
use ib_inject::{InjectError, PenSink, TouchPoint};
use ib_protocol::{Key, Modifiers, PenFeatures, PenSample};

pub struct DisplaySink {
    main: Box<dyn PenSink>,
    /// The extra screen's portal backend, while it is shown.
    extra: Option<Box<dyn PenSink>>,
    /// The main backend is the uinput pen (pressure, mapped by the desktop).
    uinput: bool,
}

impl std::fmt::Debug for DisplaySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplaySink")
            .field("main", &self.main.name())
            .field("extra", &self.extra.is_some())
            .finish_non_exhaustive()
    }
}

impl DisplaySink {
    pub fn new(main: Box<dyn PenSink>) -> Self {
        let uinput = main.name() == "linux-uinput";
        Self {
            main,
            extra: None,
            uinput,
        }
    }

    /// Where finger gestures, scrolling and keys go.
    fn others(&mut self) -> &mut Box<dyn PenSink> {
        if self.uinput {
            &mut self.main
        } else {
            self.extra.as_mut().unwrap_or(&mut self.main)
        }
    }

    /// Where the pen goes.
    fn pen(&mut self) -> &mut Box<dyn PenSink> {
        if self.uinput {
            &mut self.main
        } else {
            self.extra.as_mut().unwrap_or(&mut self.main)
        }
    }
}

impl PenSink for DisplaySink {
    fn name(&self) -> &'static str {
        self.main.name()
    }

    fn features(&self) -> PenFeatures {
        self.main.features()
    }

    fn emit(&mut self, sample: &PenSample, at: DesktopPoint) -> Result<(), InjectError> {
        self.pen().emit(sample, at)
    }

    fn touch(&mut self, frame: &[TouchPoint]) -> Result<(), InjectError> {
        self.others().touch(frame)
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        self.others().scroll(dx_px, dy_px)
    }

    fn scroll_end(&mut self) -> Result<(), InjectError> {
        self.others().scroll_end()
    }

    fn set_display(&mut self, id: u64, bounds: Region) {
        if let Some(mut old) = self.extra.take() {
            let _ = old.release();
        }
        if crate::platform::is_extra_display(id) {
            self.extra = crate::platform::extra_input_sink();
            tracing::info!(
                extra_session = self.extra.is_some(),
                pressure_pen = self.uinput,
                "input follows the extra screen"
            );
            if self.uinput {
                gnome::map(gnome::Device::Pen, gnome::Target::Virtual);
                gnome::map(gnome::Device::Touch, gnome::Target::Virtual);
            }
        } else if self.uinput {
            // With the extra screen there, "automatic" could pick either.
            gnome::map(gnome::Device::Pen, gnome::Target::Primary);
            gnome::map(gnome::Device::Touch, gnome::Target::Primary);
        }
        self.main.set_display(id, bounds);
    }

    fn keyboard(&self) -> bool {
        self.main.keyboard()
    }

    fn type_text(&mut self, text: &str) -> Result<(), InjectError> {
        self.others().type_text(text)
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) -> Result<(), InjectError> {
        self.others().key(key, modifiers)
    }

    fn move_pointer(&mut self, at: DesktopPoint) -> Result<(), InjectError> {
        self.pen().move_pointer(at)
    }

    fn release(&mut self) -> Result<(), InjectError> {
        if let Some(extra) = self.extra.as_mut() {
            let _ = extra.release();
        }
        self.main.release()
    }
}

impl Drop for DisplaySink {
    fn drop(&mut self) {
        if self.uinput {
            gnome::reset(gnome::Device::Pen);
            gnome::reset(gnome::Device::Touch);
        }
    }
}

/// GNOME's per-device mapping of pen tablets and touchscreens
/// (`org.gnome.desktop.peripherals.tablet` / `.touchscreen`, keyed by
/// vendor:product): which monitor a device covers. Set with the `gsettings`
/// tool; the monitors come from Mutter's display config.
pub mod gnome {
    /// One monitor: its connector and the [vendor, product, serial] GNOME
    /// identifies it by.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Monitor {
        pub primary: bool,
        pub connector: String,
        pub spec: [String; 3],
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Target {
        /// The virtual monitor of the extra screen.
        Virtual,
        /// The primary monitor.
        Primary,
    }

    /// The uinput devices with a monitor mapping.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Device {
        Pen,
        Touch,
    }

    fn settings_path(device: Device) -> String {
        let (schema, group, product) = match device {
            Device::Pen => ("tablet", "tablets", ib_inject::PEN_PRODUCT),
            Device::Touch => ("touchscreen", "touchscreens", ib_inject::TOUCH_PRODUCT),
        };
        format!(
            "org.gnome.desktop.peripherals.{schema}:/org/gnome/desktop/peripherals/{group}/{:04x}:{product:04x}/",
            ib_inject::DEVICE_VENDOR
        )
    }

    /// Maps a uinput device onto `target`; logs and carries on if it can't.
    pub fn map(device: Device, target: Target) {
        let monitors = current_monitors();
        let chosen = monitors.iter().find(|m| match target {
            Target::Virtual => m.connector.starts_with("Meta-"),
            Target::Primary => m.primary,
        });
        let Some(m) = chosen else {
            tracing::warn!(?device, ?target, ?monitors, "no monitor to map to");
            return;
        };
        let value = format!(
            "[{}]",
            m.spec
                .iter()
                .map(|s| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match std::process::Command::new("gsettings")
            .args(["set", &settings_path(device), "output", &value])
            .status()
        {
            Ok(s) if s.success() => tracing::info!(?device, connector = %m.connector, "mapped"),
            other => tracing::warn!(?device, ?other, "could not map to {}", m.connector),
        }
    }

    /// Back to GNOME's automatic mapping.
    pub fn reset(device: Device) {
        let _ = std::process::Command::new("gsettings")
            .args(["reset", &settings_path(device), "output"])
            .status();
    }

    fn current_monitors() -> Vec<Monitor> {
        let out = std::process::Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.gnome.Mutter.DisplayConfig",
                "--object-path",
                "/org/gnome/Mutter/DisplayConfig",
                "--method",
                "org.gnome.Mutter.DisplayConfig.GetCurrentState",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                parse_logical_monitors(&String::from_utf8_lossy(&o.stdout))
            }
            _ => Vec::new(),
        }
    }

    /// The monitors of `GetCurrentState`'s logical monitors, from gdbus's
    /// text output: `(x, y, scale, uint32 t, primary, [('conn', 'vendor',
    /// 'product', 'serial')], {...})`.
    pub fn parse_logical_monitors(text: &str) -> Vec<Monitor> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find(", [(") {
            let before = rest[..i].trim_end();
            let primary = if before.ends_with("true") {
                Some(true)
            } else if before.ends_with("false") {
                Some(false)
            } else {
                None
            };
            rest = &rest[i + 4..];
            if let Some(primary) = primary
                && let Some(strings) = quoted_strings::<4>(rest)
            {
                let [connector, vendor, product, serial] = strings;
                found.push(Monitor {
                    primary,
                    connector,
                    spec: [vendor, product, serial],
                });
            }
        }
        found
    }

    /// The first `N` quoted strings of `s` (separated by ", ").
    fn quoted_strings<const N: usize>(s: &str) -> Option<[String; N]> {
        let mut out: [String; N] = std::array::from_fn(|_| String::new());
        let mut chars = s.chars().peekable();
        for slot in &mut out {
            while chars.peek().is_some_and(|c| *c == ',' || *c == ' ') {
                chars.next();
            }
            let quote = chars.next().filter(|c| *c == '\'' || *c == '"')?;
            loop {
                match chars.next()? {
                    '\\' => slot.push(chars.next()?),
                    c if c == quote => break,
                    c => slot.push(c),
                }
            }
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn finds_the_logical_monitors() {
            let text = "(uint32 9, [(('DP-6', 'AOC', 'Q27P10', 'AH1'), [('2560x1440@59.951', 2560, \
                1440, 59.95, 1.0, [1.0], {'is-current': <true>})], @a{sv} {})], [(0, 0, 1.0, \
                uint32 0, true, [('DP-6', 'AOC', 'Q27P10', 'AH1')], @a{sv} {}), (2560, 0, 1.0, \
                uint32 0, false, [('Meta-0', 'MetaVendor', \"Virtual remote monitor's\", '0x1')], \
                @a{sv} {})], {})";
            assert_eq!(
                parse_logical_monitors(text),
                vec![
                    Monitor {
                        primary: true,
                        connector: "DP-6".into(),
                        spec: ["AOC".into(), "Q27P10".into(), "AH1".into()],
                    },
                    Monitor {
                        primary: false,
                        connector: "Meta-0".into(),
                        spec: [
                            "MetaVendor".into(),
                            "Virtual remote monitor's".into(),
                            "0x1".into()
                        ],
                    },
                ]
            );
        }

        #[test]
        fn the_settings_path_uses_our_device_ids() {
            assert!(
                settings_path(Device::Pen)
                    .ends_with(".tablet:/org/gnome/desktop/peripherals/tablets/494b:0001/")
            );
            assert!(
                settings_path(Device::Touch).ends_with(
                    ".touchscreen:/org/gnome/desktop/peripherals/touchscreens/494b:0002/"
                )
            );
        }
    }
}
