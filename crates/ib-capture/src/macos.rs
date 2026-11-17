//! macOS screen capture with ScreenCaptureKit (macOS 12.3+).
//!
//! The system delivers a frame only when the screen content changed, so a
//! static desktop produces no frames. Needs the **Screen Recording**
//! permission, granted once in System Settings to the app that runs the
//! host (the terminal app when started from a terminal).
//!
//! Frames are copied to CPU memory (BGRA) here; the zero-copy path hands the
//! pixel buffer to VideoToolbox instead.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use ib_protocol::Region;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID,
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};

use crate::{CaptureError, Display, Frame, FrameData, ScreenSource};

/// How long the system may take to list displays or start a stream.
const SYSTEM_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FPS: i32 = 60;

/// The newest frame, replaced when the consumer is slower than the screen.
type Slot = Arc<(Mutex<Option<Frame>>, Condvar)>;

/// Moves an Objective-C object to another thread.
struct Sendable<T>(T);
// SAFETY: used only for ScreenCaptureKit objects (`SCDisplay`, `SCStream`,
// our output object), which the framework creates and calls on arbitrary
// queues; they are reference-counted and safe to use from any one thread
// at a time.
unsafe impl<T> Send for Sendable<T> {}

struct OutputIvars {
    slot: Slot,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "InkbridgeStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, ty: SCStreamOutputType) {
            if ty != SCStreamOutputType::Screen {
                return;
            }
            let capture_t_us = ib_clock::now_us();
            // Idle, blank and suspended samples carry no pixel buffer.
            if let Some(frame) = copy_frame(sample, capture_t_us) {
                let (lock, ready) = &*self.ivars().slot;
                if let Ok(mut slot) = lock.lock() {
                    *slot = Some(frame);
                    ready.notify_one();
                }
            }
        }
    }
);

impl StreamOutput {
    fn new(slot: Slot) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { slot });
        // SAFETY: plain `-[NSObject init]` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies the sample's BGRA pixels into a tightly packed frame.
fn copy_frame(sample: &CMSampleBuffer, capture_t_us: u64) -> Option<Frame> {
    // SAFETY: `sample` is a valid sample buffer for the duration of the callback.
    let buffer = unsafe { sample.image_buffer() }?;
    let flags = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: the buffer is retained by `buffer`; locked before and unlocked after reading.
    if unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) } != 0 {
        return None;
    }
    let (width, height) = (
        CVPixelBufferGetWidth(&buffer),
        CVPixelBufferGetHeight(&buffer),
    );
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    let row = width * 4;
    let mut data = Vec::new();
    if !base.is_null() && stride >= row && width > 0 && height > 0 {
        data.reserve_exact(row * height);
        for y in 0..height {
            // SAFETY: while locked, the base address points to `height` rows
            // of `stride` bytes; we read the first `row <= stride` bytes of row `y`.
            let line = unsafe { std::slice::from_raw_parts(base.add(y * stride), row) };
            data.extend_from_slice(line);
        }
    }
    // SAFETY: matches the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, flags) };
    if data.is_empty() {
        return None;
    }
    Some(Frame {
        capture_t_us,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        data: FrameData::Cpu(data),
    })
}

/// Main display: id, bounds in physical pixels and pixels per point.
fn main_display() -> (u32, Region, f64) {
    let id = CGMainDisplayID();
    let points = CGDisplayBounds(id).size;
    let mode = CGDisplayCopyDisplayMode(id);
    let mode = mode.as_deref();
    let (w, h) = (
        CGDisplayMode::pixel_width(mode),
        CGDisplayMode::pixel_height(mode),
    );
    let scale = if w > 0 && points.width > 0.0 {
        w as f64 / points.width
    } else {
        1.0
    };
    let bounds = Region {
        x: 0,
        y: 0,
        width: u32::try_from(w).unwrap_or(0),
        height: u32::try_from(h).unwrap_or(0),
    };
    (id, bounds, scale)
}

