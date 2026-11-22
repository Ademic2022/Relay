//! Software H.264 fallback via Cisco openh264 (§5.1).
//!
//! Used when no hardware encoder/decoder is available. This path needs CPU
//! pixel access, so it reads the captured surface (NV12 → I420) and copies
//! decoded planes out. Those copies are inherent to software coding and
//! are why hardware codecs are the default (§6 rule 2).

use ::openh264::OpenH264API;
use ::openh264::decoder::{Decoder as OhDecoder, DecoderConfig};
use ::openh264::encoder::{
    BitRate, Complexity, Encoder as OhEncoder, EncoderConfig as OhConfig, FrameRate, FrameType,
    IntraFramePeriod, RateControlMode, UsageType,
};
use ::openh264::formats::{YUVSlices, YUVSource};
use bytes::Bytes;
use relay_capture::{CapturedFrame, FrameData};

use crate::*;

pub struct OpenH264Encoder {
    config: EncoderConfig,
    enc: OhEncoder,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    out: Vec<u8>,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("config", &self.config)
            .finish()
    }
}

fn oh_config(c: &EncoderConfig) -> OhConfig {
    OhConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(c.bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(c.fps as f32))
        // No periodic IDR: keyframes only on request (§6 rule 6).
        .intra_frame_period(IntraFramePeriod::from_num_frames(0))
        .skip_frames(false)
        .complexity(Complexity::Low)
        .num_threads(0)
}

impl OpenH264Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
        if config.codec != Codec::H264 {
            return Err(CodecError::Unavailable);
        }
        let enc = OhEncoder::with_api_config(OpenH264API::from_source(), oh_config(&config))
            .map_err(|e| CodecError::Platform(e.to_string()))?;
        Ok(Self {
            config,
            enc,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            out: Vec::with_capacity(256 * 1024),
        })
    }

    /// Fill the I420 scratch planes from the captured frame.
    fn load(&mut self, frame: &CapturedFrame) -> Result<(usize, usize), CodecError> {
        let (y, u, v) = (&mut self.y, &mut self.u, &mut self.v);
        let mut dims = None;
        match &frame.data {
            FrameData::Native(native) => native
                .with_nv12(&mut |p| {
                    let (w, h) = (p.width & !1, p.height & !1);
                    y.resize(w * h, 0);
                    u.resize(w * h / 4, 0);
                    v.resize(w * h / 4, 0);
                    for row in 0..h {
                        let src = &p.y[row * p.y_stride..row * p.y_stride + w];
                        y[row * w..row * w + w].copy_from_slice(src);
                    }
                    let cw = w / 2;
                    for row in 0..h / 2 {
                        let src = &p.uv[row * p.uv_stride..row * p.uv_stride + w];
                        for (i, pair) in src.as_chunks::<2>().0.iter().enumerate() {
                            u[row * cw + i] = pair[0];
                            v[row * cw + i] = pair[1];
                        }
                    }
                    dims = Some((w, h));
                })
                .map_err(|e| CodecError::Platform(e.to_string()))?,
            FrameData::Cpu { .. } => return Err(CodecError::BadInput),
        }
        dims.ok_or(CodecError::BadInput)
    }
}

impl Encoder for OpenH264Encoder {
    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        capture_ts_us: u64,
        force_key: bool,
    ) -> Result<EncodedFrame, CodecError> {
        let (w, h) = self.load(frame)?;
        if force_key {
            self.enc.force_intra_frame();
        }
        let src = YUVSlices::new((&self.y, &self.u, &self.v), (w, h), (w, w / 2, w / 2));
        debug_assert_eq!(src.dimensions(), (w, h));
        let bs = self
            .enc
            .encode(&src)
            .map_err(|e| CodecError::Platform(e.to_string()))?;
        let is_key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        self.out.clear();
        bs.write_vec(&mut self.out);
        Ok(EncodedFrame {
            data: Bytes::copy_from_slice(&self.out),
            is_key,
            capture_ts_us,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        if kbps == self.config.bitrate_kbps {
            return Ok(());
        }
        self.config.bitrate_kbps = kbps;
        // openh264's safe API has no live bitrate setter; rebuilding costs
        // one keyframe. ABR changes are rate-limited by the caller.
        self.enc = OhEncoder::with_api_config(OpenH264API::from_source(), oh_config(&self.config))
            .map_err(|e| CodecError::Platform(e.to_string()))?;
        Ok(())
    }
}

pub struct OpenH264Decoder {
    dec: OhDecoder,
}

impl std::fmt::Debug for OpenH264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OpenH264Decoder")
    }
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self, CodecError> {
        let dec = OhDecoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|e| CodecError::Platform(e.to_string()))?;
        Ok(Self { dec })
    }
}

