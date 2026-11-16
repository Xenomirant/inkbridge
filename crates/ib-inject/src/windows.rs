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
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_CHANGE_FIRSTBUTTON_DOWN, POINTER_CHANGE_FIRSTBUTTON_UP,
    POINTER_CHANGE_NONE, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_FLAGS, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE,
    PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA,
};

use crate::tracker::{PenTracker, Step};
use crate::{InjectError, PenSink, TouchPoint};
use ib_protocol::TouchPhase;

/// Windows reports pen pressure in `0..=1024`.
const MAX_PRESSURE: f32 = 1024.0;

/// Wheel units per pixel of content movement. One notch (`WHEEL_DELTA` =
/// 120) scrolls about 100 px in most apps, so content tracks the fingers;
/// modern apps honour sub-notch (high-resolution) amounts.
const WHEEL_PER_PX: f64 = 1.2;

/// Maximum simultaneous fingers on the synthetic touch device.
const MAX_TOUCHES: u32 = ib_protocol::TouchContact::MAX as u32;

#[derive(Debug)]
pub struct WindowsPenSink {
    device: HSYNTHETICPOINTERDEVICE,
    tracker: PenTracker,
    last_point: POINT,
    /// Synthetic touch device, created on first use.
    touch_device: Option<HSYNTHETICPOINTERDEVICE>,
    /// Wheel units not sent yet (apps without high-resolution support act
    /// on whole notches; the remainder carries over).
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
            touch_device: None,
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
        unsafe { InjectSyntheticPointerInput(self.device, &[info]) }.map_err(|e| {
            // Full detail, so a rejection can be diagnosed from the log.
            InjectError::Rejected(format!(
                "{e} [step {step:?}, flags {:#x}, pen flags {pen_flags:#x}, at ({}, {}), pressure {pressure}, \
                 tilt ({}, {}), tool {:?}]",
                flags.0, point.x, point.y, sample.tilt_x, sample.tilt_y, sample.tool
            ))
        })
    }

    /// Puts Windows' pen back into a known state (not touching, out of
    /// range) after a rejected event, so the pen can never stay "pressed"
    /// and turn hover into drags. Errors are ignored: this is best effort.
    fn resync(&mut self) {
        let leave = PenSample {
            state: ib_protocol::PenState::OutOfRange,
            ..PenSample::default()
        };
        let point = self.last_point;
        let was_down = self.tracker.in_contact();
        self.tracker = PenTracker::new();
        if was_down {
            let _ = self.inject(
                Step {
                    in_range: true,
                    up: true,
                    ..Step::default()
                },
                &leave,
                point,
            );
        }
        let _ = self.inject(Step::default(), &leave, point);
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
            if let Err(e) = self.inject(step, sample, point) {
                self.resync();
                return Err(e);
            }
        }
        Ok(())
    }

    fn touch(&mut self, frame: &[TouchPoint]) -> Result<(), InjectError> {
        if frame.is_empty() {
            return Ok(());
        }
        let device = if let Some(d) = self.touch_device {
            d
        } else {
            // SAFETY: plain FFI call with valid constant arguments.
            let d = unsafe {
                CreateSyntheticPointerDevice(PT_TOUCH, MAX_TOUCHES, POINTER_FEEDBACK_DEFAULT)
            }
            .map_err(|e| {
                InjectError::Unavailable(format!("Windows refused a synthetic touch device: {e}"))
            })?;
            self.touch_device = Some(d);
            d
        };
        let infos: Vec<POINTER_TYPE_INFO> = frame
            .iter()
            .take(MAX_TOUCHES as usize)
            .enumerate()
            .map(|(i, t)| {
                let flags = match t.phase {
                    TouchPhase::Down => {
                        POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                    TouchPhase::Move => {
                        POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                    TouchPhase::Up => POINTER_FLAG_UP,
                    TouchPhase::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                };
                let flags = if i == 0 {
                    flags | POINTER_FLAG_PRIMARY
                } else {
                    flags
                };
                let p = POINT {
                    x: t.at.x.round() as i32,
                    y: t.at.y.round() as i32,
                };
                POINTER_TYPE_INFO {
                    r#type: PT_TOUCH,
                    Anonymous: POINTER_TYPE_INFO_0 {
                        touchInfo: POINTER_TOUCH_INFO {
                            pointerInfo: POINTER_INFO {
                                pointerType: PT_TOUCH,
                                pointerId: u32::from(t.id),
                                pointerFlags: POINTER_FLAGS(flags.0),
                                ptPixelLocation: p,
                                ..Default::default()
                            },
                            touchMask: TOUCH_MASK_CONTACTAREA,
                            // A fingertip-sized contact area.
                            rcContact: RECT {
                                left: p.x - 4,
                                top: p.y - 4,
                                right: p.x + 4,
                                bottom: p.y + 4,
                            },
                            ..Default::default()
                        },
                    },
                }
            })
            .collect();
        // SAFETY: `device` is a live touch device and `infos` are fully
        // initialised touch records, one per finger of the frame.
        unsafe { InjectSyntheticPointerInput(device, &infos) }
            .map_err(|e| InjectError::Rejected(e.to_string()))
    }

    fn scroll(&mut self, dx_px: f64, dy_px: f64) -> Result<(), InjectError> {
        // Content down → wheel up (positive); content right → scroll left
        // (negative horizontal wheel).
        let wy = self.wheel_rest.1 + dy_px * WHEEL_PER_PX;
        let wx = self.wheel_rest.0 - dx_px * WHEEL_PER_PX;
        let (iy, ix) = (wy.trunc() as i32, wx.trunc() as i32);
        self.wheel_rest = (wx - f64::from(ix), wy - f64::from(iy));
        let mut inputs = Vec::with_capacity(2);
        if iy != 0 {
            inputs.push(mouse_input(0, 0, iy.cast_unsigned(), MOUSEEVENTF_WHEEL));
        }
        if ix != 0 {
            inputs.push(mouse_input(0, 0, ix.cast_unsigned(), MOUSEEVENTF_HWHEEL));
        }
        send_inputs(&inputs)
    }

    fn move_pointer(&mut self, at: DesktopPoint) -> Result<(), InjectError> {
        // Absolute coordinates are normalised to 0..=65535 over the virtual desktop.
        // SAFETY: plain FFI calls without pointers.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        };
        let nx = ((at.x - f64::from(vx)) * 65535.0 / f64::from(vw - 1).max(1.0)).round() as i32;
        let ny = ((at.y - f64::from(vy)) * 65535.0 / f64::from(vh - 1).max(1.0)).round() as i32;
        send_inputs(&[mouse_input(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
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
        if let Some(d) = self.touch_device.take() {
            // SAFETY: created once by CreateSyntheticPointerDevice, destroyed once.
            unsafe { DestroySyntheticPointerDevice(d) };
        }
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), InjectError> {
    if inputs.is_empty() {
        return Ok(());
    }
    // INPUT is a few dozen bytes; the size always fits in i32.
    let size = i32::try_from(size_of::<INPUT>()).expect("INPUT size fits in i32");
    // SAFETY: `inputs` is a valid slice of fully initialised INPUT records.
    let sent = unsafe { SendInput(inputs, size) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(InjectError::Rejected(
            "SendInput dropped input events".into(),
        ))
    }
}
