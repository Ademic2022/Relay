//! The Host's "being controlled" banner (FR-9, FR-10, FR-33).
//!
//! A small borderless, always-on-top window at the top of the screen:
//! `*  Being controlled by alice (ID 123 456 789)   [ End session ]`.
//! Only the Host user can close it; nothing the Controller sends affects
//! it. Clicking "End session" (or closing the window) ends the session, and
//! so does the global hotkey Ctrl+Alt+Shift+X / ⌃⌥⇧X while it is shown.
//!
//! Drawn with `softbuffer` (CPU pixels; no GPU needed on the Host) and the
//! 8×8 bitmap font. If no window can be created (no GUI session), the
//! terminal line printed by the host remains the indicator.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::OnceLock;

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

#[derive(Debug)]
pub enum BannerEvent {
    Show(String),
    Hide,
    Exit,
}

/// Lets the tokio side show and hide the banner.
#[derive(Clone)]
pub struct Banner {
    proxy: Option<EventLoopProxy<BannerEvent>>,
}

impl std::fmt::Debug for Banner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Banner")
    }
}

impl Banner {
    /// A banner that only exists in the terminal (tests, no GUI).
    pub fn none() -> Self {
        Self { proxy: None }
    }

    pub fn show(&self, who: &str) {
        if let Some(p) = &self.proxy {
            let _ = p.send_event(BannerEvent::Show(who.to_string()));
        }
    }

    pub fn hide(&self) {
        if let Some(p) = &self.proxy {
            let _ = p.send_event(BannerEvent::Hide);
        }
    }
}

/// Signalled when the Host user clicks "End session" (FR-10).
pub fn end_requested() -> &'static tokio::sync::Notify {
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

const BUTTON: &str = "[ End session ]";
/// FR-10 hint, after (not part of) the clickable button.
#[cfg(target_os = "macos")]
const HOTKEY_HINT: &str = "or Ctrl+Opt+Shift+X";
#[cfg(not(target_os = "macos"))]
const HOTKEY_HINT: &str = "or Ctrl+Alt+Shift+X";
const BG: u32 = 0x00B0_2020; // red: impossible to miss
const FG: u32 = 0x00FF_FFFF;

/// The banner text for `who`, and where the button starts (in chars).
pub fn banner_text(who: &str) -> (String, usize) {
    let who: String = who
        .chars()
        .filter(|c| c.is_ascii() && !c.is_control())
        .take(60)
        .collect();
    let left = format!("  *  Being controlled by {who}    ");
    let start = left.chars().count();
    (format!("{left}{BUTTON}  {HOTKEY_HINT}  "), start)
}

/// Rasterise one line into 0RGB pixels at `scale`. Returns (pixels, w, h).
pub fn render(text: &str, scale: usize) -> (Vec<u32>, u32, u32) {
    let cols = text.chars().count();
    let (w, h) = (cols * 8 * scale, 16 * scale);
    let mut px = vec![BG; w * h];
    for (col, ch) in text.chars().enumerate() {
        let glyph = font8x8::legacy::BASIC_LEGACY[(ch as usize).min(127)];
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if bits >> gx & 1 == 1 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let (x, y) = ((col * 8 + gx) * scale + dx, (4 + gy) * scale + dy);
                            px[y * w + x] = FG;
                        }
                    }
                }
            }
        }
    }
    (px, w as u32, h as u32)
}

struct App {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    text: Option<(String, usize)>,
    cursor_x: f64,
    /// FR-10; `None` if it could not be registered (the button still works).
    hotkey: Option<relay_input::hotkey::SessionHotkey>,
}

impl App {
    fn hotkey(&mut self, on: bool) {
        if let Some(h) = &mut self.hotkey
            && let Err(e) = h.set_active(on)
        {
            tracing::warn!("{e}");
        }
    }
}

impl App {
    fn draw(&mut self) {
        let (Some(w), Some(s), Some((text, _))) = (&self.window, &mut self.surface, &self.text)
        else {
            return;
        };
        let scale = w.scale_factor().round().max(1.0) as usize;
        let (px, pw, ph) = render(text, scale);
        let (Some(nw), Some(nh)) = (NonZeroU32::new(pw), NonZeroU32::new(ph)) else {
            return;
        };
        if s.resize(nw, nh).is_err() {
            return;
        }
        if let Ok(mut buf) = s.buffer_mut() {
            let n = buf.len().min(px.len());
            buf[..n].copy_from_slice(&px[..n]);
            let _ = buf.present();
        }
    }

