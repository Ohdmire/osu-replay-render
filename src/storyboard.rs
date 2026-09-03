//! Beatmap storyboard layer (`--storyboard`): renders the map's storyboard
//! (difficulty `.osu` `[Events]` merged with the set's shared `.osb`, the
//! osu! stable semantics) through the `osu-storyboard-render` library.
//!
//! The storyboard is composited into two offscreen Rgba8Unorm textures —
//! below (Background/Fail/Pass) and above (Foreground/Overlay) the
//! playfield, matching osu!'s layer order — and GPU-copied every frame
//! into full-frame atlas slots (`Region::Storyboard` /
//! `Region::StoryboardForeground`) that the scene draws like the
//! background image. No readback, no extra scene passes.
//!
//! Background replacement (lazer `Storyboard.ReplacesBackground` +
//! `Player.storyboardReplacesBackground`): when the storyboard's
//! Background layer contains an element referencing the beatmap's own
//! background file, the host must NOT draw `Region::Background` — the
//! storyboard draws that image itself. See [`ParsedStoryboard::replaces_background`].
//!
//! Storyboard video (lazer `StoryboardVideo` / `DrawableStoryboardVideo`):
//! the first `Video,offset,"file"` element renders in the dedicated Video
//! layer (behind the Background layer), centred, cover-filling the screen,
//! fading in 500ms from its start time and out 500ms before its end.
//! Frames arrive either from an ffmpeg rawvideo pipe (desktop CLI) or a
//! JNI mailbox fed by Kotlin's MediaCodec (Android) — both land in a
//! single wgpu texture updated in place (`write_texture`).

use crate::draw::{Atlas, Region};
use osu_storyboard_render::osb::model::Layer;
use osu_storyboard_render::osb::timeline::{CompiledStoryboard, FailState};
use osu_storyboard_render::render::renderer::{
    build_draws_filtered, Draw, GpuInstance, Renderer as SbRenderer,
};
use osu_storyboard_render::render::texture::Assets as SbAssets;
#[cfg(not(target_os = "android"))]
use std::io::Read;
use std::path::PathBuf;

/// storyboard 贴图的 GPU 内存预算(解码后 RGBA 字节)。视频式逐帧动画的
/// storyboard 可引用上千张独立贴图,超出预算按 LRU 淘汰,下次用到重传。
#[cfg(not(target_os = "android"))]
const GPU_BUDGET: usize = 512 << 20;
#[cfg(target_os = "android")]
const GPU_BUDGET: usize = 256 << 20;
/// CPU 解码缓存预算(字节);同上,超限随机淘汰换重解码。
#[cfg(not(target_os = "android"))]
const CACHE_BUDGET: usize = 384 << 20;
#[cfg(target_os = "android")]
const CACHE_BUDGET: usize = 192 << 20;

/// 视频纹理在精灵渲染器里的键。
const VIDEO_KEY: &str = "\0sb-video";

/// 谱面视频元素(lazer `PrimaryVideo`:Video 层第一个)。
#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// `Video,offset,"file"` 的 offset(map 毫秒)。
    pub start_ms: f32,
    /// 视频时长(毫秒);0 = 未知(无结尾淡出)。
    pub duration_ms: f32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// Parsed storyboard, GPU-independent: built before the renderer exists so
/// `build_atlas` can reserve the composite slots.
pub struct ParsedStoryboard {
    compiled: CompiledStoryboard,
    assets: SbAssets,
    foreground: bool,
    replaces_background: bool,
    video: Option<VideoInfo>,
    /// lazer `DrawableStoryboard` 的宽屏判定特例:故事板只有视频元素时
    /// 即使谱面 `WidescreenStoryboard: 0` 也按 16:9 容器布局(老图常见,
    /// 视频独占故事板)。视口若按 4:3 处理,视频会被放得更大、裁掉更多,
    /// 看起来像异常拉伸。
    video_only: bool,
}

/// 在目录里大小写不敏感地找文件名(视频/素材常与声明大小写不符)。
fn resolve_file(dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let lower = name.to_lowercase();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_lowercase() == lower)
                .unwrap_or(false)
        })
}

