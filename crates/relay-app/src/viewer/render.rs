//! wgpu viewer window (§6 "Present").
//!
//! - Present mode Mailbox, else Immediate: never wait for vsync. Fifo only
//!   if the platform offers nothing else.
//! - I420 planes are uploaded to three R8 textures and converted to RGB in
//!   the fragment shader (BT.709, limited range). The upload is a CPU→GPU
//!   copy that exists because the M1 decoder is software (openh264); the
//!   hardware decoder path will sample decoder surfaces directly.
//! - Fit-to-window with letterboxing, computed per frame.

use std::sync::Arc;
use std::time::{Duration, Instant};

use relay_codec::{DecodedFrame, DecodedImage};
use relay_input::keymap::ModifierMapper;
use relay_input::{InputEvent, Modifiers, MouseButton, Os};
use relay_proto::v1;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use super::files_ui::{FileCmd, FileManager};
use super::{Shared, UserEvent, ViewOptions, keys, overlay::Overlay};

const SHADER: &str = r#"
struct Uniforms { scale: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(4) var samp: sampler;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Two triangles covering the letterboxed rectangle.
    var corners = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0),
        vec2(-1.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0));
    let c = corners[i];
    var o: VsOut;
    o.pos = vec4(c * u.scale, 0.0, 1.0);
    o.uv = vec2((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = (textureSample(tex_u, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (textureSample(tex_v, samp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb);
    return vec4(clamp(rgb, vec3(0.0), vec3(1.0)), 1.0);
}
"#;

struct Planes {
    w: u32,
    h: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind: wgpu::BindGroup,
}

struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Option<Planes>,
    overlay: Overlay,
    chat_panel: Overlay,
    files_panel: Overlay,
}

pub struct App {
    shared: Arc<Shared>,
    quit: tokio::sync::watch::Sender<bool>,
    gpu: Option<Gpu>,
    status: String,
    close_reason: Option<String>,
    last_title: Instant,
    meter: Option<Meter>,
    last_log: Instant,
    presented: u32,
    input: InputState,
    cursor: RemoteCursor,
    ui_tx: tokio::sync::mpsc::UnboundedSender<v1::ControlMessage>,
    quality: v1::QualityMode,
    chat: Chat,
    files: FileManager,
    files_tx: tokio::sync::mpsc::UnboundedSender<FileCmd>,
    /// Restart requested once; a second press within 3 s confirms.
    pending_power: Option<(Instant, bool)>,
    /// Short message shown at the start of the title.
    notice: Option<(Instant, String)>,
}

/// In-window chat (FR-24).
struct Chat {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// (sent at ms, from me, text)
    log: std::collections::VecDeque<(u64, bool, String)>,
    unread: u32,
    /// Open with the text being typed.
    compose: Option<String>,
}

impl Chat {
    const KEEP: usize = 50;
    const SHOW: usize = 8;
    const WRAP: usize = 64;

    fn push(&mut self, at: u64, mine: bool, text: String) {
        if self.log.len() == Self::KEEP {
            self.log.pop_front();
        }
        self.log.push_back((at, mine, text));
    }

    fn lines(&self, host: &str) -> Vec<String> {
        let mut out = Vec::new();
        let from = self.log.len().saturating_sub(Self::SHOW);
        for (at, mine, text) in self.log.iter().skip(from) {
            let who = if *mine { "you" } else { host };
            let mut first = true;
            for part in text.split('\n') {
                let chars: Vec<char> = part.chars().collect();
                for chunk in chars
                    .chunks(Self::WRAP)
                    .map(|c| c.iter().collect::<String>())
                    .chain(chars.is_empty().then(String::new))
                {
                    if first {
                        out.push(format!("{} {who}: {chunk}", crate::chat::clock(*at)));
                        first = false;
                    } else {
                        out.push(format!("      {chunk}"));
                    }
                }
            }
        }
        if out.is_empty() {
            out.push("(no messages yet)".into());
        }
        let typing: String = self
            .compose
            .as_deref()
            .unwrap_or_default()
            .chars()
            .rev()
            .take(Self::WRAP)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        out.push(format!("> {typing}_"));
        out.push("Enter send  Esc close".into());
        out
    }
}

/// The Host's pointer, shown as the local OS cursor (§6 rule 5): the
/// pointer moves at local input rate while its shape follows the Host.
#[derive(Default)]
struct RemoteCursor {
    shapes: std::collections::HashMap<u64, v1::CursorShape>,
    built: std::collections::HashMap<(u64, u32), winit::window::CustomCursor>,
    hash: u64,
    visible: bool,
    applied: Option<(u64, u32, bool)>,
}

/// Controller-side input capture (FR-13 to FR-16).
struct InputState {
    tx: tokio::sync::mpsc::UnboundedSender<v1::InputEvent>,
    swap_cmd_ctrl: bool,
    translate_chars: bool,
    mapper: Option<ModifierMapper>,
    /// Keys and buttons held, released if the window loses focus.
    keys_down: Vec<u16>,
    buttons_down: Vec<MouseButton>,
    mods: ModifiersState,
    last_pos: Option<(f32, f32)>,
}

const LOCAL_OS: Os = if cfg!(target_os = "macos") {
    Os::MacOs
} else {
    Os::Windows
};

impl InputState {
    fn send(&self, ev: InputEvent) {
        let _ = self
            .tx
            .send(crate::input_wire::to_proto(&ev, crate::clock::now_us()));
    }

    fn mapper(&mut self, host_os: Option<Os>) -> ModifierMapper {
        *self.mapper.get_or_insert_with(|| {
            ModifierMapper::new(LOCAL_OS, host_os.unwrap_or(LOCAL_OS), self.swap_cmd_ctrl)
        })
    }

    fn modifiers(&self) -> Modifiers {
        let m = self.mods;
        let mut bits = 0;
        if m.shift_key() {
            bits |= Modifiers::SHIFT;
        }
        if m.control_key() {
            bits |= Modifiers::CTRL;
        }
        if m.alt_key() {
            bits |= Modifiers::ALT;
        }
        if m.super_key() {
            bits |= Modifiers::META;
        }
        Modifiers(bits)
    }

    /// Release everything we pressed (focus loss, window close).
    fn release_all(&mut self) {
        for u in std::mem::take(&mut self.keys_down) {
            self.send(InputEvent::Key {
                usage: u,
                down: false,
                modifiers: Modifiers::default(),
            });
        }
        for b in std::mem::take(&mut self.buttons_down) {
            self.send(InputEvent::MouseButton {
                button: b,
                down: false,
            });
        }
    }
}

/// Nearest-neighbour resample of a Host cursor by `k`.
fn scale_cursor(s: &v1::CursorShape, k: f32) -> Option<winit::window::CustomCursorSource> {
    let (w, h) = (s.width as usize, s.height as usize);
    if w == 0 || h == 0 || s.rgba.len() < w * h * 4 {
        return None;
    }
    let tw = ((w as f32 * k).round() as usize).clamp(1, 256);
    let th = ((h as f32 * k).round() as usize).clamp(1, 256);
    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th {
        let sy = (y * h / th).min(h - 1);
        for x in 0..tw {
            let sx = (x * w / tw).min(w - 1);
            out[(y * tw + x) * 4..][..4].copy_from_slice(&s.rgba[(sy * w + sx) * 4..][..4]);
        }
    }
    let hx = ((s.hotspot_x as f32 * k).round() as usize).min(tw - 1);
    let hy = ((s.hotspot_y as f32 * k).round() as usize).min(th - 1);
    winit::window::CustomCursor::from_rgba(out, tw as u16, th as u16, hx as u16, hy as u16).ok()
}

fn map_button(b: winit::event::MouseButton) -> Option<MouseButton> {
    use winit::event::MouseButton as W;
    Some(match b {
        W::Left => MouseButton::Left,
        W::Right => MouseButton::Right,
        W::Middle => MouseButton::Middle,
        W::Back => MouseButton::Back,
        W::Forward => MouseButton::Forward,
        W::Other(_) => return None,
    })
}

/// Glass-to-glass samples from the test-pattern timestamp stripe.
struct Meter {
    samples: Vec<std::time::Duration>,
}

impl Meter {
    fn sample(&mut self, f: &DecodedFrame) {
        let DecodedImage::I420 { y, y_stride, .. } = &f.image else {
            return;
        };
        let stamp =
            relay_capture::pattern::read_stamp(y, *y_stride, f.width as usize, f.height as usize);
        let now = relay_capture::pattern::unix_ms32();
        let d = now.wrapping_sub(stamp);
        if d > 50 && std::env::var_os("RELAY_DEBUG_STAMP").is_some() {
            let (w, h) = (f.width as usize, f.height as usize);
            let bw = w / relay_capture::pattern::STRIPE_BLOCKS;
            let row = (h / 20).max(8) / 2;
            let px: Vec<u8> = (0..32)
                .map(|b| y[row * y_stride + b * bw + bw / 2])
                .collect();
            eprintln!("d={d} stamp={stamp} now={now} px={px:?}");
        }
        if d < 10_000 {
            self.samples
                .push(std::time::Duration::from_millis(u64::from(d)));
        }
    }

    fn report(&mut self) {
        if let Some(s) = latency_bench::summarize(&self.samples) {
            tracing::info!(
                "glass-to-glass (pattern stamp → presented): n={} p50={:?} p95={:?} p99={:?} max={:?}",
                s.count,
                s.p50,
                s.p95,
                s.p99,
                s.max
            );
        }
        self.samples.clear();
    }
}

impl App {
    pub fn new(
        shared: Arc<Shared>,
        quit: tokio::sync::watch::Sender<bool>,
        input_tx: tokio::sync::mpsc::UnboundedSender<v1::InputEvent>,
        ui_tx: tokio::sync::mpsc::UnboundedSender<v1::ControlMessage>,
        chat_tx: tokio::sync::mpsc::UnboundedSender<String>,
        files_tx: tokio::sync::mpsc::UnboundedSender<FileCmd>,
        opts: &ViewOptions,
    ) -> Self {
        Self {
            files: FileManager::new(),
            files_tx,
            pending_power: None,
            notice: None,
            chat: Chat {
                tx: chat_tx,
                log: std::collections::VecDeque::new(),
                unread: 0,
                compose: None,
            },
            ui_tx,
            quality: opts.quality,
            input: InputState {
                tx: input_tx,
                swap_cmd_ctrl: opts.cmd_ctrl_swap,
                translate_chars: opts.translate_chars,
                mapper: None,
                keys_down: Vec::new(),
                buttons_down: Vec::new(),
                mods: ModifiersState::empty(),
                last_pos: None,
            },
            cursor: RemoteCursor::default(),
            meter: opts.measure.then(|| Meter {
                samples: Vec::new(),
            }),
            last_log: Instant::now(),
            presented: 0,
            shared,
            quit,
            gpu: None,
            status: "Connecting…".into(),
            close_reason: None,
            last_title: Instant::now(),
        }
    }

    pub fn close_reason(&self) -> Option<&str> {
        self.close_reason.as_deref().filter(|s| !s.is_empty())
    }

    fn notify(&mut self, text: &str) {
        self.notice = Some((Instant::now(), text.to_string()));
        self.update_title();
    }

    fn refresh_files(&mut self) {
        let open = self.files.open;
        let lines = self.files.lines();
        let Some(g) = &mut self.gpu else { return };
        g.files_panel.visible = open;
        if open {
            let scale = g.window.scale_factor().round().max(1.0) as usize;
            g.files_panel.set_text(&g.device, &g.queue, &lines, scale);
        }
        g.draw();
    }

    fn refresh_chat(&mut self) {
        let host = self.shared.host_name.lock().expect("poisoned").clone();
        let host: String = host.chars().take(20).collect();
        let lines = self.chat.lines(&host);
        let open = self.chat.compose.is_some();
        let Some(g) = &mut self.gpu else { return };
        g.chat_panel.visible = open;
        if open {
            let scale = g.window.scale_factor().round().max(1.0) as usize;
            g.chat_panel.set_text(&g.device, &g.queue, &lines, scale);
        }
        g.draw();
    }

    /// While the chat panel is open, keys edit the message instead of
    /// going to the host.
    fn chat_key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let Some(buf) = self.chat.compose.as_mut() else {
            return;
        };
        match ev.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => self.chat.compose = None,
            PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter) => {
                let text = crate::chat::sanitize(buf);
                buf.clear();
                if !text.is_empty() {
                    let _ = self.chat.tx.send(text.clone());
                    self.chat.push(crate::chat::now_ms(), true, text);
                }
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                buf.pop();
            }
            _ => {
                if let Some(t) = &ev.text
                    && buf.chars().count() < crate::chat::MAX_CHARS
                {
                    buf.extend(t.chars().filter(|c| !c.is_control()));
                }
            }
        }
        self.refresh_chat();
    }

    /// Ask the host to switch displays (FR-21).
    fn pick_monitor(&mut self, key: KeyCode) {
        let displays = self.shared.displays.lock().expect("poisoned").clone();
        if displays.len() < 2 {
            return;
        }
        let cur = self
            .shared
            .current_display
            .load(std::sync::atomic::Ordering::Relaxed);
        let idx = displays.iter().position(|d| d.id == cur).unwrap_or(0);
        let next = match key {
            KeyCode::KeyM => (idx + 1) % displays.len(),
            other => {
                let n = (other as usize).wrapping_sub(KeyCode::Digit1 as usize);
                if n >= displays.len() {
                    return;
                }
                n
            }
        };
        let id = displays[next].id;
        let _ = self.ui_tx.send(crate::auth::control(
            v1::control_message::Msg::SelectMonitor(v1::SelectMonitor { display_id: id }),
        ));
        tracing::info!("switching to display {} of {}", next + 1, displays.len());
    }

    /// "display 2/3 · 2560x1440" for the title and overlay.
    fn display_label(&self) -> String {
        let displays = self.shared.displays.lock().expect("poisoned");
        let cur = self
            .shared
            .current_display
            .load(std::sync::atomic::Ordering::Relaxed);
        match displays.iter().position(|d| d.id == cur) {
            Some(i) if displays.len() > 1 => {
                let d = &displays[i];
                format!(
                    "display {}/{} · {}x{}",
                    i + 1,
                    displays.len(),
                    d.width,
                    d.height
                )
            }
            Some(i) => format!("{}x{}", displays[i].width, displays[i].height),
            None => String::new(),
        }
    }

    fn update_overlay(&mut self) {
        let label = self.display_label();
        let Some(g) = &mut self.gpu else { return };
        if !g.overlay.visible {
            return;
        }
        let s = self.shared.stats.lock().expect("poisoned").clone();
        let (w, h) = g.planes.as_ref().map_or((0, 0), |p| (p.w, p.h));
        let lines = vec![
            format!("{:.0} fps  {:.2} Mbit/s  {w}x{h}", s.fps, s.mbps),
            format!(
                "RTT {:.0} ms  queue {:.0} ms  {}",
                s.rtt_ms, s.queue_ms, s.path
            ),
            format!(
                "loss {:.1}%  FEC-recovered {}  dropped {}",
                s.loss_pct, s.recovered, s.dropped
            ),
            format!("decode {:.1} ms  mode {:?}", s.decode_ms, self.quality),
            label,
            "Ctrl+Alt+ S stats  Q quality  M/1-9 monitor  C chat  F files".to_string(),
        ];
        let scale = g.window.scale_factor().round().max(1.0) as usize;
        g.overlay.set_text(&g.device, &g.queue, &lines, scale);
    }

    fn update_title(&mut self) {
        self.update_overlay();
        let Some(gpu) = &self.gpu else { return };
        let host = self.shared.host_name.lock().expect("poisoned").clone();
        let title = if gpu.planes.is_some() {
            let s = self.shared.stats.lock().expect("poisoned").clone();
            format!(
                "Relay — {host} — {} — {:.0} fps · {:.1} Mbit/s · RTT {:.0} ms · decode {:.1} ms · {}",
                self.display_label(),
                s.fps,
                s.mbps,
                s.rtt_ms,
                s.decode_ms,
                s.path
            )
        } else {
            format!("Relay — {host} — {}", self.status)
        };
        let title = match self
            .notice
            .as_ref()
            .filter(|(t, _)| t.elapsed() < Duration::from_secs(3))
        {
            Some((_, n)) => format!("{n} · {title}"),
            None => title,
        };
        let title = match self.files.summary() {
            Some(t) => format!("{t} · {title}"),
            None => title,
        };
        let title = if self.chat.unread > 0 {
            format!("✉ {} (Ctrl+Alt+C) · {title}", self.chat.unread)
        } else {
            title
        };
        gpu.window.set_title(&title);
        self.last_title = Instant::now();
    }

    fn control_allowed(&self) -> bool {
        self.gpu.as_ref().is_some_and(|g| g.planes.is_some())
            && self.shared.host.lock().expect("poisoned").control
    }

    /// Window pixel → normalised Host display coordinate (FR-16), using the
    /// same letterbox placement as the renderer.
    fn normalise(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let g = self.gpu.as_ref()?;
        let p = g.planes.as_ref()?;
        let vp = relay_input::mapping::Viewport {
            view_w: g.config.width as f32,
            view_h: g.config.height as f32,
            frame_w: p.w as f32,
            frame_h: p.h as f32,
            mode: relay_input::mapping::ViewMode::Fit,
        };
        // winit positions are already physical pixels.
        Some(vp.to_normalised(x as f32, y as f32, 1.0))
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        let PhysicalKey::Code(code) = ev.physical_key else {
            return;
        };
        let down = ev.state == ElementState::Pressed;
        let host_os = self.shared.host.lock().expect("poisoned").os;
        let st = &mut self.input;
        let m = st.modifiers();
        // "Translate characters" mode: printable text without shortcut
        // modifiers is sent as characters (FR-14).
        if st.translate_chars
            && down
            && !m.contains(Modifiers::CTRL)
            && !m.contains(Modifiers::META)
            && let Some(t) = ev
                .text
                .as_ref()
                .filter(|t| t.chars().all(|c| !c.is_control()))
        {
            st.send(InputEvent::Text(t.to_string()));
            return;
        }
        let Some(usage) = keys::hid_usage(code) else {
            return;
        };
        if ev.repeat && !down {
            return;
        }
        if down {
            if !st.keys_down.contains(&usage) {
                st.keys_down.push(usage);
            }
        } else if let Some(i) = st.keys_down.iter().position(|&u| u == usage) {
            st.keys_down.remove(i);
        } else if !st.translate_chars {
            return; // key-up for a key pressed before we had focus
        }
        let mapped = st.mapper(host_os).apply(InputEvent::Key {
            usage,
            down,
            modifiers: m,
        });
        st.send(mapped);
    }

    fn on_cursor(&mut self, el: &ActiveEventLoop, m: v1::CursorMessage) {
        match m.msg {
            Some(v1::cursor_message::Msg::Shape(s)) => {
                if !s.rgba.is_empty() {
                    self.cursor.shapes.insert(s.hash, s);
                }
            }
            Some(v1::cursor_message::Msg::Position(p)) => {
                self.cursor.hash = p.shape_hash;
                self.cursor.visible = p.visible;
            }
            None => {}
        }
        self.apply_cursor(el);
    }

    fn apply_cursor(&mut self, el: &ActiveEventLoop) {
        let Some(g) = &self.gpu else { return };
        let Some(p) = &g.planes else { return };
        // Size the Host cursor like the video: host px × fit scale, in
        // the units winit expects (points on macOS, pixels elsewhere).
        let fit = (g.config.width as f32 / p.w as f32).min(g.config.height as f32 / p.h as f32);
        let units = if cfg!(target_os = "macos") {
            fit / g.window.scale_factor() as f32
        } else {
            fit
        };
        let key_scale = (units * 100.0).round() as u32;
        let c = &mut self.cursor;
        let want = (c.hash, key_scale, c.visible);
        if c.applied == Some(want) {
            return;
        }
        if !c.visible {
            g.window.set_cursor_visible(false);
            c.applied = Some(want);
            return;
        }
        let Some(shape) = c.shapes.get(&c.hash) else {
            return; // shape not received yet; keep the current one
        };
        let cursor = match c.built.get(&(c.hash, key_scale)) {
            Some(cc) => cc.clone(),
            None => {
                let Some(src) = scale_cursor(shape, units) else {
                    return;
                };
                let cc = el.create_custom_cursor(src);
                tracing::info!(
                    "cursor: host shape {}x{} (hash {:x}) at {units:.2}x",
                    shape.width,
                    shape.height,
                    shape.hash
                );
                c.built.insert((c.hash, key_scale), cc.clone());
                cc
            }
        };
        g.window.set_cursor(cursor);
        g.window.set_cursor_visible(true);
        c.applied = Some(want);
    }

    fn end(&mut self, el: &ActiveEventLoop) {
        self.input.release_all();
        let _ = self.quit.send(true);
        el.exit();
    }
}

