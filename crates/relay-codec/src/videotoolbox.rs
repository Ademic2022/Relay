//! VideoToolbox hardware H.264 encoder (§6 "Encode", macOS).
//!
//! - Low-latency rate control (`EnableLowLatencyRateControl`), hardware
//!   required, `RealTime`, `AllowFrameReordering = false`,
//!   `MaxFrameDelayCount = 0`: no B-frames, no lookahead (§6 rule 1).
//! - Takes the IOSurface-backed `CVPixelBuffer` from ScreenCaptureKit
//!   directly: zero-copy capture → encode (§6 rule 2).
//! - Constrained Baseline profile (see `configure`).
//! - Keyframes only on request (`ForceKeyFrame`), never periodic.
//! - Output is converted from AVCC to Annex-B, with SPS/PPS before each
//!   keyframe, so any H.264 decoder can join at a keyframe.

#![allow(unsafe_code, non_upper_case_globals, non_snake_case)]

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use bytes::Bytes;
use relay_capture::macos::MacFrame;
use relay_capture::{CapturedFrame, FrameData};

use crate::*;

// ------------------------------------------------------------ FFI

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type OSStatus = i32;
type VTCompressionSessionRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CVImageBufferRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const kCMTimeFlags_Valid: u32 = 1;
const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
const kCFNumberSInt32Type: CFIndex = 3;
const kCFNumberFloat64Type: CFIndex = 13;

#[repr(C)]
struct CFDictionaryKeyCallBacks([usize; 6]);
#[repr(C)]
struct CFDictionaryValueCallBacks([usize; 5]);

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample: CMSampleBufferRef,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: CFIndex, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        kcb: *const CFDictionaryKeyCallBacks,
        vcb: *const CFDictionaryValueCallBacks,
    ) -> CFDictionaryRef;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFArrayGetCount(a: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: CFIndex) -> CFTypeRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetDataBuffer(sb: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sb: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: CMSampleBufferRef, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(bb: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        bb: CMBlockBufferRef,
        offset: usize,
        len: usize,
        dst: *mut c_void,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    fn VTCompressionSessionCreate(
        alloc: CFAllocatorRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        compressed_alloc: CFAllocatorRef,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image: CVImageBufferRef,
        pts: CMTime,
        duration: CMTime,
        frame_props: CFDictionaryRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        until: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}

fn check(st: OSStatus, what: &str) -> Result<(), CodecError> {
    if st == 0 {
        Ok(())
    } else {
        Err(CodecError::Platform(format!("{what}: OSStatus {st}")))
    }
}

/// Owned CF object released on drop.
struct Cf(CFTypeRef);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created with a +1 retain count by a Create function.
            unsafe { CFRelease(self.0) }
        }
    }
}

fn cf_i32(v: i32) -> Cf {
    // SAFETY: valid pointer to an i32 of the declared type.
    Cf(unsafe { CFNumberCreate(ptr::null(), kCFNumberSInt32Type, (&raw const v).cast()) })
}

fn cf_f64(v: f64) -> Cf {
    // SAFETY: as above.
    Cf(unsafe { CFNumberCreate(ptr::null(), kCFNumberFloat64Type, (&raw const v).cast()) })
}

