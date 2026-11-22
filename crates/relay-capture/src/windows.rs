//! Windows capture: DXGI Desktop Duplication (primary), Windows.Graphics.Capture
//! (fallback). Implemented in M1.
//!
//! Notes (§10): re-create duplication on `DXGI_ERROR_ACCESS_LOST` and
//! re-attach to `OpenInputDesktop()`; BGRA→NV12 on the GPU via the D3D11
//! video processor, never a CPU readback.

use std::time::Duration;

use crate::*;

#[derive(Debug)]
pub struct DxgiCapturer;

impl DxgiCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unsupported)
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Err(CaptureError::Unsupported)
    }
    fn select(&mut self, _display_id: u32) -> Result<(), CaptureError> {
        Err(CaptureError::Unsupported)
    }
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        Err(CaptureError::Unsupported)
    }
}
