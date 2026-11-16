//! Platform-neutral pen state machine.
//!
//! OS pen APIs want explicit transitions (Windows: `DOWN`/`UP`/`UPDATE`
//! flags; macOS: proximity enter/leave + mouse down/up). [`PenTracker`] turns
//! the stream of per-sample states into those transitions, including the
//! two-step "lift, then leave range" when the pen leaves while touching.

use ib_protocol::PenState;

/// One OS-level pen event to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Step {
    pub in_range: bool,
    pub in_contact: bool,
    /// Contact starts with this event.
    pub down: bool,
    /// Contact ends with this event.
    pub up: bool,
}

#[derive(Debug, Default, Clone)]
pub struct PenTracker {
    in_range: bool,
    in_contact: bool,
}

impl PenTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn in_contact(&self) -> bool {
        self.in_contact
    }

    /// Steps needed to reach `state` (0–2 events).
    pub fn step(&mut self, state: PenState) -> Vec<Step> {
        let prev = (self.in_range, self.in_contact);
        let steps = match (state, prev.1) {
            (PenState::Contact, false) => vec![Step {
                in_range: true,
                in_contact: true,
                down: true,
                up: false,
            }],
            (PenState::Contact, true) => vec![Step {
                in_range: true,
                in_contact: true,
                ..Step::default()
            }],
            (PenState::Hover, true) => vec![Step {
                in_range: true,
                up: true,
                ..Step::default()
            }],
            (PenState::Hover, false) => vec![Step {
                in_range: true,
                ..Step::default()
            }],
            (PenState::OutOfRange, true) => vec![
                Step {
                    in_range: true,
                    up: true,
                    ..Step::default()
                },
                Step::default(), // leave range
            ],
            (PenState::OutOfRange, false) if prev.0 => vec![Step::default()],
            (PenState::OutOfRange, false) => vec![], // already out of range
        };
        if let Some(last) = steps.last() {
            self.in_range = last.in_range;
            self.in_contact = last.in_contact;
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PenState::{Contact, Hover, OutOfRange};

    fn run(states: &[PenState]) -> Vec<Step> {
        let mut t = PenTracker::new();
        states.iter().flat_map(|s| t.step(*s)).collect()
    }

    #[test]
    fn stroke_produces_exactly_one_down_and_one_up() {
        let steps = run(&[Hover, Contact, Contact, Contact, Hover, OutOfRange]);
        assert_eq!(steps.iter().filter(|s| s.down).count(), 1);
        assert_eq!(steps.iter().filter(|s| s.up).count(), 1);
        assert!(!steps.last().unwrap().in_range);
    }

    #[test]
    fn leaving_while_touching_lifts_first() {
        let steps = run(&[Contact, OutOfRange]);
        assert_eq!(steps.len(), 3);
        assert!(steps[1].up && steps[1].in_range);
        assert_eq!(steps[2], Step::default());
    }

    #[test]
    fn redundant_out_of_range_emits_nothing() {
        assert_eq!(run(&[OutOfRange, OutOfRange]), vec![]);
        assert_eq!(run(&[Hover, OutOfRange, OutOfRange]).len(), 2);
    }

    #[test]
    fn touching_without_hover_still_goes_down() {
        let steps = run(&[Contact]);
        assert!(steps[0].down && steps[0].in_contact && steps[0].in_range);
    }
}
