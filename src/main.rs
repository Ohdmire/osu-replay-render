//! osu_replay_render: offscreen osu! replay renderer (wgpu + Argon skin).
//!
//! Usage:
//!   osu_replay_render <beatmap.osu> [replay.osr] [options]
//!
//! Options:
//!   --autoplay             Generate the replay from the beatmap (lazer
//!                          OsuAutoGenerator port) - beatmap preview, no
//!                          .osr needed
//!   --out <file.mp4>       Pipe frames to ffmpeg and encode (default if given)
//!   --png-dir <dir>        Write PNG frames to a directory instead
//!   --size <WxH>           Output size (default 1920x1080)
//!   --fps <n>              Output fps (default 60; frames are sampled from
//!                          the 60fps game-frame snapshots)
//!   --start <ms>           Start time in replay ms (default: beginning)
//!   --end <ms>             End time in replay ms (default: end)
//!   --score classic        Show classic (stable-style) score
//!   --limit <n>            Render at most n frames (testing)

use osu_replay_render::{build_atlas, decode_image_file, draw, draw::Image, game, osu_background_file, osu_general_value, render::Renderer, scene};

use scene::{Assets, SceneState};
use std::io::Write;

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
    /// auto (default: probe NVENC, fall back to x264) | x264 | x265 | nvenc.
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
    /// Autoplay mod: generate the replay from the beatmap itself (lazer
    /// `OsuAutoGenerator` port) instead of reading an .osr — beatmap
    /// preview without a replay file.
    autoplay: bool,
}

fn parse_args() -> Result<(Options, String, Option<String>), String> {
    let args: Vec<String> = std::env::args().collect();
    // `--autoplay` (Autoplay mod, beatmap preview) makes the replay file
    // optional; positionals stay "map first, replay second, then flags".
    let autoplay = args.iter().any(|a| a == "--autoplay");
    let min_args = if autoplay { 2 } else { 3 };
    if args.len() < min_args {
        return Err(format!("usage: {} <beatmap.osu> [replay.osr] [--autoplay] [--out file.mp4] [--png-dir dir] [--size WxH] [--fps n] [--start ms] [--end ms] [--score classic] [--skin argon|argon-pro] [--no-guides] [--audio [file.mp3]] [--bg] [--bg-opacity 0..1] [--limit n]", args.get(0).map(|s| s.as_str()).unwrap_or("osu_replay_render")));
    }
    let map_path = args[1].clone();
    let replay_path = if autoplay { None } else { Some(args[2].clone()) };
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
        encoder: "auto".to_string(),
        quality: 18,
        probe: None,
        limit: None,
        ffmpeg_extra: Vec::new(),
        guides: true,
        audio: None,
        bg: false,
        bg_opacity: 0.3,
        audio_offset: 15.0,
        autoplay,
    };
    let mut i = min_args;
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
                if !matches!(enc.as_str(), "auto" | "x264" | "x265" | "nvenc") {
                    return Err("--encoder must be auto, x264, x265 or nvenc".into());
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
            "--autoplay" => {
                opts.autoplay = true;
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
        i += 1;
    }
    Ok((opts, map_path, replay_path))
}

enum Output {
    /// Frames are sent to a dedicated writer thread through a bounded
    /// channel (backpressure); the thread owns the ffmpeg child, writes
    /// each frame with ONE write (contiguous at typical widths, else a
    /// single repack), and joins at `finish`. Spent frame buffers are
    /// recycled back through `ret` so the render thread never reallocates
    /// the ~8MB readback payload per frame.
    Ffmpeg {
        tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
        ret: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
        handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    },
    PngDir(String),
    None,
}

impl Output {
    /// Returns a buffer to fill the next frame into: a recycled one from
    /// the writer thread when available, a fresh allocation otherwise.
    fn take_buf(&mut self, frame_bytes: usize) -> Vec<u8> {
        match self {
            Output::Ffmpeg { ret, .. } => {
                if let Some(rx) = ret {
                    if let Ok(mut buf) = rx.try_recv() {
                        buf.clear();
                        buf.reserve(frame_bytes);
                        return buf;
                    }
                }
                Vec::with_capacity(frame_bytes)
            }
            _ => Vec::with_capacity(frame_bytes),
        }
    }

