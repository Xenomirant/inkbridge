//! Linux Wayland pointer fallback through the RemoteDesktop portal: no root
//! or setup needed, but **no pressure or tilt** (the pen acts as a mouse:
//! hover moves, contact holds the left button). Used when uinput access
//! (the Advanced option) isn't granted.
//!
//! A future libei backend can add real stylus events once compositors
//! support libei's tablet interface.

use ib_geometry::DesktopPoint;
use ib_portal::{BTN_LEFT, Pointer};
use ib_protocol::{PenFeatures, PenSample, PenState};

use crate::tracker::PenTracker;
use crate::{InjectError, PenSink};

pub struct PortalPointerSink {
    pointer: Pointer,
    /// Desktop (frame pixel) → portal logical coordinates.
    scale: f64,
    tracker: PenTracker,
    pressed: bool,
}

impl std::fmt::Debug for PortalPointerSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalPointerSink")
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

fn gone() -> InjectError {
    InjectError::Unavailable("the desktop ended the remote-control session".into())
}

impl PortalPointerSink {
    /// `scale` converts desktop pixels (the captured frames' coordinates)
    /// into the portal's logical coordinates (1.0 without display scaling).
    pub fn new(pointer: Pointer, scale: f64) -> Self {
        Self {
            pointer,
            scale: if scale > 0.0 { scale } else { 1.0 },
            tracker: PenTracker::new(),
            pressed: false,
        }
    }

    fn logical(&self, at: DesktopPoint) -> (f64, f64) {
        (at.x * self.scale, at.y * self.scale)
    }

    fn button(&mut self, pressed: bool) -> Result<(), InjectError> {
        if self.pressed == pressed {
            return Ok(());
        }
        self.pressed = pressed;
        self.pointer
            .button(BTN_LEFT, pressed)
            .then_some(())
            .ok_or_else(gone)
    }
}

impl PenSink for PortalPointerSink {
    fn name(&self) -> &'static str {
        "linux-portal-pointer"
    }

    fn features(&self) -> PenFeatures {
        PenFeatures {
            pressure: false,
            tilt: false,
            rotation: false,
            hover: true,
            eraser: false,
            barrel_button: false,
        }
    }

    fn emit(&mut self, sample: &PenSample, at: DesktopPoint) -> Result<(), InjectError> {
        let (x, y) = self.logical(at);
        for step in self.tracker.step(sample.state) {
            if step.up {
                self.button(false)?;
            }
            if step.in_range && !self.pointer.move_to(x, y) {
                return Err(gone());
            }
            if step.down {
                self.button(true)?;
            }
        }
        Ok(())
    }

    fn move_pointer(&mut self, at: DesktopPoint) -> Result<(), InjectError> {
        let (x, y) = self.logical(at);
        self.pointer.move_to(x, y).then_some(()).ok_or_else(gone)
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        // Portal axis: positive = scroll down/right, i.e. content moves
        // up/left — the opposite of our content-movement convention.
        self.pointer
            .axis(-dx_px * self.scale, -dy_px * self.scale, false)
            .then_some(())
            .ok_or_else(gone)
    }

    fn release(&mut self) -> Result<(), InjectError> {
        let _ = self.tracker.step(PenState::OutOfRange);
        self.button(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_portal::PointerCmd;

    fn sample(state: PenState) -> PenSample {
        PenSample {
            state,
            ..PenSample::default()
        }
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<PointerCmd>) -> Vec<PointerCmd> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn stroke_moves_presses_and_releases_in_order() {
        let (p, mut rx) = ib_portal::Pointer::detached();
        let mut sink = PortalPointerSink::new(p, 0.5);
        let at = |x, y| DesktopPoint { x, y };
        sink.emit(&sample(PenState::Hover), at(100.0, 50.0))
            .unwrap();
        sink.emit(&sample(PenState::Contact), at(102.0, 50.0))
            .unwrap();
        sink.emit(&sample(PenState::Contact), at(104.0, 52.0))
            .unwrap();
        sink.emit(&sample(PenState::Hover), at(104.0, 52.0))
            .unwrap();
        let ev = drain(&mut rx);
        assert_eq!(
            ev,
            vec![
                PointerCmd::MoveTo { x: 50.0, y: 25.0 },
                PointerCmd::MoveTo { x: 51.0, y: 25.0 },
                PointerCmd::Button {
                    code: BTN_LEFT,
                    pressed: true
                },
                PointerCmd::MoveTo { x: 52.0, y: 26.0 },
                PointerCmd::Button {
                    code: BTN_LEFT,
                    pressed: false
                },
                PointerCmd::MoveTo { x: 52.0, y: 26.0 },
            ]
        );
    }

    #[test]
    fn release_lifts_a_held_button_once() {
        let (p, mut rx) = ib_portal::Pointer::detached();
        let mut sink = PortalPointerSink::new(p, 1.0);
        sink.emit(&sample(PenState::Contact), DesktopPoint { x: 1.0, y: 1.0 })
            .unwrap();
        drain(&mut rx);
        sink.release().unwrap();
        sink.release().unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![PointerCmd::Button {
                code: BTN_LEFT,
                pressed: false
            }]
        );
    }

    #[test]
    fn scroll_direction_matches_the_portal() {
        let (p, mut rx) = ib_portal::Pointer::detached();
        let mut sink = PortalPointerSink::new(p, 1.0);
        // Content moves down = scroll up = negative portal axis.
        sink.scroll(0.0, 30.0).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![PointerCmd::Axis {
                dx: -0.0,
                dy: -30.0,
                finish: false
            }]
        );
    }

    #[test]
    fn ended_session_is_reported() {
        let (p, rx) = ib_portal::Pointer::detached();
        drop(rx);
        let mut sink = PortalPointerSink::new(p, 1.0);
        assert!(matches!(
            sink.move_pointer(DesktopPoint { x: 0.0, y: 0.0 }),
            Err(InjectError::Unavailable(_))
        ));
    }
}
