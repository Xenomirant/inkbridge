//! Dedicated pen-injection thread.
//!
//! Owns the [`PenSink`] and the [`PenStream`] reassembler. Inputs arrive over
//! a channel from the async network tasks; each sample is mapped to desktop
//! coordinates and injected immediately.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use ib_geometry::{DesktopPoint, Region, Rotation, to_desktop};
use ib_inject::{InjectError, PenSink, TouchPoint};
use ib_protocol::{PenBatch, PenSample, TouchContact};
use ib_session::PenStream;

use crate::scroll::ScrollEngine;
use crate::touch::{TouchMapper, TouchMapping};

/// How often resting fingers are re-sent so the OS keeps them down.
const TOUCH_KEEPALIVE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum PenInput {
    Batch(PenBatch),
    Transition(PenSample),
    SetRegion(Region),
    /// The tablet now shows another display (the capture backend's id).
    SetDisplay(u64, Region),
    /// Multi-finger gesture frame.
    Touch(Vec<TouchContact>),
    /// How finger gestures map to the desktop (per session: absolute when the
    /// tablet shows the screen, anchored at the cursor otherwise).
    SetTouchMapping(TouchMapping),
    /// Two-finger scroll movement (see `ControlMessage::Scroll`).
    Scroll {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        t_us: u64,
        end: bool,
    },
    /// Text typed on the tablet's keyboard.
    Text(String),
    /// A key or shortcut from the tablet.
    Key(ib_protocol::Key, ib_protocol::Modifiers),
    /// The tablet disconnected: release the pen and reset sequence state.
    Disconnected,
    Shutdown,
}

pub type SinkFactory = Box<dyn FnOnce() -> Result<Box<dyn PenSink>, InjectError> + Send>;

/// Starts the pen thread. The sink is created **on** the thread, so
/// backends with thread affinity are safe.
pub fn spawn(
    make_sink: SinkFactory,
    region: Region,
) -> std::io::Result<(Sender<PenInput>, JoinHandle<()>)> {
    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name("inkbridge-pen".into())
        .spawn(move || {
            crate::platform::raise_thread_priority();
            match make_sink() {
                Ok(sink) => run(sink, region, &rx),
                Err(e) => tracing::error!("pen injection unavailable: {e}"),
            }
        })?;
    Ok((tx, handle))
}

