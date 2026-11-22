//! macOS capture: ScreenCaptureKit `SCStream` with `minimumFrameInterval`
//! = 1/60 and NV12 IOSurface output (zero-copy into VideoToolbox).
//! Implemented in M1. Requires the Screen Recording TCC permission.

use std::time::Duration;

use crate::*;

#[derive(Debug)]
pub struct SckCapturer;

impl SckCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unsupported)
    }
}

impl ScreenCapturer for SckCapturer {
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