impl Gpu {
    fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;

        let caps = surface.get_capabilities(&adapter);
        // YUV→RGB output is already gamma-encoded: use a non-sRGB format.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        tracing::info!(
            "viewer: {:?}, {format:?}, {present_mode:?}",
            adapter.get_info().backend
        );
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let overlay = Overlay::new(&device, format);
        let mut chat_panel = Overlay::new(&device, format);
        chat_panel.bottom = true;
        let files_panel = Overlay::new(&device, format);
        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            uniforms,
            planes: None,
            overlay,
            chat_panel,
            files_panel,
        })
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn plane_tex(&self, w: u32, h: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn upload(&mut self, f: &DecodedFrame) {
        let DecodedImage::I420 {
            y,
            u,
            v,
            y_stride,
            uv_stride,
        } = &f.image
        else {
            return;
        };
        if self
            .planes
            .as_ref()
            .is_none_or(|p| p.w != f.width || p.h != f.height)
        {
            let (ty, tu, tv) = (
                self.plane_tex(f.width, f.height),
                self.plane_tex(f.width / 2, f.height / 2),
                self.plane_tex(f.width / 2, f.height / 2),
            );
            let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
            let (vy, vu, vv) = (view(&ty), view(&tu), view(&tv));
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("yuv"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&vy),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&vu),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&vv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.planes = Some(Planes {
                w: f.width,
                h: f.height,
                y: ty,
                u: tu,
                v: tv,
                bind,
            });
        }
        let p = self.planes.as_ref().expect("set");
        let write = |tex: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&p.y, y, *y_stride, f.width, f.height);
        write(&p.u, u, *uv_stride, f.width / 2, f.height / 2);
        write(&p.v, v, *uv_stride, f.width / 2, f.height / 2);
    }

    fn draw(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut scale = [1.0f32, 1.0, 0.0, 0.0];
        if let Some(p) = &self.planes {
            let (vw, vh) = (self.config.width as f32, self.config.height as f32);
            let s = (vw / p.w as f32).min(vh / p.h as f32);
            scale[0] = p.w as f32 * s / vw;
            scale[1] = p.h as f32 * s / vh;
        }
        let bytes: Vec<u8> = scale.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.queue.write_buffer(&self.uniforms, 0, &bytes);

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(p) = &self.planes {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &p.bind, &[]);
                pass.draw(0..6, 0..1);
            }
            self.overlay.draw(
                &self.queue,
                &mut pass,
                self.config.width as f32,
                self.config.height as f32,
            );
        }
        self.queue.submit([enc.finish()]);
        self.window.pre_present_notify();
        self.queue.present(frame);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        el.set_control_flow(ControlFlow::Wait);
        let attrs = Window::default_attributes()
            .with_title("Relay")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 760.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.close_reason = Some(format!("window: {e}"));
                return self.end(el);
            }
        };
        match Gpu::new(window) {
            Ok(g) => self.gpu = Some(g),
            Err(e) => {
                self.close_reason = Some(format!("GPU init: {e:#}"));
                return self.end(el);
            }
        }
        self.update_title();
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                let Some(gpu) = &mut self.gpu else { return };
                // Newest decoded frame only; older ones were superseded.
                if let Some(f) = self.shared.frames.try_recv() {
                    let _s = tracing::trace_span!("present").entered();
                    let first = gpu.planes.is_none();
                    gpu.upload(&f);
                    gpu.draw();
                    self.presented += 1;
                    if first {
                        self.cursor.applied = None;
                        self.apply_cursor(el);
                    }
                    if let Some(m) = &mut self.meter {
                        m.sample(&f);
                    }
                }
                if self.last_log.elapsed().as_secs() >= 5 {
                    let s = self.shared.stats.lock().expect("poisoned").clone();
                    tracing::info!(
                        "viewer: presented {:.0} fps, decode {:.1} ms, {:.2} Mbit/s, RTT {:.1} ms, dropped {}",
                        f64::from(self.presented) / self.last_log.elapsed().as_secs_f64(),
                        s.decode_ms,
                        s.mbps,
                        s.rtt_ms,
                        s.dropped
                    );
                    if let Some(m) = &mut self.meter {
                        m.report();
                    }
                    self.presented = 0;
                    self.last_log = Instant::now();
                }
                if self.last_title.elapsed().as_millis() >= 500 {
                    self.update_title();
                    if self.files.open && self.files.has_running() {
                        self.refresh_files();
                    }
                }
            }
            UserEvent::Cursor(m) => self.on_cursor(el, m),
            UserEvent::Files(e) => {
                self.files.on_event(e);
                self.refresh_files();
                self.update_title();
            }
            UserEvent::Chat(m) => {
                self.chat.push(m.sent_at_ms, false, m.text);
                if self.chat.compose.is_none() {
                    self.chat.unread += 1;
                }
                self.refresh_chat();
                self.update_title();
            }
            UserEvent::Status(s) => {
                self.status = s;
                self.update_title();
            }
            UserEvent::Closed(reason) => {
                self.close_reason = Some(reason);
                self.end(el);
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => self.end(el),
            WindowEvent::ModifiersChanged(m) => self.input.mods = m.state(),
            // FR-27: drop files on the window to upload them to the host's
            // current folder (its Downloads until one is opened).
            WindowEvent::DroppedFile(path) => {
                tracing::info!("uploading dropped file {}", path.display());
                let _ = self.files_tx.send(FileCmd::Upload {
                    local: path,
                    remote_dir: self.files.remote_dir(),
                });
            }
            WindowEvent::Focused(false) => self.input.release_all(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                // Local shortcuts, never sent to the host:
                // Ctrl+Alt+S stats overlay, Ctrl+Alt+Q quality mode.
                let m = self.input.mods;
                if event.state == ElementState::Pressed && m.control_key() && m.alt_key() {
                    match event.physical_key {
                        PhysicalKey::Code(KeyCode::KeyS) => {
                            if let Some(g) = &mut self.gpu {
                                g.overlay.visible = !g.overlay.visible;
                            }
                            self.update_overlay();
                            if let Some(g) = &mut self.gpu {
                                g.draw();
                            }
                            return;
                        }
                        // FR-21: Ctrl+Alt+M next monitor, Ctrl+Alt+1…9 pick one.
                        PhysicalKey::Code(
                            code @ (KeyCode::KeyM
                            | KeyCode::Digit1
                            | KeyCode::Digit2
                            | KeyCode::Digit3
                            | KeyCode::Digit4
                            | KeyCode::Digit5
                            | KeyCode::Digit6
                            | KeyCode::Digit7
                            | KeyCode::Digit8
                            | KeyCode::Digit9),
                        ) => {
                            self.pick_monitor(code);
                            return;
                        }
                        // FR-14: Ctrl+Alt+End sends Ctrl+Alt+Del to a Windows host.
                        PhysicalKey::Code(KeyCode::End) => {
                            let _ = self.ui_tx.send(crate::auth::control(
                                v1::control_message::Msg::SendSas(v1::SendSas {}),
                            ));
                            self.notify("Ctrl+Alt+Del sent");
                            return;
                        }
                        // FR-34: Ctrl+Alt+R restarts the host (Shift: Safe Mode).
                        // Press twice within 3 s to confirm.
                        PhysicalKey::Code(KeyCode::KeyR) => {
                            let safe = m.shift_key();
                            let confirmed = self.pending_power.is_some_and(|(t, s)| {
                                s == safe && t.elapsed() < Duration::from_secs(3)
                            });
                            if confirmed {
                                self.pending_power = None;
                                let action = if safe {
                                    v1::PowerAction::RebootSafeMode
                                } else {
                                    v1::PowerAction::Reboot
                                };
                                let _ = self.ui_tx.send(crate::auth::control(
                                    v1::control_message::Msg::Power(v1::PowerRequest {
                                        action: action as i32,
                                    }),
                                ));
                                self.notify("Restart requested");
                            } else {
                                self.pending_power = Some((Instant::now(), safe));
                                self.notify(if safe {
                                    "Press Ctrl+Alt+Shift+R again to restart the host in Safe Mode"
                                } else {
                                    "Press Ctrl+Alt+R again to restart the host"
                                });
                            }
                            return;
                        }
                        PhysicalKey::Code(KeyCode::KeyF) => {
                            self.files.open = !self.files.open;
                            if self.files.open {
                                self.input.release_all();
                                if self.files.remote_dir().is_empty() {
                                    let _ = self.files_tx.send(FileCmd::ListRemote(String::new()));
                                }
                            }
                            self.refresh_files();
                            return;
                        }
                        PhysicalKey::Code(KeyCode::KeyC) => {
                            if self.chat.compose.take().is_none() {
                                // Release anything held before keys stop
                                // reaching the host.
                                self.input.release_all();
                                self.chat.compose = Some(String::new());
                                self.chat.unread = 0;
                            }
                            self.refresh_chat();
                            self.update_title();
                            return;
                        }
                        PhysicalKey::Code(KeyCode::KeyQ) => {
                            self.quality = match self.quality {
                                v1::QualityMode::Speed => v1::QualityMode::Balanced,
                                v1::QualityMode::Balanced => v1::QualityMode::Quality,
                                v1::QualityMode::Quality => v1::QualityMode::Speed,
                            };
                            let _ = self.ui_tx.send(crate::auth::control(
                                v1::control_message::Msg::SetQuality(v1::SetQuality {
                                    mode: self.quality as i32,
                                }),
                            ));
                            tracing::info!("quality mode {:?}", self.quality);
                            self.update_overlay();
                            return;
                        }
                        _ => {}
                    }
                }
                // Ctrl+Alt+Enter toggles full screen.
                if event.state == ElementState::Pressed
                    && event.physical_key == PhysicalKey::Code(KeyCode::Enter)
                    && m.control_key()
                    && m.alt_key()
                {
                    if let Some(g) = &self.gpu {
                        let fs = g.window.fullscreen().is_some();
                        g.window.set_fullscreen(
                            (!fs).then_some(winit::window::Fullscreen::Borderless(None)),
                        );
                    }
                    return;
                }
                if self.files.open {
                    for cmd in self.files.key(&event) {
                        let _ = self.files_tx.send(cmd);
                    }
                    self.refresh_files();
                    return;
                }
                if self.chat.compose.is_some() {
                    self.chat_key(&event);
                    return;
                }
                if !is_synthetic && self.control_allowed() {
                    self.on_key(&event);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.control_allowed()
                    && let Some((x, y)) = self.normalise(position.x, position.y)
                    && self.input.last_pos != Some((x, y))
                {
                    self.input.last_pos = Some((x, y));
                    self.input.send(InputEvent::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(b) = map_button(button)
                    && self.control_allowed()
                {
                    let down = state == ElementState::Pressed;
                    let st = &mut self.input;
                    st.buttons_down.retain(|x| *x != b);
                    if down {
                        st.buttons_down.push(b);
                    }
                    st.send(InputEvent::MouseButton { button: b, down });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.control_allowed() {
                    let ev = match delta {
                        MouseScrollDelta::LineDelta(x, y) => InputEvent::Scroll {
                            dx: x * relay_input::PX_PER_WHEEL_NOTCH,
                            dy: y * relay_input::PX_PER_WHEEL_NOTCH,
                            precise: false,
                        },
                        MouseScrollDelta::PixelDelta(p) => {
                            let scale = self.gpu.as_ref().map_or(1.0, |g| g.window.scale_factor());
                            InputEvent::Scroll {
                                dx: (p.x / scale) as f32,
                                dy: (p.y / scale) as f32,
                                precise: true,
                            }
                        }
                    };
                    self.input.send(ev);
                }
            }
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(s.width, s.height);
                    g.draw();
                }
                self.apply_cursor(el);
            }
            WindowEvent::RedrawRequested => {
                if let Some(g) = &mut self.gpu {
                    g.draw();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;

    #[test]
    fn panel_lines_wrap_and_show_compose() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Chat {
            tx,
            log: Default::default(),
            unread: 0,
            compose: Some("typing".into()),
        };
        assert_eq!(c.lines("host")[0], "(no messages yet)");
        c.push(0, false, "x".repeat(70));
        c.push(0, true, "two\nlines".into());
        let l = c.lines("host");
        assert!(l[0].ends_with(&format!("host: {}", "x".repeat(Chat::WRAP))));
        assert_eq!(l[1].trim(), "xxxxxx");
        assert!(l[2].ends_with("you: two"));
        assert_eq!(l[3].trim(), "lines");
        assert_eq!(l[4], "> typing_");
        for _ in 0..60 {
            c.push(0, true, "m".into());
        }
        assert_eq!(c.log.len(), Chat::KEEP);
    }
}
