//! Offscreen wgpu renderer: uploads the atlas, renders a `DrawList` to a
//! texture without MSAA, then either read back BGRA/NV12 or present it
//! straight to a window surface.

use crate::draw::{Atlas, Blend, DrawList, Vertex};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

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

struct VsOut2 {
    @builtin(position) pos: vec4<f32>,
    @location(0) start: vec2<f32>,
    @location(1) end: vec2<f32>,
    @location(2) radius: f32,
};

struct PrepassIn {
    @location(0) pos: vec2<f32>,
    @location(1) start: vec2<f32>,
    @location(2) end: vec2<f32>,
    @location(3) radius: f32,
};

@vertex
fn vs_body_pre(in: PrepassIn) -> VsOut2 {
    var out: VsOut2;
    let ndc = vec2<f32>(in.pos.x / screen.size.x * 2.0 - 1.0, 1.0 - in.pos.y / screen.size.y * 2.0);
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.start = in.start;
    out.end = in.end;
    out.radius = in.radius;
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
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let mode = in.aux.x;
    var c: vec4<f32>;

    if (mode == 0.0) {
        // Atlas RGB is premultiplied at pack time. osu-framework's
        // `sh_Texture.fs` outputs straight `v_Colour * texel`, then
        // `BlendingParameters.Mixture` / `Additive` multiply RGB by
        // SrcAlpha in the blend unit. Doing that multiply after a linear
        // sample lets alpha-0 texels whose RGB is still white bleed into
        // the colour (cursor trails). Premultiply first, then scale only
        // by the vertex alpha — one multiply, same as SrcAlpha on a
        // straight sample that was not contaminated.
        let t = textureSampleLevel(tex, samp, in.uv.xy, 0.0);
        let a = in.color.a * t.a;
        c = vec4<f32>(in.color.rgb * t.rgb * in.color.a, a);
    } else if (mode == 12.0) {
        // Premultiplied atlas region (storyboard slot). RGB is already
        // coverage-weighted. Additive sprites add RGB and leave alpha at
        // 0, so multiplying RGB by alpha again would erase the glow and
        // a saturated alpha would hide the dimmed background.
        let t = textureSampleLevel(tex, samp, in.uv.xy, 0.0);
        c = vec4<f32>(in.color.rgb * t.rgb, in.color.a * t.a);
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
        // Border/body seam gets the same analytic 0.75px feather as the
        // outer aa() - the hard switch staircased on curved caps.
        let d = length(in.local);
        let r = in.aux.y;
        let b = in.aux.z;
        let seam = clamp((d - (r - b)) / max(min(0.75, b), 1e-3) + 0.5, 0.0, 1.0);
        let col = mix(in.color, in.color2, seam);
        let a = aa(r - d) * col.a;
        c = vec4<f32>(col.rgb * a, a);
    } else if (mode == 11.0) {
        // Rounded rectangle: aux.y = corner radius, colour2.xy = half
        // extents. Corner colours interpolate into the vertical gradient.
        let half_ = in.color2.xy;
        let r = min(in.aux.y, min(half_.x, half_.y));
        let q = abs(in.local) - (half_ - vec2<f32>(r));
        let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        let a = aa(-d) * in.color.a;
        c = vec4<f32>(in.color.rgb * a, a);
    } else {
        // Flat colour.
        c = vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }

    return c;
}
"#;

const BODY_LAYER_SHADER: &str = r#"
struct Screen {
    size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var tex: texture_2d_array<f32>;
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

@fragment
fn fs_body_layer(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv.xy / screen.size;
    let layer = i32(in.uv.w + 0.5);
    let d = textureSampleLevel(tex, samp, uv, layer, 0.0).r * in.aux.y;
    let r = in.aux.y;
    let b = in.aux.z;

    if (in.aux.w > 0.5) {
        let position = clamp(1.0 - d / r, 0.0, 1.0);
        let SHADOW = 0.078125;
        let BORDER = 0.1875;
        let px = 0.75 / r;
        let ramp_a = 0.25 * clamp(position / SHADOW, 0.0, 1.0) * in.color2.a;
        let shadow_col = vec4<f32>(0.0, 0.0, 0.0, ramp_a);
        let w1 = smoothstep(SHADOW - px, SHADOW + px, position);
        let t = clamp((position - BORDER) / (1.0 - BORDER), 0.0, 1.0);
        let inner = vec3<f32>(in.local.x, in.local.y, in.uv.z);
        let grad_col = vec4<f32>(mix(in.color.rgb, inner, t), in.color.a);
        let w2 = smoothstep(BORDER - px, BORDER + px, position);
        let col = mix(mix(shadow_col, in.color2, w1), grad_col, w2);
        let aa_a = aa(r - d) * col.a;
        return vec4<f32>(col.rgb * aa_a, aa_a);
    }

    let seam = clamp((d - (r - b)) / max(min(0.75, b), 1e-3) + 0.5, 0.0, 1.0);
    let col = mix(in.color, in.color2, seam);
    let alpha = aa(r - d) * col.a;
    return vec4<f32>(col.rgb * alpha, alpha);
}
"#;

/// RGB → YUV420 (BT.601 limited range) on the GPU. One Y invocation packs
/// 4 horizontal luma bytes into one storage word; one chroma invocation
/// packs a full storage word (or two for NV12), so no two invocations ever
/// touch the same word — no atomics needed. Requires width % 8 == 0 and
/// even height (word alignment of every plane row); callers fall back to a
/// CPU conversion otherwise.
const YUV_SHADER: &str = r#"
struct Params {
    size: vec2<u32>,
    // Output plane dimensions (width rounded up to 8, height even).
    padded: vec2<u32>,
};
@group(0) @binding(0) var<uniform> params: Params;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(2) @binding(0) var<storage, read_write> out: array<u32>;

fn luma_y(c: vec3<f32>) -> u32 {
    let y = 16.0 + 219.0 * dot(c, vec3<f32>(0.299, 0.587, 0.114));
    return u32(clamp(y, 16.0, 235.0) + 0.5);
}

fn chroma_uv(c: vec3<f32>) -> vec2<u32> {
    let cb = 128.0 + 224.0 * dot(c, vec3<f32>(-0.168736, -0.331264, 0.5));
    let cr = 128.0 + 224.0 * dot(c, vec3<f32>(0.5, -0.418688, -0.081312));
    return vec2<u32>(u32(clamp(cb, 16.0, 240.0) + 0.5), u32(clamp(cr, 16.0, 240.0) + 0.5));
}

fn load_rgb(x: u32, y: u32, w: u32, h: u32) -> vec3<f32> {
    if (x >= w || y >= h) {
        return vec3<f32>(0.0);
    }
    return textureLoad(tex, vec2<i32>(i32(x), i32(y)), 0).rgb;
}

fn block_avg(x: u32, y: u32, w: u32, h: u32) -> vec3<f32> {
    var sum = vec3<f32>(0.0, 0.0, 0.0);
    for (var dy = 0u; dy < 2u; dy = dy + 1u) {
        for (var dx = 0u; dx < 2u; dx = dx + 1u) {
            sum = sum + load_rgb(x + dx, y + dy, w, h);
        }
    }
    return sum * 0.25;
}

@compute @workgroup_size(8, 4)
fn cs_y(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = params.size.x;
    let h = params.size.y;
    let pw = params.padded.x;
    let ph = params.padded.y;
    let x0 = gid.x * 4u;
    let y = gid.y;
    if (x0 >= pw || y >= ph) { return; }
    var word = 0u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let c = load_rgb(x0 + i, y, w, h);
        word = word | (luma_y(c) << (8u * i));
    }
    out[y * (pw / 4u) + gid.x] = word;
}

