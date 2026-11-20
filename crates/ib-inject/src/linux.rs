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

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType,
    RelativeAxisCode, UinputAbsSetup,
};
use ib_geometry::{DesktopPoint, Region};
use ib_protocol::{Buttons, PenFeatures, PenSample, Tool, TouchPhase};

use crate::tracker::{PenTracker, Step};
use crate::{InjectError, PenSink, TouchPoint};

/// Pressure resolution reported to applications.
const MAX_PRESSURE: i32 = 4096;
/// Hi-res wheel units per notch (kernel convention).
const WHEEL_HI_RES_PER_NOTCH: f64 = 120.0;
/// Wheel units per pixel of content movement (~100 px per notch).
const WHEEL_PER_PX: f64 = WHEEL_HI_RES_PER_NOTCH / 100.0;
/// Fingers the virtual touchscreen tracks at once (multitouch slots).
const TOUCH_SLOTS: u8 = 10;

pub struct UinputPenSink {
    pen: VirtualDevice,
    wheel: Option<VirtualDevice>,
    desktop: Region,
    tracker: PenTracker,
    tool: Tool,
    last_at: DesktopPoint,
    wheel_rest: (f64, f64),
    touch: Option<VirtualDevice>,
    fingers: Fingers,
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
            tracker: PenTracker::new(),
            tool: Tool::Pen,
            last_at: DesktopPoint { x: 0.0, y: 0.0 },
            wheel_rest: (0.0, 0.0),
            touch: None,
            fingers: Fingers::default(),
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
        let x = (at.x - f64::from(self.desktop.x)).round() as i32;
        let y = (at.y - f64::from(self.desktop.y)).round() as i32;
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

/// Multitouch (protocol B) state of the virtual touchscreen: which slots
/// hold a finger, and the next tracking id.
#[derive(Debug, Default)]
struct Fingers {
    /// Tablet finger id per slot.
    slots: [Option<u8>; TOUCH_SLOTS as usize],
    next_tracking_id: i32,
}

impl Fingers {
    /// Kernel events for one touch frame (positions relative to `origin`),
    /// without the closing SYN_REPORT.
    fn events(&mut self, frame: &[TouchPoint], origin: DesktopPoint) -> Vec<InputEvent> {
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
            let x = (t.at.x - origin.x).round() as i32;
            let y = (t.at.y - origin.y).round() as i32;
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
            ev.push(abs(
                AbsoluteAxisCode::ABS_X,
                (first.at.x - origin.x).round() as i32,
            ));
            ev.push(abs(
                AbsoluteAxisCode::ABS_Y,
                (first.at.y - origin.y).round() as i32,
            ));
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
        self.events(&frame, DesktopPoint { x: 0.0, y: 0.0 })
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

    fn touch(&mut self, frame: &[TouchPoint]) -> Result<(), InjectError> {
        let origin = DesktopPoint {
            x: f64::from(self.desktop.x),
            y: f64::from(self.desktop.y),
        };
        let ev = self.fingers.events(frame, origin);
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

    #[test]
    fn a_pinch_uses_two_slots_and_lifts_them() {
        let mut f = Fingers::default();
        let origin = DesktopPoint { x: 100.0, y: 0.0 };
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
        let o = DesktopPoint { x: 0.0, y: 0.0 };
        f.events(&[finger(1, 0.0, 0.0, TouchPhase::Down)], o);
        f.events(&[finger(1, 0.0, 0.0, TouchPhase::Up)], o);
        let again = f.events(&[finger(1, 0.0, 0.0, TouchPhase::Down)], o);
        assert_eq!(codes(&again)[..2], [(SLOT, 0), (ID, 1)]);
    }

    #[test]
    fn unknown_lifts_and_full_slots_send_nothing() {
        let mut f = Fingers::default();
        let o = DesktopPoint { x: 0.0, y: 0.0 };
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
        let o = DesktopPoint { x: 0.0, y: 0.0 };
        f.events(&[finger(4, 1.0, 1.0, TouchPhase::Down)], o);
        assert_eq!(codes(&f.lift_all()), vec![(SLOT, 0), (ID, -1), (TOUCH, 0)]);
        assert_eq!(codes(&f.lift_all()), []);
    }
}
