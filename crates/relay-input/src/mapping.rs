//! HiDPI-aware coordinate mapping (FR-16).
//!
//! Controller: viewer window point → normalised display coordinate.
//! Host: normalised coordinate → absolute physical pixel on the display.

/// How the remote frame is placed in the viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewMode {
    /// Scale to fit, keep aspect ratio, letterbox.
    Fit,
    /// One remote pixel per physical viewer pixel, scrolled by `(sx, sy)`
    /// physical pixels.
    OneToOne { sx: f32, sy: f32 },
}

/// The viewer surface and remote frame, both in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub view_w: f32,
    pub view_h: f32,
    pub frame_w: f32,
    pub frame_h: f32,
    pub mode: ViewMode,
}

impl Viewport {
    /// Where the frame is drawn: (offset_x, offset_y, scale).
    pub fn placement(&self) -> (f32, f32, f32) {
        match self.mode {
            ViewMode::Fit => {
                let s = (self.view_w / self.frame_w).min(self.view_h / self.frame_h);
                (
                    (self.view_w - self.frame_w * s) / 2.0,
                    (self.view_h - self.frame_h * s) / 2.0,
                    s,
                )
            }
            ViewMode::OneToOne { sx, sy } => (-sx, -sy, 1.0),
        }
    }

    /// Map a viewer point (logical px × `window_scale`) to normalised 0..1,
    /// clamped so the pointer can always reach the screen edges.
    pub fn to_normalised(&self, x_logical: f32, y_logical: f32, window_scale: f32) -> (f32, f32) {
        let (ox, oy, s) = self.placement();
        let px = (x_logical * window_scale - ox) / s;
        let py = (y_logical * window_scale - oy) / s;
        (
            (px / self.frame_w).clamp(0.0, 1.0),
            (py / self.frame_h).clamp(0.0, 1.0),
        )
    }
}

/// Host side: normalised → absolute physical pixel in the virtual desktop.
pub fn to_host_pixel(nx: f32, ny: f32, disp_x: i32, disp_y: i32, w: u32, h: u32) -> (i32, i32) {
    let x = disp_x + (nx.clamp(0.0, 1.0) * (w.saturating_sub(1)) as f32).round() as i32;
    let y = disp_y + (ny.clamp(0.0, 1.0) * (h.saturating_sub(1)) as f32).round() as i32;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterbox_centre_and_edges() {
        // 4K remote in a 1600x1200 physical window: letterboxed top/bottom.
        let v = Viewport {
            view_w: 1600.0,
            view_h: 1200.0,
            frame_w: 3840.0,
            frame_h: 2160.0,
            mode: ViewMode::Fit,
        };
        // Retina window: logical 800x600, scale 2.
        let (x, y) = v.to_normalised(400.0, 300.0, 2.0);
        assert!((x - 0.5).abs() < 1e-4 && (y - 0.5).abs() < 1e-4);
        assert_eq!(v.to_normalised(0.0, 0.0, 2.0), (0.0, 0.0));
        assert_eq!(v.to_normalised(800.0, 600.0, 2.0), (1.0, 1.0));
    }

    #[test]
    fn one_to_one_scroll() {
        let v = Viewport {
            view_w: 1000.0,
            view_h: 1000.0,
            frame_w: 2000.0,
            frame_h: 2000.0,
            mode: ViewMode::OneToOne {
                sx: 1000.0,
                sy: 0.0,
            },
        };
        let (x, y) = v.to_normalised(0.0, 500.0, 1.0);
        assert!((x - 0.5).abs() < 1e-4 && (y - 0.25).abs() < 1e-4);
    }

    #[test]
    fn host_pixel_secondary_display() {
        assert_eq!(to_host_pixel(0.0, 0.0, -2560, 0, 2560, 1440), (-2560, 0));
        assert_eq!(to_host_pixel(1.0, 1.0, -2560, 0, 2560, 1440), (-1, 1439));
    }
}
