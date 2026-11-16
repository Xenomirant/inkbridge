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

#[cfg(all(target_os = "linux", feature = "wayland"))]
mod pipewire;
#[cfg(all(target_os = "linux", feature = "wayland"))]
pub use pipewire::PipeWireScreenSource;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(windows)]
pub use windows::WindowsScreenSource;
#[cfg(target_os = "linux")]
pub use x11::X11ScreenSource;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameData {
    /// Tightly packed BGRA8, `width * height * 4` bytes.
    Cpu(Vec<u8>),
    /// A GPU texture owned by the capture backend; only an encoder on the
    /// same device consumes it (zero-copy path).
    Gpu(GpuFrame),
}

/// Windows: a BGRA D3D11 texture plus the device it lives on. The device is
/// multithread-protected, so the encoder thread may use it.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuFrame {
    pub texture: ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    pub device: ::windows::Win32::Graphics::Direct3D11::ID3D11Device,
}

// SAFETY: D3D11 devices are free-threaded and this one is created with
// multithread protection enabled; textures are only used through it.
#[cfg(windows)]
unsafe impl Send for GpuFrame {}

/// Other platforms have no GPU path yet.
#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuFrame(());

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

/// The best capture backend on this OS, if any.
pub fn platform_source() -> Result<Box<dyn ScreenSource>, CaptureError> {
    #[cfg(windows)]
    {
        Ok(Box::new(WindowsScreenSource::new()))
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland sessions need the screen-cast portal (XWayland only sees
        // X11 apps, not the desktop): the host requests it at startup and
        // builds a `PipeWireScreenSource` from the granted stream.
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
            return Err(CaptureError::Unavailable(
                "Wayland needs the desktop's screen-sharing permission (portal), \
                 which wasn't granted or isn't built in"
                    .into(),
            ));
        }
        Ok(Box::new(X11ScreenSource::connect()?))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(CaptureError::Unavailable(
            "no screen capture backend for this OS yet (macOS in M4)".into(),
        ))
    }
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
            capture_t_us: ib_clock::now_us(),
            width: region.width,
            height: region.height,
            data: FrameData::Cpu([shade, shade, shade, 0xFF].repeat(px)),
        }))
    }
}
