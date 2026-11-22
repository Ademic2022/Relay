//! Windows capture: DXGI Desktop Duplication (§6 "Capture", §10).
//!
//! - One D3D11 device per capturer, created on the adapter that owns the
//!   output, multithread-protected so the Media Foundation encoder can use
//!   the same device from its own threads.
//! - Each acquired desktop texture is converted BGRA → NV12 on the GPU by
//!   the D3D11 video processor (BT.709, limited range) straight into an
//!   encoder-ready texture, then released to DWM. No CPU readback
//!   (§6 rules 2 and "Colour convert").
//! - Frames with no new desktop image (pointer-only updates) are skipped:
//!   damage-driven capture (§6 rule 4).
//! - `DXGI_ERROR_ACCESS_LOST` (mode change, UAC, desktop switch): the
//!   thread re-attaches to the input desktop and duplication is rebuilt.
//!
//! The desktop image from DXGI does not include the mouse pointer; it is
//! sent on the cursor channel and drawn by the Controller (§6 rule 5, M2).
//!
//! Not yet handled: rotated outputs, HDR (FP16) desktops.

#![allow(unsafe_code)]

use std::any::Any;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{E_FAIL, HMODULE, RECT};
use ::windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use ::windows::Win32::Graphics::Direct3D11::*;
use ::windows::Win32::Graphics::Dxgi::Common::*;
use ::windows::Win32::Graphics::Dxgi::*;
use ::windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};

/// `MONITORINFOF_PRIMARY` (winuser.h).
const MONITORINFOF_PRIMARY: u32 = 1;
use ::windows::Win32::System::StationsAndDesktops::{
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
};
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use ::windows::core::Interface;

use crate::*;

/// NV12 textures in flight between capture and encoder.
const RING: usize = 4;

fn plat(e: ::windows::core::Error) -> CaptureError {
    if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL {
        CaptureError::AccessLost
    } else {
        CaptureError::Platform(e.to_string())
    }
}

// ------------------------------------------------------------ device

/// The D3D11 device and immediate context shared by capture and encode.
///
/// D3D11 devices are free-threaded; the immediate context is not, so it
/// is only used under `ctx` and the device is multithread-protected for
/// Media Foundation's internal threads.
pub struct D3dDevice {
    pub device: ID3D11Device,
    ctx: Mutex<ID3D11DeviceContext>,
}

// SAFETY: see type docs: device is free-threaded, context is behind a mutex.
unsafe impl Send for D3dDevice {}
unsafe impl Sync for D3dDevice {}

impl std::fmt::Debug for D3dDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("D3dDevice")
    }
}

impl D3dDevice {
    fn new(adapter: &IDXGIAdapter1) -> Result<Self, CaptureError> {
        let mut device = None;
        let mut ctx = None;
        // SAFETY: out-pointers are valid for the call.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(plat)?;
        }
        let device: ID3D11Device = device.ok_or(CaptureError::Platform("no device".into()))?;
        let ctx: ID3D11DeviceContext = ctx.ok_or(CaptureError::Platform("no context".into()))?;
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            // SAFETY: plain setter.
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok(Self {
            device,
            ctx: Mutex::new(ctx),
        })
    }

    /// Run `f` with the immediate context.
    pub fn with_context<R>(&self, f: impl FnOnce(&ID3D11DeviceContext) -> R) -> R {
        f(&self.ctx.lock().expect("poisoned"))
    }
}

// ------------------------------------------------------------ frames

/// An NV12 D3D11 texture produced by the capturer.
pub struct WinFrame {
    pub texture: ID3D11Texture2D,
    pub device: Arc<D3dDevice>,
    width: u32,
    height: u32,
}

// SAFETY: D3D11 resources are free-threaded; context use goes through
// `D3dDevice::with_context`.
unsafe impl Send for WinFrame {}
unsafe impl Sync for WinFrame {}

impl std::fmt::Debug for WinFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WinFrame({}x{})", self.width, self.height)
    }
}

impl NativeFrame for WinFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// GPU → CPU readback through a staging texture. Software encoder only.
    fn with_nv12(&self, f: &mut dyn FnMut(Nv12<'_>)) -> Result<(), CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: valid descriptor and out-pointer.
        unsafe {
            self.device
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(plat)?;
        }
        let staging = staging.ok_or(CaptureError::Platform("no staging texture".into()))?;
        self.device.with_context(|ctx| {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: same-format, same-size copy; mapped for read below.
            unsafe {
                ctx.CopyResource(&staging, &self.texture);
                ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(plat)?;
            }
            let pitch = mapped.RowPitch as usize;
            let (w, h) = (self.width as usize, self.height as usize);
            // SAFETY: an NV12 staging map is `pitch * h` luma bytes followed
            // by `pitch * h / 2` interleaved chroma bytes.
            let (y, uv) = unsafe {
                let base = mapped.pData as *const u8;
                (
                    std::slice::from_raw_parts(base, pitch * h),
                    std::slice::from_raw_parts(base.add(pitch * h), pitch * h / 2),
                )
            };
            f(Nv12 {
                width: w,
                height: h,
                y,
                y_stride: pitch,
                uv,
                uv_stride: pitch,
            });
            // SAFETY: matches the Map above.
            unsafe { ctx.Unmap(&staging, 0) };
            Ok(())
        })
    }
}

