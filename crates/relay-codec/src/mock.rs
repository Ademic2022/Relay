//! Pass-through codec for tests: the "bitstream" is a tiny header.

use bytes::{BufMut, BytesMut};
use relay_capture::CapturedFrame;

use crate::*;

#[derive(Debug)]
pub struct MockEncoder {
    config: EncoderConfig,
    frames: u64,
}

impl MockEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        Self { config, frames: 0 }
    }
}

impl Encoder for MockEncoder {
    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        capture_ts_us: u64,
        force_key: bool,
    ) -> Result<EncodedFrame, CodecError> {
        let is_key = force_key || self.frames == 0;
        self.frames += 1;
        // Size roughly tracks bitrate so ABR/network tests see real load.
        let size = (self.config.bitrate_kbps as usize * 1000 / 8 / self.config.fps.max(1) as usize)
            .max(16);
        let mut b = BytesMut::with_capacity(size);
        b.put_u32_le(frame.width);
        b.put_u32_le(frame.height);
        b.put_u8(is_key as u8);
        b.resize(size, 0);
        Ok(EncodedFrame {
            data: b.freeze(),
            is_key,
            capture_ts_us,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        self.config.bitrate_kbps = kbps;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MockDecoder;

impl Decoder for MockDecoder {
    fn decode(
        &mut self,
        data: &[u8],
        capture_ts_us: u64,
    ) -> Result<Option<DecodedFrame>, CodecError> {
        let hdr: [u8; 8] = data
            .get(..8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| CodecError::Bitstream("short".into()))?;
        Ok(Some(DecodedFrame {
            width: u32::from_le_bytes(hdr[..4].try_into().expect("4")),
            height: u32::from_le_bytes(hdr[4..].try_into().expect("4")),
            gpu_handle: None,
            capture_ts_us,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_capture::{ScreenCapturer, mock::MockCapturer};
    use std::time::Duration;

    #[test]
    fn round_trip() {
        let mut cap = MockCapturer::single_1080p();
        cap.select(1).unwrap();
        let f = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        let mut enc = MockEncoder::new(EncoderConfig::low_latency(
            Codec::H264,
            1920,
            1080,
            60,
            6000,
        ));
        let e = enc.encode(&f, 42, false).unwrap();
        assert!(e.is_key, "first frame is key");
        assert_eq!(e.data.len(), 12_500);
        let d = MockDecoder
            .decode(&e.data, e.capture_ts_us)
            .unwrap()
            .unwrap();
        assert_eq!((d.width, d.height, d.capture_ts_us), (1920, 1080, 42));
        assert!(!enc.encode(&f, 43, false).unwrap().is_key);
        assert!(enc.encode(&f, 44, true).unwrap().is_key);
    }
}
