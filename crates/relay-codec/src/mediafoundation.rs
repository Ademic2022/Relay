//! Media Foundation hardware H.264 encoder (§6 "Encode", Windows).
//!
//! - Picks the first hardware H.264 encoder MFT (NVENC, Quick Sync or AMF
//!   through the vendor's MFT). Software MFTs are skipped: the openh264
//!   fallback is used instead.
//! - Shares the capturer's D3D11 device through an `IMFDXGIDeviceManager`
//!   and takes the capturer's NV12 texture as an `IMFSample`: zero-copy
//!   capture → encode (§6 rule 2).
//! - `CODECAPI_AVLowLatencyMode`, low-delay VBR, no B-frames, infinite GOP,
//!   Constrained Baseline (decodable by the openh264 fallback): §6 rule 1.
//! - Hardware MFTs are asynchronous. `encode` feeds one frame and waits for
//!   that frame's output, so no frame is held back.

#![allow(unsafe_code, non_upper_case_globals)] // FFI; MF event constants

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use ::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use ::windows::Win32::Media::MediaFoundation::*;
use ::windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use ::windows::Win32::System::Variant::VARIANT;
use ::windows::core::{GUID, Interface};
use bytes::Bytes;
use relay_capture::windows::WinFrame;
use relay_capture::{CapturedFrame, FrameData};

use crate::*;

/// How long `encode` waits for the MFT's output of one frame.
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(100);

fn plat(what: &str, e: ::windows::core::Error) -> CodecError {
    CodecError::Platform(format!("{what}: {e}"))
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

struct Session {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    out_size: u32,
    need_input: u32,
}

pub struct MediaFoundationEncoder {
    config: EncoderConfig,
    session: Option<Session>,
    out: Vec<u8>,
}

// SAFETY: only driven through &mut self from the encode thread; the MFT
// and D3D device are free-threaded (device is multithread-protected).
unsafe impl Send for MediaFoundationEncoder {}

impl std::fmt::Debug for MediaFoundationEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFoundationEncoder")
            .field("config", &self.config)
            .finish()
    }
}

