//! Library surface of the replay renderer, used by the CLI (`main.rs`) and
//! by external embedders (e.g. the OPP live preview) that render frames on
//! demand into a window or read pixels back.

pub mod autoplay;
pub mod draw;
pub mod game;
pub mod hitsound;
pub mod hud;
pub mod pp;
pub mod render;
pub mod scene;
/// osu!(lazer) skinning abstraction port: user skin directories
/// (`--skin <dir>`) with the built-in argon skin as fallback.
pub mod skin;
/// 原生窗口直渲(窗口 surface,跨平台:Windows Win32 / Linux Xlib)。
/// 宿主以 raw window handle 传入自己的窗口;句柄类型经
/// [`raw_window_handle`] 再导出,宿主无需直接依赖该 crate。
pub mod surface;

pub use raw_window_handle;

use draw::{Atlas, Image, Region, TtfFont};
use skin::SkinTexture;

const TORUS_BOLD_TTF: &[u8] = include_bytes!("../assets/fonts/TorusPro-Bold.ttf");
const TORUS_SEMI_BOLD_TTF: &[u8] = include_bytes!("../assets/fonts/TorusPro-SemiBold.ttf");
const CURSOR_TRAIL_PNG: &[u8] = include_bytes!("../assets/cursor/cursortrail.png");
const REPEAT_EDGE_PNG: &[u8] = include_bytes!("../assets/cursor/repeat-edge-piece.png");
const APPROACH_CIRCLE_PNG: &[u8] = include_bytes!("../assets/cursor/approachcircle.png");

const COUNTER_DIGITS: [&[u8]; 10] = [
    include_bytes!("../assets/counter/argon-counter-0.png"),
    include_bytes!("../assets/counter/argon-counter-1.png"),
    include_bytes!("../assets/counter/argon-counter-2.png"),
    include_bytes!("../assets/counter/argon-counter-3.png"),
    include_bytes!("../assets/counter/argon-counter-4.png"),
    include_bytes!("../assets/counter/argon-counter-5.png"),
    include_bytes!("../assets/counter/argon-counter-6.png"),
    include_bytes!("../assets/counter/argon-counter-7.png"),
    include_bytes!("../assets/counter/argon-counter-8.png"),
    include_bytes!("../assets/counter/argon-counter-9.png"),
];
const COUNTER_DOT_PNG: &[u8] = include_bytes!("../assets/counter/argon-counter-dot.png");
const COUNTER_PERCENT_PNG: &[u8] = include_bytes!("../assets/counter/argon-counter-percentage.png");
const COUNTER_X_PNG: &[u8] = include_bytes!("../assets/counter/argon-counter-x.png");
const COUNTER_WIREFRAMES_PNG: &[u8] = include_bytes!("../assets/counter/argon-counter-wireframes.png");

