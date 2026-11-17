//! macOS pen backend: `CGEvent` mouse events with the tablet-point subtype
//! (pressure, tilt, rotation) plus tablet proximity events (pen or eraser).
//!
//! No driver or admin rights: posting events needs the **Accessibility**
//! permission, granted once in System Settings to the app that runs the host
//! (the terminal app when started from a terminal).
//!
//! Applications decide what they read: most drawing apps use the tablet
//! fields, some only see a mouse. A virtual HID tablet would need a
//! DriverKit entitlement (see `docs/architecture.md`).

use ib_geometry::{DesktopPoint, Region};
use ib_protocol::{Buttons, PenFeatures, PenSample, PenState, Tool};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGEvent, CGEventField,
    CGEventMouseSubtype, CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton,
    CGPreflightPostEventAccess, CGRequestPostEventAccess, CGScrollEventUnit,
};

use std::time::{Duration, Instant};

use crate::tracker::{PenTracker, Step};
use crate::{InjectError, PenSink};

/// Any non-zero id; ties point events to the proximity event of our "tablet".
const DEVICE_ID: i64 = 1;
/// `NSPointingDeviceType` values carried by proximity events.
const POINTER_PEN: i64 = 1;
const POINTER_ERASER: i64 = 3;
/// `NX_TABLET_CAPABILITY_*`: device id, absolute x/y, buttons, tilt x/y,
/// pressure, rotation.
const CAPABILITIES: i64 = 0x0001 | 0x0002 | 0x0004 | 0x0040 | 0x0080 | 0x0100 | 0x0400 | 0x2000;

/// Main display: bounds in physical pixels and the pixels-per-point scale
/// (2.0 on Retina panels in their default mode).
pub fn main_display() -> (Region, f64) {
    display(CGMainDisplayID())
}

/// The same for any display, by its Core Graphics id.
fn display(id: u32) -> (Region, f64) {
    let points = CGDisplayBounds(id).size;
    let mode = CGDisplayCopyDisplayMode(id);
    let mode = mode.as_deref();
    let (w, h) = (
        CGDisplayMode::pixel_width(mode),
        CGDisplayMode::pixel_height(mode),
    );
    let scale = if w > 0 && points.width > 0.0 {
        w as f64 / points.width
    } else {
        1.0
    };
    let region = Region {
        x: 0,
        y: 0,
        width: u32::try_from(w).unwrap_or(0),
        height: u32::try_from(h).unwrap_or(0),
    };
    (region, scale)
}

/// The mouse event that carries one tracker step, or `None` when the step
/// only leaves proximity. `right` selects the right button (barrel held
/// when the stroke started).
fn mouse_event(step: Step, right: bool) -> Option<(CGEventType, CGMouseButton)> {
    if !step.in_range {
        return None;
    }
    let button = if right {
        CGMouseButton::Right
    } else {
        CGMouseButton::Left
    };
    let ty = match (step.down, step.up, step.in_contact, right) {
        (true, _, _, false) => CGEventType::LeftMouseDown,
        (true, _, _, true) => CGEventType::RightMouseDown,
        (_, true, _, false) => CGEventType::LeftMouseUp,
        (_, true, _, true) => CGEventType::RightMouseUp,
        (_, _, true, false) => CGEventType::LeftMouseDragged,
        (_, _, true, true) => CGEventType::RightMouseDragged,
        _ => return Some((CGEventType::MouseMoved, CGMouseButton::Left)),
    };
    Some((ty, button))
}

/// Tilt in degrees (ours: towards +x / +y, y pointing down the screen) to
/// the tablet fields' `-1..=1`, where positive y tilts away from the user.
fn tilt_fields(sample: &PenSample) -> (f64, f64) {
    (
        f64::from(sample.tilt_x) / 90.0,
        -f64::from(sample.tilt_y) / 90.0,
    )
}

/// Splits `acc + delta` into whole scroll units and the fraction to carry.
fn whole_units(acc: &mut f64, delta: f64) -> i32 {
    let total = *acc + delta;
    let whole = total.trunc();
    *acc = total - whole;
    whole as i32
}

#[derive(Debug)]
pub struct MacPenSink {
    /// Physical pixels per point of the display the pen works on.
    scale: f64,
    /// That display's top-left corner in global points.
    origin: CGPoint,
    tracker: PenTracker,
    /// Tool announced by the last proximity-enter event, if in proximity.
    proximity: Option<Tool>,
    /// The current stroke uses the right button.
    right: bool,
    last: CGPoint,
    scroll_carry: (f64, f64),
    access: Access,
}

