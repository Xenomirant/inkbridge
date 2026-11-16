//! Screen capture.
//!
//! [`ScreenSource`] delivers frames of one display region. Backends (per
//! milestone, `cfg(target_os)`-gated): Windows Graphics Capture with a DXGI
//! fallback (M2), ScreenCaptureKit (M4), xdg-desktop-portal + `PipeWire` and
//! X11 (M3).
//!
//! Real backends hand out **GPU textures** ([`FrameData::Gpu`]) so the encoder
//! can consume them without a CPU copy; [`FrameData::Cpu`] exists for the
//! software encoder fallback and tests.

use std::time::Duration;

use ib_protocol::Region;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture unavailable: {0}")]
    Unavailable(String),
    #[error("capture permission denied: {0}")]
    PermissionDenied(String),
    #[error("capture failed: {0}")]
    Failed(String),
}

/// A display as seen by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub id: u64,
    pub name: String,
    /// Bounds in desktop coordinates (physical pixels).
    pub bounds: Region,
    pub primary: bool,
}

/// Opaque handle to a GPU texture owned by the capture backend (e.g. a D3D11
/// texture pointer or an `IOSurface` ID). Only the matching encoder backend
/// interprets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuHandle(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameData {
    /// Tightly packed BGRA8, `width * height * 4` bytes.
    Cpu(Vec<u8>),
    Gpu(GpuHandle),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Host clock, microseconds, when the OS composed this frame.
    pub capture_t_us: u64,
    pub width: u32,
    pub height: u32,
    pub data: FrameData,
}

pub trait ScreenSource: Send {
    fn name(&self) -> &'static str;

    fn displays(&self) -> Result<Vec<Display>, CaptureError>;

    /// Starts (or retargets) capture of `region`.
    fn start(&mut self, region: Region) -> Result<(), CaptureError>;

    /// Waits up to `timeout` for the next **changed** frame. `Ok(None)` means
    /// nothing changed, which is how a static screen costs zero bandwidth.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError>;
}

/// Produces solid-colour CPU frames whose colour changes every frame.
#[derive(Debug, Default)]
pub struct FakeScreenSource {
    region: Option<Region>,
    frame_no: u64,
}

impl ScreenSource for FakeScreenSource {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn displays(&self) -> Result<Vec<Display>, CaptureError> {
        Ok(vec![Display {
            id: 1,
            name: "Fake 1920x1080".into(),
            bounds: Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            primary: true,
        }])
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        self.region = Some(region);
        Ok(())
    }

    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let region = self
            .region
            .ok_or_else(|| CaptureError::Failed("capture not started".into()))?;
        self.frame_no += 1;
        let shade = (self.frame_no % 256) as u8;
        let px = (region.width as usize) * (region.height as usize);
        Ok(Some(Frame {
            capture_t_us: self.frame_no * 16_667,
            width: region.width,
            height: region.height,
            data: FrameData::Cpu([shade, shade, shade, 0xFF].repeat(px)),
        }))
    }
}
