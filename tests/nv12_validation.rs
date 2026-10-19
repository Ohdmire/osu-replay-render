//! NV12 output validation: renders a known high-contrast pattern through the
//! BGRA path and the GPU NV12 path (including widths not divisible by 8) and
//! checks that the result is neither black nor structurally wrong.

use osu_replay_render::draw::{Atlas, Blend, DrawList, Vertex};
use osu_replay_render::render::Renderer;

fn quad(list: &mut DrawList, x0: f32, y0: f32, x1: f32, y1: f32, color: [f32; 4]) {
    let base = list.vertices.len() as u32;
    for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
        list.vertices.push(Vertex {
            pos: [x, y],
            local: [x, y],
            color,
            color2: [0.0; 4],
            // mode 6 = flat colour (see the scene shader's fallback).
            uv: [0.0; 4],
            aux: [6.0, 0.0, 0.0, 0.0],
        });
    }
    list.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    list.runs.push((Blend::Alpha, base, 6));
}

fn pattern(w: u32, h: u32) -> DrawList {
    let mut list = DrawList::new();
    let (wf, hf) = (w as f32, h as f32);
    quad(&mut list, 0.0, 0.0, wf, hf, [0.08, 0.12, 0.22, 1.0]);
    quad(&mut list, wf * 0.05, hf * 0.10, wf * 0.45, hf * 0.45, [0.9, 0.1, 0.1, 1.0]);
    quad(&mut list, wf * 0.50, hf * 0.10, wf * 0.95, hf * 0.45, [0.1, 0.8, 0.2, 1.0]);
    quad(&mut list, wf * 0.05, hf * 0.55, wf * 0.45, hf * 0.90, [0.1, 0.2, 0.9, 1.0]);
    quad(&mut list, wf * 0.50, hf * 0.55, wf * 0.95, hf * 0.90, [0.95, 0.95, 0.10, 1.0]);
    list.finish();
    list
}

fn rgb_from_yuv(y: u8, u: u8, v: u8) -> [f32; 3] {
    // Inverse of the shader's BT.601 limited-range conversion.
    let y = (y as f32 - 16.0) * (255.0 / 219.0);
    let u = (u as f32 - 128.0) * (255.0 / 224.0);
    let v = (v as f32 - 128.0) * (255.0 / 224.0);
    [
        (y + 1.402 * v).clamp(0.0, 255.0),
        (y - 0.344_136 * u - 0.714_136 * v).clamp(0.0, 255.0),
        (y + 1.772 * u).clamp(0.0, 255.0),
    ]
}

fn check_size(w: u32, h: u32) {
    let atlas = Atlas::build(&[]);
    let mut renderer = Renderer::new(w, h, &atlas);
    assert!(renderer.yuv_ready(), "{w}x{h} should support NV12");
    let list = pattern(w, h);

    // BGRA reference (padded rows).
    let bgra = renderer.render(&list, [0.0, 0.0, 0.0, 1.0]);
    let stride = renderer.padded_row as usize;

    // GPU NV12.
    renderer.render_deferred_yuv(&list, [0.0, 0.0, 0.0, 1.0], true);
    let mut nv12 = Vec::new();
    renderer.read_oldest_yuv_into(&mut nv12);
    assert_eq!(nv12.len(), renderer.yuv_frame_bytes());

    let mut sum_sq = 0.0f64;
    let mut n = 0.0f64;
    let mut bg_sum = 0.0f64;
    let mut bg_dark = 0usize;
    let mut nv_dark = 0usize;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let bi = y * stride + x * 4;
            let b = bgra[bi] as f32;
            let g = bgra[bi + 1] as f32;
            let r = bgra[bi + 2] as f32;
            let lum = 0.299 * r + 0.587 * g + 0.114 * b;
            bg_sum += lum as f64;
            if lum < 12.0 {
                bg_dark += 1;
            }

            // Default test pattern is NV12 (interleaved).
            let yy = nv12[y * w as usize + x] as u8;
            let uv = (w as usize * h as usize) + (y / 2) * w as usize + (x / 2) * 2;
            let u = nv12[uv] as u8;
            let v = nv12[uv + 1] as u8;
            if yy < 20 {
                nv_dark += 1;
            }
            let [nr, ng, nb] = rgb_from_yuv(yy, u, v);
            let d = (nr - r).powi(2) + (ng - g).powi(2) + (nb - b).powi(2);
            sum_sq += d as f64 / 3.0;
            n += 1.0;
        }
    }
    let mse = sum_sq / n;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
    let mean = bg_sum / n;
    assert!(
        mean > 25.0,
        "{w}x{h}: BGRA frame suspiciously dark (mean luma {mean:.1})"
    );
    assert!(
        bg_dark as f64 / n < 0.5,
        "{w}x{h}: BGRA frame mostly black ({}% dark)",
        100.0 * bg_dark as f64 / n
    );
    assert!(
        nv_dark as f64 / n < 0.5,
        "{w}x{h}: NV12 frame mostly black ({}% dark)",
        100.0 * nv_dark as f64 / n
    );
    assert!(
        psnr > 32.0,
        "{w}x{h}: NV12/BGRA mismatch, PSNR {psnr:.2} dB (mean RGB diff too large)"
    );
}

#[test]
fn nv12_matches_bgra_and_is_not_black() {
    for (w, h) in [(1280, 720), (1366, 768), (1920, 1080)] {
        check_size(w, h);
    }
}
