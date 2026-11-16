//! Delay-based video bitrate control (receiver side).
//!
//! The tablet measures each frame's one-way network delay. Constant clock
//! offset errors cancel out because only the delay *above a recent minimum*
//! (the queuing delay) is used — the same principle as WebRTC's delay-based
//! congestion control: a growing queue is the earliest sign the link is
//! saturated, well before packets are lost.
//!
//! AIMD: multiplicative decrease on queuing, slow increase when the queue
//! has stayed empty for a while.

use std::collections::VecDeque;

/// Tuning, in one place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateConfig {
    pub min_kbps: u32,
    pub max_kbps: u32,
    /// Queuing delay above which the link counts as overused (µs).
    pub overuse_us: u64,
    /// Queuing delay below which the link counts as idle (µs).
    pub idle_us: u64,
    /// Window over which the delay baseline (minimum) is tracked (µs).
    pub baseline_window_us: u64,
    /// Evaluation period (µs).
    pub interval_us: u64,
    /// How long the queue must stay idle before increasing (µs).
    pub increase_after_us: u64,
    pub decrease_factor: f64,
    pub increase_factor: f64,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            min_kbps: 2_000,
            max_kbps: 25_000,
            overuse_us: 25_000,
            idle_us: 8_000,
            baseline_window_us: 10_000_000,
            interval_us: 250_000,
            increase_after_us: 2_000_000,
            decrease_factor: 0.85,
            increase_factor: 1.05,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BitrateController {
    cfg: RateConfig,
    target_kbps: f64,
    /// (time, one-way delay) samples for the baseline window.
    delays: VecDeque<(u64, u64)>,
    /// Queuing delays since the last evaluation.
    recent: Vec<u64>,
    last_eval_us: Option<u64>,
    /// Last time the queue was not idle (or the rate decreased).
    last_busy_us: u64,
}

impl BitrateController {
    pub fn new(cfg: RateConfig) -> Self {
        Self {
            target_kbps: f64::from(cfg.max_kbps),
            cfg,
            delays: VecDeque::new(),
            recent: Vec::new(),
            last_eval_us: None,
            last_busy_us: 0,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps as u32
    }

    /// Records one frame's one-way delay measured at `now_us`. Returns the
    /// new target when it changed (to be sent to the host).
    pub fn on_delay(&mut self, now_us: u64, delay_us: u64) -> Option<u32> {
        self.delays.push_back((now_us, delay_us));
        while self
            .delays
            .front()
            .is_some_and(|(t, _)| now_us.saturating_sub(*t) > self.cfg.baseline_window_us)
        {
            self.delays.pop_front();
        }
        let baseline = self
            .delays
            .iter()
            .map(|(_, d)| *d)
            .min()
            .unwrap_or(delay_us);
        self.recent.push(delay_us.saturating_sub(baseline));

        let last = *self.last_eval_us.get_or_insert(now_us);
        if now_us.saturating_sub(last) < self.cfg.interval_us {
            return None;
        }
        self.last_eval_us = Some(now_us);
        self.recent.sort_unstable();
        let queuing = self.recent[self.recent.len() / 2]; // median: robust to one late frame
        self.recent.clear();

        let before = self.target_kbps();
        if queuing > self.cfg.overuse_us {
            self.target_kbps *= self.cfg.decrease_factor;
            self.last_busy_us = now_us;
        } else if queuing > self.cfg.idle_us {
            self.last_busy_us = now_us; // hold
        } else if now_us.saturating_sub(self.last_busy_us) >= self.cfg.increase_after_us {
            self.target_kbps *= self.cfg.increase_factor;
            self.last_busy_us = now_us; // pace increases
        }
        self.target_kbps = self
            .target_kbps
            .clamp(f64::from(self.cfg.min_kbps), f64::from(self.cfg.max_kbps));
        let after = self.target_kbps();
        (after != before).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds 60 fps frames with the given one-way delay for `secs` seconds.
    fn run(c: &mut BitrateController, t: &mut u64, secs: u64, delay_us: u64) -> Vec<u32> {
        let mut changes = Vec::new();
        for _ in 0..secs * 60 {
            *t += 16_667;
            if let Some(k) = c.on_delay(*t, delay_us) {
                changes.push(k);
            }
        }
        changes
    }

    #[test]
    fn steady_link_stays_at_max() {
        let mut c = BitrateController::new(RateConfig::default());
        let mut t = 0;
        assert_eq!(run(&mut c, &mut t, 5, 3_000), vec![]);
        assert_eq!(c.target_kbps(), 25_000);
    }

    #[test]
    fn queuing_cuts_quickly_then_recovers_slowly() {
        let cfg = RateConfig::default();
        let mut c = BitrateController::new(cfg);
        let mut t = 0;
        run(&mut c, &mut t, 2, 3_000); // establishes the baseline
        // Congestion: frames arrive 60 ms above the baseline.
        let down = run(&mut c, &mut t, 2, 63_000);
        assert_ne!(down, vec![]);
        // The first evaluation after the queue drains still covers congested
        // frames (one more decrease); measure the low point after it.
        run(&mut c, &mut t, 1, 3_000);
        let low = c.target_kbps();
        assert!(low < 15_000, "should have backed off, got {low}");
        // Link clears: increases are slow and paced.
        run(&mut c, &mut t, 3, 3_000);
        let partial = c.target_kbps();
        assert!(
            partial > low && partial < 25_000,
            "slow recovery, got {partial}"
        );
        run(&mut c, &mut t, 60, 3_000);
        assert_eq!(c.target_kbps(), 25_000);
    }

    #[test]
    fn constant_clock_offset_does_not_matter() {
        // A 2-second clock error shifts every delay equally: no reaction.
        let mut c = BitrateController::new(RateConfig::default());
        let mut t = 0;
        assert_eq!(run(&mut c, &mut t, 5, 2_003_000), vec![]);
    }

    #[test]
    fn never_below_minimum() {
        let mut c = BitrateController::new(RateConfig::default());
        let mut t = 0;
        run(&mut c, &mut t, 1, 1_000);
        // Delay keeps growing: queue never drains.
        for s in 0..30u64 {
            run(&mut c, &mut t, 1, 1_000 + (s + 1) * 50_000);
        }
        assert_eq!(c.target_kbps(), RateConfig::default().min_kbps);
    }
}