/// ffprobe 视频流参数:`csv=p=0` 输出形如 `1280,720,2997/100,220.553654`
/// (宽,高,平均帧率,时长秒;时长缺失时回退 format=duration)。
/// `ffprobe` 为宿主注入的完整路径(danser 发行包/手动路径场景 PATH 里
/// 没有),None 时退回 PATH。
#[cfg(not(target_os = "android"))]
fn probe_video(info: &mut VideoInfo, ffprobe: Option<&std::path::Path>) {
    let bin = ffprobe.unwrap_or_else(|| std::path::Path::new("ffprobe"));
    let run = |entries: &str| -> Option<String> {
        let out = std::process::Command::new(bin)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", entries, "-of", "csv=p=0"])
            .arg(&info.path)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let Some(csv) = run("stream=width,height,avg_frame_rate,duration") else { return };
    let mut parts = csv.split(',');
    if let (Some(w), Some(h)) = (parts.next().and_then(|v| v.parse().ok()), parts.next().and_then(|v| v.parse().ok())) {
        info.width = w;
        info.height = h;
    }
    if let Some(rate) = parts.next() {
        info.fps = match rate.split_once('/') {
            Some((a, b)) if b.parse::<f64>().map_or(false, |b| b > 0.0) => {
                a.parse::<f64>().unwrap_or(0.0) / b.parse::<f64>().unwrap_or(1.0)
            }
            _ => rate.parse().unwrap_or(0.0),
        };
    }
    // 显示矩阵旋转(手机拍摄视频):ffmpeg rawvideo 输出的帧已按元数据
    // 转置,而 stream width/height 是编码方向尺寸 —— ±90°/±270° 时必须
    // 交换宽高,否则帧按错误尺寸上传纹理,画面错乱变形。
    if let Some(rot) = run("stream_side_data=rotation")
        .and_then(|v| v.lines().next().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string))
        .and_then(|v| v.parse::<f32>().ok())
        .map(f32::round)
    {
        if (rot as i32).rem_euclid(180) == 90 {
            std::mem::swap(&mut info.width, &mut info.height);
        }
    }
    let secs: Option<f64> = parts.next().and_then(|v| v.parse().ok());
    let secs = match secs {
        Some(s) if s > 0.0 => Some(s),
        // 容器无流级时长时查 format=duration。
        _ => run("format=duration").and_then(|v| v.parse().ok()),
    };
    if let Some(s) = secs {
        info.duration_ms = (s * 1000.0) as f32;
    }
}

/// Parses the beatmap's storyboard (no GPU state). `None` when the map has
/// none (no Events elements besides the old-style background row, which
/// the host already draws as `Region::Background`). `beatmap_background`
/// is the `[Events]` background filename parsed with the beatmap.
pub fn parse_beatmap(
    map_path: &std::path::Path,
    beatmap_background: Option<&str>,
) -> Option<ParsedStoryboard> {
    parse_beatmap_bins(map_path, beatmap_background, None)
}

