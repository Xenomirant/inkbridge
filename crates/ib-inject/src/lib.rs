//! Pen injection into the host OS.
//!
//! [`PenSink`] is the boundary between platform-neutral code and the OS.
//! Backends (added per milestone, each behind `cfg(target_os)`):
//! - Windows: synthetic pointer device `PT_PEN` (M1), `SendInput` mouse fallback;
//! - macOS: `CGEvent` tablet pointer/proximity events (M4);
//! - Linux: uinput virtual tablet (Advanced option) and libei pointer (M3).
//!
//! [`FakePenSink`] records events for tests and the CI loopback pipeline.

use ib_geometry::DesktopPoint;
use ib_protocol::{PenFeatures, PenSample};

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

    /// Lifts the pen and leaves proximity. Called on disconnect, session end
    /// and errors, so no button or contact can stay "stuck" on the desktop.
    fn release(&mut self) -> Result<(), InjectError>;
}

/// Records everything it receives. Used by tests and the loopback pipeline.
#[derive(Debug, Default)]
pub struct FakePenSink {
    pub events: Vec<(PenSample, DesktopPoint)>,
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

    fn release(&mut self) -> Result<(), InjectError> {
        self.releases += 1;
        Ok(())
    }
}
