//! X11 screen capture (Linux X11 sessions; Wayland uses the screen-cast
//! portal instead).
//!
//! - **Shared memory** (MIT-SHM): the X server writes pixels straight into
//!   memory we share with it, instead of streaming them through the socket.
//! - **Damage**: frames are only taken when the server reports a change, so
//!   a static screen costs nothing.
//!
//! Pure-Rust client (`x11rb`): no X11 development packages needed to build.

use std::time::{Duration, Instant};

use ib_protocol::Region;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::ImageFormat;
use x11rb::rust_connection::RustConnection;

use crate::{CaptureError, Display, Frame, FrameData, ScreenSource};

fn failed(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(e.to_string())
}

/// A System V shared-memory segment attached to us and to the X server.
struct ShmSegment {
    id: i32,
    ptr: *mut u8,
    len: usize,
    seg: u32,
}

impl ShmSegment {
    fn new(conn: &RustConnection, len: usize) -> Result<Self, CaptureError> {
        // SAFETY: plain SysV shm syscalls; the segment is marked for removal
        // right after both sides attach, so it can't leak if we crash.
        unsafe {
            let id = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
            if id < 0 {
                return Err(failed(std::io::Error::last_os_error()));
            }
            let ptr = libc::shmat(id, std::ptr::null(), 0);
            if ptr as isize == -1 {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                return Err(failed(std::io::Error::last_os_error()));
            }
            let seg = conn.generate_id().map_err(failed)?;
            let attached = conn
                .shm_attach(seg, id.cast_unsigned(), false)
                .map_err(failed)
                .and_then(|c| c.check().map_err(failed));
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            if let Err(e) = attached {
                libc::shmdt(ptr);
                return Err(e);
            }
            Ok(Self {
                id,
                ptr: ptr.cast(),
                len,
                seg,
            })
        }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: detaching our own mapping exactly once.
        unsafe { libc::shmdt(self.ptr.cast()) };
        let _ = self.id;
    }
}

struct Active {
    region: Region,
    shm: ShmSegment,
    damage: u32,
    /// Capture at least once after start (no damage event yet).
    pending: bool,
}

/// Captures a region of the X11 root window.
pub struct X11ScreenSource {
    conn: RustConnection,
    root: u32,
    size: (u16, u16),
    active: Option<Active>,
}

// SAFETY: the shared-memory pointer is only used through `&mut self`.
unsafe impl Send for X11ScreenSource {}

impl std::fmt::Debug for X11ScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11ScreenSource")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl X11ScreenSource {
    /// Connects to `$DISPLAY`.
    pub fn connect() -> Result<Self, CaptureError> {
        let (conn, screen) =
            RustConnection::connect(None).map_err(|e| CaptureError::Unavailable(e.to_string()))?;
        let s = &conn.setup().roots[screen];
        let (root, size) = (s.root, (s.width_in_pixels, s.height_in_pixels));
        conn.damage_query_version(1, 1)
            .map_err(failed)?
            .reply()
            .map_err(|e| {
                CaptureError::Unavailable(format!("X server lacks the DAMAGE extension: {e}"))
            })?;
        conn.shm_query_version()
            .map_err(failed)?
            .reply()
            .map_err(|e| CaptureError::Unavailable(format!("X server lacks MIT-SHM: {e}")))?;
        Ok(Self {
            conn,
            root,
            size,
            active: None,
        })
    }

    fn stop(&mut self) {
        if let Some(a) = self.active.take() {
            let _ = self.conn.damage_destroy(a.damage);
            let _ = self.conn.shm_detach(a.shm.seg);
            let _ = self.conn.flush();
        }
    }

    /// Drains pending events; true if anything was damaged.
    fn damaged(&mut self) -> Result<bool, CaptureError> {
        let mut any = false;
        while let Some(ev) = self.conn.poll_for_event().map_err(failed)? {
            if let Event::DamageNotify(_) = ev {
                any = true;
            }
        }
        if any && let Some(a) = &self.active {
            self.conn
                .damage_subtract(a.damage, x11rb::NONE, x11rb::NONE)
                .map_err(failed)?;
        }
        Ok(any)
    }

    fn grab(&mut self) -> Result<Frame, CaptureError> {
        let a = self
            .active
            .as_ref()
            .ok_or_else(|| failed("capture not started"))?;
        let r = a.region;
        let capture_t_us = ib_clock::now_us();
        let reply = self
            .conn
            .shm_get_image(
                self.root,
                i16::try_from(r.x).map_err(failed)?,
                i16::try_from(r.y).map_err(failed)?,
                u16::try_from(r.width).map_err(failed)?,
                u16::try_from(r.height).map_err(failed)?,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                a.shm.seg,
                0,
            )
            .map_err(failed)?
            .reply()
            .map_err(failed)?;
        if reply.depth != 24 && reply.depth != 32 {
            return Err(CaptureError::Unavailable(format!(
                "unsupported X11 colour depth {}",
                reply.depth
            )));
        }
        let len = (r.width * r.height * 4) as usize;
        // SAFETY: the server wrote `len` bytes into our segment before replying.
        let pixels = unsafe { std::slice::from_raw_parts(a.shm.ptr, len.min(a.shm.len)) }.to_vec();
        Ok(Frame {
            capture_t_us,
            width: r.width,
            height: r.height,
            data: FrameData::Cpu(pixels),
        })
    }
}

