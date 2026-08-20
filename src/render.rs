//! Offscreen wgpu renderer: uploads the atlas, renders a `DrawList` to a
//! texture with MSAA, reads the frame back as BGRA bytes.

use crate::draw::{Atlas, Blend, DrawList, Vertex};
use std::num::NonZeroU64;

const SHADER: &str = r#"
struct Screen {
    size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) color2: vec4<f32>,
    @location(4) uv: vec4<f32>,
    @location(5) aux: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) color2: vec4<f32>,
    @location(3) uv: vec4<f32>,
    @location(4) aux: vec4<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let ndc = vec2<f32>(in.pos.x / screen.size.x * 2.0 - 1.0, 1.0 - in.pos.y / screen.size.y * 2.0);
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.local = in.local;
    out.color = in.color;
    out.color2 = in.color2;
    out.uv = in.uv;
    out.aux = in.aux;
    return out;
}

fn aa(sdf: f32) -> f32 {
    return clamp(sdf / 0.75 + 0.5, 0.0, 1.0);
}

// ----- Slider body distance-field prepass (lazer PathDrawNode style) -----

struct VsOut2 {
    @builtin(position) pos: vec4<f32>,
    @location(0) start: vec2<f32>,
    @location(1) end: vec2<f32>,
    @location(2) radius: f32,
};

@vertex
fn vs_body_pre(in: VsIn) -> VsOut2 {
    var out: VsOut2;
    let ndc = vec2<f32>(in.pos.x / screen.size.x * 2.0 - 1.0, 1.0 - in.pos.y / screen.size.y * 2.0);
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.start = in.color.xy;
    out.end = in.color.zw;
    out.radius = in.aux.y;
    return out;
}

fn distToSeg(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let ab = b - a;
    let len2 = dot(ab, ab);
    var t = 0.0;
    if (len2 > 0.000001) {
        t = clamp(dot(p - a, ab) / len2, 0.0, 1.0);
    }
    let closest = a + ab * t;
    return distance(p, closest);
}

@fragment
fn fs_body_pre(in: VsOut2) -> @location(0) f32 {
    let p = vec2<f32>(in.pos.x, in.pos.y);
    return distToSeg(p, in.start, in.end) / in.radius;
}