/// [`parse_beatmap`] + ffprobe 注入:宿主解析出的 ffprobe 完整路径
/// (danser 发行包、设置页手动路径等 PATH 之外的来源);None 时退回
/// PATH 查找。ffmpeg 的注入见 [`StoryboardLayer::set_video_bins`]。
pub fn parse_beatmap_bins(
    map_path: &std::path::Path,
    beatmap_background: Option<&str>,
    ffprobe: Option<&std::path::Path>,
) -> Option<ParsedStoryboard> {
    let loaded = osu_storyboard_render::loader::load_beatmap(map_path, true)?;
    let root = loaded.root.clone();
    let mut story = loaded.story;

    // 视频元素(lazer PrimaryVideo:第一个 Video)。桌面侧顺手 ffprobe
    // 尺寸/帧率/时长(Android 由 Kotlin MediaExtractor 上报)。
    let mut video = story.videos.first().and_then(|v| {
        resolve_file(&root, &v.path).map(|path| VideoInfo {
            path,
            start_ms: v.start_time,
            duration_ms: 0.0,
            width: 0,
            height: 0,
            fps: 0.0,
        })
    });
    if let Some(v) = &mut video {
        #[cfg(not(target_os = "android"))]
        probe_video(v, ffprobe);
        #[cfg(not(target_os = "android"))]
        if v.width == 0 || v.height == 0 {
            // probe 失败将导致 pump_video 永不解码(width 门槛),必须
            // 留下诊断线索(常见原因:ffprobe 不在 PATH/注入目录)。
            eprintln!(
                "storyboard: ffprobe 视频参数探测失败({:?}),视频层禁用;ffprobe bin = {:?}",
                v.path,
                ffprobe.unwrap_or_else(|| std::path::Path::new("ffprobe"))
            );
        }
    }

    // 接管型背景裸精灵剔除(移植层规则):编辑器把谱面背景写进 .osb 时
    // 生成的是无命令、Background 层、引用背景文件本身的常驻精灵
    // (compile 视作 start=0/end=∞/alpha=1 永久铺底)。宿主开故事板时
    // 已按 sb_replaces_bg 隐藏自己的背景层,再保留它就成了"故事板下面
    // 还垫一层背景"。剔除条件与下方 ReplacesBackground 的判定同源
    // (引用同一背景文件),仅收紧到"完全无命令"——带命令的用法是
    // 真实故事板内容,保留。
    if let Some(bg) = beatmap_background {
        let bg = osu_storyboard_render::render::texture::normalize_path(bg).to_lowercase();
        story.elements.retain(|e| {
            let s = e.sprite();
            !(s.layer == Layer::Background
                && s.commands.is_empty()
                && s.loops.is_empty()
                && s.triggers.is_empty()
                && osu_storyboard_render::render::texture::normalize_path(&s.path).to_lowercase()
                    == bg)
        });
        // 剔除后连一个元素都没有(且没有视频) = 这个"故事板"只是接管
        // 背景,宿主按无故事板处理(自己的背景层照常画)。
        if story.elements.is_empty() && story.videos.is_empty() {
            return None;
        }
    }

    // 背景抑制(lazer `Storyboard.ReplacesBackground`):Background 层存在
    // 引用谱面背景文件的元素。旧版背景行已被 loader 剔除(lazer 的解码器
    // 同样不把它算作 storyboard 元素),因此这里比较的是 .osb/手写精灵。
    let replaces_background = beatmap_background
        .map(|bg| {
            let bg = osu_storyboard_render::render::texture::normalize_path(bg).to_lowercase();
            story.elements.iter().any(|e| {
                e.sprite().layer == Layer::Background
                    && osu_storyboard_render::render::texture::normalize_path(&e.sprite().path)
                        .to_lowercase()
                        == bg
            })
        })
        .unwrap_or(false);

    let foreground = story
        .elements
        .iter()
        .any(|e| matches!(e.sprite().layer, Layer::Foreground | Layer::Overlay));
    // lazer onlyHasVideoElements:背景行已被 loader 剔除、接管型背景裸
    // 精灵已在上面剔除,剩下的元素为空且带视频 = 只有视频的故事板。
    let video_only = story.elements.is_empty() && !story.videos.is_empty();
    let compiled = CompiledStoryboard::compile(story);
    let mut assets = SbAssets::disk(&root);
    assets.set_cache_budget(CACHE_BUDGET);
    Some(ParsedStoryboard { compiled, assets, foreground, replaces_background, video, video_only })
}

impl ParsedStoryboard {
    /// Whether the storyboard uses Foreground/Overlay layers (drives the
    /// `Region::StoryboardForeground` atlas slot and the scene's above-
    /// playfield draw).
    pub fn has_foreground(&self) -> bool {
        self.foreground
    }

    /// Whether the host must hide `Region::Background` while this
    /// storyboard renders (lazer `storyboardReplacesBackground`).
    pub fn replaces_background(&self) -> bool {
        self.replaces_background
    }

    /// The map's storyboard video, if any (path/offset resolved).
    pub fn video(&self) -> Option<&VideoInfo> {
        self.video.as_ref()
    }

