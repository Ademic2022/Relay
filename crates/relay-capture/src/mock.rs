//! Test-pattern capturer: lets session logic run without a display or GPU.

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::*;

/// Emits a BGRA frame on every call where `changed` is true; otherwise
/// reports no damage.
#[derive(Debug)]
pub struct MockCapturer {
    displays: Vec<DisplayInfo>,
    selected: Option<u32>,
    frame_no: u64,
    /// Set false to simulate a static screen.
    pub changed: bool,
}

impl MockCapturer {
    pub fn new(displays: Vec<DisplayInfo>) -> Self {
        Self {
            displays,
            selected: None,
            frame_no: 0,
            changed: true,
        }
    }

    pub fn single_1080p() -> Self {
        Self::new(vec![DisplayInfo {
            id: 1,
            name: "Mock 1080p".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }])
    }
}

impl ScreenCapturer for MockCapturer {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(self.displays.clone())
    }

    fn select(&mut self, display_id: u32) -> Result<(), CaptureError> {
        if !self.displays.iter().any(|d| d.id == display_id) {
            return Err(CaptureError::NoSuchDisplay(display_id));
        }
        self.selected = Some(display_id);
        Ok(())
    }

    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let id = self.selected.ok_or(CaptureError::NoSuchDisplay(0))?;
        if !self.changed {
            return Ok(None);
        }
        let d = self
            .displays
            .iter()
            .find(|d| d.id == id)
            .ok_or(CaptureError::NoSuchDisplay(id))?;
        self.frame_no += 1;
        // Small fixed-size payload: the mock encoder ignores pixel content.
        let marker = Bytes::from(self.frame_no.to_le_bytes().to_vec());
        Ok(Some(CapturedFrame {
            width: d.width,
            height: d.height,
            format: PixelFormat::Bgra8,
            data: FrameData::Cpu {
                data: marker,
                stride: 0,
            },
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: d.width,
                h: d.height,
            }],
            captured_at: Instant::now(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_screen_yields_nothing() {
        let mut c = MockCapturer::single_1080p();
        c.select(1).unwrap();
        assert!(c.next_frame(Duration::ZERO).unwrap().is_some());
        c.changed = false;
        assert!(c.next_frame(Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn unknown_display() {
        let mut c = MockCapturer::single_1080p();
        assert!(matches!(c.select(9), Err(CaptureError::NoSuchDisplay(9))));
    }
}