fn cf_dict(pairs: &[(CFStringRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: keys/vals are valid CF objects; callbacks retain them.
    Cf(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            pairs.len() as CFIndex,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}

// ------------------------------------------------------------ encoder

/// Where the output callback leaves its result for `encode` to pick up.
/// Annex-B bytes and keyframe flag from one callback.
type Encoded = Result<(Vec<u8>, bool), CodecError>;

#[derive(Default)]
struct Output(Mutex<Option<Encoded>>);

pub struct VideoToolboxEncoder {
    config: EncoderConfig,
    session: VTCompressionSessionRef,
    output: Box<Output>,
}

// SAFETY: the session is only used through &mut self; VideoToolbox
// sessions may be driven from any thread.
unsafe impl Send for VideoToolboxEncoder {}

impl std::fmt::Debug for VideoToolboxEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolboxEncoder")
            .field("config", &self.config)
            .finish()
    }
}

extern "C" fn on_output(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: OSStatus,
    _flags: u32,
    sample: CMSampleBufferRef,
) {
    // SAFETY: refcon is the Box<Output> owned by the encoder, which
    // outlives the session (invalidated in Drop before the box is freed).
    let out = unsafe { &*(refcon as *const Output) };
    let res = if status != 0 {
        Err(CodecError::Platform(format!(
            "encode callback: OSStatus {status}"
        )))
    } else if sample.is_null() {
        Err(CodecError::Platform("encoder dropped the frame".into()))
    } else {
        // SAFETY: sample is a valid CMSampleBuffer for this callback.
        unsafe { to_annex_b(sample) }
    };
    *out.0.lock().expect("poisoned") = Some(res);
}

/// AVCC sample → Annex-B bytes, plus keyframe flag.
unsafe fn to_annex_b(sample: CMSampleBufferRef) -> Encoded {
    const START: [u8; 4] = [0, 0, 0, 1];
    unsafe {
        let is_key = {
            let arr = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
            if arr.is_null() || CFArrayGetCount(arr) == 0 {
                true
            } else {
                let dict = CFArrayGetValueAtIndex(arr, 0);
                let not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync);
                not_sync.is_null() || not_sync == kCFBooleanFalse
            }
        };

        let mut out = Vec::new();
        if is_key {
            let desc = CMSampleBufferGetFormatDescription(sample);
            let mut count = 0usize;
            let mut nal_len = 0i32;
            check(
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    desc,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &raw mut count,
                    &raw mut nal_len,
                ),
                "parameter set count",
            )?;
            for i in 0..count {
                let mut p: *const u8 = ptr::null();
                let mut n = 0usize;
                check(
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        desc,
                        i,
                        &raw mut p,
                        &raw mut n,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    ),
                    "parameter set",
                )?;
                out.extend_from_slice(&START);
                out.extend_from_slice(std::slice::from_raw_parts(p, n));
            }
        }

        let bb = CMSampleBufferGetDataBuffer(sample);
        let len = CMBlockBufferGetDataLength(bb);
        let mut avcc = vec![0u8; len];
        check(
            CMBlockBufferCopyDataBytes(bb, 0, len, avcc.as_mut_ptr().cast()),
            "copy bitstream",
        )?;
        let mut i = 0;
        while i + 4 <= avcc.len() {
            let n = u32::from_be_bytes(avcc[i..i + 4].try_into().expect("4")) as usize;
            i += 4;
            let end = i
                .checked_add(n)
                .filter(|&e| e <= avcc.len())
                .ok_or_else(|| CodecError::Bitstream("NAL length past end of sample".into()))?;
            out.extend_from_slice(&START);
            out.extend_from_slice(&avcc[i..end]);
            i = end;
        }
        Ok((out, is_key))
    }
}

