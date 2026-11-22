//! Synthetic test-pattern source (no permissions, no display needed).
//!
//! Draws a moving bar plus a 32-block binary stripe encoding the frame's
//! capture time as wall-clock Unix milliseconds (low 32 bits), so the
//! latency bench (§11) can recover it from presented frames. Across two
//! machines the clocks must be NTP-synced; on one machine they are shared. On macOS frames are IOSurface-backed
//! `CVPixelBuffer`s, so they exercise the same hardware-encode path as
//! real capture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::*;

pub const STRIPE_BLOCKS: usize = 32;

#[derive(Debug)]
pub struct PatternCapturer {
    width: u32,
    height: u32,
    fps: u32,
    next: Instant,
    n: u64,
}

/// Wall-clock Unix time in ms, low 32 bits (wraps every ~49 days).
pub fn unix_ms32() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u32)
}

impl PatternCapturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width: width & !1,
            height: height & !1,
            fps: fps.max(1),
            next: Instant::now(),
            n: 0,
        }
    }
}

/// Fill NV12 planes with the pattern for frame `n`, stamping `ms`.
#[allow(clippy::too_many_arguments)] // plain plane/stride pairs, hot path
pub fn draw(
    y: &mut [u8],
    ys: usize,
    uv: &mut [u8],
    uvs: usize,
    w: usize,
    h: usize,
    n: u64,
    ms: u32,
) {
    let bar = (n as usize * 8) % w.max(1);
    for row in 0..h {
        let line = &mut y[row * ys..row * ys + w];
        for (col, px) in line.iter_mut().enumerate() {
            *px = if col.abs_diff(bar) < 24 {
                235
            } else {
                (16 + ((col / 32 + row / 32) % 2) * 40) as u8
            };
        }
    }
    for row in 0..h / 2 {
        uv[row * uvs..row * uvs + w].fill(128);
    }
    // Timestamp stripe: 32 blocks, MSB first, white = 1, Gray-coded so a
    // block the encoder has not refreshed yet reads as the previous value
    // (off by 1 ms) rather than flipping a high bit.
    let ms = ms ^ (ms >> 1);
    let bw = w / STRIPE_BLOCKS;
    let bh = (h / 20).max(8);
    for bit in 0..STRIPE_BLOCKS {
        let v = if ms >> (31 - bit) & 1 == 1 { 235 } else { 16 };
        for row in 0..bh {
            y[row * ys + bit * bw..row * ys + (bit + 1) * bw].fill(v);
        }
    }
}

/// Read the stamp back from a luma plane.
pub fn read_stamp(y: &[u8], ys: usize, w: usize, h: usize) -> u32 {
    let bw = w / STRIPE_BLOCKS;
    let row = (h / 20).max(8) / 2;
    let mut g = (0..STRIPE_BLOCKS).fold(0u32, |acc, bit| {
        let px = y[row * ys + bit * bw + bw / 2];
        (acc << 1) | u32::from(px > 128)
    });
    // Gray → binary.
    let mut shift = 1;
    while shift < 32 {
        g ^= g >> shift;
        shift <<= 1;
    }
    g
}

