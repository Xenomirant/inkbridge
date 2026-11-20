//! A virtual display on macOS: the system treats it as a real extra
//! monitor, so the tablet can be a second screen instead of a copy of one.
//!
//! Uses `CGVirtualDisplay`, a **private** CoreGraphics API (no driver, no
//! admin rights). Apple can change it in any release, so every class and
//! selector is looked up at run time and a missing one is an error, never
//! a crash. See ADR-0008.

use std::ffi::CStr;
use std::sync::Mutex;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};

use crate::CaptureError;

/// Pixels per millimetre of a typical tablet panel (about 210 dpi). The
/// real size isn't known; the system only uses it to pick a default scale.
const PIXELS_PER_MM: f64 = 8.3;
const REFRESH_HZ: f64 = 60.0;

/// Size steps of a virtual display, as tablet pixels per point: from
/// "larger text" (2.0: every point is 2x2 pixels of the tablet's area,
/// pixel for pixel when the area is what the decoder takes) to "more
/// space" (1.0). Every step is drawn at 2x and scaled into the picture,
/// as a Mac scales its own Retina screen.
pub const SIZE_STEPS: [f64; 6] = [2.0, 1.75, 1.5, 4.0 / 3.0, 1.2, 1.0];

/// The display's size in points at size step `step` for a tablet area of
/// `width × height` pixels (even sides).
pub fn points_at(width: u32, height: u32, step: usize) -> (u32, u32) {
    let per_point = SIZE_STEPS[step.min(SIZE_STEPS.len() - 1)];
    let even = |v: u32| {
        let p = (f64::from(v) / per_point) as u32;
        (p.max(2) / 2) * 2
    };
    (even(width), even(height))
}

/// The modes a virtual display offers: only the one at `step`. With more
/// modes, macOS keeps whichever is current when the list changes, so a new
/// step would not take effect; the size is chosen on the tablet instead
/// of in System Settings.
pub fn virtual_modes(width: u32, height: u32, step: usize) -> Vec<(u32, u32)> {
    vec![points_at(width, height, step.min(SIZE_STEPS.len() - 1))]
}

/// Live virtual displays: id and the widest picture each can hold.
static LIVE: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Pixels per point of one of our virtual displays that is `points_wide`
/// points wide, or `None` for any other display.
///
/// The display's modes are drawn at 2x wherever that fits its pixel
/// limit, which [`MacVirtualDisplay::new`] sets so that every size step
/// fits. (Each mode also exists at 1x, hidden in System Settings unless
/// all resolutions are shown; choosing one of those would make the picture
/// and the pen disagree.)
pub(crate) fn scale_of(id: u32, points_wide: f64) -> Option<f64> {
    let live = LIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let &(_, max) = live.iter().find(|d| d.0 == id)?;
    Some(scale_within(points_wide, max))
}

fn scale_within(points_wide: f64, max_pixels: u32) -> f64 {
    if points_wide * 2.0 <= f64::from(max_pixels) {
        2.0
    } else {
        1.0
    }
}

/// An extra display that exists until this value is dropped.
pub struct MacVirtualDisplay {
    display: Retained<AnyObject>,
    size: (u32, u32),
    step: usize,
    sharp: bool,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: the display object is reference-counted and only receives
// `applySettings:` and `displayID` from one thread at a time (the host
// keeps it behind a mutex); the system calls it on its own queue.
unsafe impl Send for MacVirtualDisplay {}

impl std::fmt::Debug for MacVirtualDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacVirtualDisplay")
            .field("id", &self.id())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

fn unavailable(what: &str) -> CaptureError {
    CaptureError::Unavailable(format!(
        "this macOS version can't create an extra display ({what} is missing)"
    ))
}

/// The class `name`, if it exists and its instances answer every selector
/// in `selectors`.
fn class(name: &CStr, selectors: &[Sel]) -> Result<&'static AnyClass, CaptureError> {
    let class = AnyClass::get(name).ok_or_else(|| unavailable(&name.to_string_lossy()))?;
    for &sel in selectors {
        if class.instance_method(sel).is_none() {
            return Err(unavailable(sel.name().to_str().unwrap_or("a method")));
        }
    }
    Ok(class)
}

impl MacVirtualDisplay {
    /// Whether this macOS has the API at all.
    pub fn supported() -> bool {
        Classes::find().is_ok()
    }

