//! macOS capture via ScreenCaptureKit (§6 "Capture").
//!
//! - `SCStream` with `minimumFrameInterval` = 1/60 and NV12 (`420v`)
//!   IOSurface-backed output, so VideoToolbox encodes without a copy.
//! - `queueDepth` = 3 (the minimum SCK accepts); the callback forwards
//!   into a newest-wins slot so frames never queue up behind the encoder.
//! - Idle frames (`SCFrameStatus::Idle`) carry no new content and are
//!   dropped: damage-driven capture (§6 rule 4).
//!
//! Requires the Screen Recording permission (TCC).

#![allow(unsafe_code)] // CoreGraphics FFI for pixel sizes

use std::any::Any;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use screencapturekit::cm::{CMSampleBufferExt, CMSampleBufferSCExt, SCFrameStatus};
use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
use screencapturekit::prelude::*;

use crate::*;

// ------------------------------------------------------------ CoreGraphics

#[repr(C)]
struct CgDisplayModeOpaque(u8);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut CgDisplayModeOpaque;
    fn CGDisplayModeGetPixelWidth(mode: *mut CgDisplayModeOpaque) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *mut CgDisplayModeOpaque) -> usize;
    fn CGDisplayModeRelease(mode: *mut CgDisplayModeOpaque);
    fn CGMainDisplayID() -> u32;
    fn CGCursorIsVisible() -> u8;
}

/// Physical pixel size of a display, if CoreGraphics knows it.
fn pixel_size(display_id: u32) -> Option<(u32, u32)> {
    // SAFETY: Copy/Release are balanced; null is checked.
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display_id);
        if mode.is_null() {
            return None;
        }
        let w = CGDisplayModeGetPixelWidth(mode);
        let h = CGDisplayModeGetPixelHeight(mode);
        CGDisplayModeRelease(mode);
        Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
    }
}

fn map_err(e: impl std::fmt::Display) -> CaptureError {
    let s = e.to_string();
    if s.to_lowercase().contains("permission") || s.contains("-3801") || s.contains("declined") {
        CaptureError::PermissionDenied
    } else {
        CaptureError::Platform(s)
    }
}

// ------------------------------------------------------------ frames

/// An IOSurface-backed `CVPixelBuffer` from SCK.
#[derive(Debug)]
pub struct MacFrame {
    pub pixel_buffer: CVPixelBuffer,
}

impl MacFrame {
    /// Raw `CVPixelBufferRef` for VideoToolbox. Valid while `self` lives.
    pub fn cv_pixel_buffer_ptr(&self) -> *mut std::ffi::c_void {
        self.pixel_buffer.as_ptr()
    }
}

impl NativeFrame for MacFrame {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn with_nv12(&self, f: &mut dyn FnMut(Nv12<'_>)) -> Result<(), CaptureError> {
        let pb = &self.pixel_buffer;
        let guard = pb
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|e| CaptureError::Platform(format!("CVPixelBufferLock: {e}")))?;
        if guard.plane_count() != 2 {
            return Err(CaptureError::Platform("expected biplanar NV12".into()));
        }
        let (w, h) = (guard.width(), guard.height());
        let ys = guard.bytes_per_row_of_plane(0);
        let uvs = guard.bytes_per_row_of_plane(1);
        let uvh = guard.height_of_plane(1);
        let (yp, uvp) = match (
            guard.base_address_of_plane(0),
            guard.base_address_of_plane(1),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return Err(CaptureError::Platform("null plane".into())),
        };
        // SAFETY: the planes stay mapped while `guard` lives; sizes come
        // from CoreVideo for the same buffer.
        let (y, uv) = unsafe {
            (
                std::slice::from_raw_parts(yp, ys * h),
                std::slice::from_raw_parts(uvp, uvs * uvh),
            )
        };
        f(Nv12 {
            width: w,
            height: h,
            y,
            y_stride: ys,
            uv,
            uv_stride: uvs,
        });
        Ok(())
    }
}

// ------------------------------------------------------------ capturer

