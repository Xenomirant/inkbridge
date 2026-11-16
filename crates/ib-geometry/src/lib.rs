//! Coordinate mapping for Inkbridge.
//!
//! Model:
//! - The **client** shows the streamed image letterboxed on its screen and
//!   reports pen positions normalised to that image (`0..=1` on both axes).
//! - The **host** streams a [`Region`] of its desktop and maps normalised
//!   positions back into desktop coordinates with [`to_desktop`].
//! - [`fit_aspect`] picks a region matching the tablet's aspect ratio, so the
//!   image is never stretched and pen strokes keep their true proportions.
//!
//! Everything here is pure arithmetic and property-tested.

pub use ib_protocol::Region;

/// A point in desktop coordinates (physical pixels). Sub-pixel precision is
/// kept because pen APIs accept it (Windows pointer injection uses
/// HIMETRIC-like precision; macOS uses floating-point coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesktopPoint {
    pub x: f64,
    pub y: f64,
}

/// Orientation of the client image relative to the desktop region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    None,
    /// Client rotated 90° clockwise relative to the desktop.
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    /// Maps normalised client coordinates into normalised region coordinates.
    pub fn apply(self, x: f64, y: f64) -> (f64, f64) {
        match self {
            Self::None => (x, y),
            Self::Cw90 => (y, 1.0 - x),
            Self::Cw180 => (1.0 - x, 1.0 - y),
            Self::Cw270 => (1.0 - y, x),
        }
    }
}

/// Maps a normalised position (already clamped to `0..=1`) inside `region`
/// to desktop coordinates.
///
/// The far edge maps to the last pixel, not one past it, so a pen at `1.0`
/// can never land on a neighbouring monitor.
pub fn to_desktop(region: Region, rotation: Rotation, x: f32, y: f32) -> DesktopPoint {
    let (nx, ny) = rotation.apply(f64::from(x).clamp(0.0, 1.0), f64::from(y).clamp(0.0, 1.0));
    let max_dx = f64::from(region.width.saturating_sub(1));
    let max_dy = f64::from(region.height.saturating_sub(1));
    DesktopPoint {
        x: f64::from(region.x) + nx * max_dx,
        y: f64::from(region.y) + ny * max_dy,
    }
}

/// The largest region with the given aspect ratio (`width / height`) that
/// fits inside `bounds`, centred. Used to choose a streamed area matching the
/// tablet's screen.
///
/// # Panics
/// Never; a non-finite or non-positive `aspect` returns `bounds` unchanged.
pub fn fit_aspect(bounds: Region, aspect: f64) -> Region {
    if !(aspect.is_finite() && aspect > 0.0) || bounds.width == 0 || bounds.height == 0 {
        return bounds;
    }
    let bw = f64::from(bounds.width);
    let bh = f64::from(bounds.height);
    let (w, h) = if bw / bh > aspect {
        // Bounds are wider than wanted: full height, reduced width.
        ((bh * aspect).round().clamp(1.0, bw), bh)
    } else {
        (bw, (bw / aspect).round().clamp(1.0, bh))
    };
    let (w, h) = (w as u32, h as u32);
    // Centring offsets are at most u32::MAX / 2, which always fits in i32.
    let off = |slack: u32| i32::try_from(slack / 2).unwrap_or(i32::MAX);
    Region {
        x: bounds.x.saturating_add(off(bounds.width - w)),
        y: bounds.y.saturating_add(off(bounds.height - h)),
        width: w,
        height: h,
    }
}

/// Which part of `display` a tablet with a `width × height` viewport maps to.
///
/// - Same orientation (both landscape or both portrait): the whole display,
///   so every corner stays reachable; the tablet letterboxes slightly.
/// - Different orientation (portrait tablet, landscape monitor): a centred
///   strip with the tablet's aspect at full size, instead of shrinking the
///   whole desktop into a tiny band.
pub fn region_for_viewport(display: Region, width: u32, height: u32) -> Region {
    if width == 0 || height == 0 {
        return display;
    }
    let tablet_landscape = width >= height;
    let display_landscape = display.width >= display.height;
    if tablet_landscape == display_landscape {
        display
    } else {
        fit_aspect(display, f64::from(width) / f64::from(height))
    }
}

