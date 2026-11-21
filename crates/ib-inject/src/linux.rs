//! Linux pen injection through uinput: a virtual pen tablet.
//!
//! The kernel exposes it like any Wacom-style tablet, so libinput delivers
//! pressure, tilt, eraser and buttons to applications on both X11 and
//! Wayland. Needs write access to `/dev/uinput`, which is root-only by
//! default: granting it is an opt-in **Advanced option** (a one-time udev
//! rule, see `docs/linux.md`). Without it the host falls back to features
//! that need no root.
//!
//! Finger gestures go to a second virtual device, a multitouch touchscreen
//! (created on first use), so apps pinch-zoom natively as on a real one.
//! A third, a keyboard, types for the tablet (see `linux_keys`).

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    PropType, RelativeAxisCode, UinputAbsSetup,
};
use ib_geometry::{DesktopPoint, Region};
use ib_protocol::{Buttons, Key, Modifiers, PenFeatures, PenSample, Tool, TouchPhase};

use crate::tracker::{PenTracker, Step};
use crate::{InjectError, PenSink, TouchPoint};

/// Pressure resolution reported to applications.
const MAX_PRESSURE: i32 = 4096;
/// Hi-res wheel units per notch (kernel convention).
const WHEEL_HI_RES_PER_NOTCH: f64 = 120.0;
/// Wheel units per pixel of content movement (~100 px per notch).
const WHEEL_PER_PX: f64 = WHEEL_HI_RES_PER_NOTCH / 100.0;
/// Vendor id of the virtual devices (on the virtual bus): desktops keep
/// per-device settings by vendor and product (GNOME maps a pen tablet or a
/// touchscreen to a monitor that way), so ours must be fixed and distinct.
pub const DEVICE_VENDOR: u16 = 0x494b;
/// Product ids of the virtual pen and touchscreen (see [`DEVICE_VENDOR`]).
pub const PEN_PRODUCT: u16 = 0x0001;
pub const TOUCH_PRODUCT: u16 = 0x0002;
const KEYBOARD_PRODUCT: u16 = 0x0003;
const SCROLL_PRODUCT: u16 = 0x0004;

fn device_id(product: u16) -> InputId {
    InputId::new(BusType::BUS_VIRTUAL, DEVICE_VENDOR, product, 1)
}

/// Fingers the virtual touchscreen tracks at once (multitouch slots).
const TOUCH_SLOTS: u8 = 10;

pub struct UinputPenSink {
    pen: VirtualDevice,
    wheel: Option<VirtualDevice>,
    desktop: Region,
    /// Where pen and touch positions land on the devices' axes.
    map: DeviceMap,
    tracker: PenTracker,
    tool: Tool,
    last_at: DesktopPoint,
    wheel_rest: (f64, f64),
    touch: Option<VirtualDevice>,
    fingers: Fingers,
    /// Created with the pen: events sent to a brand-new device are lost
    /// until the desktop has opened it, which would drop the first key.
    /// Or why there is none.
    keyboard: Result<VirtualDevice, String>,
}

impl std::fmt::Debug for UinputPenSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UinputPenSink")
            .field("desktop", &self.desktop)
            .finish_non_exhaustive()
    }
}

fn unavailable(e: &std::io::Error) -> InjectError {
    if e.kind() == std::io::ErrorKind::PermissionDenied || e.kind() == std::io::ErrorKind::NotFound
    {
        InjectError::Unavailable(
            "pen pressure on Linux needs access to /dev/uinput. This is an optional, one-time \
             setup (Advanced option): see docs/linux.md, or run `inkbridge-host --setup-uinput`."
                .into(),
        )
    } else {
        InjectError::Unavailable(format!("could not create the virtual pen: {e}"))
    }
}