fn run(sink: Box<dyn PenSink>, region: Region, rx: &Receiver<PenInput>) {
    tracing::info!(backend = sink.name(), ?region, "pen thread ready");
    let mut t = PenThread::new(sink, region);
    loop {
        // Tick finely while inertia scrolls; otherwise just keep resting
        // fingers alive.
        let wait = if t.scroll.is_flinging() {
            FLING_TICK
        } else {
            TOUCH_KEEPALIVE
        };
        match rx.recv_timeout(wait) {
            Ok(PenInput::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(input) => t.handle(input),
            Err(RecvTimeoutError::Timeout) => t.on_idle(),
        }
    }
    let _ = t.sink.release();
}

/// Inertia tick interval (~120 Hz): smooth regardless of the network.
const FLING_TICK: Duration = Duration::from_millis(8);

/// State owned by the pen thread.
struct PenThread {
    sink: Box<dyn PenSink>,
    region: Region,
    stream: PenStream,
    touch: TouchMapper,
    mapping: TouchMapping,
    /// Where the pen last was (pointer position).
    pointer: DesktopPoint,
    scroll: ScrollEngine,
    /// A two-finger scroll is in progress (between first message and `end`).
    scrolling: bool,
    /// Scroll events were injected and the sink hasn't been told the
    /// gesture ended (fingers up and inertia over).
    scroll_open: bool,
}

impl PenThread {
    fn new(sink: Box<dyn PenSink>, region: Region) -> Self {
        Self {
            sink,
            region,
            stream: PenStream::new(),
            // Pen-only sessions show no image on the tablet: anchor at the cursor.
            touch: TouchMapper::new(TouchMapping::AnchoredAtPointer),
            mapping: TouchMapping::AnchoredAtPointer,
            pointer: DesktopPoint {
                x: f64::from(region.x),
                y: f64::from(region.y),
            },
            scroll: ScrollEngine::new(),
            scrolling: false,
            scroll_open: false,
        }
    }

    fn handle(&mut self, input: PenInput) {
        match input {
            PenInput::Batch(batch) => {
                for s in self.stream.on_batch(batch) {
                    self.pen(s);
                }
            }
            PenInput::Transition(t) => {
                if let Some(s) = self.stream.on_transition(t) {
                    self.pen(s);
                }
            }
            PenInput::SetRegion(r) => self.region = r,
            PenInput::SetDisplay(id, bounds) => {
                // Nothing may stay pressed on the display we leave.
                self.cancel_gestures();
                let _ = self.sink.release();
                self.sink.set_display(id, bounds);
            }
            PenInput::SetTouchMapping(m) => {
                self.cancel_gestures();
                self.mapping = m;
                self.touch = TouchMapper::new(m);
            }
            PenInput::Touch(contacts) => {
                let contacts: Vec<TouchContact> = contacts
                    .into_iter()
                    .filter(|c| c.x.is_finite() && c.y.is_finite())
                    .map(|c| TouchContact {
                        x: c.x.clamp(0.0, 1.0),
                        y: c.y.clamp(0.0, 1.0),
                        ..c
                    })
                    .collect();
                let frame = self.touch.map(&contacts, self.region, self.pointer);
                self.inject_touch(&frame);
            }
            PenInput::Scroll {
                x,
                y,
                dx,
                dy,
                t_us,
                end,
            } => self.on_scroll(x, y, dx, dy, t_us, end),
            PenInput::Text(text) => {
                // Content isn't logged; it is what the user types.
                tracing::debug!(chars = text.chars().count(), "typing text from the tablet");
                if let Err(e) = self.sink.type_text(&text) {
                    tracing::warn!("{e}");
                }
            }
            PenInput::Key(key, modifiers) => {
                tracing::debug!(?key, ?modifiers, "key from the tablet");
                if let Err(e) = self.sink.key(key, modifiers) {
                    tracing::warn!("{e}");
                }
            }
            PenInput::Disconnected => {
                self.cancel_gestures();
                if self.stream.release().is_some()
                    && let Err(e) = self.sink.release()
                {
                    tracing::warn!("{e}");
                }
                self.stream = PenStream::new();
            }
            PenInput::Shutdown => {}
        }
    }

    /// Nothing arrived for a while: advance inertia, keep resting fingers down.
    fn on_idle(&mut self) {
        if self.scroll.is_flinging() {
            let (dx, dy) = self.scroll.tick(ib_clock::now_us());
            self.inject_scroll(dx, dy);
            self.finish_scroll_if_over();
        }
        if let Some(frame) = self.touch.keep_alive() {
            let frame = frame.to_vec();
            self.inject_touch(&frame);
        }
    }

    fn pen(&mut self, s: PenSample) {
        if s.state == ib_protocol::PenState::Contact {
            self.scroll.stop(); // writing stops any inertia at once
            self.finish_scroll_if_over();
        }
        self.pointer = emit(&mut self.sink, s, self.region);
    }

    fn on_scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32, t_us: u64, end: bool) {
        if ![x, y, dx, dy].iter().all(|v| v.is_finite()) {
            return;
        }
        if !self.scrolling {
            self.scrolling = true;
            self.scroll.stop();
            // Scroll the window under the fingers (when the tablet shows the
            // screen); pen-only sessions scroll where the cursor already is.
            if self.mapping == TouchMapping::Absolute {
                let at = to_desktop(
                    self.region,
                    Rotation::None,
                    x.clamp(0.0, 1.0),
                    y.clamp(0.0, 1.0),
                );
                if let Err(e) = self.sink.move_pointer(at) {
                    tracing::debug!("{e}");
                }
            }
        }
        let px = (
            f64::from(dx) * f64::from(self.region.width),
            f64::from(dy) * f64::from(self.region.height),
        );
        let (sx, sy) = self.scroll.on_move(px.0, px.1, t_us);
        self.inject_scroll(sx, sy);
        if end {
            self.scrolling = false;
            self.scroll.on_end(t_us, ib_clock::now_us());
            self.finish_scroll_if_over();
        }
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        match self.sink.scroll(dx, dy) {
            Ok(()) => self.scroll_open = true,
            Err(e) => tracing::debug!("{e}"),
        }
    }

    /// Tells the sink the scroll gesture ended, once the fingers are up and
    /// inertia has stopped.
    fn finish_scroll_if_over(&mut self) {
        if self.scroll_open && !self.scrolling && !self.scroll.is_flinging() {
            self.scroll_open = false;
            if let Err(e) = self.sink.scroll_end() {
                tracing::debug!("{e}");
            }
        }
    }

    fn inject_touch(&mut self, frame: &[TouchPoint]) {
        if let Err(e) = self.sink.touch(frame) {
            tracing::debug!("{e}");
        }
    }

    fn cancel_gestures(&mut self) {
        let cancelled = self.touch.cancel();
        if !cancelled.is_empty() {
            self.inject_touch(&cancelled);
        }
        self.scroll.stop();
        self.scrolling = false;
        self.finish_scroll_if_over();
    }
}

