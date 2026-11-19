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

/// The monitors, main one first. A display's id is the number Windows gives
/// it (`\\.\DISPLAY2` → 2), which the pen sink uses too. Bounds start at
/// (0, 0): regions are relative to the display they are on.
pub fn windows_displays() -> Result<Vec<Display>, CaptureError> {
    let primary = Monitor::primary()
        .map_err(|e| CaptureError::Unavailable(e.to_string()))?
        .as_raw_hmonitor();
    let monitors = Monitor::enumerate().map_err(|e| CaptureError::Unavailable(e.to_string()))?;
    let mut all: Vec<Display> = monitors
        .iter()
        .filter_map(|m| {
            let id = u64::try_from(m.index().ok()?).ok()?;
            let (width, height) = (m.width().ok()?, m.height().ok()?);
            let is_primary = m.as_raw_hmonitor() == primary;
            Some(Display {
                id,
                name: display_name(m.name().ok(), id),
                bounds: Region {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                primary: is_primary,
            })
        })
        .filter(|d| d.bounds.width > 0 && d.bounds.height > 0)
        .collect();
    // Main display first, then in Windows' numbering.
    all.sort_by_key(|d| (!d.primary, d.id));
    Ok(all)
}

/// The monitor's own name, or Windows' number for it.
fn display_name(friendly: Option<String>, id: u64) -> String {
    match friendly {
        Some(name) if !name.trim().is_empty() => format!("{} (display {id})", name.trim()),
        _ => format!("Display {id}"),
    }
}

/// The monitor with Windows' number `id`, or the primary one for `None`.
fn monitor(id: Option<u64>) -> Result<Monitor, CaptureError> {
    let Some(id) = id else {
        return Monitor::primary().map_err(|e| CaptureError::Unavailable(e.to_string()));
    };
    Monitor::enumerate()
        .map_err(|e| CaptureError::Unavailable(e.to_string()))?
        .into_iter()
        .find(|m| m.index().ok().and_then(|i| u64::try_from(i).ok()) == Some(id))
        .ok_or_else(|| CaptureError::Unavailable(format!("display {id} is not connected")))
}

/// Captures (a region of) one monitor, the primary one unless
/// [`on_display`](Self::on_display) chose another.
pub struct WindowsScreenSource {
    mailbox: Arc<Mailbox>,
    control: Option<CaptureControl<Handler, String>>,
    gpu: bool,
    display: Option<u64>,
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
            display: None,
        }
    }

    /// GPU frames (for a hardware encoder on the same device).
    pub fn new_gpu() -> Self {
        Self {
            mailbox: Arc::new(Mailbox::default()),
            control: None,
            gpu: true,
            display: None,
        }
    }

    /// Captures the monitor with Windows' number `id` (see
    /// [`windows_displays`]) instead of the primary one.
    #[must_use]
    pub fn on_display(mut self, id: u64) -> Self {
        self.display = Some(id);
        self
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
        windows_displays()
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        self.stop();
        let monitor = monitor(self.display)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_is_named_after_its_monitor_and_number() {
        assert_eq!(
            display_name(Some("AOC AG271QG4".into()), 2),
            "AOC AG271QG4 (display 2)"
        );
        assert_eq!(display_name(Some("  ".into()), 3), "Display 3");
        assert_eq!(display_name(None, 1), "Display 1");
    }

    /// Needs a desktop session (not CI): every connected monitor is listed
    /// once, the main one first.
    #[test]
    #[ignore = "needs a Windows desktop session"]
    fn the_monitors_are_listed_main_first() {
        let all = windows_displays().unwrap();
        assert!(all.first().is_some_and(|d| d.primary), "{all:?}");
        assert_eq!(all.iter().filter(|d| d.primary).count(), 1);
        eprintln!("{all:#?}");
    }
}