    fn place(&self, el: &ActiveEventLoop) {
        let (Some(w), Some((text, _))) = (&self.window, &self.text) else {
            return;
        };
        let scale = w.scale_factor().round().max(1.0) as u32;
        let (pw, ph) = (text.chars().count() as u32 * 8 * scale, 16 * scale);
        let _ = w.request_inner_size(PhysicalSize::new(pw, ph));
        if let Some(m) = w.current_monitor().or_else(|| el.primary_monitor()) {
            let (mp, ms) = (m.position(), m.size());
            let x = mp.x + (ms.width.saturating_sub(pw) / 2) as i32;
            w.set_outer_position(PhysicalPosition::new(x, mp.y + 8 * scale as i32));
        }
    }
}

impl ApplicationHandler<BannerEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        el.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: BannerEvent) {
        match ev {
            BannerEvent::Show(who) => {
                self.text = Some(banner_text(&who));
                if self.window.is_none() {
                    let attrs = Window::default_attributes()
                        .with_title("Relay — session active")
                        .with_decorations(false)
                        .with_resizable(false)
                        .with_window_level(WindowLevel::AlwaysOnTop)
                        .with_inner_size(PhysicalSize::new(400, 32));
                    match el.create_window(attrs) {
                        Ok(w) => {
                            let w = Arc::new(w);
                            match softbuffer::Context::new(w.clone())
                                .and_then(|c| softbuffer::Surface::new(&c, w.clone()))
                            {
                                Ok(s) => self.surface = Some(s),
                                Err(e) => tracing::warn!("banner surface: {e}"),
                            }
                            self.window = Some(w);
                        }
                        Err(e) => tracing::warn!("banner window: {e}"),
                    }
                }
                if let Some(w) = &self.window {
                    w.set_visible(true);
                }
                self.place(el);
                self.draw();
                self.hotkey(true);
            }
            BannerEvent::Hide => {
                self.hotkey(false);
                self.text = None;
                if let Some(w) = &self.window {
                    w.set_visible(false);
                }
            }
            BannerEvent::Exit => el.exit(),
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => self.draw(),
            WindowEvent::CursorMoved { position, .. } => self.cursor_x = position.x,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (Some(w), Some((_, start))) = (&self.window, &self.text) else {
                    return;
                };
                let scale = w.scale_factor().round().max(1.0);
                let x0 = (*start as f64) * 8.0 * scale;
                let x1 = x0 + BUTTON.len() as f64 * 8.0 * scale;
                if (x0..x1).contains(&self.cursor_x) {
                    end_requested().notify_one();
                }
            }
            // Closing the banner ends the session too.
            WindowEvent::CloseRequested => end_requested().notify_one(),
            _ => {}
        }
    }
}

/// Run `work` with a banner available. The window event loop needs the
/// main thread (macOS), so `work` runs on another thread.
pub fn run_with_banner<F>(work: F) -> anyhow::Result<()>
where
    F: FnOnce(Banner) -> anyhow::Result<()> + Send + 'static,
{
    let event_loop = match EventLoop::<BannerEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            tracing::warn!("no banner window ({e}); using the terminal");
            return work(Banner::none());
        }
    };
    let proxy = event_loop.create_proxy();
    let banner = Banner {
        proxy: Some(proxy.clone()),
    };
    let worker = std::thread::Builder::new()
        .name("relay-host".into())
        .spawn(move || {
            let r = work(banner);
            let _ = proxy.send_event(BannerEvent::Exit);
            r
        })?;
    let mut app = App {
        window: None,
        surface: None,
        text: None,
        cursor_x: 0.0,
        // Created on the main thread: macOS delivers hot keys on its run loop.
        hotkey: relay_input::hotkey::SessionHotkey::new(Box::new(|| end_requested().notify_one()))
            .map_err(|e| tracing::warn!("end-session hotkey unavailable: {e}"))
            .ok(),
    };
    event_loop.run_app(&mut app)?;
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("host thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_pixels() {
        let (t, start) = banner_text("alice (ID 123 456 789)\u{1b}[2J");
        assert!(t.contains("Being controlled by alice"));
        assert!(!t.contains('\u{1b}'), "control characters stripped");
        assert_eq!(&t[start..start + BUTTON.len()], BUTTON);
        assert!(
            t.ends_with(&format!("{HOTKEY_HINT}  ")),
            "hotkey hint shown (FR-10)"
        );
        let (px, w, h) = render(&t, 2);
        assert_eq!(px.len(), (w * h) as usize);
        assert!(px.contains(&FG) && px.contains(&BG));
    }
}