impl VideoToolboxEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
        if config.codec != Codec::H264 {
            return Err(CodecError::Unavailable);
        }
        let output = Box::new(Output::default());
        let mut session: VTCompressionSessionRef = ptr::null_mut();
        // SAFETY: FFI with valid arguments; on success we own `session`.
        unsafe {
            let spec = cf_dict(&[
                (
                    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
                    kCFBooleanTrue,
                ),
                (
                    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                    kCFBooleanTrue,
                ),
            ]);
            let st = VTCompressionSessionCreate(
                ptr::null(),
                config.width as i32,
                config.height as i32,
                kCMVideoCodecType_H264,
                spec.0,
                ptr::null(),
                ptr::null(),
                on_output,
                (&raw const *output).cast_mut().cast(),
                &raw mut session,
            );
            if st != 0 {
                return Err(CodecError::Unavailable);
            }
        }
        let mut enc = Self {
            config,
            session,
            output,
        };
        enc.configure()?;
        Ok(enc)
    }

    fn set(&self, key: CFStringRef, value: CFTypeRef, what: &str) -> Result<(), CodecError> {
        // SAFETY: valid session and CF objects.
        check(
            unsafe { VTSessionSetProperty(self.session, key, value) },
            what,
        )
    }

    fn configure(&mut self) -> Result<(), CodecError> {
        let c = &self.config;
        // SAFETY: static CF constants from VideoToolbox / CoreFoundation.
        unsafe {
            self.set(
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue,
                "RealTime",
            )?;
            self.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
                "AllowFrameReordering",
            )?;
            // Constrained Baseline: decodable by every H.264 decoder,
            // including the openh264 fallback (which lacks CABAC). Switch to
            // Constrained High once both ends negotiate hardware decoding.
            self.set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
                "ProfileLevel",
            )?;
            let delay = cf_i32(0);
            // Not all encoders expose this; low-latency RC already implies it.
            let _ = self.set(
                kVTCompressionPropertyKey_MaxFrameDelayCount,
                delay.0,
                "MaxFrameDelayCount",
            );
            let fps = cf_f64(f64::from(c.fps));
            self.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                fps.0,
                "ExpectedFrameRate",
            )?;
            // Effectively "never": keyframes only on explicit request.
            let gop = cf_i32(i32::MAX);
            self.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                gop.0,
                "MaxKeyFrameInterval",
            )?;
            let _ = self.set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                kCFBooleanTrue,
                "PrioritizeEncodingSpeedOverQuality",
            );
            self.apply_bitrate()?;
            check(
                VTCompressionSessionPrepareToEncodeFrames(self.session),
                "PrepareToEncodeFrames",
            )?;
        }
        Ok(())
    }

    fn apply_bitrate(&self) -> Result<(), CodecError> {
        let bps = cf_i32(
            i32::try_from(self.config.bitrate_kbps.saturating_mul(1000)).unwrap_or(i32::MAX),
        );
        // SAFETY: static CF constant.
        self.set(
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            bps.0,
            "AverageBitRate",
        )
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // SAFETY: we own the session; invalidate stops callbacks before
        // `output` is freed.
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast_const());
        }
    }
}

