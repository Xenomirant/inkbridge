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

use crate::touch::{TouchMapper, TouchMapping};

/// How often resting fingers are re-sent so the OS keeps them down.
const TOUCH_KEEPALIVE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum PenInput {
    Batch(PenBatch),
    Transition(PenSample),
    SetRegion(Region),
    /// Multi-finger gesture frame.
    Touch(Vec<TouchContact>),
    /// How finger gestures map to the desktop (per session: absolute when the
    /// tablet shows the screen, anchored at the cursor otherwise).
    SetTouchMapping(TouchMapping),
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

fn run(mut sink: Box<dyn PenSink>, mut region: Region, rx: &Receiver<PenInput>) {
    tracing::info!(backend = sink.name(), ?region, "pen thread ready");
    let mut stream = PenStream::new();
    // M1 sessions are pen-only (no image on the tablet): anchor at the cursor.
    let mut touch = TouchMapper::new(TouchMapping::AnchoredAtPointer);
    let mut pointer = DesktopPoint {
        x: f64::from(region.x),
        y: f64::from(region.y),
    };
    let inject_touch = |sink: &mut Box<dyn PenSink>, frame: &[TouchPoint]| {
        if let Err(e) = sink.touch(frame) {
            tracing::debug!("{e}");
        }
    };
    loop {
        let input = match rx.recv_timeout(TOUCH_KEEPALIVE) {
            Ok(input) => input,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(frame) = touch.keep_alive() {
                    let frame = frame.to_vec();
                    inject_touch(&mut sink, &frame);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match input {
            PenInput::Batch(batch) => {
                for s in stream.on_batch(batch) {
                    pointer = emit(&mut sink, s, region);
                }
            }
            PenInput::Transition(t) => {
                if let Some(s) = stream.on_transition(t) {
                    pointer = emit(&mut sink, s, region);
                }
            }
            PenInput::SetRegion(r) => region = r,
            PenInput::SetTouchMapping(m) => {
                let cancelled = touch.cancel();
                if !cancelled.is_empty() {
                    inject_touch(&mut sink, &cancelled);
                }
                touch = TouchMapper::new(m);
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
                let frame = touch.map(&contacts, region, pointer);
                inject_touch(&mut sink, &frame);
            }
            PenInput::Disconnected => {
                let cancelled = touch.cancel();
                if !cancelled.is_empty() {
                    inject_touch(&mut sink, &cancelled);
                }
                if stream.release().is_some()
                    && let Err(e) = sink.release()
                {
                    tracing::warn!("{e}");
                }
                stream = PenStream::new();
            }
            PenInput::Shutdown => break,
        }
    }
    let _ = sink.release();
}

/// Injects one pen sample; returns where it landed (the pointer position).
fn emit(sink: &mut Box<dyn PenSink>, s: PenSample, region: Region) -> DesktopPoint {
    let at = to_desktop(region, Rotation::None, s.x, s.y);
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