// ------------------------------------------------------------ outputs

struct Output {
    id: u32,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    desc: DXGI_OUTPUT_DESC,
}

fn outputs() -> Result<Vec<Output>, CaptureError> {
    // SAFETY: COM factory creation and enumeration.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(plat)?;
        let mut out = Vec::new();
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let desc = output.GetDesc().map_err(plat)?;
                if desc.AttachedToDesktop.as_bool()
                    && let Ok(o1) = output.cast::<IDXGIOutput1>()
                {
                    out.push(Output {
                        id: (ai << 8 | oi) + 1,
                        adapter: adapter.clone(),
                        output: o1,
                        desc,
                    });
                }
                oi += 1;
            }
            ai += 1;
        }
        Ok(out)
    }
}

fn display_info(o: &Output) -> DisplayInfo {
    let r = o.desc.DesktopCoordinates;
    let name_len = o.desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
    let name = String::from_utf16_lossy(&o.desc.DeviceName[..name_len]);
    let mut dpi = (96u32, 96u32);
    let mut primary = r.left == 0 && r.top == 0;
    // SAFETY: plain Win32 queries on a valid HMONITOR.
    unsafe {
        let _ = GetDpiForMonitor(o.desc.Monitor, MDT_EFFECTIVE_DPI, &mut dpi.0, &mut dpi.1);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(o.desc.Monitor, &mut mi).as_bool() {
            primary = mi.dwFlags & MONITORINFOF_PRIMARY != 0;
        }
    }
    DisplayInfo {
        id: o.id,
        name,
        x: r.left,
        y: r.top,
        width: (r.right - r.left) as u32,
        height: (r.bottom - r.top) as u32,
        scale: dpi.0 as f32 / 96.0,
        primary,
    }
}

// ------------------------------------------------------------ converter

/// BGRA → NV12 on the GPU via the D3D11 video processor.
struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    /// Converts `in_w`×`in_h` BGRA into `w`×`h` NV12, scaling on the way.
    fn new(dev: &D3dDevice, in_w: u32, in_h: u32, w: u32, h: u32) -> Result<Self, CaptureError> {
        let vdev: ID3D11VideoDevice = dev.device.cast().map_err(plat)?;
        let vctx: ID3D11VideoContext = dev.with_context(|c| c.cast()).map_err(plat)?;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: w,
            OutputHeight: h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: COM calls with valid descriptors.
        unsafe {
            let enumerator = vdev
                .CreateVideoProcessorEnumerator(&content)
                .map_err(plat)?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).map_err(plat)?;

            // Input: full-range RGB. Output: BT.709 limited-range YCbCr.
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4), // YCbCr_Matrix=709, Nominal_Range=16..235
            };
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &input_cs);
            vctx.VideoProcessorSetOutputColorSpace(&processor, &output_cs);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex = None;
                dev.device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .map_err(plat)?;
                let tex = tex.ok_or(CaptureError::Platform("no NV12 texture".into()))?;
                let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&tex, &enumerator, &ov_desc, Some(&mut ov))
                    .map_err(plat)?;
                ring.push((
                    tex,
                    ov.ok_or(CaptureError::Platform("no output view".into()))?,
                ));
            }
            Ok(Self {
                vdev,
                vctx,
                enumerator,
                processor,
                ring,
                next: 0,
            })
        }
    }

    /// Convert `src` (BGRA desktop texture) into the next ring texture.
    fn convert(
        &mut self,
        dev: &D3dDevice,
        src: &ID3D11Texture2D,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let (tex, ov) = &self.ring[self.next];
        self.next = (self.next + 1) % self.ring.len();
        let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        // SAFETY: COM calls on resources owned by this device; the context
        // is held for the Blt.
        unsafe {
            let mut iv = None;
            self.vdev
                .CreateVideoProcessorInputView(src, &self.enumerator, &iv_desc, Some(&mut iv))
                .map_err(plat)?;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(iv),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            let streams = [stream];
            let res = dev.with_context(|_| {
                self.vctx
                    .VideoProcessorBlt(&self.processor, ov, 0, &streams)
            });
            // Release the input view we moved into the struct.
            let [mut s] = streams;
            ManuallyDrop::drop(&mut s.pInputSurface);
            res.map_err(plat)?;
        }
        Ok(tex.clone())
    }
}