/// Whether macOS lets this process post input events (the Accessibility
/// permission). Never shows a prompt.
pub fn pen_access_granted() -> bool {
    CGPreflightPostEventAccess()
}

/// How often a missing permission is checked again while the pen is used.
const ACCESS_RECHECK: Duration = Duration::from_secs(1);

/// The Accessibility permission, asked for when the pen is first used
/// rather than when the host starts.
#[derive(Debug, Default)]
struct Access {
    granted: bool,
    asked: bool,
    checked: Option<Instant>,
}

impl Access {
    /// `Ok(true)`: events may be posted. `Ok(false)`: not yet, drop the
    /// event quietly. `Err`: the system prompt was just shown (once).
    fn ready(&mut self) -> Result<bool, InjectError> {
        if self.granted {
            return Ok(true);
        }
        if self.checked.is_some_and(|t| t.elapsed() < ACCESS_RECHECK) {
            return Ok(false);
        }
        self.checked = Some(Instant::now());
        if pen_access_granted() {
            self.granted = true;
            return Ok(true);
        }
        if self.asked {
            return Ok(false);
        }
        self.asked = true;
        // Shows the system prompt (once per app).
        CGRequestPostEventAccess();
        Err(InjectError::Unavailable(
            "macOS is asking for the Accessibility permission, which Inkbridge uses only to \
             move the pointer and draw with your pen. Allow it in System Settings > Privacy & \
             Security > Accessibility for the app that runs inkbridge-host (your terminal app). \
             If the pen still does nothing afterwards, start the host again"
                .into(),
        ))
    }
}

impl Default for MacPenSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MacPenSink {
    /// Never prompts: the Accessibility permission is requested when the
    /// first pen event arrives.
    pub fn new() -> Self {
        Self::with_scale(main_display().1)
    }

    /// A pen for another display than the main one: desktop pixels are
    /// then relative to that display's top-left corner.
    pub fn on_display(id: u32) -> Self {
        let mut sink = Self::with_scale(1.0);
        sink.use_display(id);
        sink
    }

    fn use_display(&mut self, id: u32) {
        let scale = display(id).1;
        self.scale = if scale > 0.0 { scale } else { 1.0 };
        self.origin = CGDisplayBounds(id).origin;
    }

    fn with_scale(scale: f64) -> Self {
        Self {
            scale: if scale > 0.0 { scale } else { 1.0 },
            origin: CGPoint { x: 0.0, y: 0.0 },
            tracker: PenTracker::new(),
            proximity: None,
            right: false,
            last: CGPoint { x: 0.0, y: 0.0 },
            scroll_carry: (0.0, 0.0),
            access: Access::default(),
        }
    }

    /// Desktop pixels to global display points.
    fn to_points(&self, at: DesktopPoint) -> CGPoint {
        CGPoint {
            x: self.origin.x + at.x / self.scale,
            y: self.origin.y + at.y / self.scale,
        }
    }

    fn post(event: Option<&CGEvent>) -> Result<(), InjectError> {
        if event.is_none() {
            return Err(InjectError::Rejected("could not create the event".into()));
        }
        CGEvent::post(CGEventTapLocation::HIDEventTap, event);
        Ok(())
    }

    fn post_proximity(&mut self, entering: Option<Tool>) -> Result<(), InjectError> {
        let event = CGEvent::new(None);
        let e = event.as_deref();
        CGEvent::set_type(e, CGEventType::TabletProximity);
        CGEvent::set_location(e, self.last);
        let int = |f, v| CGEvent::set_integer_value_field(e, f, v);
        int(
            CGEventField::TabletProximityEventEnterProximity,
            i64::from(entering.is_some()),
        );
        let tool = entering.or(self.proximity).unwrap_or_default();
        int(
            CGEventField::TabletProximityEventPointerType,
            match tool {
                Tool::Pen => POINTER_PEN,
                Tool::Eraser => POINTER_ERASER,
            },
        );
        int(CGEventField::TabletProximityEventDeviceID, DEVICE_ID);
        int(CGEventField::TabletProximityEventPointerID, DEVICE_ID);
        int(CGEventField::TabletProximityEventSystemTabletID, DEVICE_ID);
        int(
            CGEventField::TabletProximityEventCapabilityMask,
            CAPABILITIES,
        );
        self.proximity = entering;
        Self::post(e)
    }