impl UinputPenSink {
    /// Creates the virtual tablet covering `desktop` (the bounding box of all
    /// monitors, in physical pixels).
    pub fn new(desktop: Region) -> Result<Self, InjectError> {
        let axis = |code, min: i32, max: i32, res: i32| {
            UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, 0, res))
        };
        let w = i32::try_from(desktop.width.max(2)).unwrap_or(i32::MAX);
        let h = i32::try_from(desktop.height.max(2)).unwrap_or(i32::MAX);
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in [
            KeyCode::BTN_TOOL_PEN,
            KeyCode::BTN_TOOL_RUBBER,
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_STYLUS,
        ] {
            keys.insert(k);
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT); // a display tablet: positions are on the screen
        let pen = VirtualDevice::builder()
            .map_err(|e| unavailable(&e))?
            .name("Inkbridge Pen")
            .input_id(device_id(PEN_PRODUCT))
            .with_keys(&keys)
            .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, w - 1, 10)))
            .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, h - 1, 10)))
            .and_then(|b| {
                b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_PRESSURE, 0, MAX_PRESSURE, 0))
            })
            .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_TILT_X, -90, 90, 0)))
            .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_TILT_Y, -90, 90, 0)))
            .and_then(|b| b.with_properties(&props))
            .and_then(evdev::uinput::VirtualDeviceBuilder::build)
            .map_err(|e| unavailable(&e))?;
        Ok(Self {
            pen,
            wheel: None,
            desktop,
            map: DeviceMap::new(desktop, (desktop.width, desktop.height)),
            tracker: PenTracker::new(),
            tool: Tool::Pen,
            last_at: DesktopPoint { x: 0.0, y: 0.0 },
            wheel_rest: (0.0, 0.0),
            touch: None,
            fingers: Fingers::default(),
            keyboard: keyboard_device().map_err(|e| e.to_string()),
        })
    }

    fn key(code: KeyCode, down: bool) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.0, i32::from(down))
    }

    fn abs(code: AbsoluteAxisCode, v: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, code.0, v)
    }

    fn tool_key(tool: Tool) -> KeyCode {
        if tool == Tool::Eraser {
            KeyCode::BTN_TOOL_RUBBER
        } else {
            KeyCode::BTN_TOOL_PEN
        }
    }

    fn events(&self, step: Step, sample: &PenSample, at: DesktopPoint) -> Vec<InputEvent> {
        let (x, y) = self.map.point(at);
        let pressure = if step.in_contact {
            (sample.pressure * MAX_PRESSURE as f32).round() as i32
        } else {
            0
        };
        let mut ev = vec![
            Self::abs(AbsoluteAxisCode::ABS_X, x),
            Self::abs(AbsoluteAxisCode::ABS_Y, y),
            Self::abs(AbsoluteAxisCode::ABS_PRESSURE, pressure),
            Self::abs(AbsoluteAxisCode::ABS_TILT_X, sample.tilt_x.round() as i32),
            Self::abs(AbsoluteAxisCode::ABS_TILT_Y, sample.tilt_y.round() as i32),
            Self::key(Self::tool_key(sample.tool), step.in_range),
            Self::key(
                KeyCode::BTN_STYLUS,
                sample.buttons.contains(Buttons::BARREL),
            ),
        ];
        if step.down || step.up {
            ev.push(Self::key(KeyCode::BTN_TOUCH, step.down));
        }
        ev
    }

    fn emit_events(&mut self, events: &[InputEvent]) -> Result<(), InjectError> {
        // `emit` appends the SYN_REPORT that closes the frame.
        self.pen
            .emit(events)
            .map_err(|e| InjectError::Rejected(e.to_string()))
    }

    fn wheel_device(&mut self) -> Result<&mut VirtualDevice, InjectError> {
        if self.wheel.is_none() {
            let mut rel = AttributeSet::<RelativeAxisCode>::new();
            for a in [
                RelativeAxisCode::REL_WHEEL,
                RelativeAxisCode::REL_HWHEEL,
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL_HI_RES,
            ] {
                rel.insert(a);
            }
            let mut keys = AttributeSet::<KeyCode>::new();
            keys.insert(KeyCode::BTN_LEFT); // libinput only treats it as a pointer with a button
            let dev = VirtualDevice::builder()
                .map_err(|e| unavailable(&e))?
                .name("Inkbridge Scroll")
                .input_id(device_id(SCROLL_PRODUCT))
                .with_relative_axes(&rel)
                .and_then(|b| b.with_keys(&keys))
                .and_then(evdev::uinput::VirtualDeviceBuilder::build)
                .map_err(|e| unavailable(&e))?;
            self.wheel = Some(dev);
        }
        Ok(self.wheel.as_mut().expect("created above"))
    }

    fn touch_device(&mut self) -> Result<&mut VirtualDevice, InjectError> {
        if self.touch.is_none() {
            let axis =
                |code, max: i32| UinputAbsSetup::new(code, AbsInfo::new(0, 0, max, 0, 0, 10));
            let w = i32::try_from(self.desktop.width.max(2)).unwrap_or(i32::MAX) - 1;
            let h = i32::try_from(self.desktop.height.max(2)).unwrap_or(i32::MAX) - 1;
            let mut keys = AttributeSet::<KeyCode>::new();
            keys.insert(KeyCode::BTN_TOUCH);
            let mut props = AttributeSet::<PropType>::new();
            props.insert(PropType::DIRECT);
            let dev = VirtualDevice::builder()
                .map_err(|e| unavailable(&e))?
                .name("Inkbridge Touch")
                .input_id(device_id(TOUCH_PRODUCT))
                .with_keys(&keys)
                .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, w)))
                .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, h)))
                .and_then(|b| {
                    b.with_absolute_axis(&axis(
                        AbsoluteAxisCode::ABS_MT_SLOT,
                        i32::from(TOUCH_SLOTS) - 1,
                    ))
                })
                .and_then(|b| {
                    b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 65535))
                })
                .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, w)))
                .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, h)))
                .and_then(|b| b.with_properties(&props))
                .and_then(evdev::uinput::VirtualDeviceBuilder::build)
                .map_err(|e| unavailable(&e))?;
            self.touch = Some(dev);
        }
        Ok(self.touch.as_mut().expect("created above"))
    }
}