@fragment
fn fs_body_main(in: VsOut) -> @location(0) vec4<f32> {
    // Single composite sample of the distance field: border band at the
    // rim, body colour inside, analytic AA at the outer edge, all scaled
    // by the fade alpha (no per-segment compositing).
    let uv = in.uv.xy / screen.size;
    let d = textureSampleLevel(tex, samp, uv, 0.0).r * in.aux.y;
    let r = in.aux.y;
    let b = in.aux.z;
    var col = in.color;
    if (d > r - b) {
        col = in.color2;
    }
    let a = aa(r - d) * col.a;
    return vec4<f32>(col.rgb * a, a);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let mode = in.aux.x;
    var c: vec4<f32>;

    if (mode == 0.0) {
        // Textured (atlas): sample and premultiply by total alpha so the
        // One/OneMinusSrcAlpha blend behaves like straight-alpha compositing.
        let t = textureSampleLevel(tex, samp, in.uv.xy, 0.0);
        let a = in.color.a * t.a;
        c = vec4<f32>(in.color.rgb * t.rgb * a, a);
    } else if (mode == 1.0) {
        // Ring: annulus band, outer radius aux.y, thickness aux.z.
        let d = length(in.local);
        let outer = in.aux.y;
        let inner = outer - in.aux.z;
        let sdf = min(outer - d, d - inner);
        let a = aa(sdf) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 2.0) {
        // Disc.
        let d = length(in.local);
        let a = aa(in.aux.y - d) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 3.0) {
        // Additive radial glow (gaussian-ish).
        let d = length(in.local);
        let r = in.aux.y;
        let x = clamp(d / r, 0.0, 1.0);
        let a = exp(-x * x * 4.5) * (1.0 - x * x) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 9.0) {
        // Ring-shaped glow: peaks at aux.y, falls off over aux.z both ways.
        let d = length(in.local);
        let q = (d - in.aux.y) / in.aux.z;
        let a = exp(-q * q * 4.5) * max(0.0, 1.0 - q * q) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 10.0) {
        // Framework EdgeEffect glow, Hollow = false (lazer FlashPiece):
        // alpha 1 inside aux.y, quadratic falloff ((aux.y + aux.z - d) /
        // aux.z)^2 outward (masking shader with BlendRange = aux.z and
        // AlphaExponent = 2).
        let d = length(in.local);
        let r0 = in.aux.y;
        let ext = in.aux.z;
        var f = 1.0;
        if (d > r0) {
            f = clamp((r0 + ext - d) / ext, 0.0, 1.0);
            f = f * f;
        }
        let a = f * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 4.0) {
        // Stroke band: t = aux.y in [-1..1], border for |t| > 1 - aux.z.
        let t = abs(in.aux.y);
        let portion = in.aux.z;
        var col = in.color;
        if (t > 1.0 - portion) {
            col = in.color2;
        }
        c = vec4<f32>(col.rgb * col.a, col.a);
    } else if (mode == 5.0) {
        // Capsule: local.x along the segment (half length aux.y), radius aux.z.
        let hl = in.aux.y;
        let axial = clamp(in.local.x, -hl, hl);
        let d = length(vec2<f32>(in.local.x - axial, in.local.y));
        let a = aa(in.aux.z - d) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 7.0) {
        // Arc band: radius aux.y, thickness aux.z, angles [aux.w, color2.x) rad.
        let d = length(in.local);
        let band = abs(d - in.aux.y) <= in.aux.z * 0.5 + 0.75;
        var ang = atan2(in.local.y, in.local.x);
        let a0 = in.aux.w;
        let a1 = in.color2.x;
        // Normalize angle into [a0, a0 + 2pi).
        var rel = ang - a0;
        let two_pi = 6.28318530718;
        rel = (rel - floor(rel / two_pi) * two_pi);
        let span = a1 - a0;
        let in_arc = rel <= span;
        let inside = select(0.0, 1.0, band && in_arc);
        // Angular soft edge (approximate AA at the caps).
        let ang_fade = min(1.0, min(rel, span - rel) * in.aux.y / 0.75);
        let a = inside * ang_fade * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else if (mode == 8.0) {
        // Cap disc: body fill with a radial border band at the rim (slider
        // end caps; overlapping the band seamlessly since colours match).
        let d = length(in.local);
        let r = in.aux.y;
        let b = in.aux.z;
        var col = in.color;
        if (d > r - b) {
            col = in.color2;
        }
        let a = aa(r - d) * col.a;
        c = vec4<f32>(col.rgb * a, a);
    } else {
        // Flat colour.
        c = vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }

    return c;
}
"#;

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline_alpha: wgpu::RenderPipeline,
    pipeline_additive: wgpu::RenderPipeline,
    atlas_bind: wgpu::BindGroup,
    screen_bind: wgpu::BindGroup,
    body_tex: wgpu::Texture,
    body_bind: wgpu::BindGroup,
    body_pre_pipeline: wgpu::RenderPipeline,
    body_main_pipeline: wgpu::RenderPipeline,
    /// Dedicated prepass buffers: the prepass commands must not see the
    /// scene vertex data (all queue writes land before the encoder runs).
    body_vbo: wgpu::Buffer,
    body_ibo: wgpu::Buffer,
    target: wgpu::Texture,
    msaa: wgpu::Texture,
    vbo: wgpu::Buffer,
    ibo: wgpu::Buffer,
    /// Ring of readback buffers: frames are submitted and copied into the
    /// next slot WITHOUT waiting; the oldest pending slot is mapped one or
    /// more frames later, so the GPU keeps a queue of work (no per-frame
    /// pipeline stall).
    readback_ring: Vec<wgpu::Buffer>,
    readback_next: usize,
    readback_pending: std::collections::VecDeque<usize>,
    pub width: u32,
    pub height: u32,
    pub padded_row: u32,
}

fn sample_count() -> u32 {
    if std::env::var("NO_MSAA").is_ok() { 1 } else { 4 }
}

pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    pollster::block_on(fut)
}

