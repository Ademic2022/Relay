//! Video codec abstraction (§6 "Encode"/"Decode").
//!
//! Every encoder must honour §6 rule 1: no B-frames, no lookahead, no
//! frame reordering, VBV ≤ 1 frame. [`EncoderConfig::low_latency`] is the
//! only constructor, so these settings cannot be forgotten.

use bytes::Bytes;
use relay_capture::CapturedFrame;

pub mod mock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    Cbr,
    LowDelayVbr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub rate_control: RateControl,
    /// VBV buffer in frames. Must be ≤ 1.0.
    pub vbv_frames: f32,
    /// Rolling I-slices instead of periodic IDRs (§6 rule 6).
    pub intra_refresh: bool,
    pub chroma_444: bool,
    // Fixed by construction; exposed read-only for backends and tests.
    b_frames: u32,
    lookahead: u32,
}

impl EncoderConfig {
    pub fn low_latency(codec: Codec, width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            codec,
            width,
            height,
            fps,
            bitrate_kbps,
            rate_control: RateControl::LowDelayVbr,
            vbv_frames: 1.0,
            intra_refresh: true,
            chroma_444: false,
            b_frames: 0,
            lookahead: 0,
        }
    }

    pub fn b_frames(&self) -> u32 {
        self.b_frames
    }

    pub fn lookahead(&self) -> u32 {
        self.lookahead
    }

    /// VBV buffer size in kilobits for this bitrate and framerate.
    pub fn vbv_kbits(&self) -> u32 {
        let per_frame = self.bitrate_kbps as f32 / self.fps.max(1) as f32;
        (per_frame * self.vbv_frames.min(1.0)).ceil() as u32
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub is_key: bool,
    pub capture_ts_us: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("codec not available on this machine")]
    Unavailable,
    #[error("unsupported frame format")]
    BadInput,
    #[error("bitstream error: {0}")]
    Bitstream(String),
    #[error("platform error: {0}")]
    Platform(String),
}

pub trait Encoder: Send {
    fn config(&self) -> &EncoderConfig;
    /// Encode one frame. Synchronous and returns this frame's output
    /// directly: an encoder that buffers frames violates §6 rule 1.
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        capture_ts_us: u64,
        force_key: bool,
    ) -> Result<EncodedFrame, CodecError>;
    /// Apply a new bitrate without a keyframe (ABR, §6 rule 7).
    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError>;
}

/// A decoded frame. Real decoders return a GPU texture handle that the
/// renderer samples directly (zero-copy to present).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub gpu_handle: Option<u64>,
    pub capture_ts_us: u64,
}

pub trait Decoder: Send {
    fn decode(
        &mut self,
        data: &[u8],
        capture_ts_us: u64,
    ) -> Result<Option<DecodedFrame>, CodecError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_latency_invariants() {
        let c = EncoderConfig::low_latency(Codec::H264, 1920, 1080, 60, 12_000);
        assert_eq!(c.b_frames(), 0);
        assert_eq!(c.lookahead(), 0);
        assert!(c.vbv_frames <= 1.0);
        assert_eq!(c.vbv_kbits(), 200);
    }
}