// NV12: UV pairs interleaved after the Y plane, one pair per 2x2 block.
@compute @workgroup_size(8, 4)
fn cs_uv_nv12(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = params.size.x;
    let h = params.size.y;
    let pw = params.padded.x;
    let ph = params.padded.y;
    let cw = pw / 2u;
    let ch = ph / 2u;
    let cx0 = gid.x * 4u;
    let cy = gid.y;
    if (cx0 >= cw || cy >= ch) { return; }
    var words = array<u32, 2>(0u, 0u);
    for (var i = 0u; i < 4u; i = i + 1u) {
        let uv = chroma_uv(block_avg((cx0 + i) * 2u, cy * 2u, w, h));
        let j = i / 2u;
        let s = (i % 2u) * 16u;
        words[j] = words[j] | (uv.x << s) | (uv.y << (s + 8u));
    }
    let base = (pw * ph + cy * pw + cx0 * 2u) / 4u;
    out[base] = words[0];
    out[base + 1u] = words[1];
}

// I420: U plane then V plane after the Y plane.
@compute @workgroup_size(8, 4)
fn cs_uv_i420(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = params.size.x;
    let h = params.size.y;
    let pw = params.padded.x;
    let ph = params.padded.y;
    let cw = pw / 2u;
    let ch = ph / 2u;
    let cx0 = gid.x * 4u;
    let cy = gid.y;
    if (cx0 >= cw || cy >= ch) { return; }
    var wu = 0u;
    var wv = 0u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let uv = chroma_uv(block_avg((cx0 + i) * 2u, cy * 2u, w, h));
        wu = wu | (uv.x << (8u * i));
        wv = wv | (uv.y << (8u * i));
    }
    let u_base = (pw * ph + cy * cw + cx0) / 4u;
    let v_base = (pw * ph + cw * ch + cy * cw + cx0) / 4u;
    out[u_base] = wu;
    out[v_base] = wv;
}
"#;

struct YuvPass {
    pipeline_y: wgpu::ComputePipeline,
    pipeline_uv_nv12: wgpu::ComputePipeline,
    pipeline_uv_i420: wgpu::ComputePipeline,
    params_bind: wgpu::BindGroup,
    tex_bind: wgpu::BindGroup,
    out_bind: wgpu::BindGroup,
    out_buf: wgpu::Buffer,
    ring: Vec<Arc<wgpu::Buffer>>,
    ring_free: std::collections::VecDeque<usize>,
    pending: std::collections::VecDeque<(usize, wgpu::SubmissionIndex, bool)>,
    /// Actual packed NV12/I420 bytes (w*h*3/2).
    bytes: u32,
    /// Padded GPU buffer/copy size (padded_w*height*3/2).
    padded_bytes: u32,
    padded_width: u32,
    groups_y: (u32, u32),
    groups_uv: (u32, u32),
}

/// Which ring a readback frame belongs to. The writer thread returns the
/// slot to the matching free list after unmapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadbackKind {
    Bgra,
    Yuv,
}

/// One GPU frame handed to the writer thread. The writer maps the buffer,
/// waits only for the submission that filled it, writes it to ffmpeg, then
/// unmaps and returns the slot. No full-frame CPU staging copy.
pub struct ReadbackFrame {
    pub buffer: Arc<wgpu::Buffer>,
    pub submission: wgpu::SubmissionIndex,
    pub slot: usize,
    pub kind: ReadbackKind,
    pub width: u32,
    pub height: u32,
    /// BGRA row stride in bytes (0 for packed YUV).
    pub stride: u32,
    /// Bytes to write (BGRA: stride*height; YUV: w*h*3/2).
    pub bytes: usize,
    /// YUV only: true = NV12, false = I420.
    pub interleaved: bool,
}