    /// Adds a display named `name` for a tablet area of `width × height`
    /// pixels, at size step `step` of [`SIZE_STEPS`].
    ///
    /// `sharp` tells the two kinds of extra screen apart (the whole drawing
    /// area, or what the decoder takes pixel for pixel): the system
    /// remembers the arrangement of each separately.
    pub fn new(
        name: &str,
        width: u32,
        height: u32,
        step: usize,
        sharp: bool,
    ) -> Result<Self, CaptureError> {
        let classes = Classes::find()?;
        let queue = DispatchQueue::new("org.inkbridge.virtual-display", None);
        // Room for every size step at 2x, in either orientation.
        let long = width.max(height);
        let max = points_at(long, long, SIZE_STEPS.len() - 1).0 * 2;
        let size = CGSize {
            width: f64::from(width) / PIXELS_PER_MM,
            height: f64::from(height) / PIXELS_PER_MM,
        };
        // SAFETY: the classes and selectors were checked by `Classes::find`;
        // argument types match the properties' declared types (`unsigned
        // int`, `CGSize`, `NSString *`, `dispatch_queue_t`).
        let display: Option<Retained<AnyObject>> = unsafe {
            let descriptor: Retained<AnyObject> = msg_send![classes.descriptor, new];
            let _: () = msg_send![&*descriptor, setQueue: &*queue];
            let _: () = msg_send![&*descriptor, setName: &*NSString::from_str(name)];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: max];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: max];
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: size];
            // Any fixed ids: the system remembers arrangement and scale by them.
            let _: () = msg_send![&*descriptor, setVendorID: 0x1b1b_u32];
            let _: () = msg_send![&*descriptor, setProductID: 0x1b01_u32];
            let _: () = msg_send![&*descriptor, setSerialNum: 1 + u32::from(sharp)];
            let alloc: objc2::rc::Allocated<AnyObject> = msg_send![classes.display, alloc];
            msg_send![alloc, initWithDescriptor: &*descriptor]
        };
        let display = display.ok_or_else(|| {
            CaptureError::Failed("the system refused to create an extra display".into())
        })?;
        let this = Self {
            display,
            size: (0, 0),
            step: usize::MAX,
            sharp,
            _queue: queue,
        };
        LIVE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((this.id(), max));
        this.resized(width, height, step)
    }

    /// Changes the display to suit a `width × height` area (the tablet
    /// was rotated) at size step `step`.
    pub fn resize(&mut self, width: u32, height: u32, step: usize) -> Result<(), CaptureError> {
        if (self.size, self.step) != ((width, height), step) {
            self.apply(width, height, step)?;
            self.size = (width, height);
            self.step = step;
        }
        Ok(())
    }

    fn resized(mut self, width: u32, height: u32, step: usize) -> Result<Self, CaptureError> {
        self.resize(width, height, step)?;
        Ok(self)
    }

    fn apply(&self, width: u32, height: u32, step: usize) -> Result<(), CaptureError> {
        let classes = Classes::find()?;
        // SAFETY: as in `new`: checked selectors, matching argument types
        // (`unsigned int`, `double`, `NSArray *`); debug builds verify them.
        let applied: bool = unsafe {
            let modes: Vec<Retained<AnyObject>> = virtual_modes(width, height, step)
                .into_iter()
                .map(|(w, h)| {
                    let alloc: objc2::rc::Allocated<AnyObject> = msg_send![classes.mode, alloc];
                    msg_send![alloc, initWithWidth: w, height: h, refreshRate: REFRESH_HZ]
                })
                .collect();
            let settings: Retained<AnyObject> = msg_send![classes.settings, new];
            let _: () = msg_send![&*settings, setHiDPI: 1_u32];
            let _: () = msg_send![&*settings, setModes: &*NSArray::from_retained_slice(&modes)];
            msg_send![&*self.display, applySettings: &*settings]
        };
        if applied {
            Ok(())
        } else {
            Err(CaptureError::Failed(format!(
                "the system refused a {width}x{height} extra display"
            )))
        }
    }

    /// Whether this is the pixel-for-pixel kind.
    pub fn sharp(&self) -> bool {
        self.sharp
    }

    /// The size step it shows.
    pub fn step(&self) -> usize {
        self.step
    }

    /// The tablet drawing area this display was last shaped for.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The display's id, as capture and the pen use it.
    pub fn id(&self) -> u32 {
        // SAFETY: `displayID` was checked by `Classes::find` and returns a
        // `CGDirectDisplayID` (`uint32_t`).
        unsafe { msg_send![&*self.display, displayID] }
    }
}

