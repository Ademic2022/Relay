//! Adaptive bitrate controller (§6 rule 7).
//!
//! Fed by Controller feedback every 100 ms. Adjusts in order: bitrate →
//! framerate (60→30) → resolution scale (1.0→0.75→0.5). Queueing-delay
//! growth has priority over throughput: > 10 ms growth cuts bitrate by 25%
//! immediately.

use relay_proto::v1::AbrFeedback;

/// Delay growth that triggers an immediate cut.
pub const DELAY_CUT_US: i32 = 10_000;
pub const DELAY_CUT_FACTOR: f64 = 0.75;
/// Loss above this (per mille) trims bitrate.
pub const LOSS_TRIM_PERMILLE: u32 = 20;
pub const LOSS_TRIM_FACTOR: f64 = 0.9;
/// Consecutive clean reports before probing upward (500 ms at 100 ms).
pub const PROBE_AFTER: u32 = 5;
pub const PROBE_FACTOR: f64 = 1.08;
/// Headroom over the next level's floor needed to step back up.
const UPGRADE_HEADROOM: f64 = 1.3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub fps: u32,
    pub scale: f32,
}

/// Degradation ladder, best first.
pub const LADDER: [Level; 4] = [
    Level {
        fps: 60,
        scale: 1.0,
    },
    Level {
        fps: 30,
        scale: 1.0,
    },
    Level {
        fps: 30,
        scale: 0.75,
    },
    Level {
        fps: 30,
        scale: 0.5,
    },
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbrConfig {
    /// Lowest bitrate acceptable at full resolution, 60 fps.
    pub floor_kbps: u32,
    pub max_kbps: u32,
    pub start_kbps: u32,
    /// Absolute minimum at the bottom of the ladder.
    pub min_kbps: u32,
}

impl AbrConfig {
    /// Speed mode at 1080p60: 3–15 Mbps (§9).
    pub const SPEED_1080P: AbrConfig = AbrConfig {
        floor_kbps: 3_000,
        max_kbps: 15_000,
        start_kbps: 8_000,
        min_kbps: 300,
    };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbrTarget {
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub scale: f32,
}

#[derive(Debug)]
pub struct AbrController {
    cfg: AbrConfig,
    kbps: f64,
    level: usize,
    clean: u32,
}

impl AbrController {
    pub fn new(cfg: AbrConfig) -> Self {
        Self {
            cfg,
            kbps: f64::from(cfg.start_kbps),
            level: 0,
            clean: 0,
        }
    }

    pub fn target(&self) -> AbrTarget {
        let l = LADDER[self.level];
        AbrTarget {
            bitrate_kbps: self.kbps.round() as u32,
            fps: l.fps,
            scale: l.scale,
        }
    }

    /// Bitrate floor for a ladder level, proportional to pixel rate.
    fn floor(&self, level: usize) -> f64 {
        let l = LADDER[level];
        let px = f64::from(l.scale) * f64::from(l.scale);
        let fr = f64::from(l.fps) / 60.0;
        (f64::from(self.cfg.floor_kbps) * px * fr).max(f64::from(self.cfg.min_kbps))
    }

    /// Process one feedback report. Returns the new target if it changed.
    pub fn on_feedback(&mut self, fb: &AbrFeedback) -> Option<AbrTarget> {
        let before = self.target();

        if fb.delay_trend_us > DELAY_CUT_US {
            self.clean = 0;
            self.kbps *= DELAY_CUT_FACTOR;
        } else if fb.loss_permille > LOSS_TRIM_PERMILLE {
            self.clean = 0;
            self.kbps *= LOSS_TRIM_FACTOR;
        } else if fb.delay_trend_us <= 1_000 && fb.loss_permille <= 5 {
            self.clean += 1;
            if self.clean >= PROBE_AFTER {
                self.clean = 0;
                self.kbps *= PROBE_FACTOR;
            }
        } else {
            self.clean = 0;
        }

        // Step down the ladder while below this level's floor.
        while self.level + 1 < LADDER.len() && self.kbps < self.floor(self.level) {
            self.level += 1;
        }
        // Step up when comfortably above the better level's floor.
        while self.level > 0 && self.kbps >= self.floor(self.level - 1) * UPGRADE_HEADROOM {
            self.level -= 1;
        }
        self.kbps = self
            .kbps
            .clamp(f64::from(self.cfg.min_kbps), f64::from(self.cfg.max_kbps));

        let after = self.target();
        (after != before).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(delay_trend_us: i32, loss_permille: u32) -> AbrFeedback {
        AbrFeedback {
            delay_trend_us,
            loss_permille,
            ..Default::default()
        }
    }

    #[test]
    fn delay_growth_cuts_25_percent_immediately() {
        let mut a = AbrController::new(AbrConfig::SPEED_1080P);
        let t = a.on_feedback(&fb(10_001, 0)).unwrap();
        assert_eq!(t.bitrate_kbps, 6_000);
        assert_eq!((t.fps, t.scale), (60, 1.0));
    }

    #[test]
    fn delay_has_priority_over_clean_loss() {
        let mut a = AbrController::new(AbrConfig::SPEED_1080P);
        a.on_feedback(&fb(20_000, 0));
        assert_eq!(a.target().bitrate_kbps, 6_000);
    }

    #[test]
    fn degrade_order_bitrate_then_fps_then_scale() {
        let mut a = AbrController::new(AbrConfig::SPEED_1080P);
        let mut seen = vec![a.target()];
        for _ in 0..30 {
            if let Some(t) = a.on_feedback(&fb(50_000, 0)) {
                seen.push(t);
            }
        }
        let first_fps_drop = seen.iter().position(|t| t.fps == 30).unwrap();
        let first_scale_drop = seen.iter().position(|t| t.scale < 1.0).unwrap();
        assert!(first_fps_drop > 1, "bitrate drops before fps");
        assert!(first_scale_drop > first_fps_drop, "fps drops before scale");
        let last = a.target();
        assert_eq!((last.fps, last.scale), (30, 0.5));
        assert_eq!(last.bitrate_kbps, AbrConfig::SPEED_1080P.min_kbps);
    }

    #[test]
    fn recovers_when_clean() {
        let mut a = AbrController::new(AbrConfig::SPEED_1080P);
        for _ in 0..10 {
            a.on_feedback(&fb(50_000, 0));
        }
        assert!(a.target().fps == 30);
        for _ in 0..2_000 {
            a.on_feedback(&fb(0, 0));
        }
        let t = a.target();
        assert_eq!((t.fps, t.scale), (60, 1.0));
        assert_eq!(t.bitrate_kbps, AbrConfig::SPEED_1080P.max_kbps);
    }

    #[test]
    fn loss_trims() {
        let mut a = AbrController::new(AbrConfig::SPEED_1080P);
        assert_eq!(a.on_feedback(&fb(0, 50)).unwrap().bitrate_kbps, 7_200);
    }
}
