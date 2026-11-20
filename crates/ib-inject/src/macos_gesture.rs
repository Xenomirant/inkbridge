//! Pinch-to-zoom on macOS: two fingers on the tablet become a trackpad
//! magnify gesture, which apps zoom with (Preview, Safari, Krita, Blender,
//! Inkscape…).
//!
//! macOS has no public API to post gestures. A gesture event is a
//! `CGEvent` of type 29 with a few undocumented fields (the gesture's HID
//! type, the magnification, the phase), which AppKit delivers to the app
//! as an `NSEvent` of type `.magnify`. Checked with the ignored test
//! `macos::tests::pinch_reaches_an_app`; if a macOS release changes the
//! fields, the gesture does nothing and two-finger scrolling still works.

use crate::TouchPoint;
use ib_geometry::DesktopPoint;
use ib_protocol::TouchPhase;

/// `CGEventType` of a gesture event (`NSEventTypeGesture`).
pub(crate) const GESTURE_EVENT: u32 = 29;
/// Field: the gesture's HID event type.
pub(crate) const FIELD_HID_TYPE: u32 = 110;
/// Field: the magnification change of this event (a double).
pub(crate) const FIELD_MAGNIFICATION: u32 = 113;
/// Field: the gesture's phase (`IOHIDEventPhaseBits`).
pub(crate) const FIELD_PHASE: u32 = 132;
/// `kIOHIDEventTypeZoom`: a magnify gesture.
pub(crate) const HID_ZOOM: i64 = 8;

/// A step of a pinch, at the point between the fingers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PinchStep {
    Begin {
        at: DesktopPoint,
    },
    /// Magnification change since the last step (0.1 = 10 % larger).
    Change {
        at: DesktopPoint,
        magnification: f64,
    },
    End {
        at: DesktopPoint,
        cancelled: bool,
    },
}

impl PinchStep {
    /// The phase value for [`FIELD_PHASE`]: began, changed, ended, cancelled.
    pub(crate) fn phase(self) -> i64 {
        match self {
            Self::Begin { .. } => 1,
            Self::Change { .. } => 2,
            Self::End {
                cancelled: false, ..
            } => 4,
            Self::End {
                cancelled: true, ..
            } => 8,
        }
    }
}

/// Turns two-finger touch frames into pinch steps.
#[derive(Debug, Default)]
pub(crate) struct Pinch {
    /// Distance between the fingers at the last step, while pinching.
    spread: Option<f64>,
    at: Option<DesktopPoint>,
}

impl Pinch {
    pub(crate) fn frame(&mut self, frame: &[TouchPoint]) -> Option<PinchStep> {
        let ending = frame
            .iter()
            .find(|p| matches!(p.phase, TouchPhase::Up | TouchPhase::Cancel));
        if let Some(end) = ending {
            self.spread.take()?;
            let at = self.at.take().unwrap_or(end.at);
            return Some(PinchStep::End {
                at,
                cancelled: end.phase == TouchPhase::Cancel,
            });
        }
        let [a, b, ..] = frame else { return None };
        let at = DesktopPoint {
            x: f64::midpoint(a.at.x, b.at.x),
            y: f64::midpoint(a.at.y, b.at.y),
        };
        let spread = (a.at.x - b.at.x).hypot(a.at.y - b.at.y);
        self.at = Some(at);
        match self.spread.replace(spread) {
            None => Some(PinchStep::Begin { at }),
            Some(before) if before > 0.0 => Some(PinchStep::Change {
                at,
                magnification: spread / before - 1.0,
            }),
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finger(id: u8, x: f64, phase: TouchPhase) -> TouchPoint {
        TouchPoint {
            id,
            at: DesktopPoint { x, y: 100.0 },
            phase,
        }
    }

    #[test]
    fn spreading_fingers_zoom_in_from_the_point_between_them() {
        use TouchPhase::{Down, Move, Up};
        let mut p = Pinch::default();
        let begin = p.frame(&[finger(0, 100.0, Down), finger(1, 200.0, Down)]);
        assert_eq!(
            begin,
            Some(PinchStep::Begin {
                at: DesktopPoint { x: 150.0, y: 100.0 }
            })
        );
        let Some(PinchStep::Change { magnification, at }) =
            p.frame(&[finger(0, 50.0, Move), finger(1, 250.0, Move)])
        else {
            panic!("expected a change");
        };
        assert!((magnification - 1.0).abs() < 1e-9, "twice the spread");
        assert!((at.x - 150.0).abs() < 1e-9);
        let end = p.frame(&[finger(0, 50.0, Up), finger(1, 250.0, Up)]);
        assert!(matches!(
            end,
            Some(PinchStep::End {
                cancelled: false,
                ..
            })
        ));
        assert_eq!(end.map(PinchStep::phase), Some(4));
        // Nothing more until the next pinch.
        assert_eq!(p.frame(&[finger(0, 50.0, Up)]), None);
    }

    #[test]
    fn a_pen_touching_down_cancels_the_pinch() {
        use TouchPhase::{Cancel, Down};
        let mut p = Pinch::default();
        p.frame(&[finger(0, 100.0, Down), finger(1, 200.0, Down)]);
        let end = p.frame(&[finger(0, 100.0, Cancel), finger(1, 200.0, Cancel)]);
        assert_eq!(end.map(PinchStep::phase), Some(8));
    }
}