impl Decoder for OpenH264Decoder {
    fn decode(
        &mut self,
        data: &[u8],
        capture_ts_us: u64,
    ) -> Result<Option<DecodedFrame>, CodecError> {
        let Some(yuv) = self
            .dec
            .decode(data)
            .map_err(|e| CodecError::Bitstream(e.to_string()))?
        else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let (ys, us, _) = yuv.strides();
        // Copy out of the decoder's internal buffer, which the next decode
        // call overwrites. ~3 MB at 1080p; software path only.
        Ok(Some(DecodedFrame {
            width: w as u32,
            height: h as u32,
            image: DecodedImage::I420 {
                y: yuv.y()[..ys * h].to_vec(),
                u: yuv.u()[..us * (h / 2)].to_vec(),
                v: yuv.v()[..us * (h / 2)].to_vec(),
                y_stride: ys,
                uv_stride: us,
            },
            capture_ts_us,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_capture::{NativeFrame, Nv12, PixelFormat, Rect};
    use std::any::Any;
    use std::sync::Arc;
    use std::time::Instant;

    #[derive(Debug)]
    struct TestFrame {
        w: usize,
        h: usize,
        y: Vec<u8>,
        uv: Vec<u8>,
    }

    impl NativeFrame for TestFrame {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn with_nv12(
            &self,
            f: &mut dyn FnMut(Nv12<'_>),
        ) -> Result<(), relay_capture::CaptureError> {
            f(Nv12 {
                width: self.w,
                height: self.h,
                y: &self.y,
                y_stride: self.w,
                uv: &self.uv,
                uv_stride: self.w,
            });
            Ok(())
        }
    }

    fn frame(w: usize, h: usize, shift: usize) -> CapturedFrame {
        let y = (0..w * h).map(|i| ((i % w + shift) % 256) as u8).collect();
        let uv = vec![128u8; w * h / 2];
        CapturedFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Nv12,
            data: FrameData::Native(Arc::new(TestFrame { w, h, y, uv })),
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: w as u32,
                h: h as u32,
            }],
            captured_at: Instant::now(),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let cfg = EncoderConfig::low_latency(Codec::H264, 320, 240, 30, 1_000);
        let mut enc = OpenH264Encoder::new(cfg).unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();
        let mut decoded = 0;
        for n in 0..10 {
            let e = enc
                .encode(&frame(320, 240, n * 4), n as u64, false)
                .unwrap();
            if n == 0 {
                assert!(e.is_key);
            }
            if let Some(d) = dec.decode(&e.data, e.capture_ts_us).unwrap() {
                assert_eq!((d.width, d.height), (320, 240));
                let DecodedImage::I420 { y, y_stride, .. } = &d.image else {
                    panic!("expected I420");
                };
                // Luma ramp survives (lossy, so loose tolerance).
                let px = y[10 * y_stride + 100] as i32;
                assert!((px - ((100 + n * 4) % 256) as i32).abs() < 12, "px={px}");
                decoded += 1;
            }
        }
        assert_eq!(decoded, 10, "low-latency decoder outputs every frame");
        assert!(enc.encode(&frame(320, 240, 0), 99, true).unwrap().is_key);
    }
}

#[cfg(test)]
mod stamp_tests {
    use super::*;
    use relay_capture::pattern::{mem_frame, read_stamp};
    use relay_capture::{PixelFormat, Rect};
    use std::time::Instant;

    #[test]
    fn software_round_trip_updates_every_frame() {
        let (w, h) = (1920usize, 1080usize);
        let mut enc = OpenH264Encoder::new(EncoderConfig::low_latency(
            Codec::H264,
            w as u32,
            h as u32,
            60,
            8_000,
        ))
        .unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();
        let mut bad = Vec::new();
        for n in 0..60u64 {
            let ms = 1_000_000 + n as u32 * 17;
            let f = CapturedFrame {
                width: w as u32,
                height: h as u32,
                format: PixelFormat::Nv12,
                data: FrameData::Native(mem_frame(w, h, n, ms)),
                dirty: vec![Rect {
                    x: 0,
                    y: 0,
                    w: w as u32,
                    h: h as u32,
                }],
                captured_at: Instant::now(),
            };
            let e = enc.encode(&f, n, false).unwrap();
            let d = dec.decode(&e.data, 0).unwrap().unwrap();
            let DecodedImage::I420 { y, y_stride, .. } = &d.image else {
                panic!()
            };
            let got = read_stamp(y, *y_stride, w, h);
            if got != ms {
                bad.push((n, ms as i64 - got as i64, e.data.len()));
            }
        }
        assert!(
            bad.is_empty(),
            "{} stale: {:?}",
            bad.len(),
            &bad[..bad.len().min(12)]
        );
    }
}