impl Encoder for VideoToolboxEncoder {
    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &CapturedFrame,
        capture_ts_us: u64,
        force_key: bool,
    ) -> Result<EncodedFrame, CodecError> {
        let FrameData::Native(native) = &frame.data else {
            return Err(CodecError::BadInput);
        };
        let mac = native
            .as_any()
            .downcast_ref::<MacFrame>()
            .ok_or(CodecError::BadInput)?;
        let pts = CMTime {
            value: capture_ts_us as i64,
            timescale: 1_000_000,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        };
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        };
        // SAFETY: static CF constants.
        let props = force_key.then(|| unsafe {
            cf_dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)])
        });
        *self.output.0.lock().expect("poisoned") = None;
        // SAFETY: the pixel buffer is alive for the call (`frame` borrows
        // it); VideoToolbox retains it as long as it needs it.
        unsafe {
            check(
                VTCompressionSessionEncodeFrame(
                    self.session,
                    mac.cv_pixel_buffer_ptr(),
                    pts,
                    invalid,
                    props.as_ref().map_or(ptr::null(), |d| d.0),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "EncodeFrame",
            )?;
            // Return this frame's output now: no frame is held back.
            check(
                VTCompressionSessionCompleteFrames(self.session, pts),
                "CompleteFrames",
            )?;
        }
        let (data, is_key) = self
            .output
            .0
            .lock()
            .expect("poisoned")
            .take()
            .ok_or_else(|| CodecError::Platform("no output after CompleteFrames".into()))??;
        Ok(EncodedFrame {
            data: Bytes::from(data),
            is_key,
            capture_ts_us,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        self.config.bitrate_kbps = kbps;
        self.apply_bitrate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openh264::OpenH264Decoder;
    use relay_capture::{PixelFormat, Rect};
    use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
    use std::sync::Arc;
    use std::time::Instant;

    const FMT_420V: u32 = u32::from_be_bytes(*b"420v");

    fn frame(w: usize, h: usize, shift: usize) -> CapturedFrame {
        let pb = CVPixelBuffer::create(w, h, FMT_420V).expect("CVPixelBufferCreate");
        {
            let mut g = pb.lock(CVPixelBufferLockFlags::from_bits(0)).expect("lock");
            let ys = g.bytes_per_row_of_plane(0);
            let uvs = g.bytes_per_row_of_plane(1);
            let yp = g.base_address_of_plane_mut(0).unwrap();
            let uvp = g.base_address_of_plane_mut(1).unwrap();
            // SAFETY: plane pointers and strides from CoreVideo, locked.
            unsafe {
                for row in 0..h {
                    for col in 0..w {
                        *yp.add(row * ys + col) = (16 + (col + shift) % 200) as u8;
                    }
                }
                for row in 0..h / 2 {
                    std::ptr::write_bytes(uvp.add(row * uvs), 128, w);
                }
            }
        }
        CapturedFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Nv12,
            data: FrameData::Native(Arc::new(MacFrame { pixel_buffer: pb })),
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
    fn hardware_encode_then_software_decode() {
        let cfg = EncoderConfig::low_latency(Codec::H264, 1280, 720, 60, 8_000);
        let mut enc = match VideoToolboxEncoder::new(cfg) {
            Ok(e) => e,
            Err(CodecError::Unavailable) => return eprintln!("no HW H.264 encoder; skipped"),
            Err(e) => panic!("{e}"),
        };
        let mut dec = OpenH264Decoder::new().unwrap();
        let mut keys = 0;
        let mut decoded = 0;
        let t = Instant::now();
        for n in 0..30 {
            let e = enc
                .encode(&frame(1280, 720, n * 3), n as u64 * 16_666, n == 20)
                .unwrap();
            keys += e.is_key as u32;
            if n == 0 || n == 20 {
                assert!(e.is_key, "frame {n} must be key");
                assert_eq!(&e.data[..4], &[0, 0, 0, 1]);
            }
            if let Some(d) = dec.decode(&e.data, e.capture_ts_us).unwrap() {
                assert_eq!((d.width, d.height), (1280, 720));
                decoded += 1;
            }
        }
        assert_eq!(keys, 2, "keyframes only on request");
        assert_eq!(decoded, 30, "no frame held back by the encoder");
        eprintln!("30 HW encodes + SW decodes in {:?}", t.elapsed());
    }
}

#[cfg(test)]
mod stamp_tests {
    use super::*;
    use crate::openh264::OpenH264Decoder;
    use relay_capture::pattern::{draw, read_stamp};
    use relay_capture::{PixelFormat, Rect};
    use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
    use std::sync::Arc;
    use std::time::Instant;

    fn pattern_frame(w: usize, h: usize, n: u64, ms: u32) -> CapturedFrame {
        let pb = CVPixelBuffer::create(w, h, u32::from_be_bytes(*b"420v")).unwrap();
        {
            let mut g = pb.lock(CVPixelBufferLockFlags::from_bits(0)).unwrap();
            let (ys, uvs) = (g.bytes_per_row_of_plane(0), g.bytes_per_row_of_plane(1));
            let yp = g.base_address_of_plane_mut(0).unwrap();
            let uvp = g.base_address_of_plane_mut(1).unwrap();
            // SAFETY: locked planes, CoreVideo sizes.
            let (y, uv) = unsafe {
                (
                    std::slice::from_raw_parts_mut(yp, ys * h),
                    std::slice::from_raw_parts_mut(uvp, uvs * h / 2),
                )
            };
            draw(y, ys, uv, uvs, w, h, n, ms);
        }
        CapturedFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Nv12,
            data: FrameData::Native(Arc::new(MacFrame { pixel_buffer: pb })),
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
    fn every_frame_fully_updates() {
        let (w, h) = (1920, 1080);
        let Ok(mut enc) =
            VideoToolboxEncoder::new(EncoderConfig::low_latency(Codec::H264, w, h, 60, 8_000))
        else {
            return;
        };
        let mut dec = OpenH264Decoder::new().unwrap();
        let mut bad = Vec::new();
        for n in 0..120u64 {
            let ms = 1_000_000 + n as u32 * 17;
            let e = enc
                .encode(
                    &pattern_frame(w as usize, h as usize, n, ms),
                    n * 16_667,
                    false,
                )
                .unwrap();
            let d = dec.decode(&e.data, 0).unwrap().unwrap();
            let crate::DecodedImage::I420 { y, y_stride, .. } = &d.image else {
                panic!()
            };
            let got = read_stamp(y, *y_stride, w as usize, h as usize);
            if got != ms {
                bad.push((n, ms as i64 - got as i64, e.data.len()));
            }
        }
        assert!(
            bad.is_empty(),
            "{} stale frames (n, expected-got, bytes): {:?}",
            bad.len(),
            &bad[..bad.len().min(12)]
        );
    }
}