    fn post_point(
        &self,
        ty: CGEventType,
        button: CGMouseButton,
        sample: &PenSample,
        in_contact: bool,
    ) -> Result<(), InjectError> {
        let event = CGEvent::new_mouse_event(None, ty, self.last, button);
        let e = event.as_deref();
        let pressure = if in_contact {
            f64::from(sample.pressure)
        } else {
            0.0
        };
        let (tilt_x, tilt_y) = tilt_fields(sample);
        let barrel = sample.buttons.contains(Buttons::BARREL);
        let int = |f, v| CGEvent::set_integer_value_field(e, f, v);
        let double = |f, v| CGEvent::set_double_value_field(e, f, v);
        int(
            CGEventField::MouseEventSubtype,
            i64::from(CGEventMouseSubtype::TabletPoint.0),
        );
        int(CGEventField::TabletEventDeviceID, DEVICE_ID);
        int(
            CGEventField::TabletEventPointButtons,
            i64::from(in_contact) | (i64::from(barrel) << 1),
        );
        if ty != CGEventType::MouseMoved {
            int(CGEventField::MouseEventClickState, 1);
        }
        double(CGEventField::MouseEventPressure, pressure);
        double(CGEventField::TabletEventPointPressure, pressure);
        double(CGEventField::TabletEventTiltX, tilt_x);
        double(CGEventField::TabletEventTiltY, tilt_y);
        double(
            CGEventField::TabletEventRotation,
            f64::from(sample.rotation),
        );
        Self::post(e)
    }

    fn apply(&mut self, steps: &[Step], sample: &PenSample) -> Result<(), InjectError> {
        for &step in steps {
            if step.down {
                self.right = sample.buttons.contains(Buttons::BARREL);
            }
            let Some((ty, button)) = mouse_event(step, self.right) else {
                if self.proximity.is_some() {
                    self.post_proximity(None)?;
                }
                continue;
            };
            if self.proximity != Some(sample.tool) {
                if self.proximity.is_some() {
                    self.post_proximity(None)?;
                }
                self.post_proximity(Some(sample.tool))?;
            }
            self.post_point(ty, button, sample, step.in_contact)?;
        }
        Ok(())
    }
}