/// The virtual keyboard for the tablet's typing.
fn keyboard_device() -> Result<VirtualDevice, InjectError> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in crate::linux_keys::all_keys() {
        keys.insert(k);
    }
    VirtualDevice::builder()
        .map_err(|e| unavailable(&e))?
        .name("Inkbridge Keyboard")
        .input_id(device_id(KEYBOARD_PRODUCT))
        .with_keys(&keys)
        .and_then(evdev::uinput::VirtualDeviceBuilder::build)
        .map_err(|e| unavailable(&e))
}

impl UinputPenSink {
    /// Sends key strokes, one report each (apps see every press and release).
    fn strokes(&mut self, strokes: &[crate::linux_keys::Stroke]) -> Result<(), InjectError> {
        let kb = self
            .keyboard
            .as_mut()
            .map_err(|e| InjectError::Unavailable(e.clone()))?;
        for &(code, down) in strokes {
            kb.emit(&[Self::key(code, down)])
                .map_err(|e| InjectError::Rejected(e.to_string()))?;
        }
        Ok(())
    }
}

/// Positions → a virtual device's axis range. The desktop stretches that
/// range over the monitor the device is mapped to, so a position is taken
/// relative to that display's bounds (`target`) and scaled to the range.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DeviceMap {
    target: Region,
    range: (u32, u32),
}

impl DeviceMap {
    fn new(target: Region, range: (u32, u32)) -> Self {
        Self { target, range }
    }

    fn point(&self, at: DesktopPoint) -> (i32, i32) {
        let axis = |v: f64, start: i32, len: u32, range: u32| {
            let span = f64::from(len.max(2) - 1);
            let last = f64::from(range.max(2) - 1);
            ((v - f64::from(start)) * last / span)
                .round()
                .clamp(0.0, last) as i32
        };
        (
            axis(at.x, self.target.x, self.target.width, self.range.0),
            axis(at.y, self.target.y, self.target.height, self.range.1),
        )
    }
}

/// Multitouch (protocol B) state of the virtual touchscreen: which slots
/// hold a finger, and the next tracking id.
#[derive(Debug, Default)]
struct Fingers {
    /// Tablet finger id per slot.
    slots: [Option<u8>; TOUCH_SLOTS as usize],
    next_tracking_id: i32,
}

