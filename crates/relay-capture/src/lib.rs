//! Screen capture abstraction (§6 "Capture", FR-20, FR-21).
//!
//! Real backends keep frames on the GPU: a [`FrameData::Native`] frame wraps
//! the platform surface (IOSurface-backed `CVPixelBuffer`, D3D11 texture)
//! that a hardware encoder consumes without a CPU copy (§6 rule 2).
//! [`FrameData::Cpu`] exists for the mock backend only.

use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_cursor;
pub mod mock;
pub mod pattern;
#[cfg(target_os = "windows")]
pub mod windows;

/// A Host display (FR-20).
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Position in the virtual desktop, physical pixels.
    pub x: i32,
    pub y: i32,
    /// Size in physical pixels.
    pub width: u32,
    pub height: u32,
    /// Logical-to-physical scale (e.g. 2.0 on Retina).
    pub scale: f32,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Nv12,
}

/// Borrowed NV12 planes (8-bit Y plane + interleaved CbCr at half size).
#[derive(Debug, Clone, Copy)]
pub struct Nv12<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub y_stride: usize,
    pub uv: &'a [u8],
    pub uv_stride: usize,
}

/// A platform frame surface. Hardware encoders downcast via [`as_any`];
/// software fallbacks read pixels with [`with_nv12`].
///
/// [`as_any`]: NativeFrame::as_any
/// [`with_nv12`]: NativeFrame::with_nv12
pub trait NativeFrame: Send + Sync + Debug {
    fn as_any(&self) -> &dyn Any;
    /// CPU view of the pixels. On GPU-backed surfaces this maps the surface
    /// for reading; only software codecs may call it.
    fn with_nv12(&self, f: &mut dyn FnMut(Nv12<'_>)) -> Result<(), CaptureError>;
}

#[derive(Debug, Clone)]
pub enum FrameData {
    Native(Arc<dyn NativeFrame>),
    /// Tightly packed pixels; `stride` bytes per row.
    Cpu {
        data: Bytes,
        stride: usize,
    },
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: FrameData,
    /// Regions changed since the previous frame. Empty = nothing changed.
    pub dirty: Vec<Rect>,
    pub captured_at: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("display {0} not found")]
    NoSuchDisplay(u32),
    /// Desktop switch, mode change or UAC (DXGI_ERROR_ACCESS_LOST). The
    /// caller re-creates the capturer and continues.
    #[error("capture access lost; re-create the capturer")]
    AccessLost,
    #[error("screen recording permission not granted")]
    PermissionDenied,
    #[error("not supported on this platform/build")]
    Unsupported,
    #[error("platform error: {0}")]
    Platform(String),
}

/// A cursor image, straight-alpha RGBA, in Host pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Vec<u8>,
    /// Content hash; the Controller caches shapes by it.
    pub hash: u64,
    /// Pixels per point of the image (2 for Retina cursors, else 1), in
    /// thousandths, so the Controller can size it to the video.
    pub scale_milli: u32,
}

impl CursorImage {
    pub fn new(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8>, scale: f32) -> Self {
        // FNV-1a: fast, stable, collision-resistant enough for a cache key.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in width
            .to_le_bytes()
            .iter()
            .chain(&height.to_le_bytes())
            .chain(&rgba)
        {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        Self {
            width,
            height,
            hot_x,
            hot_y,
            rgba,
            hash,
            scale_milli: (scale * 1000.0).round() as u32,
        }
    }
}

/// Pointer state on the captured display.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorState {
    pub visible: bool,
    /// Normalised 0..1 within the captured display.
    pub x: f32,
    pub y: f32,
    pub image: Option<std::sync::Arc<CursorImage>>,
}

/// A screen capturer for one display at a time.
pub trait ScreenCapturer: Send {
    /// Current pointer state, if the backend tracks it separately from the
    /// video (§6 rule 5). Polled after each `next_frame`.
    fn cursor(&mut self) -> Option<CursorState> {
        None
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError>;
    /// Start (or switch to) capturing `display_id`. Only that display is
    /// captured (FR-21).
    fn select(&mut self, display_id: u32) -> Result<(), CaptureError>;
    /// Wait up to `timeout` for the next changed frame. `Ok(None)` means
    /// nothing changed (§6 rule 4: damage-driven capture).
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError>;
}

/// The default capturer for this OS.
pub fn platform_capturer() -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    #[cfg(target_os = "windows")]
    return windows::DxgiCapturer::new().map(|c| Box::new(c) as _);
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::SckCapturer::new()));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Err(CaptureError::Unsupported)
}
