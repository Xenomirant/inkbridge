//! Windows pen injection via a synthetic pointer device (`PT_PEN`).
//!
//! Requires Windows 10 1809+. No admin rights or drivers. Applications see a
//! real pen: pressure, tilt, rotation, barrel button and eraser, Windows Ink
//! and handwriting recognition all work.
//!
//! Limits: by design Windows doesn't let a normal process inject into
//! elevated (admin) windows (UIPI); see the Advanced options in
//! `docs/architecture.md`. Coordinates are physical pixels, so the host
//! process must be per-monitor DPI aware.
//!
//! Flag sequences follow Microsoft's documentation for
//! `InjectSyntheticPointerInput`; Sunshine's GPL implementation was used as a
//! cross-check of the conventions, no code was copied.

use ib_geometry::DesktopPoint;
use ib_protocol::{Buttons, PenFeatures, PenSample, Tool};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_CHANGE_FIRSTBUTTON_DOWN, POINTER_CHANGE_FIRSTBUTTON_UP,
    POINTER_CHANGE_NONE, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON, POINTER_FLAG_INCONTACT,
    POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE, PEN_MASK_ROTATION,
    PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN,
};

use crate::tracker::{PenTracker, Step};
use crate::{InjectError, PenSink};

/// Windows reports pen pressure in `0..=1024`.
const MAX_PRESSURE: f32 = 1024.0;

/// Wheel units per pixel of finger movement. One notch (`WHEEL_DELTA` = 120)
/// scrolls roughly 100 px in most apps, so content tracks the fingers.
const WHEEL_PER_PX: f64 = 1.2;

#[derive(Debug)]
pub struct WindowsPenSink {
    device: HSYNTHETICPOINTERDEVICE,
    tracker: PenTracker,
    last_point: POINT,
    /// Fractional wheel units not yet sent (smooth scrolling sends
    /// sub-notch amounts; apps without high-resolution support accumulate).
    wheel_rest: (f64, f64),
}

// SAFETY: a synthetic pointer device handle is a process-wide user32 object,
// not tied to the creating thread. The host uses each sink from one pen thread.
unsafe impl Send for WindowsPenSink {}

impl WindowsPenSink {
    pub fn new() -> Result<Self, InjectError> {
        // SAFETY: plain FFI call with valid constant arguments.
        let device = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .map_err(|e| {
            InjectError::Unavailable(format!(
                "Windows refused to create a synthetic pen (needs Windows 10 1809 or newer): {e}"
            ))
        })?;
        Ok(Self {
            device,
            tracker: PenTracker::new(),
            last_point: POINT::default(),
            wheel_rest: (0.0, 0.0),
        })
    }

    fn inject(&self, step: Step, sample: &PenSample, point: POINT) -> Result<(), InjectError> {
        let mut flags = POINTER_FLAG_PRIMARY;
        if step.in_range {
            flags |= POINTER_FLAG_INRANGE;
        }
        if step.in_contact {
            flags |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
        }
        flags |= if step.down {
            POINTER_FLAG_DOWN
        } else if step.up {
            POINTER_FLAG_UP
        } else {
            POINTER_FLAG_UPDATE
        };
        let button_change = if step.down {
            POINTER_CHANGE_FIRSTBUTTON_DOWN
        } else if step.up {
            POINTER_CHANGE_FIRSTBUTTON_UP
        } else {
            POINTER_CHANGE_NONE
        };

        let mut pen_flags = 0;
        if sample.buttons.contains(Buttons::BARREL) {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if sample.tool == Tool::Eraser && step.in_range {
            pen_flags |= PEN_FLAG_INVERTED;
            if step.in_contact {
                pen_flags |= PEN_FLAG_ERASER;
            }
        }

        let pressure = if step.in_contact {
            (sample.pressure * MAX_PRESSURE).round() as u32
        } else {
            0
        };
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerFlags: POINTER_FLAGS(flags.0),
                        ptPixelLocation: point,
                        ButtonChangeType: button_change,
                        ..Default::default()
                    },
                    penFlags: pen_flags,
                    penMask: PEN_MASK_PRESSURE
                        | PEN_MASK_ROTATION
                        | PEN_MASK_TILT_X
                        | PEN_MASK_TILT_Y,
                    pressure: pressure.min(1024),
                    rotation: (sample.rotation.round() as u32) % 360,
                    tiltX: sample.tilt_x.round() as i32,
                    tiltY: sample.tilt_y.round() as i32,
                },
            },
        };
        // SAFETY: `device` is valid for the lifetime of `self`; `info` is a
        // fully initialised pen record.
        unsafe { InjectSyntheticPointerInput(self.device, &[info]) }
            .map_err(|e| InjectError::Rejected(e.to_string()))
    }
}

impl PenSink for WindowsPenSink {
    fn name(&self) -> &'static str {
        "windows-pointer"
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
        let point = POINT {
            x: at.x.round() as i32,
            y: at.y.round() as i32,
        };
        self.last_point = point;
        for step in self.tracker.step(sample.state) {
            self.inject(step, sample, point)?;
        }
        Ok(())
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        // Fingers down → content down → wheel up (positive). Fingers right →
        // content right → scroll left (negative horizontal wheel).
        let wy = self.wheel_rest.1 + dy_px * WHEEL_PER_PX;
        let wx = self.wheel_rest.0 - dx_px * WHEEL_PER_PX;
        let (iy, ix) = (wy.trunc() as i32, wx.trunc() as i32);
        self.wheel_rest = (wx - f64::from(ix), wy - f64::from(iy));
        let wheel = |amount: i32, flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    mouseData: amount.cast_unsigned(),
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        let mut inputs = Vec::with_capacity(2);
        if iy != 0 {
            inputs.push(wheel(iy, MOUSEEVENTF_WHEEL));
        }
        if ix != 0 {
            inputs.push(wheel(ix, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        // INPUT is a few dozen bytes; the size always fits in i32.
        let size = i32::try_from(size_of::<INPUT>()).expect("INPUT size fits in i32");
        // SAFETY: `inputs` is a valid slice of fully initialised INPUT records.
        let sent = unsafe { SendInput(&inputs, size) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(InjectError::Rejected(
                "SendInput dropped scroll events".into(),
            ))
        }
    }

    fn release(&mut self) -> Result<(), InjectError> {
        let leave = PenSample {
            state: ib_protocol::PenState::OutOfRange,
            ..PenSample::default()
        };
        for step in self.tracker.step(leave.state) {
            self.inject(step, &leave, self.last_point)?;
        }
        Ok(())
    }
}

impl Drop for WindowsPenSink {
    fn drop(&mut self) {
        let _ = self.release();
        // SAFETY: the handle came from CreateSyntheticPointerDevice and is
        // destroyed exactly once.
        unsafe { DestroySyntheticPointerDevice(self.device) };
    }
}
