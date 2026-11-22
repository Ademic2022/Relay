//! Live stats overlay (FR-19, M4): a few lines of text drawn over the
//! video, top-left. Text is rasterised on the CPU with an 8×8 bitmap font
//! into a small RGBA texture (a few KB, refreshed twice a second) and
//! alpha-blended in the same render pass as the video.

const SHADER: &str = r#"
struct R { rect: vec4<f32> };
@group(0) @binding(0) var<uniform> r: R;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var c = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
                                vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let k = c[i];
    var o: VsOut;
    o.pos = vec4(mix(r.rect.x, r.rect.z, k.x), mix(r.rect.y, r.rect.w, k.y), 0.0, 1.0);
    o.uv = k;
    return o;
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

const MARGIN: f32 = 8.0;
pub const PAD: usize = 4;

pub struct Overlay {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    tex: Option<(wgpu::Texture, wgpu::BindGroup, u32, u32)>,
    pub visible: bool,
    pub anchor: Anchor,
}

/// Where an overlay sits in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    TopLeft,
    BottomLeft,
    /// Top edge, centred (the session toolbar, FR-19).
    TopCenter,
    /// Centred both ways (the file manager).
    Center,
}

/// Top-left corner (physical pixels) of a `w`×`h` overlay in a `vw`×`vh`
/// window.
pub fn place(anchor: Anchor, w: f32, h: f32, vw: f32, vh: f32) -> (f32, f32) {
    match anchor {
        Anchor::TopLeft => (MARGIN, MARGIN),
        Anchor::BottomLeft => (MARGIN, vh - MARGIN - h),
        Anchor::TopCenter => (((vw - w) / 2.0).max(0.0), 0.0),
        Anchor::Center => (((vw - w) / 2.0).max(0.0), ((vh - h) / 2.0).max(0.0)),
    }
}

/// Rasterise `lines` (ASCII) at integer `scale`: white text on a
/// translucent black box. Returns RGBA, width, height.
pub fn rasterise(lines: &[String], scale: usize) -> (Vec<u8>, u32, u32) {
    let cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(1)
        .max(1);
    let (w, h) = (
        (cols * 8 + 2 * PAD) * scale,
        (lines.len().max(1) * 10 + 2 * PAD) * scale,
    );
    let mut px = vec![0u8; w * h * 4];
    for p in px.as_chunks_mut::<4>().0 {
        p.copy_from_slice(&[0, 0, 0, 170]);
    }
    for (row, line) in lines.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let glyph = font8x8::legacy::BASIC_LEGACY[(ch as usize).min(127)];
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..8 {
                    if bits >> gx & 1 == 0 {
                        continue;
                    }
                    let x0 = (PAD + col * 8 + gx) * scale;
                    let y0 = (PAD + row * 10 + gy) * scale;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let i = ((y0 + dy) * w + x0 + dx) * 4;
                            px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
            }
        }
    }
    (px, w as u32, h as u32)
}

impl Overlay {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("overlay"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay"),
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
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay rect"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            uniform,
            tex: None,
            visible: false,
            anchor: Anchor::TopLeft,
        }
    }

    /// Replace the text.
    pub fn set_text(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lines: &[String],
        scale: usize,
    ) {
        let (rgba, w, h) = rasterise(lines, scale);
        if self.tex.as_ref().is_none_or(|t| (t.2, t.3) != (w, h)) {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("overlay"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("overlay"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.tex = Some((tex, bind, w, h));
        }
        let (tex, ..) = self.tex.as_ref().expect("created");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Where it is drawn (physical pixels: x, y, w, h), if visible.
    pub fn rect(&self, vw: f32, vh: f32) -> Option<(f32, f32, f32, f32)> {
        let (_, _, w, h) = self.tex.as_ref().filter(|_| self.visible)?;
        let (x, y) = place(self.anchor, *w as f32, *h as f32, vw, vh);
        Some((x, y, *w as f32, *h as f32))
    }

    /// Record the overlay into `pass` for a `vw`×`vh` surface.
    pub fn draw(&self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass<'_>, vw: f32, vh: f32) {
        let Some((_, bind, w, h)) = &self.tex else {
            return;
        };
        if !self.visible {
            return;
        }
        let (px, py) = place(self.anchor, *w as f32, *h as f32, vw, vh);
        let x0 = -1.0 + 2.0 * px / vw;
        let y0 = 1.0 - 2.0 * py / vh;
        let rect = [x0, y0, x0 + 2.0 * *w as f32 / vw, y0 - 2.0 * *h as f32 / vh];
        let bytes: Vec<u8> = rect.iter().flat_map(|f| f.to_le_bytes()).collect();
        queue.write_buffer(&self.uniform, 0, &bytes);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterises_text() {
        let (px, w, h) = rasterise(&["Hi".to_string(), "60 fps".to_string()], 2);
        assert_eq!(
            (w, h),
            (((6 * 8 + 8) * 2) as u32, ((2 * 10 + 8) * 2) as u32)
        );
        assert!(
            px.chunks(4).any(|p| p == [255, 255, 255, 255]),
            "glyph pixels drawn"
        );
        assert!(
            px.chunks(4).any(|p| p == [0, 0, 0, 170]),
            "translucent background"
        );
    }
}
