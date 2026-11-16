//! Windows capture via Windows Graphics Capture (`windows-capture` crate).
//!
//! WGC only delivers a frame when the screen changes, which is exactly the
//! "no change, no bandwidth" behaviour we want. The capture runs on its own
//! thread; the newest frame waits in a single-slot mailbox, so a slow
//! consumer always gets the latest picture rather than a backlog.
//!
//! M2a copies the cropped region to the CPU for the software encoder; the
//! hardware path (M2b) will hand the D3D11 texture straight to the encoder.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ib_protocol::Region;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame as WgcFrame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{CaptureError, Display, Frame, FrameData, ScreenSource};

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
}

struct Handler {
    flags: Flags,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            flags: ctx.flags,
            scratch: Vec::new(),
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
        let buffer = frame
            .buffer_crop(x0, y0, x1, y1)
            .map_err(|e| e.to_string())?;
        let pixels = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();
        let out = Frame {
            capture_t_us,
            width: x1 - x0,
            height: y1 - y0,
            data: FrameData::Cpu(pixels),
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
}

impl std::fmt::Debug for WindowsScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsScreenSource")
            .field("running", &self.control.is_some())
            .finish_non_exhaustive()
    }
}

impl WindowsScreenSource {
    pub fn new() -> Self {
        Self {
            mailbox: Arc::new(Mailbox::default()),
            control: None,
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