pub struct Renderer {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline_alpha: wgpu::RenderPipeline,
    pipeline_additive: wgpu::RenderPipeline,
    body_pre_pipeline: wgpu::RenderPipeline,
    body_layer_pipeline: wgpu::RenderPipeline,
    body_layer_layout: wgpu::BindGroupLayout,
    body_layer_sampler: wgpu::Sampler,
    body_layers: Option<BodyLayerCache>,
    atlas_bind: wgpu::BindGroup,
    atlas_layout: wgpu::BindGroupLayout,
    atlas_sampler: wgpu::Sampler,
    /// The live atlas GPU texture (kept for the `atlas_view` accessor —
    /// the storyboard layer renders its composite directly into the
    /// atlas slots each frame).
    atlas_tex: wgpu::Texture,
    /// Cached view of [`atlas_tex`]: storyboard composite render target.
    atlas_view: wgpu::TextureView,
    screen_bind: wgpu::BindGroup,
    /// Dedicated prepass vertex/index buffers (must not alias the scene VBO).
    body_vbo: wgpu::Buffer,
    body_ibo: wgpu::Buffer,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    vbo: wgpu::Buffer,
    ibo: wgpu::Buffer,
    /// Ring of readback buffers: frames are submitted and copied into the
    /// next slot WITHOUT waiting; the oldest pending slot is mapped one or
    /// more frames later, so the GPU keeps a queue of work (no per-frame
    /// pipeline stall).
    readback_ring: Vec<Arc<wgpu::Buffer>>,
    readback_free: std::collections::VecDeque<usize>,
    readback_pending: std::collections::VecDeque<(usize, wgpu::SubmissionIndex)>,
    /// Async export: every submitted frame is handed to the writer thread
    /// immediately; `None` keeps the synchronous `read_oldest_into` path
    /// (PNG / tests).
    readback_tx: Option<SyncSender<ReadbackFrame>>,
    readback_return: Option<Receiver<(ReadbackKind, usize)>>,
    /// Lazy GPU RGB→YUV420 conversion (see [`YuvPass`]); None until the
    /// first `render_deferred_yuv`.
    yuv: Option<YuvPass>,
    pub width: u32,
    pub height: u32,
    pub padded_row: u32,
}


/// Compact slider-body prepass vertex: 28 bytes (the fat 80-byte scene
/// Vertex wastes 60+ bytes per prepass corner).
#[repr(C)]
#[derive(Clone, Copy)]
struct PrepassVertex {
    pos: [f32; 2],
    start: [f32; 2],
    end: [f32; 2],
    radius: f32,
}

struct BodyLayerCache {
    layers: u32,
    layer_views: Vec<wgpu::TextureView>,
    bind: wgpu::BindGroup,
}

pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    pollster::block_on(fut)
}

impl Renderer {
    pub fn new(width: u32, height: u32, atlas: &Atlas) -> Renderer {
        // 与 surface.rs 同理:尊重 WGPU_BACKEND(default() 会无视环境变量)
        let mut descriptor = wgpu::InstanceDescriptor::default();
        descriptor.backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
        let instance = wgpu::Instance::new(&descriptor);
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter");
        Self::from_adapter(adapter, width, height, atlas)
    }

