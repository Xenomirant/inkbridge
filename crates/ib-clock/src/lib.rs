//! Clock synchronisation and latency accounting.
//!
//! Latency is Inkbridge's main product metric, so every stage is measured.
//! Host and client clocks differ; [`ClockSync`] estimates the offset with an
//! NTP-style four-timestamp exchange, keeping the sample with the **lowest
//! round-trip time** in a sliding window (min-RTT filtering: the fastest
//! exchange has the least queueing asymmetry, so its offset is the most
//! accurate). [`LatencyStats`] keeps rolling percentiles per pipeline stage.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

/// Process-wide monotonic clock in microseconds. Every component on one
/// device stamps with this, so per-stage durations can be subtracted
/// directly; [`ClockSync`] relates the clocks of different devices.
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// One completed ping/pong exchange, all in microseconds.
///
/// - `t0`: local send, `t1`: remote receive, `t2`: remote send,
///   `t3`: local receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exchange {
    pub t0: u64,
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
}

impl Exchange {
    /// Round-trip time excluding the remote's processing time, or `None` if
    /// the timestamps are inconsistent (e.g. a local clock jump).
    pub fn rtt_us(&self) -> Option<u64> {
        let total = self.t3.checked_sub(self.t0)?;
        let remote = self.t2.checked_sub(self.t1)?;
        total.checked_sub(remote)
    }

    /// Estimated `remote − local` clock offset in microseconds.
    pub fn offset_us(&self) -> i64 {
        // ((t1 − t0) + (t2 − t3)) / 2, computed in i128 to avoid overflow.
        let sum = (i128::from(self.t1) - i128::from(self.t0))
            + (i128::from(self.t2) - i128::from(self.t3));
        (sum / 2) as i64
    }
}

/// Sliding-window, min-RTT clock offset estimator.
#[derive(Debug, Clone)]
pub struct ClockSync {
    window: VecDeque<(u64, i64)>, // (rtt, offset)
    capacity: usize,
}

impl ClockSync {
    /// `capacity` exchanges are kept; 16 at one ping per second adapts to
    /// clock drift within seconds while ignoring transient congestion.
    pub fn new(capacity: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Adds an exchange; inconsistent ones are ignored. Returns whether it was used.
    pub fn add(&mut self, ex: Exchange) -> bool {
        let Some(rtt) = ex.rtt_us() else { return false };
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back((rtt, ex.offset_us()));
        true
    }

    fn best(&self) -> Option<(u64, i64)> {
        self.window.iter().copied().min_by_key(|(rtt, _)| *rtt)
    }

    /// Best `remote − local` offset estimate, once any exchange is known.
    pub fn offset_us(&self) -> Option<i64> {
        self.best().map(|(_, o)| o)
    }

    /// Lowest round-trip time in the window.
    pub fn min_rtt_us(&self) -> Option<u64> {
        self.best().map(|(r, _)| r)
    }

    /// Converts a remote timestamp to the local clock.
    pub fn remote_to_local(&self, remote_us: u64) -> Option<u64> {
        let off = self.offset_us()?;
        u64::try_from(i128::from(remote_us) - i128::from(off)).ok()
    }
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new(16)
    }
}

/// Pipeline stages, in pen-to-photon order. Each value is the duration of
/// that stage alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// Pen sample → datagram sent (client).
    PenSend,
    /// Sent → received (network, one way).
    PenNetwork,
    /// Received → injected into the OS (host).
    Inject,
    /// Injected → frame containing the result captured (app redraw + capture).
    Capture,
    Encode,
    /// Encoded → received by client (network, one way).
    VideoNetwork,
    /// Received → presented on screen (decode + display).
    DecodePresent,
    /// Pen sample → photon, measured directly where possible.
    EndToEnd,
}

/// Rolling latency percentiles for one stage (fixed-size window, O(n log n)
/// query; queries happen at most a few times per second for the overlay).
#[derive(Debug, Clone)]
pub struct LatencyStats {
    samples: VecDeque<u32>, // microseconds
    capacity: usize,
}