impl MediaFoundationEncoder {
    /// Checks that a hardware H.264 encoder exists. The MFT itself is
    /// created on the first frame, bound to the capturer's D3D11 device.
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
        if config.codec != Codec::H264 {
            return Err(CodecError::Unavailable);
        }
        // SAFETY: per-thread COM init (ignored if already initialised) and
        // process-wide MF init (reference counted).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, 0).map_err(|e| plat("MFStartup", e))?;
        }
        if enum_hardware()?.is_empty() {
            return Err(CodecError::Unavailable);
        }
        Ok(Self {
            config,
            session: None,
            out: Vec::with_capacity(256 * 1024),
        })
    }

    fn open(&mut self, frame: &WinFrame) -> Result<(), CodecError> {
        let c = &self.config;
        let mut last_err = CodecError::Unavailable;
        for activate in enum_hardware()? {
            // SAFETY: COM activation and configuration of a fresh MFT.
            match unsafe { open_mft(&activate, c, frame) } {
                Ok(s) => {
                    self.session = Some(s);
                    return Ok(());
                }
                Err(e) => {
                    tracing_warn(&format!("hardware MFT rejected: {e}"));
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }
}

fn tracing_warn(msg: &str) {
    // relay-codec has no tracing dependency; stderr keeps it visible.
    eprintln!("relay-codec: {msg}");
}

fn enum_hardware() -> Result<Vec<IMFActivate>, CodecError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: MFTEnumEx allocates `list` with CoTaskMemAlloc; we take
    // ownership of each element and free the array.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut n,
        )
        .map_err(|e| plat("MFTEnumEx", e))?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            if let Some(a) = std::ptr::read(list.add(i)) {
                out.push(a);
            }
        }
        if !list.is_null() {
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

unsafe fn set_codec_api(
    api: &ICodecAPI,
    key: &GUID,
    v: VARIANT,
    required: bool,
) -> Result<(), CodecError> {
    // SAFETY: valid GUID and VARIANT pointers for the call.
    match unsafe { api.SetValue(key, &v) } {
        Ok(()) => Ok(()),
        Err(e) if required => Err(plat("ICodecAPI::SetValue", e)),
        Err(_) => Ok(()), // optional knob not supported by this vendor
    }
}

unsafe fn open_mft(
    activate: &IMFActivate,
    c: &EncoderConfig,
    frame: &WinFrame,
) -> Result<Session, CodecError> {
    unsafe {
        let mft: IMFTransform = activate
            .ActivateObject()
            .map_err(|e| plat("ActivateObject", e))?;
        let attrs = mft.GetAttributes().map_err(|e| plat("GetAttributes", e))?;
        attrs
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .map_err(|e| plat("ASYNC_UNLOCK", e))?;
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

        // Share the capturer's device.
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| plat("MFCreateDXGIDeviceManager", e))?;
        let manager = manager.ok_or(CodecError::Unavailable)?;
        manager
            .ResetDevice(&frame.device.device, token)
            .map_err(|e| plat("ResetDevice", e))?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|e| plat("SET_D3D_MANAGER", e))?;

        let codec_api: ICodecAPI = mft.cast().map_err(|e| plat("ICodecAPI", e))?;
        set_codec_api(
            &codec_api,
            &CODECAPI_AVLowLatencyMode,
            VARIANT::from(true),
            false,
        )?;
        set_codec_api(
            &codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32),
            false,
        )?;
        set_codec_api(
            &codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(c.bitrate_kbps * 1000),
            false,
        )?;
        set_codec_api(
            &codec_api,
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            VARIANT::from(0u32),
            false,
        )?;
        // 0 = no periodic keyframes; IDR only on request (§6 rule 6).
        set_codec_api(
            &codec_api,
            &CODECAPI_AVEncMPVGOPSize,
            VARIANT::from(0u32),
            false,
        )?;

        // Output type must be set before input type for encoders.
        let out = MFCreateMediaType().map_err(|e| plat("MFCreateMediaType", e))?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| plat("out major", e))?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| plat("out subtype", e))?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, c.bitrate_kbps * 1000)
            .map_err(|e| plat("bitrate", e))?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(c.width, c.height))
            .map_err(|e| plat("size", e))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(c.fps, 1))
            .map_err(|e| plat("rate", e))?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| plat("interlace", e))?;
        out.SetUINT32(
            &MF_MT_MPEG2_PROFILE,
            eAVEncH264VProfile_ConstrainedBase.0 as u32,
        )
        .or_else(|_| out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32))
        .map_err(|e| plat("profile", e))?;
        mft.SetOutputType(0, &out, 0)
            .map_err(|e| plat("SetOutputType", e))?;

        let inp = MFCreateMediaType().map_err(|e| plat("MFCreateMediaType", e))?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| plat("in major", e))?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| plat("in subtype", e))?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(c.width, c.height))
            .map_err(|e| plat("size", e))?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, pack(c.fps, 1))
            .map_err(|e| plat("rate", e))?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| plat("interlace", e))?;
        mft.SetInputType(0, &inp, 0)
            .map_err(|e| plat("SetInputType", e))?;

        let info = mft
            .GetOutputStreamInfo(0)
            .map_err(|e| plat("GetOutputStreamInfo", e))?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| plat("BEGIN_STREAMING", e))?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| plat("START_OF_STREAM", e))?;
        let events: IMFMediaEventGenerator = mft.cast().map_err(|e| plat("event generator", e))?;
        Ok(Session {
            mft,
            events,
            codec_api,
            _manager: manager,
            provides_samples,
            out_size: info.cbSize.max(1 << 20),
            need_input: 0,
        })
    }
}

