//! "All monitors" view (FR-22).
//!
//! [`Multi`] wraps the platform capturer. With two or more displays it
//! offers one more, [`ALL_DISPLAYS`], which shows every display in one
//! stream, laid out as they are arranged on the Host:
//!
//! - one child capturer per display, each asked to capture at the size of
//!   its slot (the OS downscales on the GPU where it can);
//! - their frames are composited into a single NV12 canvas, at most
//!   [`MAX_W`]×[`MAX_H`] and further scaled by the ABR (§6 rule 7);
//! - the canvas maps linearly onto the bounding box of the virtual desktop
//!   ([`desktop_bounds`]), so pointer input needs no per-display logic.
//!
//! This path copies pixels on the CPU (the single-display path does not):
//! it is a convenience overview, not the low-latency default.

use std::time::{Duration, Instant};

use crate::*;

/// Display ID of the composite view.
pub const ALL_DISPLAYS: u32 = u32::MAX;
pub const MAX_W: f64 = 3840.0;
pub const MAX_H: f64 = 2160.0;

/// Makes one more capturer of the same kind (one per display).
pub type Factory = Box<dyn Fn() -> Result<Box<dyn ScreenCapturer>, CaptureError> + Send>;

/// Bounding box of all displays in desktop units (see [`desktop_size`]).
pub fn desktop_bounds(displays: &[DisplayInfo]) -> (f64, f64, f64, f64) {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for d in displays {
        let (w, h) = desktop_size(d);
        x0 = x0.min(f64::from(d.x));
        y0 = y0.min(f64::from(d.y));
        x1 = x1.max(f64::from(d.x) + w);
        y1 = y1.max(f64::from(d.y) + h);
    }
    if displays.is_empty() {
        return (0.0, 0.0, 1.0, 1.0);
    }
    (x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0))
}

/// A size: even and at least 2.
fn even(v: f64) -> u32 {
    even_pos(v).max(2)
}

/// A position: even, may be 0.
fn even_pos(v: f64) -> u32 {
    (v.max(0.0).round() as u32) & !1
}

/// Where each display goes on the canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Per display (same order as given): canvas rect.
    pub slots: Vec<Rect>,
}

/// Lay `displays` out on a canvas: full pixel density of the sharpest
/// display, shrunk to fit [`MAX_W`]×[`MAX_H`], then × `scale`.
pub fn layout(displays: &[DisplayInfo], scale: f32) -> Layout {
    let (bx, by, bw, bh) = desktop_bounds(displays);
    // Pixels per desktop unit of the densest display (2 on Retina).
    let density = displays
        .iter()
        .map(|d| f64::from(d.width) / desktop_size(d).0.max(1.0))
        .fold(1.0, f64::max);
    let fit = (MAX_W / (bw * density))
        .min(MAX_H / (bh * density))
        .min(1.0);
    let s = density * fit * f64::from(scale.clamp(0.1, 1.0));
    let (width, height) = (even(bw * s), even(bh * s));
    let slots = displays
        .iter()
        .map(|d| {
            let (w, h) = desktop_size(d);
            let x = even_pos((f64::from(d.x) - bx) * s).min(width.saturating_sub(2));
            let y = even_pos((f64::from(d.y) - by) * s).min(height.saturating_sub(2));
            Rect {
                x,
                y,
                w: even(w * s).min(width - x),
                h: even(h * s).min(height - y),
            }
        })
        .collect();
    Layout {
        width,
        height,
        slots,
    }
}

struct Child {
    display: DisplayInfo,
    cap: Box<dyn ScreenCapturer>,
}

/// Platform capturer plus the composite "all displays" view.
pub struct Multi {
    inner: Box<dyn ScreenCapturer>,
    factory: Factory,
    /// Set while the composite view is selected.
    children: Vec<Child>,
    layout: Layout,
    scale: f32,
    /// The canvas, kept between frames: displays that did not change keep
    /// their last picture.
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl std::fmt::Debug for Multi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Multi")
            .field("all", &!self.children.is_empty())
            .field("layout", &self.layout)
            .finish()
    }
}

impl Multi {
    pub fn new(inner: Box<dyn ScreenCapturer>, factory: Factory) -> Self {
        Self {
            inner,
            factory,
            children: Vec::new(),
            layout: Layout {
                width: 2,
                height: 2,
                slots: Vec::new(),
            },
            scale: 1.0,
            y: Vec::new(),
            uv: Vec::new(),
        }
    }