    /// Device + queue on a caller-provided adapter. The caller must obtain
    /// the adapter from the SAME `wgpu::Instance` that created any surface
    /// it intends to present to (a surface id from another instance is
    /// invalid and panics inside wgpu-core).

    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The adapter's 2D texture dimension limit, clamped to 16384 (a
    /// fully-used 16384² RGBA atlas would be 1 GiB of VRAM). Call before
    /// `build_atlas` so huge skins pack at native sprite size; devices that
    /// only reach 8192 (GLES-compat floor) keep the uniform downscale
    /// fallback inside `build_atlas`.
    pub fn probe_max_texture_dimension_2d() -> u32 {
        let mut descriptor = wgpu::InstanceDescriptor::default();
        descriptor.backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all());
        let instance = wgpu::Instance::new(&descriptor);
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter");
        adapter.limits().max_texture_dimension_2d.min(16384)
    }

    /// 诊断用一行 GPU/后端描述:后端、适配器名、vendor/device ID 与
    /// 驱动版本(日志记录用)。
    pub fn gpu_info(&self) -> String {
        let i = self.adapter.get_info();
        format!(
            "{:?} · {} · vendor {:#06x} device {:#06x} · driver {} ({})",
            i.backend, i.name, i.vendor, i.device, i.driver, i.driver_info
        )
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// 热替换图集(背景图开关时用):重建纹理并上传,重绑 bind group,
    /// 管线/设备/窗口全部保留——避免整套 wgpu 初始化重付。
    pub fn set_atlas(&mut self, atlas: &Atlas) {
        let tex_size = wgpu::Extent3d { width: atlas.width, height: atlas.height, depth_or_array_layers: 1 };
        let atlas_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: tex_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            // RENDER_ATTACHMENT:storyboard 合成层每帧直接渲进图集槽位
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.queue.write_texture(
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
        let view = atlas_tex.create_view(&Default::default());
        self.atlas_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.atlas_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.atlas_sampler) },
            ],
            label: Some("atlas bind"),
        });
        self.atlas_view = view;
        self.atlas_tex = atlas_tex;
    }

    /// The live atlas GPU view: the storyboard composite render target
    /// (regions `Region::Storyboard` / `StoryboardForeground` are drawn
    /// into directly each frame).
    pub fn atlas_view(&self) -> &wgpu::TextureView {
        &self.atlas_view
    }

    pub fn target_view(&self) -> wgpu::TextureView {
        self.target.create_view(&Default::default())
    }

    pub fn from_adapter(
        adapter: wgpu::Adapter,
        width: u32,
        height: u32,
        atlas: &Atlas,
    ) -> Renderer {
        Self::from_adapter_with_readback(adapter, width, height, atlas, true)
    }

    /// `readback = false` skips the staging ring entirely (window/preview
    /// hosts that present directly and never map pixels).
    pub fn from_adapter_with_readback(
        adapter: wgpu::Adapter,
        width: u32,
        height: u32,
        atlas: &Atlas,
        readback: bool,
    ) -> Renderer {
        // The atlas may be packed past `Limits::default()`'s 8192 2D-texture
        // cap (see `probe_max_texture_dimension_2d`); requesting up to the
        // adapter's own limit is always supported.
        let required_limits = wgpu::Limits {
            max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d.min(16384),
            ..wgpu::Limits::default()
        };
        // Anime4K 超分链把 R32F 中间纹理按可过滤采样绑定:适配器支持时
        // 开启 FLOAT32_FILTERABLE(视频超分与渲染共用本设备)。
        let mut features = wgpu::Features::empty();
        if adapter.features().contains(wgpu::Features::FLOAT32_FILTERABLE) {
            features |= wgpu::Features::FLOAT32_FILTERABLE;
        }
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("renderer"),
            required_features: features,
            required_limits,
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("request device");
        // wgpu 错误捕获:默认 uncaptured handler 直接 panic 杀进程,具体
        // 错误只随 panic 打到无处可看的 stderr。这里改为记录(宿主日志
        // 文件可见)并继续 —— 壁纸进程不因单次 GPU 错误死亡,错误细节
        // 供定位。
        device.on_uncaptured_error(Box::new(|e| {
            log::error!("wgpu 未捕获错误(不致命): {e}");
            eprintln!("wgpu uncaptured error: {e}");
        }));

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
                    min_binding_size: NonZeroU64::new(16),
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

        let make_pipeline = |blend: wgpu::BlendState| {
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
                    count: 1,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
                cache: None,
            })
        };

        // Colour factors are the premultiplied form of osu-framework
        // `BlendingParameters`. Mixture is (SrcAlpha, OneMinusSrcAlpha)
        // and Additive is (SrcAlpha, One); both use alpha (One, One).
        // Output RGB is already coverage-weighted, so SrcAlpha becomes One.
        let premult_alpha = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
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

        let pipeline_alpha = make_pipeline(premult_alpha);
        let pipeline_additive = make_pipeline(premult_add);

        let body_layer_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("body layer layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
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
        let body_layer_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("body layer shader"),
            source: wgpu::ShaderSource::Wgsl(BODY_LAYER_SHADER.into()),
        });
        let body_layer_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("body layer pipeline"),
            bind_group_layouts: &[&screen_layout, &body_layer_layout],
            push_constant_ranges: &[],
        });
        let body_layer_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("body layer"),
            layout: Some(&body_layer_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &body_layer_module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[vertex_layout.clone()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &body_layer_module,
                entry_point: Some("fs_body_layer"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(premult_alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let body_layer_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("body layer sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
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
        let prepass_vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<PrepassVertex>() as u64, // 28
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute { offset: 0, shader_location: 0, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 8, shader_location: 1, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 16, shader_location: 2, format: wgpu::VertexFormat::Float32x2 },
                wgpu::VertexAttribute { offset: 24, shader_location: 3, format: wgpu::VertexFormat::Float32 },
            ],
        };
        let body_pre_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("body prepass"),
            layout: Some(&prepass_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_body_pre"),
                compilation_options: Default::default(),
                buffers: &[prepass_vertex_layout.clone()],
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
                    size: Some(NonZeroU64::new(16).unwrap()),
                }),
            }],
            label: Some("screen bind"),
        });

        // Offscreen scene target (also used by the YUV pass and PNG readback).
        // Atlas texture.
        let tex_size = wgpu::Extent3d { width: atlas.width, height: atlas.height, depth_or_array_layers: 1 };
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: tex_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            // RENDER_ATTACHMENT:storyboard 合成层每帧直接渲进图集槽位
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
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

        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());


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
            // 16 MiB ÷ (4 verts × 28 B) ≈ 149k slider segments per frame;
            // the observed dense-map spike was ~15k.
            size: 16 << 20,
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

        // NO_READBACK(壁纸等无回读宿主):跳过 3×帧大小的 MAP_READ 暂存堆
        //(2560×1440 下约 44MB 提交内存,壁纸端从不回读,纯浪费)。
        let readback_ring: Vec<Arc<wgpu::Buffer>> =
            if !readback || std::env::var_os("NO_READBACK").is_some() {
                Vec::new()
            } else {
                (0..3)
                    .map(|i| {
                        Arc::new(device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some(&format!("readback {i}")),
                            size: (padded_row * height) as u64,
                            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        }))
                    })
                    .collect()
            };
        let readback_free = (0..readback_ring.len()).collect();

        Renderer {
            adapter,
            device,
            queue,
            pipeline_alpha,
            pipeline_additive,
            body_pre_pipeline,
            body_layer_pipeline,
            body_layer_layout,
            body_layer_sampler,
            body_layers: None,
            atlas_bind,
            atlas_layout,
            atlas_sampler: sampler,
            atlas_tex,
            atlas_view,
            screen_bind,
            body_vbo,
            body_ibo,
            target,
            target_view,
            vbo,
            ibo,
            readback_ring,
            readback_free,
            readback_pending: std::collections::VecDeque::new(),
            readback_tx: None,
            readback_return: None,
            yuv: None,
            width,
            height,
            padded_row,
        }
    }

    /// Renders one frame and returns the BGRA pixels (with row padding).
    /// Synchronous convenience (PNG mode); the ffmpeg path uses
    /// `render_deferred` + `read_oldest_into` for pipelining.
    pub fn render(&mut self, list: &DrawList, clear: [f64; 4]) -> Vec<u8> {
        let encoder = self.encode_scene(list, clear);
        self.submit_frame_with(encoder);
        let mut out = Vec::new();
        self.read_oldest_into(&mut out);
        out
    }


    /// One color pass. Slider bodies are capsules, not a distance-field
    /// prepass, so a phone GPU never reloads a half-drawn frame.
    fn encode_scene_only_into(
        &mut self,
        list: &DrawList,
        clear: [f64; 4],
        target_view: &wgpu::TextureView,
    ) -> wgpu::CommandEncoder {
        self.ensure_mesh(list.vertices.len(), list.indices.len());
        self.queue.write_buffer(&self.vbo, 0, cast_slice(&list.vertices));
        self.queue.write_buffer(&self.ibo, 0, cast_slice(&list.indices));
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        let clear_color = wgpu::Color { r: clear[0], g: clear[1], b: clear[2], a: clear[3] };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &self.screen_bind, &[]);
        pass.set_bind_group(1, &self.atlas_bind, &[]);
        pass.set_vertex_buffer(0, self.vbo.slice(..));
        pass.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);
        for (blend, offset, count) in &list.runs {
            let pipeline = match blend {
                Blend::Alpha => &self.pipeline_alpha,
                Blend::Additive => &self.pipeline_additive,
            };
            pass.set_pipeline(pipeline);
            pass.draw_indexed(*offset..(*offset + *count), 0, 0..1);
        }
        drop(pass);
        encoder
    }

    /// Slider bodies: one SDF prepass per body into its own R16Float layer,
    /// then a single scene pass compositing them in draw order. No MSAA and
    /// no full-screen color pass per body.
    fn encode_bodies_into(
        &mut self,
        list: &DrawList,
        clear: [f64; 4],
        target_view: &wgpu::TextureView,
    ) -> wgpu::CommandEncoder {
        if list.bodies.is_empty() {
            return self.encode_scene_only_into(list, clear, target_view);
        }
        let mut body_quads: Vec<Vertex> = Vec::new();
        let mut body_indices: Vec<u32> = Vec::new();
        let mut prepass_verts: Vec<PrepassVertex> = Vec::new();
        let mut prepass_indices: Vec<u32> = Vec::new();
        let mut prepass_ranges: Vec<(u32, u32)> = Vec::new();
        for (layer, body) in list.bodies.iter().enumerate() {
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
                    prepass_verts.push(PrepassVertex {
                        pos: corner,
                        start: [a[0], a[1]],
                        end: [b[0], b[1]],
                        radius: body.radius,
                    });
                }
                prepass_indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
            prepass_ranges.push((start, prepass_indices.len() as u32));

            let base = body_quads.len() as u32;
            let inner = body.inner_colour;
            for corner in [[minx, miny], [maxx, miny], [maxx, maxy], [minx, maxy]] {
                body_quads.push(Vertex {
                    pos: corner,
                    local: match inner {
                        Some(c) => [c.r, c.g],
                        None => [0.0; 2],
                    },
                    color: [body.body.r, body.body.g, body.body.b, body.body.a],
                    color2: [body.border_colour.r, body.border_colour.g, body.border_colour.b, body.border_colour.a],
                    uv: [corner[0], corner[1], inner.map(|c| c.b).unwrap_or(0.0), layer as f32],
                    aux: [crate::draw::MODE_CAPSULE, body.radius, body.border, if inner.is_some() { 1.0 } else { 0.0 }],
                });
            }
            body_indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        let pre_vbytes = prepass_verts.len() * std::mem::size_of::<PrepassVertex>();
        let pre_ibytes = prepass_indices.len() * 4;
        if pre_vbytes as u64 > self.body_vbo.size() {
            self.body_vbo = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("body vbo(grown)"),
                size: (pre_vbytes as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if pre_ibytes as u64 > self.body_ibo.size() {
            self.body_ibo = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("body ibo(grown)"),
                size: (pre_ibytes as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        let mut all_verts = body_quads;
        let mut all_idx = body_indices;
        let scene_base = all_verts.len() as u32;
        all_verts.extend_from_slice(&list.vertices);
        for i in &list.indices {
            all_idx.push(*i + scene_base);
        }
        self.ensure_mesh(all_verts.len(), all_idx.len());
        self.queue.write_buffer(&self.body_vbo, 0, cast_slice(&prepass_verts));
        self.queue.write_buffer(&self.body_ibo, 0, cast_slice(&prepass_indices));
        self.queue.write_buffer(&self.vbo, 0, cast_slice(&all_verts));
        self.queue.write_buffer(&self.ibo, 0, cast_slice(&all_idx));

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        let clear_color = wgpu::Color { r: clear[0], g: clear[1], b: clear[2], a: clear[3] };
        self.draw_bodies_one_pass(&mut encoder, list, &prepass_ranges, target_view, clear_color);
        encoder
    }

    fn ensure_body_layers(&mut self, needed: u32) {
        if needed == 0 {
            return;
        }
        if self
            .body_layers
            .as_ref()
            .is_some_and(|cache| cache.layers >= needed)
        {
            return;
        }
        let layers = needed.next_power_of_two().max(4);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("body distfield layers"),
            size: wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let sample_view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("body distfield array"),
            format: None,
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            usage: None,
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: Some(layers),
        });
        let layer_views = (0..layers)
            .map(|layer| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("body distfield layer"),
                    format: None,
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    usage: None,
                    aspect: wgpu::TextureAspect::All,
                    base_mip_level: 0,
                    mip_level_count: None,
                    base_array_layer: layer,
                    array_layer_count: Some(1),
                })
            })
            .collect();
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("body layer bind"),
            layout: &self.body_layer_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&sample_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.body_layer_sampler),
                },
            ],
        });
        self.body_layers = Some(BodyLayerCache {
            layers,
            layer_views,
            bind,
        });
    }

    /// Prepass every slider into its own distance-field layer, then draw
    /// notes and slider quads in a single color pass.
    fn draw_bodies_one_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        list: &DrawList,
        prepass_ranges: &[(u32, u32)],
        target_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
    ) {
        self.ensure_body_layers(list.bodies.len() as u32);
        let cache = self.body_layers.as_ref().expect("body layers");
        for (index, &(start, end)) in prepass_ranges.iter().enumerate() {
            let mut pre = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("body prepass layer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &cache.layer_views[index],
                    resolve_target: None,
                    ops: wgpu::Operations {
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
            if start < end {
                pre.set_pipeline(&self.body_pre_pipeline);
                pre.set_bind_group(0, &self.screen_bind, &[]);
                pre.set_vertex_buffer(0, self.body_vbo.slice(..));
                pre.set_index_buffer(self.body_ibo.slice(..), wgpu::IndexFormat::Uint32);
                pre.draw_indexed(start..end, 0, 0..1);
            }
        }

        #[derive(Clone, Copy)]
        enum Op {
            Run(crate::draw::Blend, u32, u32),
            Body(usize),
        }
        let mut ops: Vec<Op> = Vec::new();
        let marks = &list.body_marks;
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

        let base = (list.bodies.len() * 6) as u32;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene one pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &self.screen_bind, &[]);
        pass.set_vertex_buffer(0, self.vbo.slice(..));
        pass.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);
        for op in ops {
            match op {
                Op::Run(blend, off, cnt) => {
                    let pipeline = match blend {
                        crate::draw::Blend::Alpha => &self.pipeline_alpha,
                        crate::draw::Blend::Additive => &self.pipeline_additive,
                    };
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(1, &self.atlas_bind, &[]);
                    let o = off + base;
                    pass.draw_indexed(o..(o + cnt), 0, 0..1);
                }
                Op::Body(bi) => {
                    pass.set_pipeline(&self.body_layer_pipeline);
                    pass.set_bind_group(1, &cache.bind, &[]);
                    let o = (bi * 6) as u32;
                    pass.draw_indexed(o..(o + 6), 0, 0..1);
                }
            }
        }
    }

    fn ensure_mesh(&mut self, vert_count: usize, index_count: usize) {
        let vbytes = vert_count * std::mem::size_of::<Vertex>();
        let ibytes = index_count * 4;
        if vbytes as u64 > self.vbo.size() {
            self.vbo = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vbo(grown)"),
                size: (vbytes as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
        }
        if ibytes as u64 > self.ibo.size() {
            self.ibo = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ibo(grown)"),
                size: (ibytes as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::INDEX
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
        }
    }

    /// Builds the full scene command encoder (offscreen target).
    pub fn encode_scene(&mut self, list: &DrawList, clear: [f64; 4]) -> wgpu::CommandEncoder {
        let target_view = self.target_view.clone();
        self.encode_bodies_into(list, clear, &target_view)
    }

    /// Builds the scene into a caller-provided view (window presentation can
    /// render straight into the swapchain, skipping the offscreen target and
    /// the full-screen blit).
    pub fn encode_scene_into(
        &mut self,
        list: &DrawList,
        clear: [f64; 4],
        view: &wgpu::TextureView,
    ) -> wgpu::CommandEncoder {
        self.encode_bodies_into(list, clear, view)
    }

    /// Renders one frame and SUBMITS it without waiting for the GPU; the
    /// copied-out frame data is retrieved later with `read_oldest` once
    /// `pending_len` frames are in flight.
    pub fn render_deferred(&mut self, list: &DrawList, clear: [f64; 4]) {
        let encoder = self.encode_scene(list, clear);
        self.submit_frame_with(encoder);
    }

    /// Number of submitted-but-not-yet-read frames (synchronous path).
    pub fn pending_len(&self) -> usize {
        self.readback_pending.len()
    }

    /// Submit one frame without any readback copy (benchmark/diagnostic).
    pub fn render_submit_only(&mut self, list: &DrawList, clear: [f64; 4]) {
        let encoder = self.encode_scene(list, clear);
        self.queue.submit(Some(encoder.finish()));
    }

    /// Free the BGRA staging ring when the caller only uses the YUV path.
    /// No-op if a synchronous BGRA read is still pending.
    pub fn release_bgra_readback(&mut self) {
        if self.readback_pending.is_empty() {
            self.readback_ring.clear();
            self.readback_free.clear();
        }
    }

    /// Block until all submitted GPU work finishes (benchmark/diagnostic).
    pub fn wait_idle(&self) {
        let _ = self.device.poll(wgpu::PollType::Wait);
    }

    /// Hand every submitted frame straight to the writer thread. The writer
    /// maps it, waits for its own submission, writes to ffmpeg, then returns
    /// the slot. The render thread never maps or copies a frame.
    pub fn enable_async_readback(
        &mut self,
        tx: SyncSender<ReadbackFrame>,
        ret: Receiver<(ReadbackKind, usize)>,
    ) {
        self.readback_tx = Some(tx);
        self.readback_return = Some(ret);
    }

    /// Drop the render-thread sender so the writer sees EOF after draining.
    pub fn disable_async_readback(&mut self) {
        self.readback_tx = None;
    }

    fn acquire_slot(&mut self, kind: ReadbackKind) -> usize {
        loop {
            let free = match kind {
                ReadbackKind::Bgra => self.readback_free.pop_front(),
                ReadbackKind::Yuv => self.yuv.as_mut().and_then(|y| y.ring_free.pop_front()),
            };
            if let Some(slot) = free {
                return slot;
            }
            let Some(rx) = self.readback_return.as_ref() else {
                panic!("readback ring exhausted (no async writer to return slots)");
            };
            let (returned_kind, slot) = rx.recv().expect("readback writer exited");
            match returned_kind {
                ReadbackKind::Bgra => self.readback_free.push_back(slot),
                ReadbackKind::Yuv => self
                    .yuv
                    .as_mut()
                    .expect("yuv ring")
                    .ring_free
                    .push_back(slot),
            }
        }
    }

    fn submit_frame_with(&mut self, mut encoder: wgpu::CommandEncoder) {
        // 无回读配置(NO_READBACK / window preview):submit only.
        if self.readback_ring.is_empty() {
            self.queue.submit(Some(encoder.finish()));
            return;
        }
        let slot = self.acquire_slot(ReadbackKind::Bgra);
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: self.readback_ring[slot].as_ref(),
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
        let idx = self.queue.submit(Some(encoder.finish()));
        if let Some(tx) = &self.readback_tx {
            let frame = ReadbackFrame {
                buffer: Arc::clone(&self.readback_ring[slot]),
                submission: idx,
                slot,
                kind: ReadbackKind::Bgra,
                width: self.width,
                height: self.height,
                stride: self.padded_row,
                bytes: self.padded_row as usize * self.height as usize,
                interleaved: true,
            };
            tx.send(frame).expect("readback writer exited");
        } else {
            self.readback_pending.push_back((slot, idx));
        }
    }

    /// Maps the OLDEST pending frame and copies it into `out` (synchronous
    /// PNG/test path). Waits only for the submission that wrote this slot.
    pub fn read_oldest_into(&mut self, out: &mut Vec<u8>) {
        let (slot, idx) = self
            .readback_pending
            .pop_front()
            .expect("read_oldest with empty pipeline");
        let buffer = Arc::clone(&self.readback_ring[slot]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::WaitForSubmissionIndex(idx));
        let data = slice.get_mapped_range();
        out.clear();
        out.extend_from_slice(&data);
        drop(data);
        buffer.unmap();
        self.readback_free.push_back(slot);
    }

    // ---- GPU YUV420 output (BGRA scene target → NV12/I420 buffer) ----

    /// Whether the GPU conversion supports this renderer's dimensions.
    /// NV12/I420 need an even width/height; widths not divisible by 8 are
    /// padded in the compute pass and compacted row-wise on the way out.
    pub fn yuv_ready(&self) -> bool {
        self.width >= 2 && self.height >= 2 && self.width % 2 == 0 && self.height % 2 == 0
    }

    /// Packed NV12/I420 frame size: `w * h * 3 / 2` bytes.
    pub fn yuv_frame_bytes(&self) -> usize {
        (self.width as usize * self.height as usize * 3) / 2
    }

    fn init_yuv(&mut self) {
        assert!(self.yuv_ready(), "GPU YUV path requires even width/height");
        let (width, height) = (self.width, self.height);
        let padded_width = (width + 7) / 8 * 8;
        let padded_height = height;
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv shader"),
            source: wgpu::ShaderSource::Wgsl(YUV_SHADER.into()),
        });

        let params_layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv params layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(16),
                },
                count: None,
            }],
        });
        let tex_layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv tex layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let out_layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv out layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv layout"),
            bind_group_layouts: &[&params_layout, &tex_layout, &out_layout],
            push_constant_ranges: &[],
        });

        let make_pipeline = |entry: &str| {
            self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("yuv pipeline"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let pipeline_y = make_pipeline("cs_y");
        let pipeline_uv_nv12 = make_pipeline("cs_uv_nv12");
        let pipeline_uv_i420 = make_pipeline("cs_uv_i420");

        let params_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(
            &params_buf,
            0,
            cast_slice(&[width, height, padded_width, padded_height]),
        );
        let params_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &params_buf,
                    offset: 0,
                    size: Some(NonZeroU64::new(16).unwrap()),
                }),
            }],
            label: Some("yuv params bind"),
        });

        let target_view = self.target.create_view(&Default::default());
        let tex_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &tex_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&target_view),
            }],
            label: Some("yuv tex bind"),
        });

        // GPU layout uses the padded width; the writer compacts each row to
        // the real width before handing it to ffmpeg.
        let bytes = self.yuv_frame_bytes() as u32;
        let padded_bytes = ((padded_width as usize * padded_height as usize * 3) / 2) as u32;
        let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv out"),
            size: padded_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let out_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &out_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &out_buf,
                    offset: 0,
                    size: None,
                }),
            }],
            label: Some("yuv out bind"),
        });
        let ring: Vec<Arc<wgpu::Buffer>> = (0..3)
            .map(|i| {
                Arc::new(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("yuv readback {i}")),
                    size: padded_bytes as u64,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }))
            })
            .collect();
        let ring_free = (0..ring.len()).collect();

        self.yuv = Some(YuvPass {
            pipeline_y,
            pipeline_uv_nv12,
            pipeline_uv_i420,
            params_bind,
            tex_bind,
            out_bind,
            out_buf,
            ring,
            ring_free,
            pending: std::collections::VecDeque::new(),
            bytes,
            padded_bytes,
            padded_width,
            groups_y: ((padded_width / 4 + 7) / 8, (padded_height + 3) / 4),
            groups_uv: ((padded_width / 8 + 7) / 8, (padded_height / 2 + 3) / 4),
        });
    }

    /// Renders one frame and converts it to YUV420 on the GPU (NV12 when
    /// `interleaved`, I420 otherwise), submitting without waiting for the
    /// GPU — mirror of [`Renderer::render_deferred`]. Retrieve converted
    /// frames with [`Renderer::read_oldest_yuv_into`].
    pub fn render_deferred_yuv(&mut self, list: &DrawList, clear: [f64; 4], interleaved: bool) {
        if self.yuv.is_none() {
            self.init_yuv();
        }
        let slot = self.acquire_slot(ReadbackKind::Yuv);
        let mut encoder = self.encode_scene(list, clear);
        {
            let y = self.yuv.as_ref().unwrap();
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("yuv pass"),
                timestamp_writes: None,
            });
            pass.set_bind_group(0, &y.params_bind, &[]);
            pass.set_bind_group(1, &y.tex_bind, &[]);
            pass.set_bind_group(2, &y.out_bind, &[]);
            pass.set_pipeline(&y.pipeline_y);
            pass.dispatch_workgroups(y.groups_y.0, y.groups_y.1, 1);
            pass.set_pipeline(if interleaved { &y.pipeline_uv_nv12 } else { &y.pipeline_uv_i420 });
            pass.dispatch_workgroups(y.groups_uv.0, y.groups_uv.1, 1);
            drop(pass);
            encoder.copy_buffer_to_buffer(&y.out_buf, 0, &y.ring[slot], 0, y.padded_bytes as u64);
        }
        let idx = self.queue.submit(Some(encoder.finish()));
        if let Some(tx) = &self.readback_tx {
            let y = self.yuv.as_ref().unwrap();
            let frame = ReadbackFrame {
                buffer: Arc::clone(&y.ring[slot]),
                submission: idx,
                slot,
                kind: ReadbackKind::Yuv,
                width: self.width,
                height: self.height,
                stride: self.yuv.as_ref().unwrap().padded_width,
                bytes: self.yuv.as_ref().unwrap().bytes as usize,
                interleaved,
            };
            tx.send(frame).expect("readback writer exited");
        } else {
            self.yuv.as_mut().unwrap().pending.push_back((slot, idx, interleaved));
        }
    }

    /// Number of submitted-but-not-yet-read YUV frames.
    pub fn yuv_pending_len(&self) -> usize {
        self.yuv.as_ref().map_or(0, |y| y.pending.len())
    }

    /// Maps the OLDEST pending YUV frame and copies it into `out`
    /// (`yuv_frame_bytes()` bytes). Synchronous path; async export uses
    /// [`ReadbackFrame`] directly.
    pub fn read_oldest_yuv_into(&mut self, out: &mut Vec<u8>) {
        out.clear();
        out.resize(self.yuv_frame_bytes(), 0);
        self.read_oldest_yuv_into_slice(out);
    }

    /// [`Self::read_oldest_yuv_into`] writing straight into a caller-owned
    /// buffer (exactly `yuv_frame_bytes()` bytes) — the JNI export path
    /// hands its direct buffer over directly, skipping the extra
    /// full-frame copy through a staging Vec.
    pub fn read_oldest_yuv_into_slice(&mut self, out: &mut [u8]) {
        let (slot, idx, interleaved) = self
            .yuv
            .as_mut()
            .and_then(|y| y.pending.pop_front())
            .expect("read_oldest_yuv with empty pipeline");
        let y = self.yuv.as_ref().unwrap();
        let (width, height, stride) = (self.width, self.height, y.padded_width);
        let buffer = Arc::clone(&y.ring[slot]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::WaitForSubmissionIndex(idx));
        let data = slice.get_mapped_range();
        if stride as usize == width as usize {
            out.copy_from_slice(&data[..out.len()]);
        } else {
            compact_yuv(&data, out, width, height, stride, interleaved);
        }
        drop(data);
        buffer.unmap();
        self.yuv.as_mut().unwrap().ring_free.push_back(slot);
    }
}