impl Session {
    /// Next MFT event, waiting up to `deadline`. `Ok(None)` on timeout.
    fn next_event(&self, deadline: Instant) -> Result<Option<MF_EVENT_TYPE>, CodecError> {
        loop {
            // SAFETY: non-blocking event poll on our own MFT.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    // SAFETY: plain getter.
                    let t = unsafe { ev.GetType() }.map_err(|e| plat("GetType", e))?;
                    return Ok(Some(MF_EVENT_TYPE(t as i32)));
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(e) => return Err(plat("GetEvent", e)),
            }
        }
    }

    /// Pull one output sample. `Ok(None)` if the MFT had nothing after all.
    fn pull(&mut self, out: &mut Vec<u8>) -> Result<Option<bool>, CodecError> {
        // SAFETY: MFT_OUTPUT_DATA_BUFFER fields are released below.
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample().map_err(|e| plat("MFCreateSample", e))?;
                let b = MFCreateMemoryBuffer(self.out_size)
                    .map_err(|e| plat("MFCreateMemoryBuffer", e))?;
                s.AddBuffer(&b).map_err(|e| plat("AddBuffer", e))?;
                Some(s)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let res = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let [mut b] = buf;
            let sample = ManuallyDrop::take(&mut b.pSample);
            ManuallyDrop::drop(&mut b.pEvents);
            match res {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Renegotiate the output type the MFT now proposes.
                    let t = self
                        .mft
                        .GetOutputAvailableType(0, 0)
                        .map_err(|e| plat("GetOutputAvailableType", e))?;
                    self.mft
                        .SetOutputType(0, &t, 0)
                        .map_err(|e| plat("SetOutputType", e))?;
                    return Ok(None);
                }
                Err(e) => return Err(plat("ProcessOutput", e)),
            }
            let sample =
                sample.ok_or_else(|| CodecError::Platform("MFT returned no sample".into()))?;
            let is_key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let mb = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| plat("ConvertToContiguousBuffer", e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| plat("Lock", e))?;
            // Copy of the compressed bitstream only (tens of KB).
            out.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
            let _ = mb.Unlock();
            Ok(Some(is_key))
        }
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            // SAFETY: orderly shutdown of our own MFT.
            unsafe {
                let _ = s.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = s.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
                if let Ok(shutdown) = s.mft.cast::<IMFShutdown>() {
                    let _ = shutdown.Shutdown();
                }
            }
        }
    }
}

impl Encoder for MediaFoundationEncoder {
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
        let win = native
            .as_any()
            .downcast_ref::<WinFrame>()
            .ok_or(CodecError::BadInput)?;
        if self.session.is_none() {
            self.open(win)?;
        }
        let s = self.session.as_mut().expect("opened");
        let deadline = Instant::now() + OUTPUT_TIMEOUT;

        // Wait until the MFT asks for input.
        while s.need_input == 0 {
            match s.next_event(deadline)? {
                Some(METransformNeedInput) => s.need_input += 1,
                Some(METransformHaveOutput) => {
                    // Stale output from an earlier frame: discard it.
                    self.out.clear();
                    let _ = s.pull(&mut self.out)?;
                }
                Some(_) => {}
                None => return Err(CodecError::Platform("encoder did not request input".into())),
            }
        }

        // SAFETY: wraps the capturer's NV12 texture (same device) in a
        // sample; the MFT holds a reference while it reads it.
        unsafe {
            if force_key {
                set_codec_api(
                    &s.codec_api,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    VARIANT::from(1u32),
                    false,
                )?;
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &win.texture, 0, false)
                .map_err(|e| plat("MFCreateDXGISurfaceBuffer", e))?;
            let sample = MFCreateSample().map_err(|e| plat("MFCreateSample", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| plat("AddBuffer", e))?;
            sample
                .SetSampleTime(capture_ts_us as i64 * 10)
                .map_err(|e| plat("SetSampleTime", e))?;
            sample
                .SetSampleDuration(10_000_000 / i64::from(self.config.fps.max(1)))
                .map_err(|e| plat("SetSampleDuration", e))?;
            s.mft
                .ProcessInput(0, &sample, 0)
                .map_err(|e| plat("ProcessInput", e))?;
        }
        s.need_input -= 1;

        // Wait for this frame's output.
        loop {
            match s.next_event(deadline)? {
                Some(METransformHaveOutput) => {
                    self.out.clear();
                    if let Some(is_key) = s.pull(&mut self.out)? {
                        return Ok(EncodedFrame {
                            data: Bytes::copy_from_slice(&self.out),
                            is_key: is_key || force_key,
                            capture_ts_us,
                        });
                    }
                }
                Some(METransformNeedInput) => s.need_input += 1,
                Some(_) => {}
                None => {
                    return Err(CodecError::Platform(
                        "encoder produced no output in time".into(),
                    ));
                }
            }
        }
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        self.config.bitrate_kbps = kbps;
        if let Some(s) = &self.session {
            // SAFETY: live bitrate change on our own encoder.
            unsafe {
                set_codec_api(
                    &s.codec_api,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(kbps * 1000),
                    false,
                )?;
            }
        }
        Ok(())
    }
}