    fn real_displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        self.inner.displays()
    }

    fn all_info(displays: &[DisplayInfo]) -> DisplayInfo {
        let (bx, by, _, _) = desktop_bounds(displays);
        let l = layout(displays, 1.0);
        DisplayInfo {
            id: ALL_DISPLAYS,
            name: "All displays".into(),
            x: bx as i32,
            y: by as i32,
            width: l.width,
            height: l.height,
            scale: 1.0,
            primary: false,
        }
    }

    /// Apply the layout: canvas cleared to black, each child asked for
    /// the size of its slot.
    fn relayout(&mut self) {
        let ds: Vec<DisplayInfo> = self.children.iter().map(|c| c.display.clone()).collect();
        self.layout = layout(&ds, self.scale);
        let (w, h) = (self.layout.width as usize, self.layout.height as usize);
        self.y = vec![16; w * h];
        self.uv = vec![128; w * h / 2];
        for (c, slot) in self.children.iter_mut().zip(&self.layout.slots) {
            let s = slot.w as f32 / c.display.width.max(1) as f32;
            // Backends that cannot scale are resampled when composited.
            let _ = c.cap.set_scale(s.clamp(0.1, 1.0));
        }
    }

    /// Draw one child frame into its slot (nearest-neighbour if the sizes
    /// differ, row copies if they match).
    fn blit(&mut self, i: usize, f: &CapturedFrame) -> Result<(), CaptureError> {
        let FrameData::Native(n) = &f.data else {
            return Ok(()); // BGRA mock frames are not composited
        };
        let slot = self.layout.slots[i];
        let cw = self.layout.width as usize;
        let (y, uv) = (&mut self.y, &mut self.uv);
        n.with_nv12(&mut |p| {
            let (sw, sh) = (slot.w as usize, slot.h as usize);
            let (ox, oy) = (slot.x as usize, slot.y as usize);
            let same = p.width == sw && p.height == sh;
            for row in 0..sh {
                let src_row = if same { row } else { row * p.height / sh };
                let dst = &mut y[(oy + row) * cw + ox..(oy + row) * cw + ox + sw];
                let src = &p.y[src_row * p.y_stride..];
                if same {
                    dst.copy_from_slice(&src[..sw]);
                } else {
                    for (col, px) in dst.iter_mut().enumerate() {
                        *px = src[col * p.width / sw];
                    }
                }
            }
            for row in 0..sh / 2 {
                let src_row = if same {
                    row
                } else {
                    row * (p.height / 2) / (sh / 2)
                };
                let base = (oy / 2 + row) * cw + ox;
                let dst = &mut uv[base..base + sw];
                let src = &p.uv[src_row * p.uv_stride..];
                if same {
                    dst.copy_from_slice(&src[..sw]);
                } else {
                    for pair in 0..sw / 2 {
                        let s = pair * (p.width / 2) / (sw / 2) * 2;
                        dst[pair * 2] = src[s];
                        dst[pair * 2 + 1] = src[s + 1];
                    }
                }
            }
        })
    }

    fn composite(&self) -> Result<CapturedFrame, CaptureError> {
        let (w, h) = (self.layout.width as usize, self.layout.height as usize);
        let frame = alloc_nv12(w, h, &mut |y, ys, uv, uvs| {
            for row in 0..h {
                y[row * ys..row * ys + w].copy_from_slice(&self.y[row * w..(row + 1) * w]);
            }
            for row in 0..h / 2 {
                uv[row * uvs..row * uvs + w].copy_from_slice(&self.uv[row * w..(row + 1) * w]);
            }
        })?;
        Ok(CapturedFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Nv12,
            data: FrameData::Native(frame),
            dirty: vec![Rect {
                x: 0,
                y: 0,
                w: w as u32,
                h: h as u32,
            }],
            captured_at: Instant::now(),
        })
    }
}

