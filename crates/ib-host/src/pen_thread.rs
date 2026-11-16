//! Dedicated pen-injection thread.
//!
//! Owns the [`PenSink`] and the [`PenStream`] reassembler. Inputs arrive over
//! a channel from the async network tasks; each sample is mapped to desktop
//! coordinates and injected immediately.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use ib_geometry::{Region, Rotation, to_desktop};
use ib_inject::{InjectError, PenSink};
use ib_protocol::{PenBatch, PenSample};
use ib_session::PenStream;

#[derive(Debug)]
pub enum PenInput {
    Batch(PenBatch),
    Transition(PenSample),
    SetRegion(Region),
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
    let emit = |sink: &mut Box<dyn PenSink>, s: PenSample, region: Region| {
        let at = to_desktop(region, Rotation::None, s.x, s.y);
        if let Err(e) = sink.emit(&s, at) {
            tracing::warn!("{e}");
        }
    };
    while let Ok(input) = rx.recv() {
        match input {
            PenInput::Batch(batch) => {
                for s in stream.on_batch(batch) {
                    emit(&mut sink, s, region);
                }
            }
            PenInput::Transition(t) => {
                if let Some(s) = stream.on_transition(t) {
                    emit(&mut sink, s, region);
                }
            }
            PenInput::SetRegion(r) => region = r,
            PenInput::Disconnected => {
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