#[derive(Default)]
struct Slot {
    frame: Option<CapturedFrame>,
    error: Option<String>,
}

struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
}

struct Handler {
    shared: Arc<Shared>,
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        if !matches!(kind, SCStreamOutputType::Screen) {
            return;
        }
        let info = sample.frame_info().unwrap_or_default();
        match info.frame_status {
            Some(SCFrameStatus::Complete | SCFrameStatus::Started) | None => {}
            _ => return, // idle / blank / suspended: nothing new to send
        }
        let Some(pb) = sample.pixel_buffer() else {
            return;
        };
        // Size from the buffer itself: it changes when the stream is
        // re-scaled (ABR).
        let (width, height) = (pb.width() as u32, pb.height() as u32);
        let dirty = info
            .dirty_rects
            .unwrap_or_default()
            .into_iter()
            .map(|r| Rect {
                x: r.origin.x.max(0.0) as u32,
                y: r.origin.y.max(0.0) as u32,
                w: r.size.width.max(0.0) as u32,
                h: r.size.height.max(0.0) as u32,
            })
            .collect::<Vec<_>>();
        let frame = CapturedFrame {
            width,
            height,
            format: crate::PixelFormat::Nv12,
            data: FrameData::Native(Arc::new(MacFrame { pixel_buffer: pb })),
            dirty: if dirty.is_empty() {
                vec![Rect {
                    x: 0,
                    y: 0,
                    w: width,
                    h: height,
                }]
            } else {
                dirty
            },
            captured_at: Instant::now(),
        };
        // Newest wins: replace any frame the encoder has not taken yet.
        self.shared.slot.lock().expect("poisoned").frame = Some(frame);
        self.shared.cv.notify_one();
    }
}

struct Delegate(Arc<Shared>);

impl SCStreamDelegateTrait for Delegate {
    fn did_stop_with_error(&self, error: SCError) {
        self.0.slot.lock().expect("poisoned").error = Some(error.to_string());
        self.0.cv.notify_all();
    }
}

/// ScreenCaptureKit capturer.
pub struct SckCapturer {
    stream: Option<SCStream>,
    shared: Arc<Shared>,
    fps: u32,
    /// Selected display bounds in global points, for cursor mapping.
    bounds: (f64, f64, f64, f64),
    cursor_img: Option<Arc<crate::CursorImage>>,
    cursor_polled: Option<Instant>,
    /// Native pixel size of the selected display.
    native: (u32, u32),
}

impl std::fmt::Debug for SckCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SckCapturer")
            .field("running", &self.stream.is_some())
            .finish()
    }
}

impl Default for SckCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl SckCapturer {
    pub fn new() -> Self {
        Self {
            stream: None,
            shared: Arc::new(Shared {
                slot: Mutex::new(Slot::default()),
                cv: Condvar::new(),
            }),
            fps: 60,
            bounds: (0.0, 0.0, 1.0, 1.0),
            cursor_img: None,
            cursor_polled: None,
            native: (0, 0),
        }
    }

    fn find(&self, display_id: u32) -> Result<SCDisplay, CaptureError> {
        SCShareableContent::get()
            .map_err(map_err)?
            .displays()
            .into_iter()
            .find(|d| d.display_id() == display_id)
            .ok_or(CaptureError::NoSuchDisplay(display_id))
    }