/// Keeps `region` inside `bounds` with at least 2×2 pixels.
fn clamp_region(region: Region, bounds: Region) -> Region {
    let x = region
        .x
        .clamp(0, bounds.width.saturating_sub(2).cast_signed());
    let y = region
        .y
        .clamp(0, bounds.height.saturating_sub(2).cast_signed());
    Region {
        x,
        y,
        width: region.width.min(bounds.width - x.cast_unsigned()).max(2),
        height: region.height.min(bounds.height - y.cast_unsigned()).max(2),
    }
}

/// A pixel region as the display-local rectangle in points that
/// ScreenCaptureKit expects.
fn source_rect(region: Region, scale: f64) -> CGRect {
    CGRect {
        origin: CGPoint {
            x: f64::from(region.x) / scale,
            y: f64::from(region.y) / scale,
        },
        size: CGSize {
            width: f64::from(region.width) / scale,
            height: f64::from(region.height) / scale,
        },
    }
}

fn describe(error: *mut NSError) -> Option<String> {
    // SAFETY: the framework passes either null or a valid error object.
    unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string())
}

/// Asks ScreenCaptureKit for the main display (blocks until it answers).
fn find_display(id: u32) -> Result<Retained<SCDisplay>, CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: the framework passes either null or a valid content object.
            let found = unsafe { content.as_ref() }
                .and_then(|c| {
                    // SAFETY: plain property reads on valid objects.
                    unsafe { c.displays() }
                        .iter()
                        .find(|d| unsafe { d.displayID() } == id)
                })
                .map(Sendable)
                .ok_or_else(|| describe(error).unwrap_or_else(|| "main display not found".into()));
            let _ = tx.send(found);
        },
    );
    // SAFETY: the block stays alive for the call and is copied by the framework.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(SYSTEM_TIMEOUT) {
        Ok(Ok(display)) => Ok(display.0),
        Ok(Err(e)) => Err(CaptureError::Failed(format!("listing displays: {e}"))),
        Err(_) => Err(CaptureError::Failed(
            "macOS did not answer the display query".into(),
        )),
    }
}

struct Active {
    stream: Retained<SCStream>,
    // Kept alive while the stream delivers to them.
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

pub struct MacScreenSource {
    display_id: u32,
    bounds: Region,
    scale: f64,
    slot: Slot,
    active: Option<Sendable<Active>>,
}

impl std::fmt::Debug for MacScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacScreenSource")
            .field("bounds", &self.bounds)
            .field("scale", &self.scale)
            .field("active", &self.active.is_some())
            .finish_non_exhaustive()
    }
}

impl MacScreenSource {
    /// Fails with a fix for the user when the Screen Recording permission
    /// is missing, and asks macOS to show its prompt.
    pub fn new() -> Result<Self, CaptureError> {
        if !CGPreflightScreenCaptureAccess() {
            // Shows the system prompt (once per app); the grant applies
            // after the host is restarted.
            CGRequestScreenCaptureAccess();
            return Err(CaptureError::PermissionDenied(
                "macOS needs the Screen Recording permission to show the screen on the tablet. \
                 Allow it in System Settings > Privacy & Security > Screen & System Audio \
                 Recording for the app that runs inkbridge-host (your terminal app), then start \
                 the host again"
                    .into(),
            ));
        }
        let (display_id, bounds, scale) = main_display();
        if bounds.width == 0 || bounds.height == 0 {
            return Err(CaptureError::Unavailable("no display found".into()));
        }
        Ok(Self {
            display_id,
            bounds,
            scale,
            slot: Arc::default(),
            active: None,
        })
    }

    fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            // SAFETY: valid stream; no completion handler needed.
            unsafe { active.0.stream.stopCaptureWithCompletionHandler(None) };
        }
        if let Ok(mut slot) = self.slot.0.lock() {
            *slot = None;
        }
    }
}

