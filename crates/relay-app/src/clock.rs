//! Monotonic microsecond clock used for `capture_ts` and delay tracking.

use std::sync::OnceLock;
use std::time::Instant;

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

pub fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

pub fn us_of(t: Instant) -> u64 {
    t.saturating_duration_since(epoch()).as_micros() as u64
}
