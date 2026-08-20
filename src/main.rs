//! osu_replay_render: offscreen osu! replay renderer (wgpu + Argon skin).
//!
//! Usage:
//!   osu_replay_render <beatmap.osu> <replay.osr> [options]
//!
//! Options:
//!   --out <file.mp4>       Pipe frames to ffmpeg and encode (default if given)
//!   --png-dir <dir>        Write PNG frames to a directory instead
//!   --size <WxH>           Output size (default 1920x1080)
//!   --fps <n>              Output fps (default 60; frames are sampled from
//!                          the 60fps game-frame snapshots)
//!   --start <ms>           Start time in replay ms (default: beginning)
//!   --end <ms>             End time in replay ms (default: end)
//!   --score classic        Show classic (stable-style) score
//!   --limit <n>            Render at most n frames (testing)

mod draw;
mod game;
mod hud;
mod render;
mod scene;

use draw::{Atlas, Image, Region, TtfFont};
use render::Renderer;
use scene::{Assets, SceneState};
use std::io::Write;

const TORUS_BOLD_TTF: &[u8] = include_bytes!("../assets/fonts/TorusPro-Bold.ttf");
const TORUS_SEMIBOLD_TTF: &[u8] = include_bytes!("../assets/fonts/TorusPro-SemiBold.ttf");
const CURSOR_TRAIL_PNG: &[u8] = include_bytes!("../assets/cursor/cursortrail.png");
const REPEAT_EDGE_PNG: &[u8] = include_bytes!("../assets/cursor/repeat-edge-piece.png");
const APPROACH_CIRCLE_PNG: &[u8] = include_bytes!("../assets/cursor/approachcircle.png");

fn counter_png(c: &str) -> Vec<u8> {
    std::fs::read(format!("assets/counter/argon-counter-{}.png", c)).unwrap_or_else(|_| {
        include_bytes!("../assets/counter/argon-counter-5.png").to_vec()
    })
}

