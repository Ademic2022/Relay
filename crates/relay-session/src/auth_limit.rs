//! Brute-force protection (FR-35).
//!
//! Tracked per key (Controller Device ID and source IP are checked
//! separately). After 5 failures the key is locked for 1 minute; each
//! further lockout doubles. Success clears the key.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 5;
pub const BASE_LOCKOUT: Duration = Duration::from_secs(60);
/// Cap so the doubling cannot overflow (≈ 17 hours).
const MAX_LOCKOUT_DOUBLINGS: u32 = 10;
/// Entries idle this long are forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    lockouts: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

#[derive(Debug)]
pub struct AuthLimiter<K> {
    entries: HashMap<K, Entry>,
}

impl<K> Default for AuthLimiter<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> AuthLimiter<K> {
    /// `Err(remaining)` if `key` is locked out right now.
    pub fn check(&self, key: &K, now: Instant) -> Result<(), Duration> {
        match self.entries.get(key).and_then(|e| e.locked_until) {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    /// Record a failed attempt. Returns the lockout if this one triggered it.
    pub fn record_failure(&mut self, key: &K, now: Instant) -> Option<Duration> {
        let e = self.entries.entry(key.clone()).or_insert(Entry {
            failures: 0,
            lockouts: 0,
            locked_until: None,
            last_seen: now,
        });
        e.last_seen = now;
        if e.locked_until.is_some_and(|u| u <= now) {
            e.locked_until = None;
            e.failures = 0;
        }
        e.failures += 1;
        if e.failures < MAX_FAILURES {
            return None;
        }
        let d = BASE_LOCKOUT * 2u32.pow(e.lockouts.min(MAX_LOCKOUT_DOUBLINGS));
        e.lockouts += 1;
        e.failures = 0;
        e.locked_until = Some(now + d);
        Some(d)
    }

    pub fn record_success(&mut self, key: &K) {
        self.entries.remove(key);
    }

    /// Drop long-idle entries; call periodically.
    pub fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, e| {
            now.saturating_duration_since(e.last_seen) < FORGET_AFTER
                || e.locked_until.is_some_and(|u| u > now)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_five_and_doubles() {
        let t0 = Instant::now();
        let mut l = AuthLimiter::default();
        let k = 123_456_789u32;
        for _ in 0..4 {
            assert_eq!(l.record_failure(&k, t0), None);
            assert!(l.check(&k, t0).is_ok());
        }
        assert_eq!(l.record_failure(&k, t0), Some(BASE_LOCKOUT));
        assert!(l.check(&k, t0 + Duration::from_secs(59)).is_err());

        let t1 = t0 + BASE_LOCKOUT;
        assert!(l.check(&k, t1).is_ok());
        for _ in 0..4 {
            l.record_failure(&k, t1);
        }
        assert_eq!(l.record_failure(&k, t1), Some(BASE_LOCKOUT * 2));
    }

    #[test]
    fn success_resets() {
        let t0 = Instant::now();
        let mut l = AuthLimiter::default();
        for _ in 0..4 {
            l.record_failure(&"1.2.3.4", t0);
        }
        l.record_success(&"1.2.3.4");
        assert_eq!(l.record_failure(&"1.2.3.4", t0), None);
    }

    #[test]
    fn keys_independent() {
        let t0 = Instant::now();
        let mut l = AuthLimiter::default();
        for _ in 0..5 {
            l.record_failure(&1u32, t0);
        }
        assert!(l.check(&1, t0).is_err());
        assert!(l.check(&2, t0).is_ok());
    }
}
