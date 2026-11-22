//! Keyframe request limiter (§6 rule 6): at most one IDR per 500 ms.
//! Requests inside the window are coalesced into one, sent when it opens.

use std::time::{Duration, Instant};

pub const MIN_IDR_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
pub struct IdrLimiter {
    last: Option<Instant>,
    pending: bool,
    pending_since: Option<Instant>,
}

/// Longest a keyframe request may be held back by congestion.
pub const MAX_DEFER: Duration = Duration::from_millis(1500);

impl IdrLimiter {
    /// Register a request. Returns true if an IDR should be produced now.
    pub fn request(&mut self, now: Instant) -> bool {
        self.defer(now);
        self.poll(now)
    }

    /// Register a request without serving it yet (link congested: a big
    /// keyframe now would only add to the queue that broke the stream).
    pub fn defer(&mut self, now: Instant) {
        if !self.pending {
            self.pending_since = Some(now);
        }
        self.pending = true;
    }

    /// Like [`poll`](Self::poll), but while `congested` the request waits,
    /// up to [`MAX_DEFER`].
    pub fn poll_congested(&mut self, now: Instant, congested: bool) -> bool {
        let overdue = self
            .pending_since
            .is_some_and(|t| now.saturating_duration_since(t) >= MAX_DEFER);
        if congested && !overdue {
            return false;
        }
        self.poll(now)
    }

    /// Call once per frame. Returns true when a coalesced IDR is due.
    pub fn poll(&mut self, now: Instant) -> bool {
        let open = self
            .last
            .is_none_or(|t| now.saturating_duration_since(t) >= MIN_IDR_INTERVAL);
        if self.pending && open {
            self.pending = false;
            self.pending_since = None;
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