    fn stop(&mut self) {
        if let Some(s) = self.stream.take() {
            let _ = s.stop_capture();
        }
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ScreenCapturer for SckCapturer {
    fn set_scale(&mut self, scale: f32) -> Result<(u32, u32), CaptureError> {
        self.rescale(scale)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        self.next_frame_impl(timeout)
    }

    fn cursor(&mut self) -> Option<crate::CursorState> {
        // The image is comparatively costly to read; poll it at ~20 Hz.
        if self
            .cursor_polled
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(50))
        {
            self.cursor_polled = Some(Instant::now());
            if let Some(img) = crate::macos_cursor::current_image()
                && self.cursor_img.as_ref().is_none_or(|c| c.hash != img.hash)
            {
                self.cursor_img = Some(img);
            }
        }
        let (px, py) = crate::macos_cursor::location();
        let (x0, y0, w, h) = self.bounds;
        let (x, y) = (((px - x0) / w) as f32, ((py - y0) / h) as f32);
        let inside = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
        // SAFETY: plain CoreGraphics getter.
        let shown = unsafe { CGCursorIsVisible() } != 0;
        Some(crate::CursorState {
            visible: shown && inside,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            image: self.cursor_img.clone(),
        })
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        // SAFETY: plain CoreGraphics getter.
        let main = unsafe { CGMainDisplayID() };
        let content = SCShareableContent::get().map_err(map_err)?;
        Ok(content
            .displays()
            .into_iter()
            .map(|d| {
                let id = d.display_id();
                let f = d.frame();
                let (pw, ph) = pixel_size(id).unwrap_or((d.width(), d.height()));
                DisplayInfo {
                    id,
                    name: format!("Display {id}"),
                    x: f.origin.x as i32,
                    y: f.origin.y as i32,
                    width: pw,
                    height: ph,
                    scale: pw as f32 / d.width().max(1) as f32,
                    primary: id == main,
                }
            })
            .collect())
    }

    fn select(&mut self, display_id: u32) -> Result<(), CaptureError> {
        self.stop();
        let display = self.find(display_id)?;
        let f = display.frame();
        self.bounds = (
            f.origin.x,
            f.origin.y,
            f.size.width.max(1.0),
            f.size.height.max(1.0),
        );
        let (w, h) = pixel_size(display_id).unwrap_or((display.width(), display.height()));
        // H.264 needs even dimensions.
        let (w, h) = (w & !1, h & !1);

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()
            .map_err(map_err)?;
        self.native = (w, h);
        let config = Self::config(w, h, self.fps)?;
        *self.shared.slot.lock().expect("poisoned") = Slot::default();
        let mut stream =
            SCStream::new_with_delegate(&filter, &config, Delegate(self.shared.clone()))
                .map_err(map_err)?;
        stream
            .add_output_handler(
                Handler {
                    shared: self.shared.clone(),
                },
                SCStreamOutputType::Screen,
            )
            .map_err(map_err)?;
        stream.start_capture().map_err(map_err)?;
        self.stream = Some(stream);
        Ok(())
    }
}

impl SckCapturer {
    fn config(w: u32, h: u32, fps: u32) -> Result<SCStreamConfiguration, CaptureError> {
        SCStreamConfiguration::new()
            .with_width(w)
            .with_height(h)
            .with_pixel_format(screencapturekit::stream::configuration::PixelFormat::YCbCr_420v)
            // The cursor travels on its own channel and the Controller
            // draws it locally (§6 rule 5).
            .with_shows_cursor(false)
            .with_minimum_frame_interval(&CMTime::new(1, fps as i32))
            .with_queue_depth(3)
            // The viewer's shader assumes BT.709 limited range.
            .with_color_matrix(screencapturekit::stream::configuration::color_matrix::ITU_R_709_2)
            .map_err(map_err)
    }
}

impl SckCapturer {
    /// Re-size the running stream without restarting it.
    fn rescale(&mut self, scale: f32) -> Result<(u32, u32), CaptureError> {
        let (w, h) = crate::scaled_size(self.native, scale);
        let stream = self.stream.as_ref().ok_or(CaptureError::NoSuchDisplay(0))?;
        stream
            .update_configuration(&Self::config(w, h, self.fps)?)
            .map_err(map_err)?;
        Ok((w, h))
    }
}

impl SckCapturer {
    fn next_frame_impl(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.stream.is_none() {
            return Err(CaptureError::NoSuchDisplay(0));
        }
        let g = self.shared.slot.lock().expect("poisoned");
        let (mut g, _) = self
            .shared
            .cv
            .wait_timeout_while(g, timeout, |s| s.frame.is_none() && s.error.is_none())
            .expect("poisoned");
        if let Some(e) = g.error.take() {
            return Err(map_err(e));
        }
        Ok(g.frame.take())
    }
}