    /// Builds the GPU layer on the host's device/queue. `width`/`height`
    /// must match the reserved atlas slots.
    pub fn into_layer(
        self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) -> StoryboardLayer {
        let make_tex = |label: &str| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let mut sb = SbRenderer::new(device, queue);
        sb.set_gpu_budget(GPU_BUDGET);
        let video = self.video.map(|info| VideoState {
            info,
            source: None,
            finished: false,
            frame_pts: f64::NEG_INFINITY,
        });
        let replaces_bg = self.replaces_background;
        StoryboardLayer {
            compiled: self.compiled,
            assets: self.assets,
            sb,
            below: make_tex("storyboard below"),
            above: self.foreground.then(|| make_tex("storyboard above")),
            width,
            height,
            video,
            ffmpeg_bin: None,
            video_only: self.video_only,
            replaces_bg,
            elements_enabled: true,
            video_enabled: true,
        }
    }
}

/// 桌面视频源:ffmpeg rawvideo(RGBA)管道,顺序解码。
#[cfg(not(target_os = "android"))]
struct VideoPipe {
    child: std::process::Child,
    stdout: std::process::ChildStdout,
    frame: Vec<u8>,
    /// 下一帧的 map 时间(ms)。
    next_pts_ms: f64,
    step_ms: f64,
}

#[cfg(not(target_os = "android"))]
impl VideoPipe {
    /// `ffmpeg` 为宿主注入的完整路径;None 时退回 PATH。
    fn spawn(info: &VideoInfo, from_map_ms: f64, ffmpeg: Option<&std::path::Path>) -> Option<VideoPipe> {
        let bin = ffmpeg.unwrap_or_else(|| std::path::Path::new("ffmpeg"));
        let seek_s = ((from_map_ms - info.start_ms as f64).max(0.0) / 1000.0).max(0.0);
        let mut cmd = std::process::Command::new(bin);
        cmd.args(["-v", "error", "-nostdin"]).stdout(std::process::Stdio::piped());
        if seek_s > 0.01 {
            cmd.arg("-ss").arg(format!("{seek_s:.3}"));
        }
        // -i 必不可少:裸路径会被 ffmpeg 当作输出文件,因"没有输入流"
        // 立即退出,管道一个字节都读不到(stderr 已丢弃,完全静默)。
        cmd.arg("-i")
            .arg(&info.path)
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                // spawn 失败会被当成"视频已耗尽"静默跳过,必须留诊断
                // (常见原因:ffmpeg 不在 PATH/注入路径)。
                eprintln!(
                    "storyboard: 视频解码管道启动失败({:?}): {error}",
                    info.path
                );
                return None;
            }
        };
        let stdout = child.stdout.take()?;
        let frame = vec![0u8; (info.width * info.height * 4) as usize];
        let step_ms = if info.fps > 0.0 { 1000.0 / info.fps } else { 33.0 };
        let next_pts_ms = info.start_ms as f64 + seek_s * 1000.0;
        Some(VideoPipe { child, stdout, frame, next_pts_ms, step_ms })
    }

    /// 读取一帧到 self.frame;EOF/错误返回 false。
    fn read_frame(&mut self) -> bool {
        let mut off = 0;
        while off < self.frame.len() {
            match self.stdout.read(&mut self.frame[off..]) {
                Ok(0) | Err(_) => return false,
                Ok(n) => off += n,
            }
        }
        true
    }
}

#[cfg(not(target_os = "android"))]
impl Drop for VideoPipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 视频运行时:源(管道或 Android 邮箱投递的帧)+ 状态。
struct VideoState {
    info: VideoInfo,
    /// 桌面:ffmpeg 管道(Android 编译期排除)。None = 尚未启动。
    #[cfg(not(target_os = "android"))]
    source: Option<VideoPipe>,
    /// Android:Kotlin MediaCodec 经 JNI 投递的最新帧(w, h, rgba)。
    #[cfg(target_os = "android")]
    source: Option<(u32, u32, Vec<u8>)>,
    /// 解码器已耗尽(管道 EOF)。
    finished: bool,
    /// 当前纹理里帧的 map 时间(ms);NEG_INFINITY = 尚无帧。
    frame_pts: f64,
}

