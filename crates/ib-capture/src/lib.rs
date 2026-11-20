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
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{MacScreenSource, mac_displays, screen_access_granted};
#[cfg(target_os = "macos")]
mod macos_virtual;
#[cfg(target_os = "macos")]
pub use macos_virtual::{MacVirtualDisplay, SIZE_STEPS, points_at, virtual_modes};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_virtual;
#[cfg(windows)]
pub use windows_virtual::{
    COMMON_MODES, WindowsVirtualDisplay, detach_virtual_monitor,
    monitor_present as vdd_monitor_present, render_gpu as vdd_render_gpu,
    settings_modes as vdd_settings_modes, settings_xml as vdd_settings_xml,
};
#[cfg(target_os = "linux")]
mod x11;
#[cfg(windows)]
pub use windows::{WindowsScreenSource, windows_displays};
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

/// macOS: an IOSurface-backed NV12 pixel buffer straight from
/// ScreenCaptureKit, which VideoToolbox encodes without a CPU copy.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct GpuFrame {
    pub buffer: objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>,
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for GpuFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuFrame").finish_non_exhaustive()
    }
}

/// Two frames are equal when they are the same pixel buffer.
#[cfg(target_os = "macos")]
impl PartialEq for GpuFrame {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(&raw const *self.buffer, &raw const *other.buffer)
    }
}

#[cfg(target_os = "macos")]
impl Eq for GpuFrame {}

// SAFETY: pixel buffers are reference-counted with atomic counts and the
// capture side never writes to one after handing it out.
#[cfg(target_os = "macos")]
unsafe impl Send for GpuFrame {}

/// Other platforms have no GPU path yet.
#[cfg(not(any(windows, target_os = "macos")))]
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

    /// A hint for the next [`start`](Self::start): the size the encoder
    /// wants. Backends where the system scales for free deliver frames of
    /// that size; the others ignore it and deliver the region's own size.
    fn set_output_size(&mut self, _size: Option<(u32, u32)>) {}

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
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(MacScreenSource::new()?))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err(CaptureError::Unavailable(
            "no screen capture backend for this OS".into(),
        ))
    }
}

/// Produces solid-colour CPU frames whose colour changes every frame.
#[derive(Debug, Default)]
pub struct FakeScreenSource {
    region: Option<Region>,
    frame_no: u64,
    /// The screen stops changing after this many frames.
    still_after: Option<u64>,
}

impl FakeScreenSource {
    /// A screen that changes `frames` times and then stays still (no
    /// further frames, as the real backends behave).
    pub fn still_after(frames: u64) -> Self {
        Self {
            still_after: Some(frames),
            ..Self::default()
        }
    }
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

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let region = self
            .region
            .ok_or_else(|| CaptureError::Failed("capture not started".into()))?;
        if self.still_after.is_some_and(|n| self.frame_no >= n) {
            std::thread::sleep(timeout);
            return Ok(None);
        }
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
