use serde::{Deserialize, Serialize};

/// Which end of the pen is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Tool {
    #[default]
    Pen,
    Eraser,
}

/// Contact state of the pen for a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PenState {
    /// In range above the screen, not touching.
    #[default]
    Hover,
    /// Touching the screen.
    Contact,
    /// Left the detection range; the host must release any pressed state.
    OutOfRange,
}

/// Pen buttons as a bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Buttons(pub u8);

impl Buttons {
    pub const BARREL: Self = Self(1);
    pub const SECONDARY: Self = Self(1 << 1);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One stylus sample, as measured on the client.
///
/// Coordinates are **normalised to the streamed region** (`0.0..=1.0` on both
/// axes), so the client never needs to know desktop geometry; the host maps
/// them with `ib-geometry`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct PenSample {
    /// Client clock, microseconds. Converted to host time with `ib-clock`.
    pub t_us: u64,
    pub x: f32,
    pub y: f32,
    /// `0.0..=1.0`; `0.0` unless in contact.
    pub pressure: f32,
    /// Tilt from vertical towards +x / +y, degrees `-90..=90`.
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// Barrel rotation, degrees `0..360`.
    pub rotation: f32,
    pub state: PenState,
    pub tool: Tool,
    pub buttons: Buttons,
}

impl PenSample {
    /// Clamps every axis into its valid range. Hosts call this on untrusted
    /// input before injection, so malformed packets can never produce
    /// out-of-range OS events.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        fn finite_or_zero(v: f32) -> f32 {
            if v.is_finite() { v } else { 0.0 }
        }
        self.x = finite_or_zero(self.x).clamp(0.0, 1.0);
        self.y = finite_or_zero(self.y).clamp(0.0, 1.0);
        self.pressure = finite_or_zero(self.pressure).clamp(0.0, 1.0);
        self.tilt_x = finite_or_zero(self.tilt_x).clamp(-90.0, 90.0);
        self.tilt_y = finite_or_zero(self.tilt_y).clamp(-90.0, 90.0);
        self.rotation = finite_or_zero(self.rotation).rem_euclid(360.0);
        if self.state != PenState::Contact {
            self.pressure = 0.0;
        }
        self
    }
}

/// A batch of samples sent as one unreliable datagram.
///
/// Android and iOS deliver several hardware samples per input frame
/// ("historical" or "coalesced" samples). Sending them together keeps the full
/// sample rate while bounding the packet count. `seq` lets the host drop
/// reordered or duplicated datagrams.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PenBatch {
    pub seq: u32,
    pub samples: Vec<PenSample>,
}

impl PenBatch {
    /// Upper bound on samples per datagram; keeps packets under the typical
    /// QUIC datagram payload limit (~1200 bytes).
    pub const MAX_SAMPLES: usize = 24;

    /// Whether `self` is newer than `last` under wrapping sequence arithmetic.
    pub const fn is_newer_than(&self, last: u32) -> bool {
        let diff = self.seq.wrapping_sub(last);
        diff != 0 && diff < u32::MAX / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn sanitize_clamps_and_rejects_non_finite() {
        let s = PenSample {
            x: f32::NAN,
            y: 2.0,
            pressure: 5.0,
            tilt_x: -200.0,
            rotation: -10.0,
            state: PenState::Contact,
            ..Default::default()
        }
        .sanitized();
        assert!(approx(s.x, 0.0));
        assert!(approx(s.y, 1.0));
        assert!(approx(s.pressure, 1.0));
        assert!(approx(s.tilt_x, -90.0));
        assert!(approx(s.rotation, 350.0));
    }

    #[test]
    fn only_contact_has_pressure() {
        let s = PenSample {
            pressure: 0.7,
            state: PenState::Hover,
            ..Default::default()
        };
        assert!(approx(s.sanitized().pressure, 0.0));
    }

    #[test]
    fn sequence_wraps() {
        let b = PenBatch {
            seq: 2,
            samples: vec![],
        };
        assert!(b.is_newer_than(1));
        assert!(b.is_newer_than(u32::MAX)); // wrapped around
        assert!(!b.is_newer_than(2)); // duplicate
        assert!(!b.is_newer_than(3)); // older
    }
}