impl Renderer {
    pub fn new(width: u32, height: u32, atlas: &Atlas) -> Renderer {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter");

        let (device, queue) = block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("renderer"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .expect("request device");

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let screen_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("screen layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(8),
                },
                count: None,
            }],
        });

        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline layout"),
            bind_group_layouts: &[&screen_layout, &atlas_layout],
            push_constant_ranges: &[],
        });

        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 16, shader_location: 2, format: wgpu::VertexFormat::Float32x4 },
                wgpu::VertexAttribute { offset: 32, shader_location: 3, format: wgpu::VertexFormat::Float32x4 },
                wgpu::VertexAttribute { offset: 48, shader_location: 4, format: wgpu::VertexFormat::Float32x4 },
                wgpu::VertexAttribute { offset: 64, shader_location: 5, format: wgpu::VertexFormat::Float32x4 },
            ],
        };

        let make_pipeline = |blend: wgpu::BlendState, sample_count: u32| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("scene pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[vertex_layout.clone()],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: sample_count,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
                cache: None,
            })
        };

        let premult_alpha = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let premult_add = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };

        let sc = sample_count();
        let pipeline_alpha = make_pipeline(premult_alpha, sc);
        let pipeline_additive = make_pipeline(premult_add, sc);


        // Atlas texture.
        let tex_size = wgpu::Extent3d { width: atlas.width, height: atlas.height, depth_or_array_layers: 1 };
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: tex_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width * 4),
                rows_per_image: Some(atlas.height),
            },
            tex_size,
        );
        let atlas_view = atlas_tex.create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let atlas_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
            label: Some("atlas bind"),
        });

        // Slider-body distance field: R16Float, min-blended capsule SDFs.
        let body_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("body layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let body_size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let body_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("body distfield"),
            size: body_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let body_view = body_tex.create_view(&Default::default());
        let body_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("body sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let body_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &body_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&body_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&body_sampler) },
            ],
            label: Some("body bind"),
        });

        let empty_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("empty layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let body_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("body main layout"),
            bind_group_layouts: &[&screen_layout, &body_layout],
            push_constant_ranges: &[],
        });

        let min_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Min,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Min,
            },
        };

        let prepass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("prepass layout"),
            bind_group_layouts: &[&screen_layout],
            push_constant_ranges: &[],
        });
        let body_pre_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("body prepass"),
            layout: Some(&prepass_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_body_pre"),
                compilation_options: Default::default(),
                buffers: &[vertex_layout.clone()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_body_pre"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::R16Float,
                    blend: Some(min_blend),
                    write_mask: wgpu::ColorWrites::RED,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let body_main_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("body main"),
            layout: Some(&body_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[vertex_layout.clone()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_body_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(premult_alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState { count: sc, mask: !0, alpha_to_coverage_enabled: false },
            multiview: None,
            cache: None,
        });

        let screen_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screen uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let screen_data: [f32; 2] = [width as f32, height as f32];
        queue.write_buffer(&screen_buf, 0, cast_slice(&screen_data));
        let screen_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &screen_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &screen_buf,
                    offset: 0,
                    size: Some(NonZeroU64::new(8).unwrap()),
                }),
            }],
            label: Some("screen bind"),
        });

        // Offscreen target + MSAA texture.
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let msaa = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa"),
            size,
            mip_level_count: 1,
            sample_count: sc,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        let vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vbo"),
            size: 4 << 20,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let ibo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ibo"),
            size: 8 << 20,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let body_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("body vbo"),
            size: 4 << 20,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let body_ibo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("body ibo"),
            size: 8 << 20,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT; // 256
        let padded_row = ((width * 4 + align - 1) / align) * align;

        let readback_ring: Vec<wgpu::Buffer> = (0..3)
            .map(|i| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("readback {i}")),
                    size: (padded_row * height) as u64,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();

        Renderer {
            device,
            queue,
            body_tex,
            body_bind,
            body_pre_pipeline,
            body_main_pipeline,
            body_vbo,
            body_ibo,
            pipeline_alpha,
            pipeline_additive,
            atlas_bind,
            screen_bind,
            target,
            msaa,
            vbo,
            ibo,
            readback_ring,
            readback_next: 0,
            readback_pending: std::collections::VecDeque::new(),
            width,
            height,
            padded_row,
        }
    }

    /// Renders one frame and returns the BGRA pixels (with row padding).
    /// Synchronous convenience (PNG mode); the ffmpeg path uses
    /// `render_deferred` + `read_oldest` for pipelining.
    pub fn render(&mut self, list: &DrawList, clear: [f64; 4]) -> Vec<u8> {
        let encoder = self.encode_scene(list, clear);
        self.submit_frame_with(encoder);
        self.read_oldest()
    }

    /// Builds the full scene command encoder (slider-body prepasses +
    /// composites + scene runs + MSAA resolve).
    fn encode_scene(&mut self, list: &DrawList, clear: [f64; 4]) -> wgpu::CommandEncoder {
        let vbytes = list.vertices.len() * std::mem::size_of::<Vertex>();
        let ibytes = list.indices.len() * 4;
        assert!(vbytes as u64 <= self.vbo.size(), "vertex buffer overflow: {}", vbytes);
        assert!(ibytes as u64 <= self.ibo.size(), "index buffer overflow: {}", ibytes);

        let use_msaa = sample_count() > 1;

        // ---- Slider bodies (lazer PathDrawNode style) --------------------
        // Per body: pass A min-blends that body's capsule-segment SDFs into
        // an R16Float field (cleared far beyond 1.0 so pixels covered by no
        // segment quad - e.g. corners of the body's AABB - stay transparent
        // instead of reading as the capsule edge). Pass B composites one quad
        // sampling the field (border band at the rim, body fill inside,
        // analytic AA), drawn under all scene elements. Each body gets its
        // own field pass so overlapping bodies never share distance values.
        let mut body_quads: Vec<Vertex> = Vec::new();
        let mut body_indices: Vec<u32> = Vec::new();
        let mut prepass_verts: Vec<Vertex> = Vec::new();
        let mut prepass_indices: Vec<u32> = Vec::new();
        // Index range of each body's segments inside the prepass buffers.
        let mut prepass_ranges: Vec<(u32, u32)> = Vec::new();
        for body in &list.bodies {
            let pad = body.radius + 1.5;
            let mut minx = f32::MAX;
            let mut miny = f32::MAX;
            let mut maxx = f32::MIN;
            let mut maxy = f32::MIN;
            let start = prepass_indices.len() as u32;
            for (a, b) in &body.segments {
                minx = minx.min(a[0] - pad).min(b[0] - pad);
                miny = miny.min(a[1] - pad).min(b[1] - pad);
                maxx = maxx.max(a[0] + pad).max(b[0] + pad);
                maxy = maxy.max(a[1] + pad).max(b[1] + pad);

                let base = prepass_verts.len() as u32;
                for corner in [
                    [a[0].min(b[0]) - pad, a[1].min(b[1]) - pad],
                    [a[0].max(b[0]) + pad, a[1].min(b[1]) - pad],
                    [a[0].max(b[0]) + pad, a[1].max(b[1]) + pad],
                    [a[0].min(b[0]) - pad, a[1].max(b[1]) + pad],
                ] {
                    prepass_verts.push(Vertex {
                        pos: corner,
                        local: [0.0; 2],
                        color: [a[0], a[1], b[0], b[1]],
                        color2: [0.0; 4],
                        uv: [0.0; 4],
                        aux: [crate::draw::MODE_CAPSULE, body.radius, 0.0, 0.0],
                    });
                }
                prepass_indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
            prepass_ranges.push((start, prepass_indices.len() as u32));

            let base = body_quads.len() as u32;
            for corner in [[minx, miny], [maxx, miny], [maxx, maxy], [minx, maxy]] {
                body_quads.push(Vertex {
                    pos: corner,
                    local: [0.0; 2],
                    color: [body.body.r, body.body.g, body.body.b, body.body.a],
                    color2: [body.border_colour.r, body.border_colour.g, body.border_colour.b, body.border_colour.a],
                    uv: [corner[0], corner[1], 0.0, 0.0],
                    aux: [crate::draw::MODE_CAPSULE, body.radius, body.border, 0.0],
                });
            }
            body_indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        let pre_vbytes = prepass_verts.len() * std::mem::size_of::<Vertex>();
        let pre_ibytes = prepass_indices.len() * 4;
        assert!(pre_vbytes as u64 <= self.body_vbo.size(), "body vertex buffer overflow: {}", pre_vbytes);
        assert!(pre_ibytes as u64 <= self.body_ibo.size(), "body index buffer overflow: {}", pre_ibytes);

        // ---- Scene geometry (body composites prepended) ------------------
        let mut all_verts = body_quads.clone();
        let mut all_idx = body_indices.clone();
        all_verts.extend_from_slice(&list.vertices);
        for i in &list.indices {
            all_idx.push(*i + body_quads.len() as u32);
        }
        self.queue.write_buffer(&self.body_vbo, 0, cast_slice(&prepass_verts));
        self.queue.write_buffer(&self.body_ibo, 0, cast_slice(&prepass_indices));
        self.queue.write_buffer(&self.vbo, 0, cast_slice(&all_verts));
        self.queue.write_buffer(&self.ibo, 0, cast_slice(&all_idx));

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });

        let target_view = self.target.create_view(&Default::default());
        let msaa_view = self.msaa.create_view(&Default::default());
        let clear_color = wgpu::Color { r: clear[0], g: clear[1], b: clear[2], a: clear[3] };

        let has_bodies = !list.bodies.is_empty();
        let body_view = self.body_tex.create_view(&Default::default());
        if has_bodies {
            // Clear the scene target first; the passes below must load.
            drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: if use_msaa { &msaa_view } else { &target_view },
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            }));

            // Interleave scene runs and body composites in draw order: each
            // body's composite lands where its slider sits in the object
            // ordering (lazer layers whole sliders by start time, so an
            // earlier slider's body covers later objects). Body marks split
            // any run that spans them.
            #[derive(Clone, Copy)]
            enum Op {
                Run(Blend, u32, u32),
                Body(usize),
            }
            let marks = &list.body_marks;
            let mut ops: Vec<Op> = Vec::new();
            let mut mi = 0usize;
            for &(blend, off, cnt) in &list.runs {
                let mut start = off;
                while mi < marks.len() && marks[mi].0 <= off {
                    ops.push(Op::Body(marks[mi].1));
                    mi += 1;
                }
                while mi < marks.len() && marks[mi].0 < off + cnt {
                    let (key, bi) = marks[mi];
                    if key > start {
                        ops.push(Op::Run(blend, start, key - start));
                    }
                    ops.push(Op::Body(bi));
                    start = key;
                    mi += 1;
                }
                if start < off + cnt {
                    ops.push(Op::Run(blend, start, off + cnt - start));
                }
            }
            while mi < marks.len() {
                ops.push(Op::Body(marks[mi].1));
                mi += 1;
            }

            // Group consecutive runs so each render pass is opened and
            // dropped within one statement (wgpu pass borrows the encoder).
            enum Seg {
                Runs(Vec<(Blend, u32, u32)>),
                Body(usize),
            }
            let mut segs: Vec<Seg> = Vec::new();
            for op in ops {
                match op {
                    Op::Run(blend, off, cnt) => match segs.last_mut() {
                        Some(Seg::Runs(runs)) => runs.push((blend, off, cnt)),
                        _ => segs.push(Seg::Runs(vec![(blend, off, cnt)])),
                    },
                    Op::Body(bi) => segs.push(Seg::Body(bi)),
                }
            }

            let base = body_indices.len() as u32;
            for seg in &segs {
                match seg {
                    Seg::Runs(runs) => {
                        let mut p = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("scene segment"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: if use_msaa { &msaa_view } else { &target_view },
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        p.set_bind_group(0, &self.screen_bind, &[]);
                        p.set_bind_group(1, &self.atlas_bind, &[]);
                        p.set_vertex_buffer(0, self.vbo.slice(..));
                        p.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);
                        for &(blend, off, cnt) in runs {
                            let pipeline = match blend {
                                Blend::Alpha => &self.pipeline_alpha,
                                Blend::Additive => &self.pipeline_additive,
                            };
                            p.set_pipeline(pipeline);
                            let o = off + base;
                            p.draw_indexed(o..(o + cnt), 0, 0..1);
                        }
                    }
                    Seg::Body(bi) => {
                        let (start, end) = prepass_ranges[*bi];
                        // Pass A: this body's segment SDFs, min-blended.
                        {
                            let mut pre = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("body prepass"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &body_view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        // Far above any real d/r (<= ~1.02
                                        // inside the segment quads): uncovered
                                        // pixels composite to zero alpha.
                                        load: wgpu::LoadOp::Clear(wgpu::Color {
                                            r: 256.0,
                                            g: 256.0,
                                            b: 256.0,
                                            a: 256.0,
                                        }),
                                        store: wgpu::StoreOp::Store,
                                    },
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                            });
                            pre.set_pipeline(&self.body_pre_pipeline);
                            pre.set_bind_group(0, &self.screen_bind, &[]);
                            pre.set_vertex_buffer(0, self.body_vbo.slice(..));
                            pre.set_index_buffer(self.body_ibo.slice(..), wgpu::IndexFormat::Uint32);
                            pre.draw_indexed(start..end, 0, 0..1);
                        }
                        // Pass B: composite this body over the scene.
                        {
                            let mut comp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("body composite"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: if use_msaa { &msaa_view } else { &target_view },
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Load,
                                        store: wgpu::StoreOp::Store,
                                    },
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                            });
                            comp.set_pipeline(&self.body_main_pipeline);
                            comp.set_bind_group(0, &self.screen_bind, &[]);
                            comp.set_bind_group(1, &self.body_bind, &[]);
                            comp.set_vertex_buffer(0, self.vbo.slice(..));
                            comp.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);
                            comp.draw_indexed((*bi * 6) as u32..(*bi * 6 + 6) as u32, 0, 0..1);
                        }
                    }
                }
            }

            if use_msaa {
                // Resolve the accumulated multisampled frame into the target.
                drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("resolve pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &msaa_view,
                        resolve_target: Some(&target_view),
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                }));
            }
        }

        if !has_bodies {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: if use_msaa { &msaa_view } else { &target_view },
                    resolve_target: if use_msaa { Some(&target_view) } else { None },
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            rpass.set_bind_group(0, &self.screen_bind, &[]);
            rpass.set_vertex_buffer(0, self.vbo.slice(..));
            rpass.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);

            rpass.set_bind_group(1, &self.atlas_bind, &[]);

            for (blend, offset, count) in &list.runs {
                let pipeline = match blend {
                    Blend::Alpha => &self.pipeline_alpha,
                    Blend::Additive => &self.pipeline_additive,
                };
                rpass.set_pipeline(pipeline);
                let off = offset + body_indices.len() as u32;
                rpass.draw_indexed(off..(off + *count), 0, 0..1);
            }
        }

        encoder
    }

    /// Renders one frame and SUBMITS it without waiting for the GPU; the
    /// copied-out frame data is retrieved later with `read_oldest` once
    /// `pending_len` frames are in flight.
    pub fn render_deferred(&mut self, list: &DrawList, clear: [f64; 4]) {
        let encoder = self.encode_scene(list, clear);
        self.submit_frame_with(encoder);
    }

    /// Number of submitted-but-not-yet-read frames.
    pub fn pending_len(&self) -> usize {
        self.readback_pending.len()
    }

    fn submit_frame_with(&mut self, mut encoder: wgpu::CommandEncoder) {
        let slot = self.readback_next;
        self.readback_next = (self.readback_next + 1) % self.readback_ring.len();
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback_ring[slot],
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));
        self.readback_pending.push_back(slot);
    }

    /// Maps and returns the OLDEST pending frame. With a frame or two of GPU
    /// work already queued this returns almost immediately; the GPU never
    /// starves while the CPU builds the next frame.
    pub fn read_oldest(&mut self) -> Vec<u8> {
        let slot = self
            .readback_pending
            .pop_front()
            .expect("read_oldest with empty pipeline");
        let buffer = &self.readback_ring[slot];
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::Maintain::Wait);
        let data = slice.get_mapped_range().to_vec();
        buffer.unmap();
        data
    }
}

// Minimal bytemuck-style cast (avoids the extra dependency).
unsafe trait Pod {}
unsafe impl Pod for Vertex {}
unsafe impl Pod for u32 {}
unsafe impl Pod for f32 {}

fn cast_slice<T: Pod>(data: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}