/// Builds the atlas (fonts, counter digits, cursor pieces, skin
/// textures, optional background) plus the two font wrappers needed by
/// the scene builder. Skin textures are packed as `Region::Skin(i)`
/// entries and their handles handed back to the skin so its
/// `get_texture` serves atlas regions.
pub fn build_atlas(
    bg_image: Option<Image>,
    skin: &mut dyn skin::SkinTextureSource,
    max_dim: u32,
) -> (Atlas, TtfFont, TtfFont) {
    let (mut bold, mut bold_images) = TtfFont::rasterize(TORUS_BOLD_TTF, true);
    let (mut semibold, mut semibold_images) = TtfFont::rasterize(TORUS_SEMI_BOLD_TTF, false);

    let mut images: Vec<(Region, Image)> = Vec::new();
    if let Some(img) = bg_image {
        images.push((Region::Background, img));
    }
    images.append(&mut bold_images);
    images.append(&mut semibold_images);

    for (d, png) in COUNTER_DIGITS.iter().enumerate() {
        let (w, h, rgba) = decode_png_bytes(png).expect("embedded png");
        images.push((Region::CounterDigit(b'0' + d as u8), Image { width: w, height: h, rgba }));
    }
    for (png, region) in [
        (COUNTER_DOT_PNG, Region::CounterDot),
        (COUNTER_PERCENT_PNG, Region::CounterPercent),
        (COUNTER_X_PNG, Region::CounterX),
        (COUNTER_WIREFRAMES_PNG, Region::CounterWireframes),
    ] {
        let (w, h, rgba) = decode_png_bytes(png).expect("embedded png");
        images.push((region, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode_png_bytes(CURSOR_TRAIL_PNG).expect("embedded png");
        images.push((Region::CursorTrail, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode_png_bytes(REPEAT_EDGE_PNG).expect("embedded png");
        images.push((Region::RepeatEdge, Image { width: w, height: h, rgba }));
    }
    {
        let (w, h, rgba) = decode_png_bytes(APPROACH_CIRCLE_PNG).expect("embedded png");
        images.push((Region::ApproachCircle, Image { width: w, height: h, rgba }));
    }

    // Skin textures (`--skin <dir>` / built-in argon sprites): decode,
    // pack, then hand the atlas handles back to the skin.
    // Skin textures (`--skin <dir>` / built-in argon sprites): decode,
    // pack, then hand the atlas handles back to the skin. Packing is
    // capped at `max_dim` per axis (the GPU texture limit of the target
    // device): the packer first widens 4096 -> max_dim, and a skin that
    // still overflows is uniformly downscaled (lazer's
    // `MaxDimensionLimitedTextureLoaderStore` semantics: the display size
    // shrinks with the pixels) until it fits.
    let skin_images = skin.texture_images();
    let mut scale = 1.0f32;
    let (atlas, skin_regions) = loop {
        let mut images = images.clone();
        let mut skin_regions: Vec<(String, SkinTexture)> = Vec::with_capacity(skin_images.len());
        for (i, (name, img)) in skin_images.iter().enumerate() {
            let region = Region::Skin(i as u32);
            let scaled = if scale >= 1.0 { img.clone() } else { skin::legacy::downscale(img, scale) };
            images.push((region, scaled.clone()));
            skin_regions.push((
                name.clone(),
                SkinTexture { region, width: scaled.width, height: scaled.height, scale_adjust: 1.0 },
            ));
        }
        match Atlas::try_build(&images, max_dim) {
            Some(atlas) => break (atlas, skin_regions),
            None => {
                let new_scale = scale * 0.9;
                eprintln!(
                    "atlas: {}x{} overflow at max_dim {max_dim}, downscaling skin textures to {:.0}%",
                    max_dim, max_dim, new_scale * 100.0
                );
                scale = new_scale;
            }
        }
    };
    skin.assign_regions(&skin_regions);
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

/// Decodes a PNG or JPEG file into an atlas image (RGBA).
///
/// Malformed or placeholder-empty files (some skins ship 0-byte pngs to
/// disable an element) surface as `Err` instead of panicking - the
/// framework's `TextureLoaderStore.Get` swallows decode exceptions and
/// reports the texture as missing, so callers skip them the same way.
/// ImageSharp detects the format from the file content rather than the
/// extension, so a png-extension miss retries as jpeg.
pub fn decode_image_file(path: &std::path::Path) -> Result<Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let lower = path.to_string_lossy().to_lowercase();
    if !lower.ends_with(".png") {
        if let Ok(img) = decode_jpeg_bytes(&bytes) {
            return Ok(img);
        }
    }
    match decode_png_bytes(&bytes) {
        Ok((w, h, rgba)) => Ok(Image { width: w, height: h, rgba }),
        Err(png_err) => decode_jpeg_bytes(&bytes)
            .map_err(|jpeg_err| format!("{}: png: {}; jpeg: {}", path.display(), png_err, jpeg_err)),
    }
}

fn decode_jpeg_bytes(bytes: &[u8]) -> Result<Image, String> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
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
}

/// A value from the beatmap's `[General]` section (e.g. `AudioFilename`).
pub fn osu_general_value(map_path: &str, key: &str) -> Option<String> {
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
pub fn osu_background_file(map_path: &str) -> Option<String> {
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

pub fn decode_png_bytes(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // EXPAND: paletted (Indexed) skins decode to RGB, their tRNS chunks
    // to a real alpha channel - old skins ship both.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    // 16-bit PNGs (Photoshop-exported skins ship them, e.g. whole
    // scoreentry sets) decode to 2-byte samples; EXPAND does not reduce
    // bit depth. Take the high byte of each sample and expand to 8-bit
    // RGBA, exactly like the 8-bit paths below.
    let sixteen = info.bit_depth == png::BitDepth::Sixteen;
    match info.color_type {
        png::ColorType::Rgba if !sixteen => Ok((w, h, buf)),
        png::ColorType::Rgba => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(8) {
                rgba.extend_from_slice(&[px[0], px[2], px[4], px[6]]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::GrayscaleAlpha if !sixteen => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(4) {
                rgba.extend_from_slice(&[px[0], px[0], px[2], px[2]]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::Grayscale if !sixteen => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in &buf {
                rgba.extend_from_slice(&[*px, *px, *px, 255]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], 255]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::Rgb if !sixteen => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Ok((w, h, rgba))
        }
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in buf.chunks_exact(6) {
                rgba.extend_from_slice(&[px[0], px[2], px[4], 255]);
            }
            Ok((w, h, rgba))
        }
        other => Err(format!(
            "unsupported png colour type {:?} depth {:?}",
            other, info.bit_depth
        )),
    }
}
