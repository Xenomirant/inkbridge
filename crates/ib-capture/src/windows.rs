//! Windows capture via Windows Graphics Capture (`windows-capture` crate).
//!
//! WGC only delivers a frame when the screen changes, which is exactly the
//! "no change, no bandwidth" behaviour we want. The capture runs on its own
//! thread; the newest frame waits in a single-slot mailbox, so a slow
//! consumer always gets the latest picture rather than a backlog.
//!
//! Two output modes:
//! - **GPU** (hardware encoder): the region is copied GPU→GPU into one of a
//!   few textures owned by this backend and handed over as-is (zero-copy);
//! - **CPU** (software encoder): the region is read back as BGRA bytes.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ib_protocol::Region;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame as WgcFrame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{CaptureError, Display, Frame, FrameData, GpuFrame, ScreenSource};

/// Textures in flight: one being encoded, one waiting, one being written.
const GPU_POOL: usize = 3;

/// Latest-frame mailbox shared with the capture thread.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<Frame>>,
    ready: Condvar,
}

struct Flags {
    mailbox: Arc<Mailbox>,
    /// Crop rectangle relative to the monitor: (x0, y0, x1, y1).
    crop: (u32, u32, u32, u32),
    gpu: bool,
}

struct Handler {
    flags: Flags,
    scratch: Vec<u8>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Vec<ID3D11Texture2D>,
    next: usize,
}

impl Handler {
    /// Next texture of the pool (allocated on first use, matching `w × h`).
    fn pool_texture(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        if self.pool.first().is_some_and(|t| {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: valid texture, valid out pointer.
            unsafe { t.GetDesc(&mut d) };
            d.Width != w || d.Height != h
        }) {
            self.pool.clear();
        }
        if self.pool.len() < GPU_POOL {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // Video-processor input (format conversion) needs these.
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: valid descriptor and out pointer.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .map_err(|e| e.to_string())?;
            self.pool
                .push(tex.ok_or("CreateTexture2D returned nothing")?);
        }
        let t = self.pool[self.next % self.pool.len()].clone();
        self.next = self.next.wrapping_add(1);
        Ok(t)
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        // The encoder thread uses this device too: serialise access to it.
        if let Ok(mt) = ctx.device_context.cast::<ID3D11Multithread>() {
            // SAFETY: plain COM call on a valid interface.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        Ok(Self {
            flags: ctx.flags,
            scratch: Vec::new(),
            device: ctx.device,
            context: ctx.device_context,
            pool: Vec::new(),
            next: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WgcFrame<'_>,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let capture_t_us = ib_clock::now_us();
        let (x0, y0, x1, y1) = self.flags.crop;
        let (x1, y1) = (x1.min(frame.width()), y1.min(frame.height()));
        let (w, h) = (x1 - x0, y1 - y0);
        let data = if self.flags.gpu {
            let dst = self.pool_texture(w, h)?;
            let region = D3D11_BOX {
                left: x0,
                top: y0,
                front: 0,
                right: x1,
                bottom: y1,
                back: 1,
            };
            // SAFETY: both textures live on this device; the box lies inside the source.
            unsafe {
                self.context.CopySubresourceRegion(
                    &dst,
                    0,
                    0,
                    0,
                    0,
                    frame.as_raw_texture(),
                    0,
                    Some(&region),
                );
            }
            FrameData::Gpu(GpuFrame {
                texture: dst,
                device: self.device.clone(),
            })
        } else {
            let buffer = frame
                .buffer_crop(x0, y0, x1, y1)
                .map_err(|e| e.to_string())?;
            FrameData::Cpu(buffer.as_nopadding_buffer(&mut self.scratch).to_vec())
        };
        let out = Frame {
            capture_t_us,
            width: w,
            height: h,
            data,
        };
        let mailbox = &self.flags.mailbox;
        if let Ok(mut slot) = mailbox.slot.lock() {
            *slot = Some(out); // overwrite: only the newest frame matters
            mailbox.ready.notify_one();
        }
        Ok(())
    }
}

/// Captures (a region of) the primary monitor.
pub struct WindowsScreenSource {
    mailbox: Arc<Mailbox>,
    control: Option<CaptureControl<Handler, String>>,
    gpu: bool,
}

impl std::fmt::Debug for WindowsScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsScreenSource")
            .field("running", &self.control.is_some())
            .finish_non_exhaustive()
    }
}

impl WindowsScreenSource {
    /// CPU frames (for the software encoder).
    pub fn new() -> Self {
        Self {
            mailbox: Arc::new(Mailbox::default()),
            control: None,
            gpu: false,
        }
    }

    /// GPU frames (for a hardware encoder on the same device).
    pub fn new_gpu() -> Self {
        Self {
            mailbox: Arc::new(Mailbox::default()),
            control: None,
            gpu: true,
        }
    }

    fn stop(&mut self) {
        if let Some(c) = self.control.take() {
            let _ = c.stop();
        }
    }
}

impl Default for WindowsScreenSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowsScreenSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ScreenSource for WindowsScreenSource {
    fn name(&self) -> &'static str {
        "windows-graphics-capture"
    }

    fn displays(&self) -> Result<Vec<Display>, CaptureError> {
        let m = Monitor::primary().map_err(|e| CaptureError::Unavailable(e.to_string()))?;
        let (w, h) = (
            m.width().map_err(|e| CaptureError::Failed(e.to_string()))?,
            m.height()
                .map_err(|e| CaptureError::Failed(e.to_string()))?,
        );
        Ok(vec![Display {
            id: 0,
            name: m.name().unwrap_or_else(|_| "Primary display".into()),
            bounds: Region {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
            primary: true,
        }])
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        self.stop();
        let monitor = Monitor::primary().map_err(|e| CaptureError::Unavailable(e.to_string()))?;
        let x0 = u32::try_from(region.x.max(0)).unwrap_or(0);
        let y0 = u32::try_from(region.y.max(0)).unwrap_or(0);
        let crop = (x0, y0, x0 + region.width, y0 + region.height);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            Flags {
                mailbox: self.mailbox.clone(),
                crop,
                gpu: self.gpu,
            },
        );
        let control = Handler::start_free_threaded(settings)
            .map_err(|e| CaptureError::Unavailable(format!("{e:?}")))?;
        self.control = Some(control);
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let slot = self
            .mailbox
            .slot
            .lock()
            .map_err(|_| CaptureError::Failed("capture mailbox poisoned".into()))?;
        let (mut slot, _) = self
            .mailbox
            .ready
            .wait_timeout_while(slot, timeout, |s| s.is_none())
            .map_err(|_| CaptureError::Failed("capture mailbox poisoned".into()))?;
        Ok(slot.take())
    }
}