// ------------------------------------------------------------ capturer

struct Session {
    device: Arc<D3dDevice>,
    dup: IDXGIOutputDuplication,
    conv: Converter,
    /// Desktop size (input) and encoded size (output, after ABR scaling).
    in_w: u32,
    in_h: u32,
    width: u32,
    height: u32,
    holding: bool,
}

pub struct DxgiCapturer {
    selected: Option<u32>,
    session: Option<Session>,
    dirty_buf: Vec<RECT>,
    cursor: Option<CursorState>,
    shape_buf: Vec<u8>,
    scale: f32,
}

// SAFETY: used from one thread at a time (&mut self); COM objects here are
// free-threaded D3D/DXGI objects.
unsafe impl Send for DxgiCapturer {}

impl std::fmt::Debug for DxgiCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapturer")
            .field("selected", &self.selected)
            .finish()
    }
}

impl DxgiCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        // Physical-pixel coordinates for DPI-scaled monitors. Fails
        // harmlessly if the process already set an awareness mode.
        // SAFETY: process-wide setter.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Ok(Self {
            selected: None,
            session: None,
            dirty_buf: Vec::new(),
            cursor: None,
            shape_buf: Vec::new(),
            scale: 1.0,
        })
    }

    fn open(&mut self, id: u32) -> Result<(), CaptureError> {
        self.session = None;
        let o = outputs()?
            .into_iter()
            .find(|o| o.id == id)
            .ok_or(CaptureError::NoSuchDisplay(id))?;
        let info = display_info(&o);
        let (w, h) = (info.width & !1, info.height & !1);
        let device = Arc::new(D3dDevice::new(&o.adapter)?);
        // SAFETY: COM call with a device on the output's adapter.
        let dup = unsafe { o.output.DuplicateOutput(&device.device) }.map_err(|e| {
            if e.code() == E_FAIL || e.code() == DXGI_ERROR_UNSUPPORTED {
                CaptureError::Platform(format!("DuplicateOutput: {e} (another app may hold duplication, or the GPU is unsupported)"))
            } else {
                plat(e)
            }
        })?;
        let (ow, oh) = crate::scaled_size((w, h), self.scale);
        let conv = Converter::new(&device, w, h, ow, oh)?;
        self.session = Some(Session {
            device,
            dup,
            conv,
            in_w: w,
            in_h: h,
            width: ow,
            height: oh,
            holding: false,
        });
        Ok(())
    }

    /// After ACCESS_LOST: follow the input desktop (Winlogon/UAC when the
    /// process runs as SYSTEM, §10) and rebuild duplication.
    fn reattach(&mut self) -> Result<(), CaptureError> {
        // SAFETY: desktop handles are only attached to this thread.
        unsafe {
            if let Ok(desk) = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(0x10000000),
            ) {
                let _ = SetThreadDesktop(desk);
            }
        }
        let id = self.selected.ok_or(CaptureError::NoSuchDisplay(0))?;
        self.open(id)
    }
}

/// DXGI pointer shape → straight-alpha RGBA.
fn pointer_to_rgba(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorImage> {
    let (w, pitch) = (si.Width as usize, si.Pitch as usize);
    let ty = si.Type as i32;
    let h = if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        si.Height as usize / 2
    } else {
        si.Height as usize
    };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        // AND mask then XOR mask, 1 bpp each.
        for y in 0..h {
            for x in 0..w {
                let bit = 0x80 >> (x % 8);
                let and = buf.get(y * pitch + x / 8)? & bit != 0;
                let xor = buf.get((y + h) * pitch + x / 8)? & bit != 0;
                let px = &mut rgba[(y * w + x) * 4..][..4];
                match (and, xor) {
                    (true, false) => {}                                    // transparent
                    (false, false) => px.copy_from_slice(&[0, 0, 0, 255]), // black
                    (false, true) => px.copy_from_slice(&[255, 255, 255, 255]),
                    (true, true) => px.copy_from_slice(&[0, 0, 0, 255]), // invert ≈ black
                }
            }
        }
    } else {
        let masked = ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
        for y in 0..h {
            for x in 0..w {
                let src = buf.get(y * pitch + x * 4..y * pitch + x * 4 + 4)?;
                let (b, g, r, a) = (src[0], src[1], src[2], src[3]);
                let px = &mut rgba[(y * w + x) * 4..][..4];
                let alpha = if masked {
                    // Mask 0 = opaque colour; 0xFF = XOR with screen (drawn
                    // as the colour unless it is black, i.e. no-op).
                    if a == 0 || (r | g | b) != 0 { 255 } else { 0 }
                } else {
                    a
                };
                px.copy_from_slice(&[r, g, b, alpha]);
            }
        }
    }
    Some(CursorImage::new(
        w as u32,
        h as u32,
        si.HotSpot.x.max(0) as u32,
        si.HotSpot.y.max(0) as u32,
        rgba,
        1.0,
    ))
}