fn build_atlas(bg_image: Option<Image>) -> (Atlas, TtfFont, TtfFont) {
    let (mut bold, mut bold_images) = TtfFont::rasterize(TORUS_BOLD_TTF, true);
    let (mut semibold, mut semibold_images) = TtfFont::rasterize(TORUS_SEMIBOLD_TTF, false);

    let mut images: Vec<(Region, Image)> = Vec::new();
    if let Some(img) = bg_image {
        images.push((Region::Background, img));
    }
    images.append(&mut bold_images);
    images.append(&mut semibold_images);

    for d in b'0'..=b'9' {
        let (w, h, rgba) = decode(&counter_png(&(d - b'0').to_string()));
        images.push((Region::CounterDigit(d), Image { width: w, height: h, rgba }));
    }
    for (name, region) in [
        ("dot", Region::CounterDot),
        ("percentage", Region::CounterPercent),
        ("x", Region::CounterX),
        ("wireframes", Region::CounterWireframes),
    ] {
        let (w, h, rgba) = decode(&counter_png(name));
        images.push((region, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode(CURSOR_TRAIL_PNG);
        images.push((Region::CursorTrail, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode(REPEAT_EDGE_PNG);
        images.push((Region::RepeatEdge, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode(APPROACH_CIRCLE_PNG);
        images.push((Region::ApproachCircle, Image { width: w, height: h, rgba }));
    }

    let atlas = Atlas::build(&images);
    if std::env::var("ATLAS_DEBUG").is_ok() {
        for r in [Region::CounterDigit(b'5'), Region::Glyph { bold: true, c: 'G', em: 24 }, Region::Glyph { bold: true, c: 'G', em: 96 }, Region::Glyph { bold: false, c: '5', em: 48 }, Region::CounterWireframes] {
            let rect = atlas.region_rect(r);
            let ink = atlas.ink(r);
            eprintln!("ATLAS {:?}: rect=({:.0},{:.0},{:.0},{:.0}) ink=({:.0},{:.0},{:.0},{:.0})", r, rect.x0, rect.y0, rect.x1, rect.y1, ink[0], ink[1], ink[2], ink[3]);
        }
    }
    bold.patch_rects(&atlas, true);
    semibold.patch_rects(&atlas, false);
    if std::env::var("ATLAS_DUMP").is_ok() {
        let file = std::fs::File::create("atlas_dump.png").unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), atlas.width, atlas.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&atlas.rgba).unwrap();
        eprintln!("atlas dumped: {}x{}", atlas.width, atlas.height);
    }
    (atlas, bold, semibold)
}

/// A value from the beatmap's `[General]` section (e.g. `AudioFilename`).
fn osu_general_value(map_path: &str, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(map_path).ok()?;
    let mut in_general = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_general = line.eq_ignore_ascii_case("[General]");
            continue;
        }
        if in_general {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case(key) {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// The background image filename from `[Events]` (`0,0,"file.jpg",...`).
fn osu_background_file(map_path: &str) -> Option<String> {
    let content = std::fs::read_to_string(map_path).ok()?;
    let mut in_events = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if in_events && line.starts_with("0,0,") {
            // `0,0,"file.jpg",0,0` - strip the opening quote, take up to the
            // closing one (filenames may contain commas).
            let rest = line[4..].trim_start_matches('"');
            let end = rest.find('"').unwrap_or(rest.len());
            let name = rest[..end].to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Decodes a PNG or JPEG file into an atlas image (RGBA).
fn decode_image_file(path: &std::path::Path) -> Result<Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let lower = path.to_string_lossy().to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
        let pixels = decoder.decode().map_err(|e| format!("jpeg decode {}: {}", path.display(), e))?;
        let info = decoder.info().ok_or("jpeg missing info")?;
        let (w, h) = (info.width as u32, info.height as u32);
        let rgba = match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => {
                let mut v = Vec::with_capacity((w * h * 4) as usize);
                for px in &pixels {
                    v.extend_from_slice(&[*px, *px, *px, 255]);
                }
                v
            }
            jpeg_decoder::PixelFormat::RGB24 => {
                let mut v = Vec::with_capacity((w * h * 4) as usize);
                for px in pixels.chunks_exact(3) {
                    v.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
                v
            }
            other => return Err(format!("unsupported jpeg pixel format {:?}", other)),
        };
        Ok(Image { width: w, height: h, rgba })
    } else {
        let (w, h, rgba) = decode(&bytes);
        Ok(Image { width: w, height: h, rgba })
    }
}

fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {    // Reuse the png decoding from draw via a tiny wrapper.
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png read info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("png decode");
    let (w, h) = (info.width, info.height);
    // draw::decode_png is private; inline conversion here.
    match info.color_type {
        png::ColorType::Rgba => (w, h, buf),
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            (w, h, rgba)
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in &buf {
                rgba.extend_from_slice(&[*px, *px, *px, 255]);
            }
            (w, h, rgba)
        }
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            (w, h, rgba)
        }
        other => panic!("unsupported png colour type {:?}", other),
    }
}

struct Options {
    out: Option<String>,
    png_dir: Option<String>,
    width: u32,
    height: u32,
    fps: f64,
    start: Option<f64>,
    end: Option<f64>,
    classic_score: bool,
    skin: String,
    /// x264 (default) | x265 | nvenc (hardware, fastest).
    encoder: String,
    /// crf; 18 by default.
    quality: u32,
    /// When set: render a single frame at this time and dump geometry JSON.
    probe: Option<f64>,
    limit: Option<usize>,
    ffmpeg_extra: Vec<String>,
    /// Whether the UR bar's window guide lines (colour axis) render.
    /// Default on; `--no-guides` disables them.
    guides: bool,
    /// Optional BGM muxed into the output (`--audio [file]`; without a
    /// value the beatmap's own audio is used).
    audio: Option<String>,
    /// Draw the beatmap background image (`--bg`).
    bg: bool,
    /// Background opacity 0..1 (lazer: 1 - DimLevel, default DimLevel 0.7).
    bg_opacity: f32,
    /// Total lazer audio offset in ms subtracted from replay time to get
    /// the audio file position. The gameplay clock ADDS the platform
    /// offset (Windows non-experimental +15, `WINDOWS_BASE_AUDIO_OFFSET`),
    /// `OsuSetting.AudioOffset` and the per-beatmap offset to the track
    /// position (FramedBeatmapClock's OffsetCorrectionClock chain), so
    /// audio_pos = replay_time - total_offset.
    audio_offset: f64,
}

fn parse_args() -> Result<Options, String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        return Err(format!("usage: {} <beatmap.osu> <replay.osr> [--out file.mp4] [--png-dir dir] [--size WxH] [--fps n] [--start ms] [--end ms] [--score classic] [--skin argon|argon-pro] [--no-guides] [--audio [file.mp3]] [--bg] [--bg-opacity 0..1] [--limit n]", args.get(0).map(|s| s.as_str()).unwrap_or("osu_replay_render")));
    }
    let mut opts = Options {
        out: None,
        png_dir: None,
        width: 1920,
        height: 1080,
        fps: 60.0,
        start: None,
        end: None,
        classic_score: false,
        skin: "argon-pro".to_string(),
        encoder: "x264".to_string(),
        quality: 18,
        probe: None,
        limit: None,
        ffmpeg_extra: Vec::new(),
        guides: true,
        audio: None,
        bg: false,
        bg_opacity: 0.3,
        audio_offset: 15.0,
    };
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                opts.out = args.get(i).cloned();
            }
            "--png-dir" => {
                i += 1;
                opts.png_dir = args.get(i).cloned();
            }
            "--fps" => {
                i += 1;
                opts.fps = args.get(i).and_then(|v| v.parse().ok()).ok_or("bad --fps")?;
                if opts.fps < 1.0 || opts.fps > 480.0 {
                    return Err("--fps must be within 1..480".into());
                }
            }
            "--size" => {
                i += 1;
                let s = args.get(i).ok_or("--size needs WxH")?;
                let mut it = s.split('x');
                opts.width = it.next().and_then(|v| v.parse().ok()).ok_or("bad --size")?;
                opts.height = it.next().and_then(|v| v.parse().ok()).ok_or("bad --size")?;
            }
            "--start" => {
                i += 1;
                opts.start = args.get(i).and_then(|v| v.parse().ok());
            }
            "--end" => {
                i += 1;
                opts.end = args.get(i).and_then(|v| v.parse().ok());
            }
            "--score" => {
                i += 1;
                if args.get(i).map(|s| s.as_str()) == Some("classic") {
                    opts.classic_score = true;
                }
            }
            "--skin" => {
                i += 1;
                let skin = args.get(i).cloned().ok_or("--skin needs a value")?;
                if skin != "argon" && skin != "argon-pro" {
                    return Err("--skin must be argon or argon-pro".into());
                }
                opts.skin = skin;
            }
            "--encoder" => {
                i += 1;
                let enc = args.get(i).cloned().ok_or("--encoder needs a value")?;
                if !matches!(enc.as_str(), "x264" | "x265" | "nvenc") {
                    return Err("--encoder must be x264, x265 or nvenc".into());
                }
                opts.encoder = enc;
            }
            "--quality" => {
                i += 1;
                opts.quality = args.get(i).and_then(|v| v.parse().ok()).ok_or("bad --quality")?;
            }
            "--probe" => {
                i += 1;
                opts.probe = args.get(i).and_then(|v| v.parse().ok());
            }
            "--limit" => {
                i += 1;
                opts.limit = args.get(i).and_then(|v| v.parse().ok());
            }
            "--no-guides" => {
                opts.guides = false;
            }
            "--audio" => {
                // Optional value: an explicit file, else the beatmap's own
                // audio (resolved later once the map path is known).
                opts.audio = match args.get(i + 1) {
                    Some(v) if !v.starts_with("--") => {
                        i += 1;
                        Some(v.clone())
                    }
                    Some(_) | None => Some(String::new()),
                };
            }
            "--bg" => {
                opts.bg = true;
            }
            "--bg-opacity" => {
                i += 1;
                opts.bg_opacity = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("bad --bg-opacity (expected 0..1)")?;
                if !(0.0..=1.0).contains(&opts.bg_opacity) {
                    return Err("--bg-opacity must be within 0..1".into());
                }
            }
            "--audio-offset" => {
                i += 1;
                opts.audio_offset = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("bad --audio-offset (expected milliseconds)")?;
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
        i += 1;
    }
    Ok(opts)
}

enum Output {
    Ffmpeg(std::process::Child),
    PngDir(String),
    None,
}

impl Output {
    fn write_frame(&mut self, bgra: &[u8], width: u32, height: u32, stride: u32, index: usize) -> std::io::Result<()> {
        match self {
            Output::Ffmpeg(child) => {
                let stdin = child.stdin.as_mut().expect("ffmpeg stdin");
                for row in 0..height {
                    let start = (row * stride) as usize;
                    let end = start + (width * 4) as usize;
                    stdin.write_all(&bgra[start..end])?;
                }
                Ok(())
            }
            Output::PngDir(dir) => {
                let path = format!("{}/frame_{:06}.png", dir, index);
                let file = std::fs::File::create(&path)?;
                let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                let mut writer = enc.write_header()?;
                // Convert BGRA -> RGBA rows.
                let mut rgba = vec![0u8; (width * height * 4) as usize];
                for row in 0..height {
                    let src = (row * stride) as usize;
                    for x in 0..width {
                        let s = src + (x * 4) as usize;
                        let d = ((row * width + x) * 4) as usize;
                        rgba[d] = bgra[s + 2];
                        rgba[d + 1] = bgra[s + 1];
                        rgba[d + 2] = bgra[s];
                        rgba[d + 3] = bgra[s + 3];
                    }
                }
                writer.write_image_data(&rgba)?;
                Ok(())
            }
            Output::None => Ok(()),
        }
    }

    fn finish(&mut self) {
        if let Output::Ffmpeg(child) = self {
            drop(child.stdin.take());
            let _ = child.wait();
        }
    }
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };

    let map_path = std::env::args().nth(1).unwrap();
    let replay_path = std::env::args().nth(2).unwrap();

    eprintln!("loading {} + {}", map_path, replay_path);
    let game = match game::load(&map_path, &replay_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!(
        "player: {} | objects: {} | snapshots: {} | final score: {} (max combo {})",
        game.player,
        game.objects.len(),
        game.snapshots.len(),
        game.final_score,
        game.final_max_combo
    );

    // Resolve optional BGM: explicit file, or the beatmap's own audio
    // (`AudioFilename`, relative to the map).
    let map_dir = std::path::Path::new(&map_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let audio_path: Option<String> = match &opts.audio {
        Some(explicit) if !explicit.is_empty() => {
            if !std::path::Path::new(explicit).exists() {
                eprintln!("error: audio file not found: {}", explicit);
                std::process::exit(1);
            }
            Some(explicit.clone())
        }
        Some(_) => {
            let name = osu_general_value(&map_path, "AudioFilename")
                .unwrap_or_else(|| "audio.mp3".to_string());
            let p = map_dir.join(&name);
            if p.exists() {
                eprintln!("audio: {} (from beatmap)", p.display());
                Some(p.to_string_lossy().into_owned())
            } else {
                eprintln!("warning: beatmap audio not found: {} - rendering without BGM", p.display());
                None
            }
        }
        None => None,
    };

    // Resolve the beatmap background (`--bg`): the `[Events]` background
    // image, decoded into the atlas and drawn full-screen at
    // `--bg-opacity` (default 1 - DimLevel 0.7, matching lazer).
    let bg_image: Option<Image> = if opts.bg {
        match osu_background_file(&map_path) {
            Some(name) => {
                let p = map_dir.join(&name);
                match decode_image_file(&p) {
                    Ok(img) => {
                        eprintln!("background: {} ({}x{}, opacity {:.2})", p.display(), img.width, img.height, opts.bg_opacity);
                        Some(img)
                    }
                    Err(e) => {
                        eprintln!("warning: {} - rendering without background", e);
                        None
                    }
                }
            }
            None => {
                eprintln!("warning: beatmap has no background image - rendering without background");
                None
            }
        }
    } else {
        None
    };

    let has_bg = bg_image.is_some();
    let (atlas, bold, semibold) = build_atlas(bg_image);
    eprintln!("atlas: {}x{}", atlas.width, atlas.height);

    let mut renderer = Renderer::new(opts.width, opts.height, &atlas);
    let mut state = SceneState::new(&game, opts.width, opts.height);
    state.pro_skin = opts.skin == "argon-pro";
    state.hud.ur_guides = opts.guides;
    state.bg_opacity = if has_bg { Some(opts.bg_opacity) } else { None };
    if opts.classic_score {
        state.hud.use_classic_score();
    }

    // Frame selection: sample the replay at the requested fps.
    let start = opts.start.unwrap_or(f64::NEG_INFINITY);
    let end = opts.end.unwrap_or(f64::INFINITY);
    let first_snap = game.snapshots.first().map(|s| s.time).unwrap_or(0.0);
    let last_snap = game.snapshots.last().map(|s| s.time).unwrap_or(0.0);

    let mut frame_times: Vec<f64> = Vec::new();
    if (opts.fps - 60.0).abs() < 1e-6 {
        // Exact game-frame cadence: use the engine snapshots 1:1.
        for s in &game.snapshots {
            if s.time >= start && s.time <= end {
                frame_times.push(s.time);
            }
        }
    } else {
        let step = 1000.0 / opts.fps;
        let mut t = first_snap.max(start.min(last_snap));
        while t <= last_snap.min(end) {
            frame_times.push(t);
            t += step;
        }
    }
    let frame_times = match opts.limit {
        Some(l) => frame_times.into_iter().take(l).collect::<Vec<_>>(),
        None => frame_times,
    };

    if frame_times.is_empty() {
        eprintln!("error: no frames to render");
        std::process::exit(1);
    }

    let encoder = opts.encoder.as_str();
    eprintln!("encoder: {}", encoder);

    // Output setup.
    let mut output = if let Some(dir) = &opts.png_dir {
        std::fs::create_dir_all(dir).expect("create png dir");
        Output::PngDir(dir.clone())
    } else if let Some(out) = &opts.out {
        // Input side: frames are piped BGRA. NVENC accepts bgr0 natively and
        // converts in hardware (fastest end-to-end: ~1.7x x264); the x264 /
        // x265 software paths go through CPU swscale to yuv420p.
        let (in_pix_fmt, encode_args): (&str, Vec<String>) = if encoder == "nvenc" {
            (
                "bgr0",
                vec![
                    "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq",
                    "-rc", "vbr", "-cq", &opts.quality.to_string(), "-b:v", "0",
                    "-movflags", "+faststart",
                ]
                .iter().map(|s| s.to_string()).collect(),
            )
        } else {
            let codec = if encoder == "x265" { "libx265" } else { "libx264" };
            (
                "bgra",
                vec![
                    "-c:v", codec, "-preset", "medium",
                    "-crf", &opts.quality.to_string(),
                    "-pix_fmt", "yuv420p", "-movflags", "+faststart",
                ]
                .iter().map(|s| s.to_string()).collect(),
            )
        };
        // The video timeline starts at the first rendered frame's replay
        // time. Lazer's gameplay clock = audio file position + platform
        // (+15ms Windows) + user AudioOffset + per-beatmap offset, so the
        // audio position for replay time T is T - total offset
        // (--audio-offset, default the +15ms Windows platform base).
        let audio_start = (frame_times.first().map(|t| *t).unwrap_or(0.0) - opts.audio_offset) / 1000.0;
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.arg("-y")
            .arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg(in_pix_fmt)
            .arg("-s").arg(format!("{}x{}", opts.width, opts.height))
            .arg("-r").arg(format!("{}", opts.fps))
            .arg("-i").arg("-");
        if let Some(audio) = &audio_path {
            cmd.arg("-ss").arg(format!("{:.3}", audio_start))
                .arg("-i").arg(audio);
        }
        cmd.args(&encode_args);
        if audio_path.is_some() {
            cmd.args(["-map", "0:v", "-map", "1:a", "-c:a", "aac", "-b:a", "192k", "-shortest"]);
        }
        let child = cmd
            .arg(out)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create("ffmpeg_err.log").unwrap())
            .spawn()
            .expect("failed to spawn ffmpeg (is it on PATH? or use --png-dir)");
        Output::Ffmpeg(child)
    } else {
        eprintln!("no output specified; use --out <file.mp4> or --png-dir <dir>");
        std::process::exit(2);
    };

    let total = frame_times.len();
    let t0 = std::time::Instant::now();
    let mut list = draw::DrawList::new();
    let assets = Assets { atlas: &atlas, bold: &bold, semibold: &semibold };
    let stats = std::env::var("RENDER_STATS").is_ok();
    let (mut s_build, mut s_render, mut s_write) = (0.0f64, 0.0f64, 0.0f64);

    for (n, &ft) in frame_times.iter().enumerate() {
        let ta = std::time::Instant::now();
        list.clear();
        let snap = game::snapshot_at(&game, ft);
        state.build_frame(&game, &assets, &snap, &mut list);
        list.finish();
        if let Some(pt) = opts.probe {
            if (ft - pt).abs() < 30.0 {
                state.probe_dump(&game, ft, "probe.json");
            }
        }
        let tb = std::time::Instant::now();
        let bgra = renderer.render(&list, [0.055, 0.055, 0.075, 1.0]);
        let tc = std::time::Instant::now();
        if let Err(e) = output.write_frame(&bgra, opts.width, opts.height, renderer.padded_row, n) {
            eprintln!("error writing frame {}: {}", n, e);
            std::process::exit(1);
        }
        let td = std::time::Instant::now();
        s_build += tb.duration_since(ta).as_secs_f64();
        s_render += tc.duration_since(tb).as_secs_f64();
        s_write += td.duration_since(tc).as_secs_f64();
        if n % 300 == 0 || n + 1 == total {
            eprintln!(
                "frame {}/{} (t={:.0}ms) elapsed {:.1}s",
                n + 1,
                total,
                ft,
                t0.elapsed().as_secs_f32()
            );
        }
    }
    if stats {
        eprintln!(
            "stats: build {:.2}s ({:.2}ms/f) | render+readback {:.2}s ({:.2}ms/f) | write {:.2}s ({:.2}ms/f) | total {:.2}s",
            s_build, s_build * 1000.0 / total as f64,
            s_render, s_render * 1000.0 / total as f64,
            s_write, s_write * 1000.0 / total as f64,
            t0.elapsed().as_secs_f64()
        );
    }

        output.finish();
    eprintln!("done: {} frames in {:.1}s", total, t0.elapsed().as_secs_f32());
}
