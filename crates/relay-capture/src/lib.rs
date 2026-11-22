//! Screen capture abstraction (§6 "Capture", FR-20, FR-21).
//!
//! Real backends keep frames on the GPU: a [`FrameData::Gpu`] frame is an
//! opaque handle to a D3D11 texture or IOSurface that the encoder consumes
//! without a CPU copy (§6 rule 2). [`FrameData::Cpu`] exists for the mock
//! backend and software fallbacks only.

use std::time::{Duration, Instant};

use bytes::Bytes;

#[cfg(target_os = "macos")]
pub mod macos;
pub mod mock;
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

/// Opaque GPU surface handle owned by a platform backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuHandle(pub u64);

#[derive(Debug, Clone)]
pub enum FrameData {
    Gpu(GpuHandle),
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

/// A screen capturer for one display at a time.
pub trait ScreenCapturer: Send {
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
    return macos::SckCapturer::new().map(|c| Box::new(c) as _);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Err(CaptureError::Unsupported)
}