impl PenSink for MacPenSink {
    fn name(&self) -> &'static str {
        "macos-cgevent"
    }

    fn set_display(&mut self, id: u64) {
        if let Ok(id) = u32::try_from(id) {
            self.use_display(id);
        }
    }

    fn features(&self) -> PenFeatures {
        PenFeatures {
            pressure: true,
            tilt: true,
            rotation: true,
            hover: true,
            eraser: true,
            barrel_button: true,
        }
    }

    fn emit(&mut self, sample: &PenSample, at: DesktopPoint) -> Result<(), InjectError> {
        if !self.access.ready()? {
            return Ok(());
        }
        self.last = self.to_points(at);
        let steps = self.tracker.step(sample.state);
        self.apply(&steps, sample)
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        if !self.access.ready()? {
            return Ok(());
        }
        // Scroll-wheel events count in points; positive = content moves
        // down / right, the same as our convention.
        let dy = whole_units(&mut self.scroll_carry.1, dy_px / self.scale);
        let dx = whole_units(&mut self.scroll_carry.0, dx_px / self.scale);
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let event = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, dy, dx, 0);
        Self::post(event.as_deref())
    }

    fn move_pointer(&mut self, at: DesktopPoint) -> Result<(), InjectError> {
        if !self.access.ready()? {
            return Ok(());
        }
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::MouseMoved,
            self.to_points(at),
            CGMouseButton::Left,
        );
        Self::post(event.as_deref())
    }

    fn release(&mut self) -> Result<(), InjectError> {
        let steps = self.tracker.step(PenState::OutOfRange);
        self.apply(&steps, &PenSample::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(states: &[PenState]) -> Vec<Step> {
        let mut t = PenTracker::new();
        states.iter().flat_map(|s| t.step(*s)).collect()
    }

    #[test]
    fn stroke_maps_to_move_down_drag_up_and_leave() {
        use PenState::{Contact, Hover, OutOfRange};
        let types: Vec<_> = steps(&[Hover, Contact, Contact, Hover, OutOfRange])
            .into_iter()
            .map(|s| mouse_event(s, false).map(|(t, _)| t))
            .collect();
        assert_eq!(
            types,
            vec![
                Some(CGEventType::MouseMoved),
                Some(CGEventType::LeftMouseDown),
                Some(CGEventType::LeftMouseDragged),
                Some(CGEventType::LeftMouseUp),
                None,
            ]
        );
    }

    #[test]
    fn barrel_stroke_uses_the_right_button_but_hover_stays_a_plain_move() {
        use PenState::{Contact, Hover};
        let ev: Vec<_> = steps(&[Hover, Contact, Contact, Hover])
            .into_iter()
            .map(|s| mouse_event(s, true).unwrap())
            .collect();
        assert_eq!(ev[0], (CGEventType::MouseMoved, CGMouseButton::Left));
        assert_eq!(ev[1], (CGEventType::RightMouseDown, CGMouseButton::Right));
        assert_eq!(
            ev[2],
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        );
        assert_eq!(ev[3], (CGEventType::RightMouseUp, CGMouseButton::Right));
    }

    #[test]
    fn pixels_become_points_on_a_retina_display() {
        let sink = MacPenSink::with_scale(2.0);
        let p = sink.to_points(DesktopPoint {
            x: 3024.0,
            y: 982.0,
        });
        assert!((p.x - 1512.0).abs() < f64::EPSILON && (p.y - 491.0).abs() < f64::EPSILON);
        // A display to the left of and above the main one.
        let mut second = MacPenSink::with_scale(1.0);
        second.origin = CGPoint {
            x: -2560.0,
            y: -300.0,
        };
        let p = second.to_points(DesktopPoint { x: 100.0, y: 50.0 });
        assert!((p.x + 2460.0).abs() < f64::EPSILON && (p.y + 250.0).abs() < f64::EPSILON);
        let unscaled = MacPenSink::with_scale(0.0).to_points(DesktopPoint { x: 7.0, y: 9.0 });
        assert!((unscaled.x - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tilt_is_normalised_and_y_is_flipped() {
        let s = PenSample {
            tilt_x: 45.0,
            tilt_y: 90.0,
            ..PenSample::default()
        };
        assert_eq!(tilt_fields(&s), (0.5, -1.0));
    }

    #[test]
    fn small_scroll_deltas_accumulate_instead_of_vanishing() {
        let mut acc = 0.0;
        let sent: i32 = (0..10).map(|_| whole_units(&mut acc, 0.4)).sum();
        assert_eq!(sent, 4);
        let mut acc = 0.0;
        assert_eq!(whole_units(&mut acc, -2.5), -2);
        assert_eq!(whole_units(&mut acc, -0.5), -1);
    }

    #[test]
    fn main_display_reports_pixels_and_a_sane_scale() {
        let (region, scale) = main_display();
        // Headless CI runners may have no display at all.
        if region.width > 0 {
            assert!(region.height > 0);
            assert!((1.0..=4.0).contains(&scale), "scale {scale}");
        }
    }

    /// Draws a short diagonal pressure stroke with the real pointer.
    /// Needs a desktop session and the Accessibility permission for the
    /// terminal app. Open a drawing app or `tools/pen-test/index.html`
    /// under the pointer first, then run:
    /// `cargo test -p ib-inject -- --ignored posts_a_pressure_stroke`
    #[test]
    #[ignore = "moves the real pointer; needs the Accessibility permission"]
    fn posts_a_pressure_stroke() {
        assert!(pen_access_granted(), "Accessibility permission");
        let mut sink = MacPenSink::new();
        let (region, _) = main_display();
        let (cx, cy) = (
            f64::from(region.width) / 2.0,
            f64::from(region.height) / 2.0,
        );
        for i in 0..=120u8 {
            let f = f64::from(i) / 120.0;
            let sample = PenSample {
                pressure: (1.0 - (2.0 * f - 1.0).abs()) as f32,
                tilt_x: 30.0,
                state: if i == 0 {
                    PenState::Hover
                } else {
                    PenState::Contact
                },
                ..PenSample::default()
            };
            let at = DesktopPoint {
                x: cx + (f - 0.5) * 600.0,
                y: cy + (f - 0.5) * 300.0,
            };
            sink.emit(&sample, at).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        sink.release().unwrap();
    }
}