/// Smallest view width (px): zooming further would only magnify pixels.
pub const MIN_VIEW_WIDTH: u32 = 320;

/// Makes a requested view of the desktop valid for a tablet whose drawing
/// area has the aspect `aspect` (width / height):
/// - same aspect as the tablet (expanded around the requested centre), so the
///   picture fills the tablet without distortion;
/// - at least [`MIN_VIEW_WIDTH`] wide;
/// - inside `display`; a view larger than the display becomes the whole
///   display (the tablet letterboxes it).
pub fn clamp_view(display: Region, requested: Region, aspect: f64) -> Region {
    if !(aspect.is_finite() && aspect > 0.0) || display.width == 0 || display.height == 0 {
        return display;
    }
    let cx = f64::from(requested.x) + f64::from(requested.width) / 2.0;
    let cy = f64::from(requested.y) + f64::from(requested.height) / 2.0;
    // Grow the smaller side so the rectangle has the tablet's aspect.
    let mut w = f64::from(requested.width.max(1));
    let mut h = f64::from(requested.height.max(1));
    if w / h < aspect {
        w = h * aspect;
    } else {
        h = w / aspect;
    }
    if w < f64::from(MIN_VIEW_WIDTH) {
        w = f64::from(MIN_VIEW_WIDTH);
        h = w / aspect;
    }
    let (dw, dh) = (f64::from(display.width), f64::from(display.height));
    if w >= dw || h >= dh {
        // Doesn't fit: show as much as possible with the tablet's shape, or
        // simply the whole display when that's what was asked for.
        let fitted = fit_aspect(display, aspect);
        return if w >= dw && h >= dh {
            display
        } else {
            fitted_around(display, fitted, cx, cy)
        };
    }
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    let (w, h) = (even(w), even(h));
    let x = (cx - f64::from(w) / 2.0).clamp(
        f64::from(display.x),
        f64::from(display.x) + dw - f64::from(w),
    );
    let y = (cy - f64::from(h) / 2.0).clamp(
        f64::from(display.y),
        f64::from(display.y) + dh - f64::from(h),
    );
    Region {
        x: x.round() as i32,
        y: y.round() as i32,
        width: w,
        height: h,
    }
}

