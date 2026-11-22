//! The Controller's 100 ms ABR report (§6 rule 7).
//!
//! - **Receive rate** from bytes seen.
//! - **Packet loss** from video shards expected vs. received. Frame loss
//!   would read ~0 once FEC recovers frames, yet the Host needs the real
//!   packet loss to size FEC. Smoothed: a window holds few packets.
//! - **Queueing delay**: one-way delay is `arrival − capture_ts`, sampled
//!   on every datagram and minimised per window, so a large frame's own
//!   transmission time does not look like queueing. Clocks are not
//!   synchronised, so the absolute value is meaningless, but its growth
//!   above the recent minimum is queueing delay.

use std::collections::VecDeque;
use std::time::Instant;

use relay_net::video::ReassemblyStats;
use relay_proto::v1;

use crate::clock;

pub struct Feedback {
    pub bytes: usize,
    since: Instant,
    window_min: Option<i64>,
    history: VecDeque<i64>,
    last: ReassemblyStats,
    loss_ewma: f64,
    /// Shards counted since the loss estimate last updated. At low rates a
    /// 100 ms window holds only a few; the estimate waits for enough
    /// rather than freezing at an old value.
    pending: (u64, u64),
}

impl Default for Feedback {
    fn default() -> Self {
        Self::new()
    }
}

impl Feedback {
    const BASELINE_WINDOWS: usize = 50; // 5 s
    const LOSS_ALPHA: f64 = 0.3;
    const MIN_SAMPLE: u64 = 20;

    pub fn new() -> Self {
        Self {
            bytes: 0,
            since: Instant::now(),
            window_min: None,
            history: VecDeque::new(),
            last: ReassemblyStats::default(),
            loss_ewma: 0.0,
            pending: (0, 0),
        }
    }

    /// Call for every video datagram (with its header's capture time).
    pub fn on_datagram(&mut self, datagram: &[u8]) {
        if let Ok(h) = relay_net::video::SliceHeader::parse(datagram) {
            self.on_frame(h.capture_ts_us);
        }
    }

    pub fn on_frame(&mut self, capture_ts_us: u64) {
        let owd = clock::now_us() as i64 - capture_ts_us as i64;
        self.window_min = Some(self.window_min.map_or(owd, |m| m.min(owd)));
    }

    pub fn report(&mut self, st: &ReassemblyStats) -> v1::AbrFeedback {
        let el = self.since.elapsed().as_secs_f64().max(1e-3);
        let receive_kbps = (self.bytes as f64 * 8.0 / el / 1000.0) as u32;
        self.pending.0 += st.shards_expected.saturating_sub(self.last.shards_expected);
        self.pending.1 += st.shards_received.saturating_sub(self.last.shards_received);
        let (expected, received) = self.pending;
        if expected >= Self::MIN_SAMPLE {
            let loss = 1.0 - (received as f64 / expected as f64).min(1.0);
            self.loss_ewma += Self::LOSS_ALPHA * (loss - self.loss_ewma);
            self.pending = (0, 0);
        }
        let mut delay_trend_us = 0;
        if let Some(m) = self.window_min.take() {
            let baseline = self.history.iter().copied().min().unwrap_or(m).min(m);
            delay_trend_us = i32::try_from(m - baseline).unwrap_or(i32::MAX);
            self.history.push_back(m);
            if self.history.len() > Self::BASELINE_WINDOWS {
                self.history.pop_front();
            }
        }
        let dropped = st.frames_dropped.saturating_sub(self.last.frames_dropped);
        self.bytes = 0;
        self.since = Instant::now();
        self.last = *st;
        v1::AbrFeedback {
            receive_kbps,
            loss_permille: (self.loss_ewma * 1000.0).round() as u32,
            delay_trend_us,
            decode_time_us: 0,
            frames_dropped: dropped as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_from_shards() {
        let mut fb = Feedback::new();
        let mut st = ReassemblyStats::default();
        for _ in 0..20 {
            st.shards_expected += 100;
            st.shards_received += 98;
            fb.report(&st);
        }
        let r = fb.report(&ReassemblyStats {
            shards_expected: st.shards_expected + 100,
            shards_received: st.shards_received + 98,
            ..st
        });
        assert!((18..=22).contains(&r.loss_permille), "{}", r.loss_permille);
    }

    #[test]
    fn low_rate_does_not_freeze_the_estimate() {
        let mut fb = Feedback::new();
        let mut st = ReassemblyStats::default();
        // Congested: 50% loss.
        for _ in 0..10 {
            st.shards_expected += 40;
            st.shards_received += 20;
            fb.report(&st);
        }
        // Then a trickle with no loss: 2 shards per window.
        let mut last = 0;
        for _ in 0..100 {
            st.shards_expected += 2;
            st.shards_received += 2;
            last = fb.report(&st).loss_permille;
        }
        assert!(last < 20, "estimate recovered: {last}");
    }
}
