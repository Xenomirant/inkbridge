//! Maps tablet finger gestures to desktop touch frames.

use ib_geometry::{DesktopPoint, Region, Rotation, to_desktop};
use ib_inject::TouchPoint;
use ib_protocol::{TouchContact, TouchPhase};

/// Where a gesture happens on the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchMapping {
    /// Fingers act where they touch the streamed image (requires video).
    Absolute,
    /// Pen-only sessions: the tablet shows no image, so a gesture starts at
    /// the pen cursor and moves relative to it.
    AnchoredAtPointer,
}

#[derive(Debug)]
pub struct TouchMapper {
    mapping: TouchMapping,
    /// Gesture origin: (desktop anchor, normalised centroid at gesture start).
    origin: Option<(DesktopPoint, (f32, f32))>,
    /// Fingers currently down, as last injected (for keep-alive frames).
    active: Vec<TouchPoint>,
}

impl TouchMapper {
    pub fn new(mapping: TouchMapping) -> Self {
        Self {
            mapping,
            origin: None,
            active: Vec::new(),
        }
    }

    /// Converts one tablet frame to desktop touch points.
    pub fn map(
        &mut self,
        frame: &[TouchContact],
        region: Region,
        pointer: DesktopPoint,
    ) -> Vec<TouchPoint> {
        let frame: Vec<&TouchContact> = frame.iter().take(TouchContact::MAX).collect();
        if frame.is_empty() {
            return Vec::new();
        }
        let (anchor, (cx, cy)) = *self.origin.get_or_insert_with(|| {
            let n = frame.len() as f32;
            let cx = frame.iter().map(|c| c.x).sum::<f32>() / n;
            let cy = frame.iter().map(|c| c.y).sum::<f32>() / n;
            (pointer, (cx, cy))
        });
        let points: Vec<TouchPoint> = frame
            .iter()
            .map(|c| {
                let at = match self.mapping {
                    TouchMapping::Absolute => to_desktop(region, Rotation::None, c.x, c.y),
                    TouchMapping::AnchoredAtPointer => DesktopPoint {
                        x: anchor.x + f64::from(c.x - cx) * f64::from(region.width),
                        y: anchor.y + f64::from(c.y - cy) * f64::from(region.height),
                    },
                };
                TouchPoint {
                    id: c.id,
                    at,
                    phase: c.phase,
                }
            })
            .collect();

        self.active = points
            .iter()
            .filter(|p| matches!(p.phase, TouchPhase::Down | TouchPhase::Move))
            .map(|p| TouchPoint {
                phase: TouchPhase::Move,
                ..*p
            })
            .collect();
        if self.active.is_empty() {
            self.origin = None; // gesture over
        }
        points
    }

    /// Frame to re-send while fingers rest, so the OS doesn't time them out.
    pub fn keep_alive(&self) -> Option<&[TouchPoint]> {
        (!self.active.is_empty()).then_some(self.active.as_slice())
    }

    /// Ends any gesture (e.g. on disconnect): every active finger is cancelled.
    pub fn cancel(&mut self) -> Vec<TouchPoint> {
        self.origin = None;
        std::mem::take(&mut self.active)
            .into_iter()
            .map(|p| TouchPoint {
                phase: TouchPhase::Cancel,
                ..p
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGION: Region = Region {
        x: 0,
        y: 0,
        width: 1000,
        height: 500,
    };

    fn c(id: u8, x: f32, y: f32, phase: TouchPhase) -> TouchContact {
        TouchContact { id, x, y, phase }
    }

    #[test]
    fn anchored_gesture_starts_at_pointer_and_moves_relative() {
        let mut m = TouchMapper::new(TouchMapping::AnchoredAtPointer);
        let ptr = DesktopPoint { x: 300.0, y: 200.0 };
        let down = m.map(
            &[
                c(0, 0.4, 0.5, TouchPhase::Down),
                c(1, 0.6, 0.5, TouchPhase::Down),
            ],
            REGION,
            ptr,
        );
        // Centred on the pointer (f32 inputs: compare with a tolerance).
        assert!((down[0].at.x - 200.0).abs() < 1e-3 && (down[1].at.x - 400.0).abs() < 1e-3);
        assert!((down[0].at.y - 200.0).abs() < 1e-3);
        // Fingers move down by 10% of the region: 50 px.
        let mv = m.map(
            &[
                c(0, 0.4, 0.6, TouchPhase::Move),
                c(1, 0.6, 0.6, TouchPhase::Move),
            ],
            REGION,
            ptr,
        );
        assert!((mv[0].at.y - 250.0).abs() < 1e-3);
        assert_eq!(m.keep_alive().map(<[_]>::len), Some(2));
        m.map(
            &[
                c(0, 0.4, 0.6, TouchPhase::Up),
                c(1, 0.6, 0.6, TouchPhase::Up),
            ],
            REGION,
            ptr,
        );
        assert_eq!(m.keep_alive(), None);
    }

    #[test]
    fn absolute_maps_like_the_pen() {
        let mut m = TouchMapper::new(TouchMapping::Absolute);
        let p = m.map(
            &[c(0, 1.0, 1.0, TouchPhase::Down)],
            REGION,
            DesktopPoint { x: 0.0, y: 0.0 },
        );
        assert_eq!((p[0].at.x, p[0].at.y), (999.0, 499.0));
    }

    #[test]
    fn cancel_releases_active_fingers_without_clicking() {
        let mut m = TouchMapper::new(TouchMapping::AnchoredAtPointer);
        m.map(
            &[
                c(0, 0.5, 0.5, TouchPhase::Down),
                c(1, 0.6, 0.5, TouchPhase::Down),
            ],
            REGION,
            DesktopPoint { x: 0.0, y: 0.0 },
        );
        let cancelled = m.cancel();
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(|p| p.phase == TouchPhase::Cancel));
        assert_eq!(m.cancel(), vec![]);
    }
}