/// The GPU half: offscreen composites + the library's sprite renderer,
/// all on the host renderer's device/queue.
pub struct StoryboardLayer {
    compiled: CompiledStoryboard,
    assets: SbAssets,
    sb: SbRenderer,
    below: wgpu::Texture,
    above: Option<wgpu::Texture>,
    width: u32,
    height: u32,
    video: Option<VideoState>,
    /// 视频解码 ffmpeg 路径(None = PATH);由宿主在首次渲染前注入。
    ffmpeg_bin: Option<PathBuf>,
    /// lazer 特例:只有视频元素的故事板按 16:9 视口布局(见
    /// ParsedStoryboard::video_only)。
    video_only: bool,
    /// lazer `storyboardReplacesBackground`:宿主不应再画 Region::Background。
    replaces_bg: bool,
    /// 元素层开关(`--storyboard`):off 时只画视频层,精灵层全部跳过。
    elements_enabled: bool,
    /// 视频层开关(`--video`):off 时不解码也不画视频。
    video_enabled: bool,
}

impl StoryboardLayer {
    /// 合成层视口是否按宽屏(lazer `DrawableStoryboard` 的宽度 = 高 ×
    /// 16/9 或 4/3):谱面 `WidescreenStoryboard` 为真,或故事板只有
    /// 视频元素(`onlyHasVideoElements` 特例,老图视频独占时 lazer 无视
    /// 谱面标记按宽屏布局)。
    fn view_widescreen(&self) -> bool {
        self.compiled.widescreen || self.video_only
    }

    pub fn has_foreground(&self) -> bool {
        self.above.is_some()
    }

    /// 背景抑制(lazer `storyboardReplacesBackground`)。
    pub fn replaces_background(&self) -> bool {
        self.replaces_bg
    }

    /// 谱面视频信息(路径/起始/时长)。
    pub fn video(&self) -> Option<&VideoInfo> {
        self.video.as_ref().map(|v| &v.info)
    }

    /// 元素层开关(`--storyboard`):off 时本层只承载视频。
    pub fn set_elements_enabled(&mut self, on: bool) {
        self.elements_enabled = on;
    }

    pub fn elements_enabled(&self) -> bool {
        self.elements_enabled
    }

    /// 视频解码用的 ffmpeg 完整路径(宿主解析:手动路径 > PATH > danser
    /// 发行包);None 时库内退回 PATH 查找。须在首次渲染前设置。
    pub fn set_video_bins(&mut self, ffmpeg: Option<&std::path::Path>) {
        self.ffmpeg_bin = ffmpeg.map(std::path::Path::to_path_buf);
    }

    /// 视频层开关(`--video`):off 时不解码(Android 侧邮箱也随之静默)
    /// 也不画视频。
    pub fn set_video_enabled(&mut self, on: bool) {
        self.video_enabled = on;
    }

    pub fn video_enabled(&self) -> bool {
        self.video_enabled
    }

    /// Android 侧投递一帧解码视频(GL 读回)。缓冲应已为顶左行序
    /// (与桌面 ffmpeg 路径一致),方向由读回端保证,绘制不翻。
    pub fn write_video_frame(&mut self, w: u32, h: u32, rgba: &[u8]) {
        if let Some(v) = &mut self.video {
            self.sb.write_frame(VIDEO_KEY, w, h, rgba);
            v.info.width = v.info.width.max(w);
            v.info.height = v.info.height.max(h);
        }
    }



    /// 标记视频时长(Kotlin MediaExtractor 探明后上报;驱动结尾淡出)。
    pub fn set_video_duration(&mut self, ms: f32) {
        if let Some(v) = &mut self.video {
            v.info.duration_ms = ms;
        }
    }

    /// Renders the storyboard at map time `t` (ms) and copies the two
    /// composites into the atlas slots of `out`. Call before the frame's
    /// scene submission; the copies are queue-ordered ahead of it.
    ///
    /// Fail/Pass: a replay renderer has no fail state — the Pass layer
    /// shows (a passing run), like the standalone renderer's default.
    pub fn render(&mut self, t: f32, out: &mut crate::render::Renderer, atlas: &Atlas) {
        self.render_ext(t, out, atlas, None);
    }

