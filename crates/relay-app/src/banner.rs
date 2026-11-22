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
    /// FR-18: black out the Host's monitors (`Some(who)`) or lift it.
    Blackout(Option<String>, std::sync::mpsc::Sender<Result<(), String>>),
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

    /// FR-18: black out the Host's monitors while `who` works on it (the
    /// stream still shows the real screen), or lift it with `None`.
    pub async fn blackout(&self, who: Option<&str>) -> Result<(), String> {
        let Some(p) = &self.proxy else {
            return Err("no screen to black out".into());
        };
        let (tx, rx) = std::sync::mpsc::channel();
        p.send_event(BannerEvent::Blackout(who.map(String::from), tx))
            .map_err(|_| "the banner is gone".to_string())?;
        tokio::task::spawn_blocking(move || {
            rx.recv_timeout(std::time::Duration::from_secs(3))
                .unwrap_or_else(|_| Err("no answer from the screen".into()))
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
    }
}

/// The black screen's message: white on black, centred, at `scale`.
pub fn render_notice(lines: &[String], w: usize, h: usize, scale: usize) -> Vec<u32> {
    let mut px = vec![0u32; w * h];
    let line_h = 14 * scale;
    let top = h.saturating_sub(lines.len() * line_h) / 2;
    for (row, line) in lines.iter().enumerate() {
        let cols = line.chars().count();
        let left = w.saturating_sub(cols * 8 * scale) / 2;
        for (col, ch) in line.chars().enumerate() {
            let glyph = font8x8::legacy::BASIC_LEGACY[(ch as usize).min(127)];
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..8 {
                    if bits >> gx & 1 == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let (x, y) = (
                                left + (col * 8 + gx) * scale + dx,
                                top + row * line_h + gy * scale + dy,
                            );
                            if x < w && y < h {
                                px[y * w + x] = 0x00C8_C8C8;
                            }
                        }
                    }
                }
            }
        }
    }
    px
}

fn notice_lines(who: &str) -> Vec<String> {
    let who: String = who
        .chars()
        .filter(|c| c.is_ascii() && !c.is_control())
        .take(60)
        .collect();
    vec![
        format!("This computer is being used remotely by {who}."),
        "The screen is dark for privacy.".into(),
        String::new(),
        "Press Ctrl+Alt+Del to get your keyboard and mouse back,".into(),
        "then Ctrl+Alt+Shift+X to end the session.".into(),
    ]
}

/// Keep `w` out of screen capture, so the Controller still sees the
/// desktop under it (Windows 10 2004+).
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn exclude_from_capture(w: &Window) -> Result<(), String> {
    use ::windows::Win32::Foundation::HWND;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = w.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return Err("not a Win32 window".into());
    };
    // SAFETY: the HWND belongs to a live window we own.
    unsafe { SetWindowDisplayAffinity(HWND(h.hwnd.get() as *mut _), WDA_EXCLUDEFROMCAPTURE) }
        .map_err(|e| format!("this Windows version cannot hide a window from capture ({e})"))
}

struct Blackout {
    window: Arc<Window>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    lines: Vec<String>,
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
    /// FR-18 privacy screen, while on.
    blackout: Option<Blackout>,
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
    fn open_blackout(&mut self, el: &ActiveEventLoop, who: &str) -> Result<(), String> {
        if !cfg!(target_os = "windows") {
            return Err("black screen is only available on Windows hosts for now".into());
        }
        if let Some(b) = &mut self.blackout {
            b.lines = notice_lines(who);
            return Ok(());
        }
        // Cover the whole virtual desktop.
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for m in el.available_monitors() {
            let (p, s) = (m.position(), m.size());
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x + s.width as i32);
            y1 = y1.max(p.y + s.height as i32);
        }
        if x0 >= x1 || y0 >= y1 {
            return Err("no monitors".into());
        }
        let attrs = Window::default_attributes()
            .with_title("Relay — privacy screen")
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(x0, y0))
            .with_inner_size(PhysicalSize::new((x1 - x0) as u32, (y1 - y0) as u32));
        let window = Arc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        // Clicks (the Controller's included) go to the apps underneath.
        let _ = window.set_cursor_hittest(false);
        #[cfg(target_os = "windows")]
        exclude_from_capture(&window)?;
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()))
            .ok();
        self.blackout = Some(Blackout {
            window,
            surface,
            lines: notice_lines(who),
        });
        self.draw_blackout();
        // FR-9: the red banner stays above the black screen.
        if let Some(b) = &self.window {
            b.set_window_level(WindowLevel::Normal);
            b.set_window_level(WindowLevel::AlwaysOnTop);
        }
        Ok(())
    }

    fn draw_blackout(&mut self) {
        let Some(b) = &mut self.blackout else { return };
        let Some(s) = &mut b.surface else { return };
        let size = b.window.inner_size();
        let (Some(nw), Some(nh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if s.resize(nw, nh).is_err() {
            return;
        }
        let scale = (b.window.scale_factor() * 2.0).round().max(2.0) as usize;
        let px = render_notice(&b.lines, size.width as usize, size.height as usize, scale);
        if let Ok(mut buf) = s.buffer_mut() {
            let n = buf.len().min(px.len());
            buf[..n].copy_from_slice(&px[..n]);
            let _ = buf.present();
        }
    }

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
            BannerEvent::Blackout(who, done) => {
                let r = match who {
                    Some(who) => self.open_blackout(el, &who),
                    None => {
                        self.blackout = None;
                        Ok(())
                    }
                };
                let _ = done.send(r);
            }
            BannerEvent::Hide => {
                // The session is over: never leave the screen dark.
                self.blackout = None;
                self.hotkey(false);
                self.text = None;
                if let Some(w) = &self.window {
                    w.set_visible(false);
                }
            }
            BannerEvent::Exit => el.exit(),
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        if self.blackout.as_ref().is_some_and(|b| b.window.id() == id) {
            if matches!(ev, WindowEvent::RedrawRequested | WindowEvent::Resized(_)) {
                self.draw_blackout();
            }
            return;
        }
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
        blackout: None,
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

    #[test]
    fn privacy_notice_is_centred_text() {
        let lines = notice_lines("alice\u{7}");
        assert!(lines[0].contains("alice") && !lines[0].contains('\u{7}'));
        assert!(
            lines.iter().any(|l| l.contains("Ctrl+Alt+Del")),
            "how to get control back"
        );
        let px = render_notice(&lines, 1200, 400, 2);
        assert_eq!(px.len(), 1200 * 400);
        assert_eq!(px[0], 0, "black");
        assert!(px.iter().any(|&p| p != 0), "text drawn");
    }
}
