//! Touchpad-style scrolling: acceleration and inertia.
//!
//! The tablet sends two-finger pan movements with its own timestamps; this
//! engine turns them into scroll amounts (desktop pixels) the way touchpad
//! drivers do:
//! - **acceleration**: slow movement scrolls ~1:1 for precision, faster
//!   movement scrolls proportionally further (up to [`MAX_GAIN`]);
//! - **inertia**: when the fingers lift mid-swipe, scrolling continues and
//!   decays, ticked on the host at a fine interval so it is smooth whatever
//!   the network does.
//!
//! Velocity uses the tablet's timestamps, so network jitter can't distort it.

/// Movement speed (desktop px/ms) up to which scrolling stays 1:1.
const PRECISE_SPEED: f64 = 0.4;
/// Extra gain per px/ms above [`PRECISE_SPEED`].
const ACCELERATION: f64 = 1.2;
const MAX_GAIN: f64 = 3.0;
/// Fling only if the fingers were still moving this fast on lift (px/ms).
const FLING_MIN_SPEED: f64 = 0.3;
/// Velocity kept per millisecond of inertia (time constant ≈ 325 ms).
const FRICTION_PER_MS: f64 = 0.997;
/// Inertia stops below this speed (px/ms).
const FLING_STOP_SPEED: f64 = 0.02;
/// Velocity is smoothed over recent movement (exponential, per sample).
const VELOCITY_SMOOTHING: f64 = 0.35;

#[derive(Debug, Default)]
pub struct ScrollEngine {
    /// Smoothed finger velocity (px/ms) and the time of the last movement
    /// (tablet clock, µs).
    velocity: (f64, f64),
    last_move_us: Option<u64>,
    /// Active inertia: current velocity (px/ms) and the last tick (host
    /// clock, µs).
    fling: Option<((f64, f64), u64)>,
}

impl ScrollEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fingers moved by (`dx`, `dy`) desktop px at tablet time `t_us`.
    /// Returns the amount to scroll now.
    pub fn on_move(&mut self, dx: f64, dy: f64, t_us: u64) -> (f64, f64) {
        self.fling = None; // touching again stops any inertia
        if let Some(last) = self.last_move_us {
            let dt_ms = (t_us.saturating_sub(last) as f64 / 1000.0).max(1.0);
            let v = (dx / dt_ms, dy / dt_ms);
            self.velocity = (
                self.velocity.0 + VELOCITY_SMOOTHING * (v.0 - self.velocity.0),
                self.velocity.1 + VELOCITY_SMOOTHING * (v.1 - self.velocity.1),
            );
        }
        self.last_move_us = Some(t_us);
        let gain = gain(speed(self.velocity));
        (dx * gain, dy * gain)
    }

    /// Fingers lifted at tablet time `t_us`; starts inertia (at host time
    /// `now_us`) if they were still moving.
    pub fn on_end(&mut self, t_us: u64, now_us: u64) {
        // A pause before lifting means "stop here", not a fling.
        let paused = self
            .last_move_us
            .is_none_or(|last| t_us.saturating_sub(last) > 60_000);
        let v = self.velocity;
        self.velocity = (0.0, 0.0);
        self.last_move_us = None;
        if !paused && speed(v) >= FLING_MIN_SPEED {
            let g = gain(speed(v));
            self.fling = Some(((v.0 * g, v.1 * g), now_us));
        }
    }

    /// Cancels inertia (pen down, new gesture, disconnect).
    pub fn stop(&mut self) {
        self.fling = None;
        self.last_move_us = None;
        self.velocity = (0.0, 0.0);
    }

    pub fn is_flinging(&self) -> bool {
        self.fling.is_some()
    }

    /// Advances inertia to host time `now_us`; returns the amount to scroll.
    pub fn tick(&mut self, now_us: u64) -> (f64, f64) {
        let Some((v, last)) = self.fling else {
            return (0.0, 0.0);
        };
        let dt = now_us.saturating_sub(last) as f64 / 1000.0;
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        // Exact integral of v·friction^t over dt, so the distance doesn't
        // depend on how often we tick.
        let k = FRICTION_PER_MS.ln();
        let decay = FRICTION_PER_MS.powf(dt);
        let dist = (decay - 1.0) / k;
        let moved = (v.0 * dist, v.1 * dist);
        let nv = (v.0 * decay, v.1 * decay);
        self.fling = (speed(nv) >= FLING_STOP_SPEED).then_some((nv, now_us));
        moved
    }
}

fn speed(v: (f64, f64)) -> f64 {
    v.0.hypot(v.1)
}

/// Pointer-acceleration curve: 1 up to the precise speed, then linear, capped.
fn gain(speed: f64) -> f64 {
    (1.0 + ACCELERATION * (speed - PRECISE_SPEED).max(0.0)).min(MAX_GAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moves at `px_per_ms` for `ms` in 8 ms steps; returns total scrolled.
    fn swipe(e: &mut ScrollEngine, t: &mut u64, px_per_ms: f64, ms: u64) -> f64 {
        let mut total = 0.0;
        for _ in 0..ms / 8 {
            *t += 8_000;
            total += e.on_move(0.0, px_per_ms * 8.0, *t).1;
        }
        total
    }

    #[test]
    fn slow_movement_is_one_to_one() {
        let mut e = ScrollEngine::new();
        let mut t = 0;
        let s = swipe(&mut e, &mut t, 0.2, 400);
        assert!((s - 0.2 * 400.0).abs() < 1.0, "got {s}");
    }

    #[test]
    fn fast_movement_accelerates_up_to_the_cap() {
        let mut e = ScrollEngine::new();
        let mut t = 0;
        let fast = swipe(&mut e, &mut t, 2.0, 400);
        assert!(fast > 2.0 * 400.0 * 1.5, "should accelerate, got {fast}");
        assert!(fast <= 2.0 * 400.0 * MAX_GAIN + 1.0);
    }

    #[test]
    fn flick_continues_and_decays_independently_of_tick_rate() {
        let run = |step_us: u64| {
            let mut e = ScrollEngine::new();
            let mut t = 0;
            swipe(&mut e, &mut t, 1.5, 200);
            e.on_end(t, 1_000_000);
            let mut now = 1_000_000;
            let mut total = 0.0;
            while e.is_flinging() {
                now += step_us;
                total += e.tick(now).1;
            }
            (total, now - 1_000_000)
        };
        let (fine, dur) = run(4_000);
        let (coarse, _) = run(16_000);
        assert!(fine > 100.0, "inertia should carry on, got {fine}");
        assert!(
            dur > 300_000 && dur < 3_000_000,
            "decays within seconds, took {dur} µs"
        );
        assert!(
            (fine - coarse).abs() / fine < 0.02,
            "tick rate must not matter: {fine} vs {coarse}"
        );
    }

    #[test]
    fn pausing_before_lift_or_slow_movement_does_not_fling() {
        let mut e = ScrollEngine::new();
        let mut t = 0;
        swipe(&mut e, &mut t, 1.5, 200);
        e.on_end(t + 200_000, 0); // held still for 200 ms, then lifted
        assert!(!e.is_flinging());
        swipe(&mut e, &mut t, 0.1, 200);
        e.on_end(t, 0);
        assert!(!e.is_flinging());
    }

    #[test]
    fn touching_again_stops_inertia() {
        let mut e = ScrollEngine::new();
        let mut t = 0;
        swipe(&mut e, &mut t, 1.5, 200);
        e.on_end(t, 0);
        assert!(e.is_flinging());
        e.on_move(0.0, 1.0, t + 10_000);
        assert!(!e.is_flinging());
    }
}