    /// [`render`] + Android 外部投递的视频帧(先落纹理再取 draw)。
    pub fn render_ext(
        &mut self,
        t: f32,
        out: &mut crate::render::Renderer,
        atlas: &Atlas,
        ext_frame: Option<&(u32, u32, Vec<u8>)>,
    ) {
        if let Some((w, h, rgba)) = ext_frame {
            self.write_video_frame(*w, *h, rgba);
        }
        if self.video_enabled {
            self.pump_video(t);
        }

        let mut below_draws = if self.elements_enabled {
            build_draws_filtered(
                &mut self.sb,
                &mut self.assets,
                &self.compiled,
                t,
                FailState::Pass,
                |layer| !matches!(layer, Layer::Foreground | Layer::Overlay),
            )
        } else {
            Vec::new()
        };
        if self.video_enabled {
            if let Some(d) = self.video_draw(t) {
                below_draws.insert(0, d);
            }
        }
        let view = self.below.create_view(&Default::default());
        self.sb.render(
            &view,
            wgpu::TextureFormat::Rgba8Unorm,
            self.width,
            self.height,
            self.view_widescreen(),
            &below_draws,
            [0.0, 0.0, 0.0, 0.0],
        );
        out.copy_into_atlas(&self.below, atlas, Region::Storyboard);

        if let Some(above) = &self.above {
            let above_draws = if self.elements_enabled {
                build_draws_filtered(
                    &mut self.sb,
                    &mut self.assets,
                    &self.compiled,
                    t,
                    FailState::Pass,
                    |layer| matches!(layer, Layer::Foreground | Layer::Overlay),
                )
            } else {
                Vec::new()
            };
            let view = above.create_view(&Default::default());
            self.sb.render(
                &view,
                wgpu::TextureFormat::Rgba8Unorm,
                self.width,
                self.height,
                self.view_widescreen(),
                &above_draws,
                [0.0, 0.0, 0.0, 0.0],
            );
            out.copy_into_atlas(above, atlas, Region::StoryboardForeground);
        }
    }

    /// 推进桌面视频解码器到时刻 t(顺序读取所有 pts ≤ t 的帧,最后一帧
    /// 落纹理;渲染时间单调递增,与管道节奏天然同步)。
    #[cfg(not(target_os = "android"))]
    fn pump_video(&mut self, t: f32) {
        let (w, h) = match &self.video {
            Some(v) if !v.finished && v.info.width > 0 => (v.info.width, v.info.height),
            _ => return,
        };
        let v = self.video.as_mut().unwrap();
        if v.source.is_none() {
            // 懒启动:接近视频开始再 spawn,-ss 直接跳到当前渲染位置。
            if t as f64 + 1000.0 < v.info.start_ms as f64 {
                return;
            }
            let info = v.info.clone();
            let ffmpeg = self.ffmpeg_bin.clone();
            v.source = VideoPipe::spawn(&info, t as f64, ffmpeg.as_deref());
            if v.source.is_none() {
                v.finished = true;
                return;
            }
        }
        let mut updated = false;
        if let Some(pipe) = &mut v.source {
            while pipe.next_pts_ms <= t as f64 {
                if !pipe.read_frame() {
                    v.finished = true;
                    break;
                }
                v.frame_pts = pipe.next_pts_ms;
                pipe.next_pts_ms += pipe.step_ms;
                updated = true;
            }
        }
        if updated {
            if let Some(pipe) = &v.source {
                self.sb.write_frame(VIDEO_KEY, w, h, &pipe.frame);
            }
        }
    }

    /// Android:邮箱帧由 render_ext 落纹理,这里只更新时间戳。
    #[cfg(target_os = "android")]
    fn pump_video(&mut self, _t: f32) {}

/// 视频层 draw(lazer `DrawableStoryboardVideo`:居中、Fill 铺满、
/// 起始 500ms 淡入、结尾前 500ms 淡出;Video 层在 Background 之下)。
    fn video_draw(&self, t: f32) -> Option<Draw> {
        let v = self.video.as_ref()?;
        if self.sb.texture(VIDEO_KEY).is_none() {
            return None; // 还没有任何帧
        }
        let (w, h) = (v.info.width, v.info.height);
        if w == 0 || h == 0 {
            return None;
        }
        let (start, dur) = (v.info.start_ms, v.info.duration_ms);
        if t < start {
            return None;
        }
        let mut alpha = ((t - start) / 500.0).min(1.0);
        if dur > 0.0 {
            let end = start + dur;
            if t > end {
                return None;
            }
            alpha = alpha.min(((end - t) / 500.0).max(0.0));
        }
        if alpha <= 0.001 {
            return None;
        }
        // 覆盖式铺满视口(lazer DrawableStoryboardVideo:RelativeSizeAxes
        // .Both + FillMode.Fill = 等比放大到铺满、超出裁切,居中)。
        let size = video_cover_size(w, h, self.width, self.height, self.view_widescreen());
        Some(Draw {
            texture: VIDEO_KEY.to_string(),
            additive: false,
            instance: GpuInstance {
                pos: [320.0, 240.0],
                size,
                anchor: [0.5, 0.5],
                rotation: 0.0,
                color: [1.0, 1.0, 1.0, alpha],
                flip: [0.0, 0.0],
                _pad: [0.0; 3],
            },
        })
    }
}

