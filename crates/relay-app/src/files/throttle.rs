//! File transfer throttle (FR-30).
//!
//! QUIC stream priority (FR-28) decides which stream's bytes leave first,
//! but file data still fills the congestion window and the bottleneck
//! queue, and that queueing delay is what video and input feel. So while
//! the video is being squeezed, file transfers are held to a small share
//! of the video bitrate, and given room back once the path is clean.
//!
//! Fed by the same Controller reports as the ABR (every 100 ms):
//!
//! - **squeezed** (queueing delay growing, or heavy loss): files get at most
//!   [`SHARE`] of the video target, halving further on every squeezed
//!   report, never below [`MIN_KBPS`] (transfers slow, never stall);
//! - **clean**: the limit grows by [`GROWTH`] per report and is lifted
//!   entirely above [`RELEASE_KBPS`] (≈ 3 s from the floor);
//! - anything in between holds.
//!
//! The limit is a token bucket shared by every transfer of the session, in
//! both directions: downloads are paced as they are written, uploads as
//! they are read (QUIC flow control then slows the Controller).

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use relay_proto::v1::AbrFeedback;
use relay_session::abr::{LOSS_TRIM_PERMILLE, PROBE_MAX_DELAY_US};

/// Files' share of the video bitrate while the video is squeezed.
pub const SHARE: f64 = 0.1;
pub const MIN_KBPS: u32 = 64;
pub const GROWTH: f64 = 1.25;
/// Above this the limit is lifted.
pub const RELEASE_KBPS: u32 = 100_000;
/// Burst allowance: this much time's worth of bytes.
const BURST: Duration = Duration::from_millis(100);
/// Smallest bucket, so one write slice always fits.
const MIN_BURST_BYTES: f64 = 16.0 * 1024.0;
/// Writes are split into slices this big so pacing is smooth.
pub const SLICE: usize = 16 * 1024;

/// Was this report a sign the video is being squeezed?
pub fn squeezed(fb: &AbrFeedback) -> bool {
    fb.delay_trend_us > PROBE_MAX_DELAY_US * 2 || fb.loss_permille > LOSS_TRIM_PERMILLE
}

/// The next limit (kbps, `None` = unlimited) after one report.
pub fn next_limit(current: Option<u32>, fb: &AbrFeedback, video_kbps: u32) -> Option<u32> {
    if squeezed(fb) {
        let share = ((f64::from(video_kbps) * SHARE) as u32).max(MIN_KBPS);
        Some(match current {
            Some(c) if c <= share => (c / 2).max(MIN_KBPS),
            _ => share,
        })
    } else if fb.delay_trend_us <= PROBE_MAX_DELAY_US {
        current
            .map(|c| (f64::from(c) * GROWTH).ceil() as u32)
            .filter(|&c| c < RELEASE_KBPS)
    } else {
        current
    }
}

#[derive(Debug)]
struct Bucket {
    limit_kbps: Option<u32>,
    /// May go negative: callers reserve, then sleep off the debt.
    tokens: f64,
    last: Instant,
}

/// Shared by every file stream of one session.
#[derive(Debug)]
pub struct Throttle {
    b: Mutex<Bucket>,
}

impl Default for Throttle {
    fn default() -> Self {
        Self {
            b: Mutex::new(Bucket {
                limit_kbps: None,
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }
}

fn bytes_per_sec(kbps: u32) -> f64 {
    f64::from(kbps) * 1000.0 / 8.0
}

impl Throttle {
    #[cfg(test)]
    pub fn limit_kbps(&self) -> Option<u32> {
        self.b.lock().expect("poisoned").limit_kbps
    }

    /// One Controller report; `video_kbps` is the ABR's current target.
    pub fn on_feedback(&self, fb: &AbrFeedback, video_kbps: u32) {
        let mut b = self.b.lock().expect("poisoned");
        let next = next_limit(b.limit_kbps, fb, video_kbps);
        if next != b.limit_kbps {
            match next {
                Some(k) => tracing::debug!("file transfers held to {k} kbps (video squeezed)"),
                None => tracing::debug!("file transfer limit lifted"),
            }
            if b.limit_kbps.is_none() {
                // Start a fresh bucket rather than one full from idle time.
                b.tokens = 0.0;
                b.last = Instant::now();
            }
        }
        b.limit_kbps = next;
    }

    /// Wait until `n` bytes may move.
    pub async fn take(&self, n: usize) {
        let wait = {
            let mut b = self.b.lock().expect("poisoned");
            let Some(kbps) = b.limit_kbps else {
                return;
            };
            let rate = bytes_per_sec(kbps);
            let now = Instant::now();
            let cap = (rate * BURST.as_secs_f64()).max(MIN_BURST_BYTES);
            b.tokens = (b.tokens + rate * now.duration_since(b.last).as_secs_f64()).min(cap);
            b.last = now;
            b.tokens -= n as f64;
            (b.tokens < 0.0).then(|| Duration::from_secs_f64(-b.tokens / rate))
        };
        if let Some(w) = wait {
            tokio::time::sleep(w).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(delay: i32, loss: u32) -> AbrFeedback {
        AbrFeedback {
            delay_trend_us: delay,
            loss_permille: loss,
            ..Default::default()
        }
    }

    #[test]
    fn squeeze_cuts_clean_recovers() {
        let (bad, clean, meh) = (fb(20_000, 0), fb(0, 0), fb(7_000, 0));
        assert_eq!(next_limit(None, &clean, 8_000), None, "free while clean");
        assert_eq!(next_limit(None, &meh, 8_000), None, "grey zone holds");
        let l = next_limit(None, &bad, 8_000);
        assert_eq!(l, Some(800), "10% of the video target");
        assert_eq!(
            next_limit(l, &bad, 8_000),
            Some(400),
            "still squeezed: halve"
        );
        assert_eq!(
            next_limit(Some(70), &bad, 300),
            Some(MIN_KBPS),
            "never below the floor"
        );
        assert_eq!(
            next_limit(Some(800), &fb(0, 200), 8_000),
            Some(400),
            "heavy loss counts"
        );
        assert_eq!(next_limit(l, &meh, 8_000), l, "grey zone holds");
        let mut l = Some(MIN_KBPS);
        let mut reports = 0;
        while l.is_some() {
            l = next_limit(l, &clean, 8_000);
            reports += 1;
        }
        assert!(
            (25..=40).contains(&reports),
            "lifted after {reports} clean reports"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_paces_to_the_limit() {
        let t = Throttle::default();
        // Unlimited: no waiting at all.
        let t0 = Instant::now();
        for _ in 0..100 {
            t.take(1 << 20).await;
        }
        assert_eq!(t0.elapsed(), Duration::ZERO);

        t.on_feedback(&fb(20_000, 0), 8_000); // 800 kbps = 100 kB/s
        assert_eq!(t.limit_kbps(), Some(800));
        let t0 = Instant::now();
        for _ in 0..20 {
            t.take(SLICE).await; // 320 KiB ≈ 3.3 s at 100 kB/s
        }
        let el = t0.elapsed().as_secs_f64();
        assert!((2.9..4.0).contains(&el), "took {el:.2} s");
    }
}