/// Injects one pen sample; returns where it landed (the pointer position).
fn emit(sink: &mut Box<dyn PenSink>, s: PenSample, region: Region) -> DesktopPoint {
    let at = to_desktop(region, Rotation::None, s.x, s.y);
    // For measuring pen quality: RUST_LOG=ib_host::pen_samples=trace
    tracing::trace!(
        target: "ib_host::pen_samples",
        t_us = s.t_us,
        x = at.x,
        y = at.y,
        p = s.pressure,
        state = ?s.state,
        "sample"
    );
    if let Err(e) = sink.emit(&s, at) {
        tracing::warn!("{e}");
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_inject::{FakePenSink, InjectError};
    use ib_protocol::PenState;
    use std::sync::{Arc, Mutex};

    /// Shares the fake's recordings with the test after the thread exits.
    struct Shared(Arc<Mutex<FakePenSink>>);

    impl PenSink for Shared {
        fn name(&self) -> &'static str {
            "shared-fake"
        }
        fn features(&self) -> ib_protocol::PenFeatures {
            self.0.lock().unwrap().features()
        }
        fn emit(
            &mut self,
            s: &PenSample,
            at: ib_geometry::DesktopPoint,
        ) -> Result<(), InjectError> {
            self.0.lock().unwrap().emit(s, at)
        }
        fn release(&mut self) -> Result<(), InjectError> {
            self.0.lock().unwrap().release()
        }
        fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), InjectError> {
            self.0.lock().unwrap().scroll(dx, dy)
        }
        fn scroll_end(&mut self) -> Result<(), InjectError> {
            self.0.lock().unwrap().scroll_end()
        }
        fn move_pointer(&mut self, at: ib_geometry::DesktopPoint) -> Result<(), InjectError> {
            self.0.lock().unwrap().move_pointer(at)
        }
    }

    #[test]
    fn two_finger_scroll_targets_fingers_and_continues_with_inertia() {
        let fake = Arc::new(Mutex::new(FakePenSink::default()));
        let f2 = fake.clone();
        let region = Region {
            x: 0,
            y: 0,
            width: 1001,
            height: 1001,
        };
        let (tx, h) = spawn(
            Box::new(move || Ok(Box::new(Shared(f2)) as Box<dyn PenSink>)),
            region,
        )
        .unwrap();
        tx.send(PenInput::SetTouchMapping(TouchMapping::Absolute))
            .unwrap();
        // A quick downward flick at the centre: 8 steps of 1% of the height.
        for i in 1..=8u64 {
            tx.send(PenInput::Scroll {
                x: 0.5,
                y: 0.5,
                dx: 0.0,
                dy: 0.01,
                t_us: i * 8_000,
                end: i == 8,
            })
            .unwrap();
        }
        std::thread::sleep(Duration::from_millis(400)); // let inertia run
        tx.send(PenInput::Shutdown).unwrap();
        h.join().unwrap();

        let fake = fake.lock().unwrap();
        assert_eq!(
            fake.pointer_moves.len(),
            1,
            "pointer moves once, at gesture start"
        );
        assert!((fake.pointer_moves[0].x - 500.0).abs() < 1.0);
        let finger_total: f64 = fake.scrolls.iter().take(8).map(|s| s.1).sum();
        let total: f64 = fake.scrolls.iter().map(|s| s.1).sum();
        assert!(
            finger_total >= 80.0,
            "content follows the fingers (≥1:1), got {finger_total}"
        );
        assert!(
            total > finger_total * 1.2,
            "inertia continues after lift: {total} vs {finger_total}"
        );
        assert!(
            fake.scrolls.iter().all(|s| s.1 >= 0.0 && s.0 == 0.0),
            "direction preserved"
        );
        assert_eq!(
            fake.scroll_ends, 0,
            "still coasting: the gesture isn't over"
        );
    }

    /// Runs `inputs` through a pen thread with a fake sink; returns the fake.
    fn run_inputs(inputs: Vec<PenInput>) -> FakePenSink {
        let fake = Arc::new(Mutex::new(FakePenSink::default()));
        let f2 = fake.clone();
        let region = Region {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let (tx, h) = spawn(
            Box::new(move || Ok(Box::new(Shared(f2)) as Box<dyn PenSink>)),
            region,
        )
        .unwrap();
        for i in inputs {
            tx.send(i).unwrap();
        }
        tx.send(PenInput::Shutdown).unwrap();
        h.join().unwrap();
        std::mem::take(&mut *fake.lock().unwrap())
    }

    fn scroll(dy: f32, t_us: u64, end: bool) -> PenInput {
        PenInput::Scroll {
            x: 0.5,
            y: 0.5,
            dx: 0.0,
            dy,
            t_us,
            end,
        }
    }

    #[test]
    fn a_scroll_that_stops_before_lifting_ends_at_once() {
        let fake = run_inputs(vec![
            scroll(0.01, 8_000, false),
            scroll(0.01, 16_000, false),
            // Fingers rest 200 ms, then lift: no inertia.
            scroll(0.0, 216_000, true),
        ]);
        assert_eq!(
            fake.scrolls.len(),
            2,
            "the two movements scroll; the lift adds none"
        );
        assert_eq!(fake.scroll_ends, 1);
    }

    #[test]
    fn the_pen_ends_a_coasting_scroll() {
        let mut inputs: Vec<PenInput> = (1..=8u64)
            .map(|i| scroll(0.01, i * 8_000, i == 8))
            .collect();
        inputs.push(PenInput::Transition(PenSample {
            t_us: 100_000,
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            state: PenState::Contact,
            ..Default::default()
        }));
        let fake = run_inputs(inputs);
        assert_eq!(fake.scroll_ends, 1);
    }

    #[test]
    fn no_scroll_no_end() {
        let fake = run_inputs(vec![PenInput::Disconnected]);
        assert_eq!(fake.scroll_ends, 0);
    }

    #[test]
    fn maps_and_injects_then_releases_on_disconnect() {
        let fake = Arc::new(Mutex::new(FakePenSink::default()));
        let f2 = fake.clone();
        let region = Region {
            x: 100,
            y: 0,
            width: 1001,
            height: 501,
        };
        let (tx, h) = spawn(
            Box::new(move || Ok(Box::new(Shared(f2)) as Box<dyn PenSink>)),
            region,
        )
        .unwrap();

        let s = |t_us, x, y| PenSample {
            t_us,
            x,
            y,
            pressure: 0.5,
            state: PenState::Contact,
            ..Default::default()
        };
        tx.send(PenInput::Batch(PenBatch {
            seq: 1,
            samples: vec![s(1, 0.0, 0.0), s(2, 1.0, 1.0)],
        }))
        .unwrap();
        tx.send(PenInput::Disconnected).unwrap();
        tx.send(PenInput::Shutdown).unwrap();
        h.join().unwrap();

        let fake = fake.lock().unwrap();
        let pts: Vec<(f64, f64)> = fake.events.iter().map(|(_, p)| (p.x, p.y)).collect();
        assert_eq!(pts, vec![(100.0, 0.0), (1100.0, 500.0)]);
        assert!(fake.releases >= 1);
    }
}
