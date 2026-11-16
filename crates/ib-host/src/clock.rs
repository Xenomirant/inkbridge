//! The host's monotonic microsecond clock, used for protocol timestamps.
//! Any monotonic base works because `ib-clock` estimates offsets.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn now_us() -> u64 {
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}