/// `fitted` (already the right size) moved as close to centre (`cx`, `cy`)
/// as `display` allows.
fn fitted_around(display: Region, fitted: Region, cx: f64, cy: f64) -> Region {
    let (w, h) = (f64::from(fitted.width), f64::from(fitted.height));
    let x = (cx - w / 2.0).clamp(
        f64::from(display.x),
        f64::from(display.x) + f64::from(display.width) - w,
    );
    let y = (cy - h / 2.0).clamp(
        f64::from(display.y),
        f64::from(display.y) + f64::from(display.height) - h,
    );
    Region {
        x: x.round() as i32,
        y: y.round() as i32,
        ..fitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MONITOR: Region = Region {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn corners_map_to_region_corners() {
        let tl = to_desktop(MONITOR, Rotation::None, 0.0, 0.0);
        let br = to_desktop(MONITOR, Rotation::None, 1.0, 1.0);
        assert_eq!((tl.x, tl.y), (-1920.0, 0.0));
        assert_eq!((br.x, br.y), (-1.0, 1079.0));
    }

    #[test]
    fn rotation_cw90_maps_client_top_left_to_region_bottom_left() {
        let p = to_desktop(MONITOR, Rotation::Cw90, 0.0, 0.0);
        assert_eq!((p.x, p.y), (-1920.0, 1079.0));
    }

    const DISPLAY: Region = Region {
        x: 0,
        y: 0,
        width: 2560,
        height: 1440,
    };

    #[test]
    fn view_takes_tablet_aspect_and_stays_inside() {
        // Portrait tablet (900:1440), zoomed onto the top-right corner.
        let v = clamp_view(
            DISPLAY,
            Region {
                x: 2400,
                y: -50,
                width: 400,
                height: 400,
            },
            900.0 / 1440.0,
        );
        assert!(
            (f64::from(v.width) / f64::from(v.height) - 0.625).abs() < 0.01,
            "{v:?}"
        );
        assert!(
            v.x >= 0
                && v.y >= 0
                && i64::from(v.x) + i64::from(v.width) <= 2560
                && i64::from(v.y) + i64::from(v.height) <= 1440,
            "{v:?}"
        );
    }

    #[test]
    fn tiny_view_is_limited_and_huge_view_is_the_whole_display() {
        let tiny = clamp_view(
            DISPLAY,
            Region {
                x: 1000,
                y: 700,
                width: 10,
                height: 10,
            },
            16.0 / 10.0,
        );
        assert_eq!(tiny.width, MIN_VIEW_WIDTH);
        let huge = clamp_view(
            DISPLAY,
            Region {
                x: -500,
                y: -500,
                width: 9000,
                height: 9000,
            },
            16.0 / 10.0,
        );
        assert_eq!(huge, DISPLAY);
    }

    #[test]
    fn too_tall_for_display_shows_largest_strip_near_centre() {
        // Portrait tablet asking for a full-height strip on the left.
        let v = clamp_view(
            DISPLAY,
            Region {
                x: 0,
                y: 0,
                width: 900,
                height: 2000,
            },
            900.0 / 1440.0,
        );
        assert_eq!((v.width, v.height, v.x, v.y), (900, 1440, 0, 0));
    }

    #[test]
    fn viewport_policy() {
        let monitor = Region {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        };
        // Landscape tablet on a landscape monitor: whole desktop.
        assert_eq!(region_for_viewport(monitor, 2560, 1600), monitor);
        // Portrait tablet: a full-height portrait strip, centred.
        let strip = region_for_viewport(monitor, 1600, 2560);
        assert_eq!((strip.height, strip.width), (1440, 900));
        assert_eq!(strip.x, (2560 - 900) / 2);
        // Degenerate viewport: whole desktop.
        assert_eq!(region_for_viewport(monitor, 0, 10), monitor);
    }

    #[test]
    fn tablet_16_10_on_16_9_monitor_is_pillarboxed() {
        let r = fit_aspect(
            Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            16.0 / 10.0,
        );
        assert_eq!(
            r,
            Region {
                x: 96,
                y: 0,
                width: 1728,
                height: 1080
            }
        );
    }

    fn region() -> impl Strategy<Value = Region> {
        (
            -10_000i32..10_000,
            -10_000i32..10_000,
            1u32..8_000,
            1u32..8_000,
        )
            .prop_map(|(x, y, width, height)| Region {
                x,
                y,
                width,
                height,
            })
    }

    fn rotation() -> impl Strategy<Value = Rotation> {
        prop_oneof![
            Just(Rotation::None),
            Just(Rotation::Cw90),
            Just(Rotation::Cw180),
            Just(Rotation::Cw270)
        ]
    }

    proptest! {
        #[test]
        fn mapped_points_stay_inside_region(r in region(), rot in rotation(), x in -1.0f32..2.0, y in -1.0f32..2.0) {
            let p = to_desktop(r, rot, x, y);
            prop_assert!(p.x >= f64::from(r.x) && p.x <= f64::from(r.x) + f64::from(r.width - 1));
            prop_assert!(p.y >= f64::from(r.y) && p.y <= f64::from(r.y) + f64::from(r.height - 1));
        }

        #[test]
        fn fitted_region_is_inside_bounds_with_requested_aspect(b in region(), aspect in 0.2f64..5.0) {
            let r = fit_aspect(b, aspect);
            prop_assert!(r.x >= b.x && r.y >= b.y);
            prop_assert!(i64::from(r.x) + i64::from(r.width) <= i64::from(b.x) + i64::from(b.width));
            prop_assert!(i64::from(r.y) + i64::from(r.height) <= i64::from(b.y) + i64::from(b.height));
            // One full side is always used.
            prop_assert!(r.width == b.width || r.height == b.height);
            // Aspect within rounding error of one pixel.
            if r.width > 50 && r.height > 50 {
                let got = f64::from(r.width) / f64::from(r.height);
                let tol = aspect * (1.0 / f64::from(r.width.min(r.height))) * 2.0;
                prop_assert!((got - aspect).abs() <= tol.max(1e-9), "got {got}, want {aspect}");
            }
        }
    }
}