impl Drop for MacVirtualDisplay {
    fn drop(&mut self) {
        let id = self.id();
        LIVE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|d| d.0 != id);
    }
}

/// The four private classes, checked for the methods used here.
struct Classes {
    descriptor: &'static AnyClass,
    mode: &'static AnyClass,
    settings: &'static AnyClass,
    display: &'static AnyClass,
}

impl Classes {
    fn find() -> Result<Self, CaptureError> {
        Ok(Self {
            descriptor: class(
                c"CGVirtualDisplayDescriptor",
                &[
                    sel!(setQueue:),
                    sel!(setName:),
                    sel!(setMaxPixelsWide:),
                    sel!(setMaxPixelsHigh:),
                    sel!(setSizeInMillimeters:),
                    sel!(setVendorID:),
                    sel!(setProductID:),
                    sel!(setSerialNum:),
                ],
            )?,
            mode: class(
                c"CGVirtualDisplayMode",
                &[sel!(initWithWidth:height:refreshRate:)],
            )?,
            settings: class(
                c"CGVirtualDisplaySettings",
                &[sel!(setHiDPI:), sel!(setModes:)],
            )?,
            display: class(
                c"CGVirtualDisplay",
                &[
                    sel!(initWithDescriptor:),
                    sel!(applySettings:),
                    sel!(displayID),
                ],
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_steps_go_from_pixel_for_pixel_to_one_point_per_pixel() {
        // The decoder's 1920x1088: step 0 is exact, the last 1x.
        assert_eq!(points_at(1920, 1088, 0), (960, 544));
        assert_eq!(points_at(1920, 1088, 2), (1280, 724));
        assert_eq!(points_at(1920, 1088, SIZE_STEPS.len() - 1), (1920, 1088));
        // The whole 1920x1200 tablet at 1.5: the earlier default.
        assert_eq!(points_at(1920, 1200, 2), (1280, 800));
        // Portrait, odd sizes: even sides.
        let (w, h) = points_at(1201, 1999, 1);
        assert_eq!((w % 2, h % 2), (0, 0));
        assert!(w < h);
    }

    #[test]
    fn only_the_chosen_step_is_offered() {
        assert_eq!(virtual_modes(1920, 1088, 3), [points_at(1920, 1088, 3)]);
        assert_eq!(virtual_modes(1920, 1088, 99), [(1920, 1088)]);
    }

    #[test]
    fn every_step_is_drawn_at_2x() {
        // A 1920-pixel-wide tablet: the display holds 2x its widest step.
        let max = points_at(1920, 1920, SIZE_STEPS.len() - 1).0 * 2;
        for step in 0..SIZE_STEPS.len() {
            let (w, _) = points_at(1920, 1088, step);
            assert!(
                (scale_within(f64::from(w), max) - 2.0).abs() < f64::EPSILON,
                "step {step}"
            );
        }
        assert!((scale_within(3000.0, max) - 1.0).abs() < f64::EPSILON);
    }

    /// Adds a real extra display for a moment (windows may shift briefly).
    /// Run: `cargo test -p ib-capture --lib an_extra_display_appears -- --ignored --nocapture`
    #[test]
    #[ignore = "creates a real virtual display on this Mac"]
    fn an_extra_display_appears_and_goes_away() {
        let listed = |id: u32| {
            crate::mac_displays()
                .into_iter()
                .find(|d| d.id == u64::from(id))
        };
        // As in the host: the process has looked at the displays before.
        let before = crate::mac_displays().len();
        let mut display = MacVirtualDisplay::new("Inkbridge test", 1920, 1200, 2, false).unwrap();
        let id = display.id();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let seen = listed(id).expect("the display is listed");
        println!("landscape: {seen:?}");
        assert_eq!((seen.bounds.width, seen.bounds.height), (2560, 1600));

        display.resize(1200, 1920, 2).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let seen = listed(display.id()).expect("still listed after a resize");
        println!("portrait: {seen:?}");
        assert_eq!((seen.bounds.width, seen.bounds.height), (1600, 2560));

        // More space: a smaller step, still at 2x.
        display.resize(1200, 1920, 4).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let seen = listed(display.id()).expect("still listed after a new size");
        println!("more space: {seen:?}");
        assert_eq!((seen.bounds.width, seen.bounds.height), (2000, 3200));

        drop(display);
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(listed(id).is_none(), "the display is gone");
        assert_eq!(crate::mac_displays().len(), before);
    }
}