impl Drop for X11ScreenSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ScreenSource for X11ScreenSource {
    fn name(&self) -> &'static str {
        "x11-shm"
    }

    fn displays(&self) -> Result<Vec<Display>, CaptureError> {
        Ok(vec![Display {
            id: 0,
            name: "X11 screen".into(),
            bounds: Region {
                x: 0,
                y: 0,
                width: u32::from(self.size.0),
                height: u32::from(self.size.1),
            },
            primary: true,
        }])
    }

    fn start(&mut self, region: Region) -> Result<(), CaptureError> {
        self.stop();
        // Keep the region on screen (X11 rejects out-of-bounds reads).
        let (sw, sh) = (u32::from(self.size.0), u32::from(self.size.1));
        let x = u32::try_from(region.x.max(0))
            .unwrap_or(0)
            .min(sw.saturating_sub(2));
        let y = u32::try_from(region.y.max(0))
            .unwrap_or(0)
            .min(sh.saturating_sub(2));
        let region = Region {
            x: i32::try_from(x).unwrap_or(0),
            y: i32::try_from(y).unwrap_or(0),
            width: region.width.min(sw - x).max(2),
            height: region.height.min(sh - y).max(2),
        };
        let shm = ShmSegment::new(&self.conn, (region.width * region.height * 4) as usize)?;
        let damage = self.conn.generate_id().map_err(failed)?;
        self.conn
            .damage_create(damage, self.root, ReportLevel::NON_EMPTY)
            .map_err(failed)?;
        self.conn.flush().map_err(failed)?;
        self.active = Some(Active {
            region,
            shm,
            damage,
            pending: true,
        });
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let deadline = Instant::now() + timeout;
        loop {
            let first = self
                .active
                .as_mut()
                .is_some_and(|a| std::mem::take(&mut a.pending));
            if first || self.damaged()? {
                return self.grab().map(Some);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an X server (`$DISPLAY`): `cargo test -p ib-capture -- --ignored`.
    #[test]
    #[ignore = "needs an X server"]
    fn captures_a_region_then_only_on_change() {
        let mut s = X11ScreenSource::connect().expect("X server");
        s.start(Region {
            x: 0,
            y: 0,
            width: 64,
            height: 32,
        })
        .unwrap();
        let f = s
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("first frame");
        assert_eq!((f.width, f.height), (64, 32));
        let FrameData::Cpu(px) = f.data else {
            panic!("cpu frame expected")
        };
        assert_eq!(px.len(), 64 * 32 * 4);
    }
}