fn compact_yuv(data: &[u8], out: &mut [u8], width: u32, height: u32, stride: u32, interleaved: bool) {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    let mut o = 0usize;
    for row in 0..h {
        let s = row * stride;
        out[o..o + w].copy_from_slice(&data[s..s + w]);
        o += w;
    }
    let uv_off = stride * h;
    if interleaved {
        for row in 0..h / 2 {
            let s = uv_off + row * stride;
            out[o..o + w].copy_from_slice(&data[s..s + w]);
            o += w;
        }
    } else {
        let cw = w / 2;
        for row in 0..h / 2 {
            let s = uv_off + row * (stride / 2);
            out[o..o + cw].copy_from_slice(&data[s..s + cw]);
            o += cw;
        }
        let v_off = uv_off + (stride / 2) * (h / 2);
        for row in 0..h / 2 {
            let s = v_off + row * (stride / 2);
            out[o..o + cw].copy_from_slice(&data[s..s + cw]);
            o += cw;
        }
    }
}

// Minimal bytemuck-style cast (avoids the extra dependency).
unsafe trait Pod {}
unsafe impl Pod for Vertex {}
unsafe impl Pod for PrepassVertex {}
unsafe impl Pod for u32 {}
unsafe impl Pod for f32 {}

fn cast_slice<T: Pod>(data: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}