impl Fingers {
    /// Kernel events for one touch frame (positions through `map`),
    /// without the closing SYN_REPORT.
    fn events(&mut self, frame: &[TouchPoint], map: &DeviceMap) -> Vec<InputEvent> {
        let abs = UinputPenSink::abs;
        // Slots are below TOUCH_SLOTS, so the conversion can't fail.
        let slot_event = |slot: usize| {
            abs(
                AbsoluteAxisCode::ABS_MT_SLOT,
                i32::try_from(slot).unwrap_or_default(),
            )
        };
        let mut ev = Vec::new();
        for t in frame {
            let held = self.slots.iter().position(|s| *s == Some(t.id));
            let (x, y) = map.point(t.at);
            match (t.phase, held) {
                (TouchPhase::Down | TouchPhase::Move, Some(slot)) => {
                    ev.push(slot_event(slot));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
                }
                (TouchPhase::Down | TouchPhase::Move, None) => {
                    // More fingers than slots: the extra ones are ignored.
                    let Some(slot) = self.slots.iter().position(Option::is_none) else {
                        continue;
                    };
                    self.slots[slot] = Some(t.id);
                    let id = self.next_tracking_id;
                    self.next_tracking_id = (id + 1) % 65536;
                    ev.push(slot_event(slot));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, id));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_X, x));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, y));
                }
                (TouchPhase::Up | TouchPhase::Cancel, Some(slot)) => {
                    self.slots[slot] = None;
                    ev.push(slot_event(slot));
                    ev.push(abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
                }
                (TouchPhase::Up | TouchPhase::Cancel, None) => {}
            }
        }
        if ev.is_empty() {
            return ev;
        }
        // Single-touch emulation for clients that read only the legacy axes.
        let down = self.slots.iter().any(Option::is_some);
        ev.push(UinputPenSink::key(KeyCode::BTN_TOUCH, down));
        if let Some(first) = frame.iter().find(|t| {
            matches!(t.phase, TouchPhase::Down | TouchPhase::Move)
                && self.slots.contains(&Some(t.id))
        }) {
            let (x, y) = map.point(first.at);
            ev.push(abs(AbsoluteAxisCode::ABS_X, x));
            ev.push(abs(AbsoluteAxisCode::ABS_Y, y));
        }
        ev
    }

    /// Lifts every finger still down.
    fn lift_all(&mut self) -> Vec<InputEvent> {
        let frame: Vec<TouchPoint> = self
            .slots
            .iter()
            .flatten()
            .map(|&id| TouchPoint {
                id,
                at: DesktopPoint { x: 0.0, y: 0.0 },
                phase: TouchPhase::Cancel,
            })
            .collect();
        // Lifting carries no position.
        let any = DeviceMap::new(
            Region {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            (2, 2),
        );
        self.events(&frame, &any)
    }
}