    /// Takes ownership of `buf` (frame data, padded rows) and queues it to
    /// the writer thread. Cheap unless the writer is more than
    /// `WRITER_QUEUE` frames behind (natural backpressure).
    fn write_frame(&mut self, mut buf: Vec<u8>, width: u32, height: u32, stride: u32, index: usize) -> std::io::Result<()> {
        match self {
            Output::Ffmpeg { tx, .. } => {
                let tx = tx.as_ref().expect("writer channel");
                if stride != width * 4 {
                    // Repack padded rows into one contiguous buffer so the
                    // writer thread does a single write per frame.
                    let mut tight = Vec::with_capacity((width * height * 4) as usize);
                    for row in 0..height as usize {
                        let start = row * stride as usize;
                        tight.extend_from_slice(&buf[start..start + width as usize * 4]);
                    }
                    buf = tight;
                }
                let _ = index;
                tx.send(buf).map_err(|_| std::io::Error::other("ffmpeg writer exited"))
            }
            Output::PngDir(dir) => {
                let data: &[u8] = &buf;
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
                        rgba[d] = data[s + 2];
                        rgba[d + 1] = data[s + 1];
                        rgba[d + 2] = data[s];
                        rgba[d + 3] = data[s + 3];
                    }
                }
                writer.write_image_data(&rgba)?;
                Ok(())
            }
            Output::None => Ok(()),
        }
    }

    /// Drops the sender (EOF for the writer thread), then joins it: the
    /// thread drains the queue, closes ffmpeg's stdin and waits for the
    /// encode to finish.
    fn finish(mut self) {
        if let Output::Ffmpeg { tx, ret, handle } = &mut self {
            drop(tx.take());
            drop(ret.take());
            match handle.take().unwrap().join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("ffmpeg writer error: {}", e),
                Err(_) => eprintln!("ffmpeg writer panicked"),
            }
        }
    }
}

/// Bound of the frame queue to the ffmpeg writer thread (frames).
const WRITER_QUEUE: usize = 3;