impl Drop for MacScreenSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ScreenSource for MacScreenSource {
    fn name(&self) -> &'static str {
        "macos-screencapturekit"
    }

    fn displays(&self) -> Result<Vec<Display>, CaptureError> {
        Ok(vec![Display {
            id: u64::from(self.display_id),
            name: "Main display".into(),
            bounds: self.bounds,
            primary: true,
        }])
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        self.stop();
        let region = clamp_region(region, self.bounds);
        let display = find_display(self.display_id)?;

        // SAFETY: plain initialisers and property setters on valid objects.
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(region.width as usize);
            config.setHeight(region.height as usize);
            if region != self.bounds {
                config.setSourceRect(source_rect(region, self.scale));
            }
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(true);
            config.setQueueDepth(3);
            config.setMinimumFrameInterval(CMTime::new(1, MAX_FPS));
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };
        let output = StreamOutput::new(Arc::clone(&self.slot));
        let queue = DispatchQueue::new("org.inkbridge.capture", None);
        // SAFETY: `output` and `queue` outlive the stream (kept in `Active`).
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| CaptureError::Failed(e.localizedDescription().to_string()))?;

        let (tx, rx) = mpsc::channel();
        let started = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(describe(error));
        });
        // SAFETY: the block stays alive for the call and is copied by the framework.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        match rx.recv_timeout(SYSTEM_TIMEOUT) {
            Ok(None) => {}
            Ok(Some(e)) => return Err(CaptureError::Failed(format!("starting capture: {e}"))),
            Err(_) => {
                return Err(CaptureError::Failed(
                    "macOS did not start the capture".into(),
                ));
            }
        }
        self.active = Some(Sendable(Active {
            stream,
            _output: output,
            _queue: queue,
        }));
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        if self.active.is_none() {
            return Err(CaptureError::Failed("capture not started".into()));
        }
        let (lock, ready) = &*self.slot;
        let slot = lock
            .lock()
            .map_err(|_| CaptureError::Failed("capture thread panicked".into()))?;
        let (mut slot, _) = ready
            .wait_timeout_while(slot, timeout, |f| f.is_none())
            .map_err(|_| CaptureError::Failed("capture thread panicked".into()))?;
        Ok(slot.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETINA: Region = Region {
        x: 0,
        y: 0,
        width: 3024,
        height: 1964,
    };

    #[test]
    fn regions_are_kept_on_the_display() {
        let r = clamp_region(
            Region {
                x: 3000,
                y: -50,
                width: 500,
                height: 5000,
            },
            RETINA,
        );
        assert_eq!(
            r,
            Region {
                x: 3000,
                y: 0,
                width: 24,
                height: 1964
            }
        );
        assert_eq!(clamp_region(RETINA, RETINA), RETINA);
    }

    #[test]
    fn source_rect_is_in_points() {
        let r = source_rect(
            Region {
                x: 200,
                y: 100,
                width: 1600,
                height: 1000,
            },
            2.0,
        );
        assert_eq!((r.origin.x, r.origin.y), (100.0, 50.0));
        assert_eq!((r.size.width, r.size.height), (800.0, 500.0));
    }

    /// Captures the real screen for a second. Needs a desktop session and the
    /// Screen Recording permission for the terminal app. Move a window while
    /// it runs so frames are produced:
    /// `cargo test -p ib-capture -- --ignored --nocapture captures_the_main_display`
    #[test]
    #[ignore = "needs a real display and the Screen Recording permission"]
    fn captures_the_main_display() {
        let mut source = MacScreenSource::new().expect("Screen Recording permission");
        let bounds = source.displays().unwrap()[0].bounds;
        source.start(bounds).unwrap();
        let mut frames = 0;
        let end = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < end {
            if let Some(f) = source.next_frame(Duration::from_millis(100)).unwrap() {
                assert_eq!((f.width, f.height), (bounds.width, bounds.height));
                let FrameData::Cpu(data) = &f.data else {
                    panic!("expected a CPU frame")
                };
                assert_eq!(data.len(), (f.width * f.height * 4) as usize);
                frames += 1;
            }
        }
        println!(
            "{frames} frames in 1 s at {}x{}",
            bounds.width, bounds.height
        );
        assert!(
            frames > 0,
            "no frame arrived (the first one is always sent)"
        );
    }
}