impl PenSink for UinputPenSink {
    fn name(&self) -> &'static str {
        "linux-uinput"
    }

    fn features(&self) -> PenFeatures {
        PenFeatures {
            pressure: true,
            tilt: true,
            rotation: false,
            hover: true,
            eraser: true,
            barrel_button: true,
        }
    }

    fn emit(&mut self, sample: &PenSample, at: DesktopPoint) -> Result<(), InjectError> {
        // Switching between pen and eraser ends proximity with the old tool first.
        if sample.tool != self.tool && self.tracker.in_contact() {
            self.release()?;
        }
        if sample.tool != self.tool {
            let old = Self::tool_key(self.tool);
            self.emit_events(&[Self::key(old, false)])?;
            self.tool = sample.tool;
        }
        self.last_at = at;
        for step in self.tracker.step(sample.state) {
            let ev = self.events(step, sample, at);
            self.emit_events(&ev)?;
        }
        Ok(())
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        // Content down → wheel up (positive); content right → scroll left.
        let wy = self.wheel_rest.1 + dy_px * WHEEL_PER_PX;
        let wx = self.wheel_rest.0 - dx_px * WHEEL_PER_PX;
        let (iy, ix) = (wy.trunc() as i32, wx.trunc() as i32);
        self.wheel_rest = (wx - f64::from(ix), wy - f64::from(iy));
        if iy == 0 && ix == 0 {
            return Ok(());
        }
        let rel =
            |code: RelativeAxisCode, v: i32| InputEvent::new(EventType::RELATIVE.0, code.0, v);
        let mut ev = Vec::with_capacity(2);
        if iy != 0 {
            ev.push(rel(RelativeAxisCode::REL_WHEEL_HI_RES, iy));
        }
        if ix != 0 {
            ev.push(rel(RelativeAxisCode::REL_HWHEEL_HI_RES, ix));
        }
        self.wheel_device()?
            .emit(&ev)
            .map_err(|e| InjectError::Rejected(e.to_string()))
    }

    /// The desktop maps the devices onto that display (the host sets GNOME's
    /// tablet mapping); positions are relative to its bounds from now on.
    fn set_display(&mut self, _id: u64, bounds: Region) {
        self.map = DeviceMap::new(bounds, (self.desktop.width, self.desktop.height));
    }

    fn keyboard(&self) -> bool {
        self.keyboard.is_ok()
    }

    fn type_text(&mut self, text: &str) -> Result<(), InjectError> {
        let (strokes, skipped) = crate::linux_keys::text_strokes(text);
        self.strokes(&strokes)?;
        if skipped > 0 {
            return Err(InjectError::Rejected(format!(
                "typing on Linux covers the characters of the US layout; {skipped} other character(s) skipped"
            )));
        }
        Ok(())
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) -> Result<(), InjectError> {
        let strokes = crate::linux_keys::key_strokes(key, modifiers)
            .ok_or_else(|| crate::linux_keys::unknown_key(key))?;
        self.strokes(&strokes)
    }

    fn touch(&mut self, frame: &[TouchPoint]) -> Result<(), InjectError> {
        let map = self.map;
        let ev = self.fingers.events(frame, &map);
        if ev.is_empty() {
            return Ok(());
        }
        self.touch_device()?
            .emit(&ev)
            .map_err(|e| InjectError::Rejected(e.to_string()))
    }

    fn release(&mut self) -> Result<(), InjectError> {
        let lift = self.fingers.lift_all();
        if !lift.is_empty()
            && let Some(dev) = self.touch.as_mut()
        {
            dev.emit(&lift)
                .map_err(|e| InjectError::Rejected(e.to_string()))?;
        }
        let leave = PenSample {
            state: ib_protocol::PenState::OutOfRange,
            tool: self.tool,
            ..PenSample::default()
        };
        let at = self.last_at;
        for step in self.tracker.step(leave.state) {
            let ev = self.events(step, &leave, at);
            self.emit_events(&ev)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finger(id: u8, x: f64, y: f64, phase: TouchPhase) -> TouchPoint {
        TouchPoint {
            id,
            at: DesktopPoint { x, y },
            phase,
        }
    }

    /// (axis or key code, value) pairs, for readable assertions.
    fn codes(ev: &[InputEvent]) -> Vec<(u16, i32)> {
        ev.iter().map(|e| (e.code(), e.value())).collect()
    }

    const SLOT: u16 = AbsoluteAxisCode::ABS_MT_SLOT.0;
    const ID: u16 = AbsoluteAxisCode::ABS_MT_TRACKING_ID.0;
    const MX: u16 = AbsoluteAxisCode::ABS_MT_POSITION_X.0;
    const MY: u16 = AbsoluteAxisCode::ABS_MT_POSITION_Y.0;
    const TOUCH: u16 = KeyCode::BTN_TOUCH.0;
    const AX: u16 = AbsoluteAxisCode::ABS_X.0;
    const AY: u16 = AbsoluteAxisCode::ABS_Y.0;

    /// Positions relative to `(x, y)`, one device unit per pixel.
    fn at_origin(x: i32, y: i32) -> DeviceMap {
        DeviceMap::new(
            Region {
                x,
                y,
                width: 1000,
                height: 1000,
            },
            (1000, 1000),
        )
    }

    #[test]
    fn positions_scale_from_the_display_to_the_device_range() {
        // The pen device spans 2560x1440 units; on a 1920x1200 extra screen
        // its corners and centre map to the device's corners and centre.
        let map = DeviceMap::new(
            Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1200,
            },
            (2560, 1440),
        );
        assert_eq!(map.point(DesktopPoint { x: 0.0, y: 0.0 }), (0, 0));
        assert_eq!(
            map.point(DesktopPoint {
                x: 1919.0,
                y: 1199.0
            }),
            (2559, 1439)
        );
        assert_eq!(map.point(DesktopPoint { x: 959.5, y: 599.5 }), (1280, 720));
        // Outside the display clamps to its edge.
        assert_eq!(map.point(DesktopPoint { x: -5.0, y: 5000.0 }), (0, 1439));
    }

    #[test]
    fn a_pinch_uses_two_slots_and_lifts_them() {
        let mut f = Fingers::default();
        let origin = &at_origin(100, 0);
        let down = f.events(
            &[
                finger(7, 200.0, 50.0, TouchPhase::Down),
                finger(9, 300.0, 50.0, TouchPhase::Down),
            ],
            origin,
        );
        assert_eq!(
            codes(&down),
            vec![
                (SLOT, 0),
                (ID, 0),
                (MX, 100),
                (MY, 50),
                (SLOT, 1),
                (ID, 1),
                (MX, 200),
                (MY, 50),
                (TOUCH, 1),
                (AX, 100),
                (AY, 50),
            ]
        );
        let moved = f.events(&[finger(9, 320.0, 60.0, TouchPhase::Move)], origin);
        assert_eq!(
            codes(&moved),
            vec![
                (SLOT, 1),
                (MX, 220),
                (MY, 60),
                (TOUCH, 1),
                (AX, 220),
                (AY, 60)
            ]
        );
        let up = f.events(
            &[
                finger(7, 200.0, 50.0, TouchPhase::Up),
                finger(9, 320.0, 60.0, TouchPhase::Cancel),
            ],
            origin,
        );
        assert_eq!(
            codes(&up),
            vec![(SLOT, 0), (ID, -1), (SLOT, 1), (ID, -1), (TOUCH, 0)]
        );
    }

    #[test]
    fn new_fingers_get_new_tracking_ids() {
        let mut f = Fingers::default();
        let o = &at_origin(0, 0);
        f.events(&[finger(1, 0.0, 0.0, TouchPhase::Down)], o);
        f.events(&[finger(1, 0.0, 0.0, TouchPhase::Up)], o);
        let again = f.events(&[finger(1, 0.0, 0.0, TouchPhase::Down)], o);
        assert_eq!(codes(&again)[..2], [(SLOT, 0), (ID, 1)]);
    }

    #[test]
    fn unknown_lifts_and_full_slots_send_nothing() {
        let mut f = Fingers::default();
        let o = &at_origin(0, 0);
        assert_eq!(
            codes(&f.events(&[finger(3, 0.0, 0.0, TouchPhase::Up)], o)),
            []
        );
        let many: Vec<TouchPoint> = (0..=TOUCH_SLOTS)
            .map(|i| finger(i, 0.0, 0.0, TouchPhase::Down))
            .collect();
        f.events(&many, o);
        let extra = f.events(&[finger(TOUCH_SLOTS, 5.0, 5.0, TouchPhase::Move)], o);
        assert_eq!(codes(&extra), [], "the 11th finger has no slot");
    }

    #[test]
    fn lift_all_releases_resting_fingers() {
        let mut f = Fingers::default();
        let o = &at_origin(0, 0);
        f.events(&[finger(4, 1.0, 1.0, TouchPhase::Down)], o);
        assert_eq!(codes(&f.lift_all()), vec![(SLOT, 0), (ID, -1), (TOUCH, 0)]);
        assert_eq!(codes(&f.lift_all()), []);
    }
}
