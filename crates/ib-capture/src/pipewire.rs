//! Wayland screen capture: frames of a monitor granted through the
//! screen-cast portal ([`ib_portal`]), received from PipeWire.
//!
//! The compositor only sends a frame when something changed (that includes
//! the cursor, which is drawn into the frames), so a static screen costs
//! nothing. Frames arrive in shared memory; only the captured region is
//! copied out, on PipeWire's thread, and the newest frame replaces any
//! frame not yet taken (latest wins: the encoder never works on old pixels).
//!
//! Zero-copy DMA-BUF import into VA-API is a later optimisation.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ib_protocol::Region;
use pipewire as pw;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;

use crate::{CaptureError, Display, Frame, FrameData, ScreenSource};

fn failed(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(e.to_string())
}

/// State shared between PipeWire's thread and the capture caller.
#[derive(Default)]
struct Shared {
    /// Monitor size in pixels once the format is negotiated.
    size: Option<(u32, u32)>,
    /// What to copy out of each frame (None until `start`).
    region: Option<Region>,
    latest: Option<Frame>,
    /// Set when the stream ends or fails.
    error: Option<String>,
}

type Slot = Arc<(Mutex<Shared>, Condvar)>;

/// Stops PipeWire's thread when the last clone of the source is dropped.
struct Stopper {
    quit: Mutex<pw::channel::Sender<()>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Stopper {
    fn drop(&mut self) {
        let _ = self
            .quit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(());
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
    }
}

/// Captures a region of the monitor shared through the portal.
///
/// Clones share one PipeWire stream (the portal grants one per session), so
/// each tablet session can own a source without asking the user again.
/// Only one clone should capture at a time.
#[derive(Clone)]
pub struct PipeWireScreenSource {
    slot: Slot,
    _stopper: Arc<Stopper>,
    /// Monitor size in the compositor's logical coordinates, if reported.
    logical_size: Option<(i32, i32)>,
}

impl std::fmt::Debug for PipeWireScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireScreenSource")
            .field("logical_size", &self.logical_size)
            .finish_non_exhaustive()
    }
}

impl PipeWireScreenSource {
    /// Starts receiving the portal stream. Keep the [`ib_portal::Grant`]
    /// alive for as long as this source is used.
    pub fn new(stream: ib_portal::Stream) -> Result<Self, CaptureError> {
        Self::new_sized(stream, None)
    }

    /// Like [`new`](Self::new), asking for frames of exactly `size`: a
    /// virtual monitor takes the size its stream negotiates.
    pub fn new_sized(
        stream: ib_portal::Stream,
        size: Option<(u32, u32)>,
    ) -> Result<Self, CaptureError> {
        let slot: Slot = Arc::default();
        let (quit, quit_rx) = pw::channel::channel();
        let wanted = size;
        let ib_portal::Stream {
            fd, node_id, size, ..
        } = stream;
        let thread_slot = Arc::clone(&slot);
        let thread = std::thread::Builder::new()
            .name("inkbridge-pipewire".into())
            .spawn(move || {
                if let Err(e) = run(fd, node_id, wanted, &thread_slot, quit_rx) {
                    let (m, cv) = &*thread_slot;
                    m.lock().unwrap_or_else(PoisonError::into_inner).error = Some(e.to_string());
                    cv.notify_all();
                }
            })
            .map_err(failed)?;
        Ok(Self {
            slot,
            _stopper: Arc::new(Stopper {
                quit: Mutex::new(quit),
                thread: Mutex::new(Some(thread)),
            }),
            logical_size: size,
        })
    }