impl ScreenCapturer for Multi {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        let mut ds = self.real_displays()?;
        if ds.len() >= 2 {
            let all = Self::all_info(&ds);
            ds.push(all);
        }
        Ok(ds)
    }

    fn select(&mut self, display_id: u32) -> Result<(), CaptureError> {
        if display_id != ALL_DISPLAYS {
            self.children.clear();
            return self.inner.select(display_id);
        }
        let ds = self.real_displays()?;
        if ds.len() < 2 {
            return Err(CaptureError::NoSuchDisplay(display_id));
        }
        let mut children = Vec::with_capacity(ds.len());
        for d in ds {
            let mut cap = (self.factory)()?;
            cap.select(d.id)?;
            children.push(Child { display: d, cap });
        }
        self.children = children;
        self.scale = 1.0;
        self.relayout();
        Ok(())
    }

    fn set_scale(&mut self, scale: f32) -> Result<(u32, u32), CaptureError> {
        if self.children.is_empty() {
            return self.inner.set_scale(scale);
        }
        self.scale = scale;
        self.relayout();
        Ok((self.layout.width, self.layout.height))
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if self.children.is_empty() {
            return self.inner.next_frame(timeout);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let mut changed = false;
            for i in 0..self.children.len() {
                if let Some(f) = self.children[i].cap.next_frame(Duration::ZERO)? {
                    self.blit(i, &f)?;
                    changed = true;
                }
            }
            if changed {
                return self.composite().map(Some);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(2)));
        }
    }

    fn cursor(&mut self) -> Option<CursorState> {
        if self.children.is_empty() {
            return self.inner.cursor();
        }
        let (cw, ch) = (self.layout.width as f32, self.layout.height as f32);
        let mut out: Option<CursorState> = None;
        for (c, slot) in self.children.iter_mut().zip(&self.layout.slots) {
            let Some(st) = c.cap.cursor() else { continue };
            if out.is_none() || st.visible {
                out = Some(CursorState {
                    x: (slot.x as f32 + st.x * slot.w as f32) / cw,
                    y: (slot.y as f32 + st.y * slot.h as f32) / ch,
                    ..st
                });
            }
            if out.as_ref().is_some_and(|o| o.visible) {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(id: u32, x: i32, y: i32, w: u32, h: u32) -> DisplayInfo {
        DisplayInfo {
            id,
            name: format!("D{id}"),
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            primary: id == 1,
        }
    }

    #[test]
    fn side_by_side_layout() {
        let ds = [disp(1, 0, 0, 1920, 1080), disp(2, 1920, 0, 1920, 1080)];
        let l = layout(&ds, 1.0);
        assert_eq!((l.width, l.height), (3840, 1080), "fits as is");
        assert_eq!(l.slots[1].x, 1920);
        let l = layout(&ds, 0.5);
        assert_eq!((l.width, l.height), (1920, 540), "ABR scale applies");

        // Three 1440p screens shrink to the 3840 cap.
        let ds = [
            disp(1, 0, 0, 2560, 1440),
            disp(2, 2560, 0, 2560, 1440),
            disp(3, -2560, 200, 2560, 1440),
        ];
        let l = layout(&ds, 1.0);
        assert!(l.width <= 3840 && l.height <= 2160);
        assert_eq!(l.slots[2].x, 0, "leftmost display at the left edge");
        assert!(l.slots[0].x > 0 && l.slots[2].y > 0);
        for s in &l.slots {
            assert!(s.x + s.w <= l.width && s.y + s.h <= l.height);
            assert!(s.x % 2 == 0 && s.w % 2 == 0, "NV12 needs even edges");
        }
        assert_eq!(desktop_bounds(&ds), (-2560.0, 0.0, 7680.0, 1640.0));
    }

    fn pattern_factory() -> Factory {
        Box::new(|| {
            Ok(Box::new(
                pattern::PatternCapturer::new(640, 360, 60)
                    .with_display(320, 180)
                    .cpu_frames(),
            ) as Box<dyn ScreenCapturer>)
        })
    }

    #[test]
    fn composite_of_two_displays() {
        let mut m = Multi::new(pattern_factory()().unwrap(), pattern_factory());
        let ds = m.displays().unwrap();
        assert_eq!(ds.len(), 3, "two displays + all");
        let all = ds.last().unwrap();
        assert_eq!(all.id, ALL_DISPLAYS);
        m.select(ALL_DISPLAYS).unwrap();
        let f = m
            .next_frame(Duration::from_secs(1))
            .unwrap()
            .expect("a frame");
        assert_eq!((f.width, f.height), (all.width, all.height));
        // Display 1's moving bar and checkerboard landed in its slot; the
        // area below display 2 (outside every display) stayed black.
        let FrameData::Native(n) = &f.data else {
            panic!("native")
        };
        let slot2 = m.layout.slots[1];
        n.with_nv12(&mut |p| {
            let below2 = p.y[(slot2.h as usize + 10) * p.y_stride + slot2.x as usize + 10];
            assert_eq!(below2, 16, "gap is black");
            let in1 = &p.y[100 * p.y_stride..100 * p.y_stride + 600];
            assert!(in1.iter().any(|&v| v > 16), "display 1 drawn");
        })
        .unwrap();
        // ABR shrinks the canvas.
        let (w, h) = m.set_scale(0.5).unwrap();
        assert_eq!((w, h), (all.width / 2, all.height / 2));
        // Back to one display: the inner capturer again.
        m.select(1).unwrap();
        assert!(m.children.is_empty());
    }
}