impl LatencyStats {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    pub fn record_us(&mut self, us: u64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples
            .push_back(u32::try_from(us).unwrap_or(u32::MAX));
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Percentile `p` in `0.0..=100.0` (nearest-rank), in microseconds.
    pub fn percentile_us(&self, p: f64) -> Option<u32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<u32> = self.samples.iter().copied().collect();
        v.sort_unstable();
        let rank = ((p.clamp(0.0, 100.0) / 100.0) * v.len() as f64).ceil() as usize;
        Some(v[rank.saturating_sub(1).min(v.len() - 1)])
    }
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self::new(512)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn symmetric_path_gives_exact_offset() {
        // Remote clock is 1_000 µs ahead; 300 µs each way; 50 µs processing.
        let ex = Exchange {
            t0: 10_000,
            t1: 11_300,
            t2: 11_350,
            t3: 10_650,
        };
        assert_eq!(ex.offset_us(), 1_000);
        assert_eq!(ex.rtt_us(), Some(600));
    }

    #[test]
    fn min_rtt_sample_wins() {
        let mut s = ClockSync::new(4);
        // Congested exchange: asymmetric delay skews its offset.
        s.add(Exchange {
            t0: 0,
            t1: 6_000,
            t2: 6_000,
            t3: 2_000,
        }); // rtt 2000
        // Clean exchange: true offset 5_000.
        s.add(Exchange {
            t0: 10_000,
            t1: 15_100,
            t2: 15_100,
            t3: 10_200,
        }); // rtt 200
        assert_eq!(s.min_rtt_us(), Some(200));
        assert_eq!(s.offset_us(), Some(5_000));
        assert_eq!(s.remote_to_local(15_100), Some(10_100));
    }

    #[test]
    fn inconsistent_exchange_is_ignored() {
        let mut s = ClockSync::default();
        assert!(!s.add(Exchange {
            t0: 100,
            t1: 0,
            t2: 0,
            t3: 50
        })); // t3 < t0
        assert_eq!(s.offset_us(), None);
    }

    #[test]
    fn window_forgets_old_samples() {
        let mut s = ClockSync::new(2);
        s.add(Exchange {
            t0: 0,
            t1: 50,
            t2: 50,
            t3: 100,
        }); // rtt 100, offset 0
        s.add(Exchange {
            t0: 0,
            t1: 600,
            t2: 600,
            t3: 1_000,
        }); // rtt 1000
        s.add(Exchange {
            t0: 0,
            t1: 800,
            t2: 800,
            t3: 1_000,
        }); // rtt 1000, evicts first
        assert_eq!(s.min_rtt_us(), Some(1_000));
    }

    #[test]
    fn percentiles() {
        let mut l = LatencyStats::new(100);
        for us in 1..=100 {
            l.record_us(us);
        }
        assert_eq!(l.percentile_us(50.0), Some(50));
        assert_eq!(l.percentile_us(95.0), Some(95));
        assert_eq!(l.percentile_us(100.0), Some(100));
        assert_eq!(LatencyStats::default().percentile_us(50.0), None);
    }

    proptest! {
        /// With symmetric one-way delays, the estimate is exact for any clocks.
        #[test]
        fn exact_under_symmetry(t0 in 0u64..1u64<<50, offset in -1i64<<40..1i64<<40, oneway in 0u64..1_000_000, proc_ in 0u64..100_000) {
            let shift = |t: u64| u64::try_from(i128::from(t) + i128::from(offset)).ok();
            let (Some(t1), Some(t2)) = (shift(t0 + oneway), shift(t0 + oneway + proc_)) else { return Ok(()) };
            let ex = Exchange { t0, t1, t2, t3: t0 + 2 * oneway + proc_ };
            prop_assert_eq!(ex.offset_us(), offset);
            prop_assert_eq!(ex.rtt_us(), Some(2 * oneway));
        }
    }
}