    /// Monitor size in pixels (waits briefly for format negotiation).
    fn pixel_size(&self) -> Result<(u32, u32), CaptureError> {
        let (m, cv) = &*self.slot;
        let guard = m.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = cv
            .wait_timeout_while(guard, Duration::from_secs(3), |s| {
                s.size.is_none() && s.error.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = &guard.error {
            return Err(CaptureError::Unavailable(e.clone()));
        }
        guard
            .size
            .ok_or_else(|| CaptureError::Unavailable("PipeWire sent no video format".into()))
    }

    /// Logical/pixel ratio: the portal's pointer works in logical
    /// coordinates, frames in pixels (differ with display scaling).
    pub fn logical_scale(&self) -> f64 {
        match (self.logical_size, self.pixel_size()) {
            (Some((lw, _)), Ok((pw_, _))) if lw > 0 && pw_ > 0 => f64::from(lw) / f64::from(pw_),
            _ => 1.0,
        }
    }
}

impl ScreenSource for PipeWireScreenSource {
    fn name(&self) -> &'static str {
        "wayland-pipewire"
    }

    fn displays(&self) -> Result<Vec<Display>, CaptureError> {
        let (width, height) = self.pixel_size()?;
        Ok(vec![Display {
            id: 0,
            name: "Shared monitor".into(),
            bounds: Region {
                x: 0,
                y: 0,
                width,
                height,
            },
            primary: true,
        }])
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        let (sw, sh) = self.pixel_size()?;
        let region = clamp_region(region, sw, sh);
        let (m, _) = &*self.slot;
        let mut s = m.lock().unwrap_or_else(PoisonError::into_inner);
        s.region = Some(region);
        s.latest = None;
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let deadline = Instant::now() + timeout;
        let (m, cv) = &*self.slot;
        let mut s = m.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(e) = &s.error {
                return Err(CaptureError::Failed(e.clone()));
            }
            if let Some(f) = s.latest.take() {
                return Ok(Some(f));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            s = cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Keeps `region` inside a `sw`×`sh` monitor (at least 2×2 pixels).
fn clamp_region(region: Region, sw: u32, sh: u32) -> Region {
    let x = u32::try_from(region.x.max(0))
        .unwrap_or(0)
        .min(sw.saturating_sub(2));
    let y = u32::try_from(region.y.max(0))
        .unwrap_or(0)
        .min(sh.saturating_sub(2));
    Region {
        x: i32::try_from(x).unwrap_or(0),
        y: i32::try_from(y).unwrap_or(0),
        width: region.width.min(sw - x).max(2),
        height: region.height.min(sh - y).max(2),
    }
}

/// Copies `region` out of a frame with `stride` bytes per row into tightly
/// packed BGRA. Returns None if the buffer is too small for the region.
fn crop(src: &[u8], stride: usize, region: Region) -> Option<Vec<u8>> {
    let (x, y) = (region.x as usize, region.y as usize);
    let (w, h) = (region.width as usize, region.height as usize);
    let row = w * 4;
    let last = (y + h - 1) * stride + x * 4 + row;
    if last > src.len() {
        return None;
    }
    let mut out = Vec::with_capacity(row * h);
    for r in 0..h {
        let start = (y + r) * stride + x * 4;
        out.extend_from_slice(&src[start..start + row]);
    }
    Some(out)
}

/// Formats offered to the compositor: both are BGRA in memory, which is
/// what the encoders take. (GNOME, KDE and wlroots all offer BGRx.)
fn format_params(fixed: Option<(u32, u32)>) -> Result<Vec<u8>, CaptureError> {
    let (default, min, max) = match fixed {
        Some((width, height)) => {
            let r = spa::utils::Rectangle { width, height };
            (r, r, r)
        }
        None => (
            spa::utils::Rectangle {
                width: 1920,
                height: 1080,
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1,
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192,
            },
        ),
    };
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            default,
            min,
            max
        ),
        // Variable rate: 0/1 lets the compositor send frames only on change.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| failed(format!("{e:?}")))?
    .0
    .into_inner())
}

/// PipeWire's thread: connects with the portal's fd and fills `slot`.
fn run(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    size: Option<(u32, u32)>,
    slot: &Slot,
    quit: pw::channel::Receiver<()>,
) -> Result<(), CaptureError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(failed)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(failed)?;
    let core = context.connect_fd_rc(fd, None).map_err(failed)?;
    let stream = pw::stream::StreamRc::new(
        core,
        "inkbridge-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(failed)?;

    let fmt_slot = Arc::clone(slot);
    let frame_slot = Arc::clone(slot);
    let state_slot = Arc::clone(slot);
    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .state_changed(move |_, _, _, new| {
            if let pw::stream::StreamState::Error(e) = new {
                let (m, cv) = &*state_slot;
                m.lock().unwrap_or_else(PoisonError::into_inner).error =
                    Some(format!("PipeWire stream: {e}"));
                cv.notify_all();
            }
        })
        .param_changed(move |_, info, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() || info.parse(param).is_err() {
                return;
            }
            let size = info.size();
            let max = info.max_framerate();
            let fps = f64::from(max.num) / f64::from(max.denom.max(1));
            tracing::info!(
                "screen cast: {}x{} {:?}, up to {fps:.2} fps",
                size.width,
                size.height,
                info.format(),
            );
            let (m, cv) = &*fmt_slot;
            m.lock().unwrap_or_else(PoisonError::into_inner).size = Some((size.width, size.height));
            cv.notify_all();
        })
        .process(move |stream, _| {
            let capture_t_us = ib_clock::now_us();
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            // Cursor-only or empty updates carry no pixels.
            if chunk.size() == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
            let stride = usize::try_from(chunk.stride()).unwrap_or(0);
            let (m, cv) = &*frame_slot;
            let Some(region) = m.lock().unwrap_or_else(PoisonError::into_inner).region else {
                return;
            };
            let Some(bytes) = data.data() else { return };
            let Some(pixels) = bytes
                .get(offset..offset + size)
                .and_then(|b| crop(b, stride, region))
            else {
                return;
            };
            let mut s = m.lock().unwrap_or_else(PoisonError::into_inner);
            s.latest = Some(Frame {
                capture_t_us,
                width: region.width,
                height: region.height,
                data: FrameData::Cpu(pixels),
            });
            cv.notify_all();
        })
        .register()
        .map_err(failed)?;

    let params = format_params(size)?;
    let mut params = [Pod::from_bytes(&params).ok_or_else(|| failed("bad format pod"))?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(failed)?;

    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| ml.quit());
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_takes_rows_with_stride() {
        // 4x3 frame, stride 20 bytes (4 px + 4 bytes padding); pixel value = index.
        let mut src = vec![0u8; 20 * 3];
        for y in 0..3 {
            for x in 0..4 {
                src[y * 20 + x * 4] = (y * 4 + x) as u8;
            }
        }
        let r = Region {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let out = crop(&src, 20, r).unwrap();
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!([out[0], out[4], out[8], out[12]], [5, 6, 9, 10]);
    }

    #[test]
    fn crop_rejects_short_buffers() {
        let r = Region {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        assert!(crop(&[0; 40], 16, r).is_none());
    }

    #[test]
    fn regions_are_clamped_to_the_monitor() {
        let r = clamp_region(
            Region {
                x: -10,
                y: 1000,
                width: 5000,
                height: 50,
            },
            1920,
            1080,
        );
        assert_eq!(
            r,
            Region {
                x: 0,
                y: 1000,
                width: 1920,
                height: 50
            }
        );
    }
}