/// 视频层显示尺寸(lazer `DrawableStoryboardVideo`:`RelativeSizeAxes.Both`
/// + `FillMode.Fill`,即等比放大到铺满视口、超出部分裁切、锚点居中)。
/// 坐标系为 osu! storyboard 的 640×480(高固定 480;宽屏时视口宽按合成
/// 层纵横比扩展,非宽屏固定 4:3)。返回 (宽, 高)。
fn video_cover_size(video_w: u32, video_h: u32, layer_w: u32, layer_h: u32, widescreen: bool) -> [f32; 2] {
    let view_w = if widescreen {
        480.0 * layer_w as f32 / layer_h.max(1) as f32
    } else {
        640.0
    };
    let scale = (view_w / video_w as f32).max(480.0 / video_h as f32);
    [video_w as f32 * scale, video_h as f32 * scale]
}

#[cfg(test)]
mod opp_video_chain_tests {
    use super::*;

    /// 端到端验证(本机需 ffmpeg/ffprobe):Video 行解析 → ffprobe 探测
    /// (注入绝对路径)→ ffmpeg 管道按帧解码出 RGBA。
    #[test]
    fn video_chain_resolves_probes_and_decodes() {
        if std::process::Command::new("ffprobe")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skip: ffprobe 不可用");
            return;
        }
        let dir = std::env::temp_dir().join("opp-sb-video-test");
        std::fs::create_dir_all(&dir).expect("dirs");
        let video = dir.join("v.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i", "testsrc=duration=1:size=160x120:rate=10", "-pix_fmt", "yuv420p", "-y"])
            .arg(&video)
            .status()
            .expect("ffmpeg lavfi");
        assert!(made.success(), "生成测试视频失败(缺 lavfi?)");
        let osu = dir.join("test.osu");
        std::fs::write(
            &osu,
            "osu file format v14\n[Events]\nVideo,0,\"v.mp4\"\n",
        )
        .expect("write osu");

        // PATH 探测 + 注入探测(用 which 结果模拟宿主解析的绝对路径)。
        let parsed = parse_beatmap(&osu, None).expect("storyboard 解析");
        let info = parsed.video().expect("video 元素解析");
        assert_eq!((info.width, info.height), (160, 120), "ffprobe 探测尺寸");
        assert!(info.duration_ms > 900.0, "ffprobe 探测时长");

        let mut info2 = info.clone();
        probe_video(&mut info2, None);
        assert_eq!((info2.width, info2.height), (160, 120), "注入路径探测");

        let mut pipe = VideoPipe::spawn(&info2, 0.0, None).expect("解码管道启动");
        assert!(pipe.read_frame(), "读出第一帧 RGBA");
        assert!(pipe.frame.len() == 160 * 120 * 4, "帧尺寸");
        drop(pipe);
        // -ss 起播路径(懒启动从中途 spawn)同样要能出帧。
        let mut seeked = VideoPipe::spawn(&info2, 500.0, None).expect("seek 管道启动");
        assert!(seeked.read_frame(), "seek 后读出帧 RGBA");

        // 显示矩阵旋转:ffmpeg rawvideo 已转置输出帧,probe 必须交换宽高,
        // 否则 write_frame 按错误尺寸上传,画面错乱变形。
        let plain = dir.join("plain.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i", "testsrc=duration=1:size=160x120:rate=10", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-y"])
            .arg(&plain)
            .status()
            .expect("ffmpeg plain");
        if status.success() {
            let rotated = dir.join("rotated.mp4");
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-nostdin", "-display_rotation", "90", "-i"])
                .arg(&plain)
                .args(["-c", "copy", "-y"])
                .arg(&rotated)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                let mut rot = VideoInfo {
                    path: rotated,
                    start_ms: 0.0,
                    duration_ms: 0.0,
                    width: 0,
                    height: 0,
                    fps: 0.0,
                };
                probe_video(&mut rot, None);
                assert_eq!((rot.width, rot.height), (120, 160), "旋转视频交换宽高");
            } else {
                eprintln!("skip: 本机 ffmpeg 不支持 -display_rotation 转封装");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 视频显示尺寸严格等比(lazer DrawableStoryboardVideo:
    /// RelativeSizeAxes.Both + FillMode.Fill = 等比铺满、超出裁切)。
    #[test]
    fn video_cover_size_preserves_aspect_ratio() {
        let ratio = |s: [f32; 2]| s[0] / s[1];
        // 16:9 视频在 16:9 层:恰好铺满。
        let full = video_cover_size(1280, 720, 1280, 720, true);
        assert!((full[0] - 853.333).abs() < 0.5 && (full[1] - 480.0).abs() < 0.01);
        // 4:3 视频在 16:9 层:宽撑满、高超出裁切,宽高比不变。
        let mut wide_layer = video_cover_size(640, 480, 1280, 720, true);
        assert!(wide_layer[0] >= 853.32 && wide_layer[1] > 480.0);
        assert!((ratio(wide_layer) - 640.0 / 480.0).abs() < 1e-3);
        // 16:9 视频在 4:3 视口(非宽屏图):高撑满、宽超出,宽高比不变。
        wide_layer = video_cover_size(1280, 720, 1280, 720, false);
        assert!((wide_layer[1] - 480.0).abs() < 1e-3 && wide_layer[0] > 640.0);
        assert!((ratio(wide_layer) - 1280.0 / 720.0).abs() < 1e-3);
        // 旋转修正后的竖屏视频在 16:9 层:等比、宽撑满、高超出裁切。
        let portrait = video_cover_size(720, 1280, 1280, 720, true);
        assert!((portrait[0] - 853.333).abs() < 0.5 && portrait[1] > 480.0);
        assert!((ratio(portrait) - 720.0 / 1280.0).abs() < 1e-3);
    }

    /// lazer `DrawableStoryboard` 特例:只有视频元素的故事板即使
    /// `WidescreenStoryboard: 0` 也按 16:9 视口布局(osu.Game
    /// DrawableStoryboard.cs 的 onlyHasVideoElements 分支);视口若按
    /// 4:3 处理,视频会被放得更大、裁掉更多,观感为异常拉伸。
    #[test]
    fn video_only_storyboard_forces_widescreen_view() {
        let dir = std::env::temp_dir().join("opp-sb-videoonly-test");
        std::fs::create_dir_all(&dir).expect("dirs");
        let osu = dir.join("nonwide-videoonly.osu");
        std::fs::write(
            &osu,
            "osu file format v14\n[General]\nWidescreenStoryboard: 0\n[Events]\nVideo,0,\"v.mp4\"\n",
        )
        .expect("write osu");
        let parsed = parse_beatmap(&osu, None).expect("只有视频的故事板可解析");
        assert!(parsed.video_only, "只有视频 → 强制宽屏视口");
        assert!(!parsed.compiled.widescreen, "谱面本身仍是非宽屏标记");

        // 对照:同一张图多一个精灵(视频文件缺失不影响判定)→ 保持 4:3。
        let osu2 = dir.join("nonwide-sprites.osu");
        std::fs::write(
            &osu2,
            "osu file format v14\n[General]\nWidescreenStoryboard: 0\n[Events]\nVideo,0,\"v.mp4\"\nSprite,Background,Centre,\"a.png\",320,240\n",
        )
        .expect("write osu2");
        let parsed2 = parse_beatmap(&osu2, None).expect("带精灵的故事板可解析");
        assert!(!parsed2.video_only, "有精灵 → 按谱面标记 4:3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