impl ScreenCapturer for DxgiCapturer {
    fn cursor(&mut self) -> Option<CursorState> {
        self.cursor.clone()
    }

    fn set_scale(&mut self, scale: f32) -> Result<(u32, u32), CaptureError> {
        self.scale = scale.clamp(0.1, 1.0);
        let s = self
            .session
            .as_mut()
            .ok_or(CaptureError::NoSuchDisplay(0))?;
        let (w, h) = crate::scaled_size((s.in_w, s.in_h), self.scale);
        s.conv = Converter::new(&s.device, s.in_w, s.in_h, w, h)?;
        s.width = w;
        s.height = h;
        Ok((w, h))
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(outputs()?.iter().map(display_info).collect())
    }

    fn select(&mut self, display_id: u32) -> Result<(), CaptureError> {
        self.selected = Some(display_id);
        self.open(display_id)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.session.is_none() {
            self.reattach()?;
        }
        let s = self.session.as_mut().expect("opened");
        // SAFETY: duplication calls on our own session; the acquired frame
        // is released before returning.
        unsafe {
            if s.holding {
                let _ = s.dup.ReleaseFrame();
                s.holding = false;
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
            match s.dup.AcquireNextFrame(ms, &mut info, &mut resource) {
                Ok(()) => s.holding = true,
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.session = None;
                    self.reattach()?;
                    return Ok(None);
                }
                Err(e) => return Err(plat(e)),
            }
            let captured_at = Instant::now();
            // Pointer state rides along with every acquired frame.
            if info.LastMouseUpdateTime != 0 {
                let p = info.PointerPosition;
                let mut c = self.cursor.clone().unwrap_or(CursorState {
                    visible: false,
                    x: 0.0,
                    y: 0.0,
                    image: None,
                });
                c.visible = p.Visible.as_bool();
                c.x = p.Position.x as f32 / s.in_w.max(1) as f32;
                c.y = p.Position.y as f32 / s.in_h.max(1) as f32;
                if info.PointerShapeBufferSize > 0 {
                    self.shape_buf
                        .resize(info.PointerShapeBufferSize as usize, 0);
                    let mut used = 0u32;
                    let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                    if s.dup
                        .GetFramePointerShape(
                            self.shape_buf.len() as u32,
                            self.shape_buf.as_mut_ptr().cast(),
                            &mut used,
                            &mut si,
                        )
                        .is_ok()
                        && let Some(img) = pointer_to_rgba(&self.shape_buf[..used as usize], &si)
                    {
                        c.image = Some(Arc::new(img));
                    }
                }
                self.cursor = Some(c);
            }
            // Pointer-only update: no new desktop image.
            if info.LastPresentTime == 0 || info.AccumulatedFrames == 0 {
                return Ok(None);
            }
            let resource = resource.ok_or(CaptureError::Platform("no frame resource".into()))?;
            let src: ID3D11Texture2D = resource.cast().map_err(plat)?;

            // Dirty rects (for the stats overlay and future ROI encoding).
            let mut dirty = Vec::new();
            if info.TotalMetadataBufferSize > 0 {
                let cap = info.TotalMetadataBufferSize as usize / std::mem::size_of::<RECT>() + 1;
                self.dirty_buf.resize(cap, RECT::default());
                let mut used = 0u32;
                if s.dup
                    .GetFrameDirtyRects(
                        (self.dirty_buf.len() * std::mem::size_of::<RECT>()) as u32,
                        self.dirty_buf.as_mut_ptr(),
                        &mut used,
                    )
                    .is_ok()
                {
                    let n = used as usize / std::mem::size_of::<RECT>();
                    dirty.extend(self.dirty_buf[..n].iter().map(|r| Rect {
                        x: r.left.max(0) as u32,
                        y: r.top.max(0) as u32,
                        w: (r.right - r.left).max(0) as u32,
                        h: (r.bottom - r.top).max(0) as u32,
                    }));
                }
            }
            if dirty.is_empty() {
                dirty.push(Rect {
                    x: 0,
                    y: 0,
                    w: s.width,
                    h: s.height,
                });
            }

            let nv12 = s.conv.convert(&s.device, &src)?;
            // Hand the desktop texture back to DWM as early as possible.
            let _ = s.dup.ReleaseFrame();
            s.holding = false;

            Ok(Some(CapturedFrame {
                width: s.width,
                height: s.height,
                format: PixelFormat::Nv12,
                data: FrameData::Native(Arc::new(WinFrame {
                    texture: nv12,
                    device: s.device.clone(),
                    width: s.width,
                    height: s.height,
                })),
                dirty,
                captured_at,
            }))
        }
    }
}
