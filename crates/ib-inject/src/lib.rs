//! Pen injection into the host OS.
//!
//! [`PenSink`] is the boundary between platform-neutral code and the OS.
//! Backends (added per milestone, each behind `cfg(target_os)`):
//! - Windows: synthetic pointer device `PT_PEN` (M1), `SendInput` mouse fallback;
//! - macOS: `CGEvent` tablet pointer/proximity events (M4);
//! - Linux: uinput virtual tablet (Advanced option) and libei pointer (M3).
//!
//! [`FakePenSink`] records events for tests and the CI loopback pipeline.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(target_os = "linux", feature = "wayland"))]
mod portal;
mod tracker;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::UinputPenSink;
#[cfg(target_os = "macos")]
pub use macos::{MacPenSink, main_display, pen_access_granted};
#[cfg(all(target_os = "linux", feature = "wayland"))]
pub use portal::PortalPointerSink;
pub use tracker::{PenTracker, Step};
#[cfg(windows)]
pub use windows::WindowsPenSink;

use ib_geometry::DesktopPoint;
use ib_protocol::{PenFeatures, PenSample, TouchPhase};

/// One finger of a touch frame, already mapped to desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    pub id: u8,
    pub at: DesktopPoint,
    pub phase: TouchPhase,
}

/// The best pen backend available on this OS. `desktop` is the bounding box
/// of all monitors in physical pixels (needed by absolute-device backends).
pub fn platform_sink(desktop: ib_geometry::Region) -> Result<Box<dyn PenSink>, InjectError> {
    #[cfg(windows)]
    {
        let _ = desktop;
        Ok(Box::new(WindowsPenSink::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(UinputPenSink::new(desktop)?))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = desktop;
        Ok(Box::new(MacPenSink::new()))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = desktop;
        Err(InjectError::Unavailable(
            "no pen backend for this OS".into(),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    /// The backend isn't usable on this system (missing API, permission not
    /// granted). The message is shown to the user with a fix.
    #[error("pen injection unavailable: {0}")]
    Unavailable(String),
    /// The OS rejected one event; the session continues.
    #[error("pen event rejected by the OS: {0}")]
    Rejected(String),
}

/// A destination for pen events on the host.
///
/// Implementations must be cheap per call: `emit` runs on the dedicated
/// high-priority pen thread for every sample.
pub trait PenSink: Send {
    /// Human-readable backend name for diagnostics ("windows-pointer").
    fn name(&self) -> &'static str;

    /// Axes this backend can actually deliver to applications.
    fn features(&self) -> PenFeatures;

    /// Injects one sanitised sample at a desktop position.
    fn emit(&mut self, sample: &PenSample, at: DesktopPoint) -> Result<(), InjectError>;

    /// Injects one multi-finger touch frame (all fingers of the gesture).
    /// The OS turns it into native panning, inertia and pinch-zoom.
    fn touch(&mut self, _frame: &[TouchPoint]) -> Result<(), InjectError> {
        Err(InjectError::Unavailable(
            "touch input not supported by this backend".into(),
        ))
    }

    /// Scrolls the window under the pointer by a content movement in desktop
    /// pixels (positive `dy` = content moves down, i.e. scroll up; positive
    /// `dx` = content moves right, i.e. scroll left).
    fn scroll(&mut self, _dx_px: f64, _dy_px: f64) -> Result<(), InjectError> {
        Err(InjectError::Unavailable(
            "scrolling not supported by this backend".into(),
        ))
    }

    /// Puts the pen on another display (an id from the capture backend's
    /// display list, with its size in pixels): desktop positions are
    /// relative to it from now on. Backends that address the whole desktop
    /// ignore this.
    fn set_display(&mut self, _id: u64, _bounds: ib_geometry::Region) {}

    /// Moves the mouse pointer (so scrolling targets the window under the
    /// fingers).
    fn move_pointer(&mut self, _at: DesktopPoint) -> Result<(), InjectError> {
        Ok(())
    }

    /// Lifts the pen and leaves proximity. Called on disconnect, session end
    /// and errors, so no button or contact can stay "stuck" on the desktop.
    fn release(&mut self) -> Result<(), InjectError>;
}

/// Records everything it receives. Used by tests and the loopback pipeline.
#[derive(Debug, Default)]
pub struct FakePenSink {
    pub events: Vec<(PenSample, DesktopPoint)>,
    pub touch_frames: Vec<Vec<TouchPoint>>,
    pub scrolls: Vec<(f64, f64)>,
    pub pointer_moves: Vec<DesktopPoint>,
    pub releases: usize,
}

impl PenSink for FakePenSink {
    fn name(&self) -> &'static str {
        "fake"
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
        self.events.push((*sample, at));
        Ok(())
    }

    fn touch(&mut self, frame: &[TouchPoint]) -> Result<(), InjectError> {
        self.touch_frames.push(frame.to_vec());
        Ok(())
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        self.scrolls.push((dx_px, dy_px));
        Ok(())
    }

    fn move_pointer(&mut self, at: DesktopPoint) -> Result<(), InjectError> {
        self.pointer_moves.push(at);
        Ok(())
    }

    fn release(&mut self) -> Result<(), InjectError> {
        self.releases += 1;
        Ok(())
    }
}