fn main() {
    let (opts, map_path, replay_path) = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };

    let game = match &replay_path {
        Some(rp) => {
            eprintln!("loading {} + {}", map_path, rp);
            game::load(&map_path, rp)
        }
        None => {
            eprintln!("loading {} (autoplay preview)", map_path);
            game::load_autoplay(&map_path)
        }
    };
    let game = match game {
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

    // auto: probe NVENC with a tiny test encode, fall back to x264.
    let encoder = if opts.encoder == "auto" {
        let probe = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i", "nullsrc=s=256x256:d=0.04",
                   "-c:v", "h264_nvenc", "-f", "null", "-"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if probe.map(|st| st.success()).unwrap_or(false) { "nvenc" } else { "x264" }
    } else {
        opts.encoder.as_str()
    };
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
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.arg("-y")
            .arg("-f").arg("rawvideo")
            .arg("-pix_fmt").arg(in_pix_fmt)
            .arg("-s").arg(format!("{}x{}", opts.width, opts.height))
            .arg("-r").arg(format!("{}", opts.fps))
            .arg("-i").arg("-");
        cmd.args(&encode_args);
        // With BGM the audio is muxed in a SECOND ffmpeg pass after
        // rendering (see below): muxing audio directly on the raw pipe can
        // grow ffmpeg's interleave queue without bound when fed fast.
        let video_tmp = if audio_path.is_some() {
            format!("{}.video.tmp.mp4", out)
        } else {
            out.clone()
        };
        let mut child = cmd
            .arg(&video_tmp)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create("ffmpeg_err.log").unwrap())
            .spawn()
            .expect("failed to spawn ffmpeg (is it on PATH? or use --png-dir)");
        // Dedicated writer thread: owns ffmpeg's stdin, one write per
        // frame, decoupled from the render loop by a bounded channel.
        // Buffers are recycled back to the render thread after use.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(WRITER_QUEUE);
        let (ret_tx, ret_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let handle = std::thread::spawn(move || {
            let mut stdin = child.stdin.take().expect("ffmpeg stdin");
            for frame in rx {
                stdin.write_all(&frame)?;
                let _ = ret_tx.send(frame);
            }
            drop(stdin); // EOF -> ffmpeg finishes the encode
            let status = child.wait()?;
            if !status.success() {
                return Err(std::io::Error::other(format!("ffmpeg exited with {:?}", status.code())));
            }
            Ok(())
        });
        Output::Ffmpeg { tx: Some(tx), ret: Some(ret_rx), handle: Some(handle) }
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
    // Index of the next frame to be written out; lags behind the frame
    // being submitted while the pipeline is running ahead.
    let mut written = 0usize;

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
        // Pipelined: submit this frame WITHOUT waiting for the GPU, and
        // only read back the OLDEST in-flight frame once the pipeline has
        // reached depth 2. Until then the GPU renders ahead while the CPU
        // keeps building the next frame; reading immediately after submit
        // would serialize CPU and GPU again.
        renderer.render_deferred(&list, [0.055, 0.055, 0.075, 1.0]);
        let tc = std::time::Instant::now();
        if renderer.pending_len() >= 2 {
            let mut buf = output.take_buf((renderer.padded_row as usize) * opts.height as usize);
            renderer.read_oldest_into(&mut buf);
            if let Err(e) = output.write_frame(buf, opts.width, opts.height, renderer.padded_row, written) {
                eprintln!("error writing frame {}: {}", written, e);
                std::process::exit(1);
            }
            written += 1;
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

    // Drain the pipeline's last frame(s) into the writer before finishing.
    while renderer.pending_len() > 0 {
        let mut buf = output.take_buf((renderer.padded_row as usize) * opts.height as usize);
        renderer.read_oldest_into(&mut buf);
        if let Err(e) = output.write_frame(buf, opts.width, opts.height, renderer.padded_row, written) {
            eprintln!("error writing final frame: {}", e);
            std::process::exit(1);
        }
        written += 1;
    }
        output.finish();

    // Second pass: mux the BGM into the video (stream copy + AAC). The
    // audio is seeked to the first frame's replay time minus the lazer
    // clock offset, exactly as before.
    if let Some(audio) = &audio_path {
        if let Some(out) = &opts.out {
            let audio_start = ((frame_times.first().map(|t| *t).unwrap_or(0.0) - opts.audio_offset) / 1000.0).max(0.0);
            let tmp = format!("{}.video.tmp.mp4", out);
            eprintln!("muxing audio: {}", audio);
            let status = std::process::Command::new("ffmpeg")
                .arg("-y")
                .arg("-v").arg("error")
                .arg("-i").arg(&tmp)
                .arg("-ss").arg(format!("{:.3}", audio_start))
                .arg("-i").arg(audio)
                .arg("-map").arg("0:v")
                .arg("-map").arg("1:a")
                .arg("-c:v").arg("copy")
                // Pin the video track timescale to the fps so each frame is
                // exactly one tick and the container reports avg_frame_rate
                // 60/1; a source with microsecond timestamps (e.g. the
                // raw-h264 demuxer rounds 1/60s to 16667us) would otherwise
                // yield 1000000/16667 (~59.9988).
                .arg("-video_track_timescale").arg(opts.fps.round().max(1.0).to_string())
                .arg("-c:a").arg("aac").arg("-b:a").arg("192k")
                .arg("-shortest")
                .arg("-movflags").arg("+faststart")
                .arg(out)
                .status()
                .expect("ffmpeg audio mux");
            let _ = std::fs::remove_file(&tmp);
            if !status.success() {
                eprintln!("error: audio mux failed");
                std::process::exit(1);
            }
        }
    }
    eprintln!("done: {} frames in {:.1}s", total, t0.elapsed().as_secs_f32());
}
