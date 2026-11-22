//! Keyframe request limiter (§6 rule 6): at most one IDR per 500 ms.
//! Requests inside the window are coalesced into one, sent when it opens.

use std::time::{Duration, Instant};

pub const MIN_IDR_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
pub struct IdrLimiter {
    last: Option<Instant>,
    pending: bool,
}

impl IdrLimiter {
    /// Register a request. Returns true if an IDR should be produced now.
    pub fn request(&mut self, now: Instant) -> bool {
        self.pending = true;
        self.poll(now)
    }

    /// Call once per frame. Returns true when a coalesced IDR is due.
    pub fn poll(&mut self, now: Instant) -> bool {
        let open = self
            .last
            .is_none_or(|t| now.saturating_duration_since(t) >= MIN_IDR_INTERVAL);
        if self.pending && open {
            self.pending = false;
            self.last = Some(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_and_coalesced() {
        let t0 = Instant::now();
        let mut l = IdrLimiter::default();
        assert!(l.request(t0));
        assert!(!l.request(t0 + Duration::from_millis(100)));
        assert!(!l.request(t0 + Duration::from_millis(200)));
        assert!(!l.poll(t0 + Duration::from_millis(499)));
        assert!(l.poll(t0 + Duration::from_millis(500)));
        assert!(
            !l.poll(t0 + Duration::from_millis(2000)),
            "coalesced into one"
        );
    }
}
