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
use std::time::Instant;

use relay_codec::{DecodedFrame, DecodedImage};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use super::{Shared, UserEvent};

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
}

pub struct App {
    shared: Arc<Shared>,
    quit: Option<tokio::sync::oneshot::Sender<()>>,
    gpu: Option<Gpu>,
    status: String,
    close_reason: Option<String>,
    last_title: Instant,
    meter: Option<Meter>,
    last_log: Instant,
    presented: u32,
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
    pub fn new(shared: Arc<Shared>, quit: tokio::sync::oneshot::Sender<()>, measure: bool) -> Self {
        Self {
            meter: measure.then(|| Meter {
                samples: Vec::new(),
            }),
            last_log: Instant::now(),
            presented: 0,
            shared,
            quit: Some(quit),
            gpu: None,
            status: "Connecting…".into(),
            close_reason: None,
            last_title: Instant::now(),
        }
    }

    pub fn close_reason(&self) -> Option<&str> {
        self.close_reason.as_deref().filter(|s| !s.is_empty())
    }

    fn update_title(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let host = self.shared.host_name.lock().expect("poisoned").clone();
        let title = if gpu.planes.is_some() {
            let s = self.shared.stats.lock().expect("poisoned").clone();
            format!(
                "Relay — {host} — {:.0} fps · {:.1} Mbit/s · RTT {:.0} ms · decode {:.1} ms · {}",
                s.fps, s.mbps, s.rtt_ms, s.decode_ms, s.path
            )
        } else {
            format!("Relay — {host} — {}", self.status)
        };
        gpu.window.set_title(&title);
        self.last_title = Instant::now();
    }

    fn end(&mut self, el: &ActiveEventLoop) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
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
                    gpu.upload(&f);
                    gpu.draw();
                    self.presented += 1;
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
                }
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
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                // M1 has no remote input; Esc leaves full screen or closes.
                if let Some(g) = &self.gpu
                    && g.window.fullscreen().is_some()
                {
                    g.window.set_fullscreen(None);
                } else {
                    self.end(el);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Character(c),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } if c.as_str() == "f" => {
                if let Some(g) = &self.gpu {
                    let fs = g.window.fullscreen().is_some();
                    g.window.set_fullscreen(
                        (!fs).then_some(winit::window::Fullscreen::Borderless(None)),
                    );
                }
            }
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(s.width, s.height);
                    g.draw();
                }
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
