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

/// The sizes in points a virtual display offers for a tablet whose drawing
/// area is `width × height` pixels. The first is the default: two thirds
/// of the tablet's pixels, drawn at 2x, which keeps text readable on a
/// small sharp panel. The others (half size at 2x, full size at 1x) can be
/// chosen in System Settings.
pub fn virtual_modes(width: u32, height: u32) -> [(u32, u32); 3] {
    let even = |v: u32| (v.max(2) / 2) * 2;
    [
        (even(width * 2 / 3), even(height * 2 / 3)),
        (even(width / 2), even(height / 2)),
        (even(width), even(height)),
    ]
}

/// Live virtual displays: id and the widest picture each can hold.
static LIVE: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Pixels per point of one of our virtual displays that is `points_wide`
/// points wide, or `None` for any other display.
///
/// The display's modes are drawn at 2x wherever that fits its pixel
/// limit, which [`MacVirtualDisplay::new`] sets so that only the
/// full-size mode doesn't fit. (Each mode also exists at 1x, hidden in
/// System Settings unless all resolutions are shown; choosing one of
/// those would make the picture and the pen disagree.)
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

    /// Adds a display named `name` sized for a tablet whose drawing area
    /// is `width × height` pixels.
    pub fn new(name: &str, width: u32, height: u32) -> Result<Self, CaptureError> {
        let classes = Classes::find()?;
        let queue = DispatchQueue::new("org.inkbridge.virtual-display", None);
        // Room for the default mode at 2x in either orientation, and for
        // the full-size mode at 1x only (see `scale_of`).
        let long = width.max(height);
        let max = (virtual_modes(long, long)[0].0 * 2).max(long);
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
            let _: () = msg_send![&*descriptor, setSerialNum: 1_u32];
            let alloc: objc2::rc::Allocated<AnyObject> = msg_send![classes.display, alloc];
            msg_send![alloc, initWithDescriptor: &*descriptor]
        };
        let display = display.ok_or_else(|| {
            CaptureError::Failed("the system refused to create an extra display".into())
        })?;
        let this = Self {
            display,
            size: (0, 0),
            _queue: queue,
        };
        LIVE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((this.id(), max));
        this.resized(width, height)
    }

    /// Changes the display to suit a `width × height` drawing area (the
    /// tablet was rotated).
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        if self.size != (width, height) {
            self.apply(width, height)?;
            self.size = (width, height);
        }
        Ok(())
    }

    fn resized(mut self, width: u32, height: u32) -> Result<Self, CaptureError> {
        self.resize(width, height)?;
        Ok(self)
    }

    fn apply(&self, width: u32, height: u32) -> Result<(), CaptureError> {
        let classes = Classes::find()?;
        // SAFETY: as in `new`: checked selectors, matching argument types
        // (`unsigned int`, `double`, `NSArray *`); debug builds verify them.
        let applied: bool = unsafe {
            let modes: Vec<Retained<AnyObject>> = virtual_modes(width, height)
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
    fn the_default_mode_is_two_thirds_of_the_tablet_with_even_sides() {
        assert_eq!(
            virtual_modes(1920, 1200),
            [(1280, 800), (960, 600), (1920, 1200)]
        );
        // Portrait, odd sizes.
        let [(w, h), ..] = virtual_modes(1201, 1999);
        assert_eq!((w % 2, h % 2), (0, 0));
        assert!(w < h);
    }

    #[test]
    fn modes_are_drawn_at_2x_except_the_full_size_one() {
        // A 1920x1200 tablet: the display holds 2560 pixels across.
        assert!((scale_within(1280.0, 2560) - 2.0).abs() < f64::EPSILON);
        assert!((scale_within(960.0, 2560) - 2.0).abs() < f64::EPSILON);
        assert!((scale_within(1920.0, 2560) - 1.0).abs() < f64::EPSILON);
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
        let mut display = MacVirtualDisplay::new("Inkbridge test", 1920, 1200).unwrap();
        let id = display.id();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let seen = listed(id).expect("the display is listed");
        println!("landscape: {seen:?}");
        assert_eq!((seen.bounds.width, seen.bounds.height), (2560, 1600));

        display.resize(1200, 1920).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let seen = listed(display.id()).expect("still listed after a resize");
        println!("portrait: {seen:?}");
        assert_eq!((seen.bounds.width, seen.bounds.height), (1600, 2560));

        drop(display);
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(listed(id).is_none(), "the display is gone");
        assert_eq!(crate::mac_displays().len(), before);
    }
}
