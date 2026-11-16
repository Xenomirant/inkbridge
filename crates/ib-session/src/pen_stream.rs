//! Reassembles the pen stream on the host.
//!
//! Inputs arrive on two paths:
//! - datagrams (`PenBatch`): fast but may be lost, duplicated or reordered;
//! - reliable transitions (`ControlMessage::PenTransition`): every change of
//!   contact state, tool or buttons, possibly arriving *after* later samples.
//!
//! [`PenStream`] emits samples in timestamp order, never twice, and applies a
//! transition only if no newer sample has been emitted. So a lost "pen up"
//! datagram is repaired by its reliable copy, while a late copy of an old
//! transition can never re-press the pen.

use ib_protocol::{PenBatch, PenSample, PenState};

#[derive(Debug, Default)]
pub struct PenStream {
    last_seq: Option<u32>,
    last_t_us: Option<u64>,
    last_state: Option<PenState>,
}

impl PenStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts a datagram batch; returns the samples to inject, sanitised.
    pub fn on_batch(&mut self, batch: PenBatch) -> Vec<PenSample> {
        if let Some(last) = self.last_seq
            && !batch.is_newer_than(last)
        {
            return Vec::new(); // duplicate or reordered: older samples are stale
        }
        self.last_seq = Some(batch.seq);
        batch
            .samples
            .into_iter()
            .take(PenBatch::MAX_SAMPLES)
            .filter_map(|s| self.accept(s))
            .collect()
    }

    /// Accepts a reliable transition; returns it if it still needs applying.
    pub fn on_transition(&mut self, sample: PenSample) -> Option<PenSample> {
        self.accept(sample)
    }

    /// The sample that releases the pen, if it may currently be pressed or
    /// in range (used on disconnect).
    pub fn release(&mut self) -> Option<PenSample> {
        match self.last_state {
            Some(PenState::OutOfRange) | None => None,
            Some(_) => {
                self.last_state = Some(PenState::OutOfRange);
                Some(PenSample {
                    state: PenState::OutOfRange,
                    ..PenSample::default()
                })
            }
        }
    }

    fn accept(&mut self, sample: PenSample) -> Option<PenSample> {
        if self.last_t_us.is_some_and(|t| sample.t_us <= t) {
            return None;
        }
        self.last_t_us = Some(sample.t_us);
        self.last_state = Some(sample.state);
        Some(sample.sanitized())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t_us: u64, state: PenState) -> PenSample {
        PenSample {
            t_us,
            state,
            pressure: 0.5,
            ..Default::default()
        }
    }

    fn batch(seq: u32, samples: Vec<PenSample>) -> PenBatch {
        PenBatch { seq, samples }
    }

    #[test]
    fn duplicates_and_reordered_batches_are_dropped() {
        let mut p = PenStream::new();
        assert_eq!(
            p.on_batch(batch(1, vec![s(10, PenState::Contact)])).len(),
            1
        );
        assert_eq!(p.on_batch(batch(1, vec![s(10, PenState::Contact)])), vec![]);
        assert_eq!(
            p.on_batch(batch(3, vec![s(30, PenState::Contact)])).len(),
            1
        );
        assert_eq!(p.on_batch(batch(2, vec![s(20, PenState::Contact)])), vec![]);
    }

    #[test]
    fn lost_pen_up_datagram_is_repaired_by_transition() {
        let mut p = PenStream::new();
        p.on_batch(batch(1, vec![s(10, PenState::Contact)]));
        // Datagram with the pen-up at t=20 is lost; its reliable copy arrives.
        let up = p.on_transition(s(20, PenState::Hover)).expect("must apply");
        assert_eq!(up.state, PenState::Hover);
        assert!(
            up.pressure.abs() < f32::EPSILON,
            "hover must carry no pressure"
        );
    }

    #[test]
    fn late_transition_never_overrides_newer_samples() {
        let mut p = PenStream::new();
        p.on_batch(batch(
            1,
            vec![
                s(10, PenState::Contact),
                s(20, PenState::Hover),
                s(30, PenState::Hover),
            ],
        ));
        // Reliable copy of the t=10 pen-down arrives late: must not re-press.
        assert_eq!(p.on_transition(s(10, PenState::Contact)), None);
    }

    #[test]
    fn release_only_when_needed() {
        let mut p = PenStream::new();
        assert_eq!(p.release(), None);
        p.on_batch(batch(1, vec![s(10, PenState::Contact)]));
        assert_eq!(p.release().map(|r| r.state), Some(PenState::OutOfRange));
        assert_eq!(p.release(), None);
    }
}