#[derive(Debug)]
struct MemFrame {
    w: usize,
    h: usize,
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl NativeFrame for MemFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn with_nv12(&self, f: &mut dyn FnMut(Nv12<'_>)) -> Result<(), CaptureError> {
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

#[cfg(target_os = "macos")]
fn make_frame(w: usize, h: usize, n: u64, ms: u32) -> Result<Arc<dyn NativeFrame>, CaptureError> {
    #![allow(unsafe_code)]
    use screencapturekit::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
    const FMT_420V: u32 = u32::from_be_bytes(*b"420v");
    let pb = CVPixelBuffer::create(w, h, FMT_420V)
        .map_err(|e| CaptureError::Platform(format!("CVPixelBufferCreate: {e}")))?;
    {
        let mut g = pb
            .lock(CVPixelBufferLockFlags::from_bits(0))
            .map_err(|e| CaptureError::Platform(format!("lock: {e}")))?;
        let (ys, uvs) = (g.bytes_per_row_of_plane(0), g.bytes_per_row_of_plane(1));
        let (yp, uvp) = match (
            g.base_address_of_plane_mut(0),
            g.base_address_of_plane_mut(1),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return Err(CaptureError::Platform("null plane".into())),
        };
        // SAFETY: planes are locked for writing; sizes from CoreVideo.
        let (y, uv) = unsafe {
            (
                std::slice::from_raw_parts_mut(yp, ys * h),
                std::slice::from_raw_parts_mut(uvp, uvs * h / 2),
            )
        };
        draw(y, ys, uv, uvs, w, h, n, ms);
    }
    Ok(Arc::new(crate::macos::MacFrame { pixel_buffer: pb }))
}

#[cfg(not(target_os = "macos"))]
fn make_frame(w: usize, h: usize, n: u64, ms: u32) -> Result<Arc<dyn NativeFrame>, CaptureError> {
    let mut y = vec![0u8; w * h];
    let mut uv = vec![0u8; w * h / 2];
    draw(&mut y, w, &mut uv, w, w, h, n, ms);
    Ok(Arc::new(MemFrame { w, h, y, uv }))
}

/// In-memory frame for tests on any OS.
pub fn mem_frame(w: usize, h: usize, n: u64, ms: u32) -> Arc<dyn NativeFrame> {
    let mut y = vec![0u8; w * h];
    let mut uv = vec![0u8; w * h / 2];
    draw(&mut y, w, &mut uv, w, w, h, n, ms);
    Arc::new(MemFrame { w, h, y, uv })
}

/// A 12x19 arrow, so the cursor channel can be exercised without a screen.
fn arrow() -> CursorImage {
    const ROWS: [&str; 19] = [
        "X...........",
        "XX..........",
        "X.X.........",
        "X..X........",
        "X...X.......",
        "X....X......",
        "X.....X.....",
        "X......X....",
        "X.......X...",
        "X........X..",
        "X.........X.",
        "X......XXXXX",
        "X...X..X....",
        "X..XX..X....",
        "X.X..X..X...",
        "XX...X..X...",
        "X.....X..X..",
        "......X..X..",
        ".......XX...",
    ];
    let mut rgba = Vec::with_capacity(12 * 19 * 4);
    for row in ROWS {
        for c in row.bytes() {
            rgba.extend_from_slice(if c == b'X' {
                &[0, 0, 0, 255]
            } else {
                &[255, 255, 255, 0]
            });
        }
    }
    CursorImage::new(12, 19, 0, 0, rgba, 1.0)
}

impl ScreenCapturer for PatternCapturer {
    fn cursor(&mut self) -> Option<CursorState> {
        let t = self.n as f32 / self.fps as f32;
        Some(CursorState {
            visible: true,
            x: 0.5 + 0.3 * t.cos(),
            y: 0.5 + 0.3 * t.sin(),
            image: Some(Arc::new(arrow())),
        })
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(vec![DisplayInfo {
            id: 1,
            name: "Test pattern".into(),
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
            scale: 1.0,
            primary: true,
        }])
    }

    fn select(&mut self, display_id: u32) -> Result<(), CaptureError> {
        if display_id != 1 {
            return Err(CaptureError::NoSuchDisplay(display_id));
        }
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next += Duration::from_secs(1) / self.fps;
        if self.next + Duration::from_millis(100) < Instant::now() {
            self.next = Instant::now(); // fell behind; don't burst
        }
        let captured_at = Instant::now();
        let ms = unix_ms32();
        let (w, h) = (self.width as usize, self.height as usize);
        let data = make_frame(w, h, self.n, ms)?;
        self.n += 1;
        Ok(Some(CapturedFrame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Nv12,
            data: FrameData::Native(data),
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: self.width,
                h: self.height,
            }],
            captured_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_round_trip() {
        let (w, h) = (640, 360);
        let mut y = vec![0u8; w * h];
        let mut uv = vec![0u8; w * h / 2];
        for ms in [0u32, 1, 2, 3, 255, 256, 123_456, u32::MAX - 1, u32::MAX] {
            draw(&mut y, w, &mut uv, w, w, h, 7, ms);
            assert_eq!(read_stamp(&y, w, w, h), ms);
        }
    }

    #[test]
    fn paces_frames() {
        let mut c = PatternCapturer::new(64, 64, 100);
        c.select(1).unwrap();
        let t = Instant::now();
        let mut n = 0;
        while n < 10 {
            if c.next_frame(Duration::from_millis(50)).unwrap().is_some() {
                n += 1;
            }
        }
        assert!(t.elapsed() >= Duration::from_millis(85));
    }
}
