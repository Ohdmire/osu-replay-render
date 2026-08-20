# osu-replay-render

用 wgpu 渲染 osu!standard 回放的 **lib + CLI** 二合一 crate,皮肤实现
osu!lazer 的 **Argon** 默认皮肤。判定与游戏状态来自
[osu-replay-judge](../osu-replay-judge)(逐事件与 lazer 全等的判定模拟器)。
支持两种使用方式:CLI 离屏渲染出视频/PNG;作为库嵌入其他程序做**窗口
surface 实时预览**(如 OPP 的回放实时预览页,`SurfaceRenderer`)。

## 用法

```
osu_replay_render <beatmap.osu> <replay.osr> [options]
```

| 选项 | 说明 |
| --- | --- |
| `--out <file.mp4>` | 管道输出到 ffmpeg 编码为 mp4 (h264, crf 18) |
| `--png-dir <dir>` | 输出 PNG 帧序列到目录 |
| `--size <WxH>` | 输出分辨率，默认 1920x1080 |
| `--fps <n>` | 输出帧率 1..480，默认 60（与游戏帧一一对应；其他帧率对光标做插值采样） |
| `--start <ms>` / `--end <ms>` | 渲染回放时间区间（毫秒） |
| `--score classic` | HUD 显示经典分（默认 standardised） |
| `--skin <argon\|argon-pro>` | 皮肤变体，默认 `argon-pro`（无 GREAT/PERFECT 判定文字、滑条身体透明度 0.92） |
| `--encoder <auto\|x264\|x265\|nvenc>` | 视频编码器：默认 `auto`（探测 NVENC 可用则用之，否则回退 x264）；NVENC 硬件编码（bgr0 直喂 + p5/hq/vbr/cq，**端到端约比 x264 快 1.7×、比 x265 快 3.2×**）；libx264 / libx265（preset medium + crf） |
| `--quality <n>` | crf（软件）/ cq（nvenc），默认 18 |
| `--no-guides` | 关闭 UR 条的窗口引导线（判定色色轴），默认开启渲染 |
| `--audio [file]` | 输出混入 BGM（AAC 192k）：带路径用指定文件；不带值自动取谱面 `[General] AudioFilename`（相对谱面目录）。音频位置按 lazer 时钟链换算：音频位置 = 回放时间 − 总偏移 |
| `--audio-offset <ms>` | BGM 对齐偏移，默认 **+15**（lazer Windows 平台偏移 `WINDOWS_BASE_AUDIO_OFFSET`；若你在游戏里开了实验性 WASAPI 或设了 AudioOffset/谱面偏移，把三项之和填进来） |
| `--bg` | 绘制谱面背景图（`[Events]` 的 `0,0,"..."`，相对谱面目录，PNG/JPEG），全屏铺满 |
| `--bg-opacity <0..1>` | 背景不透明度，默认 **0.3** = 1 − DimLevel（lazer `OsuSetting.DimLevel` 默认 0.7，与游戏内"背景暗度"语义一致） |
| `--limit <n>` | 最多渲染 n 帧（测试用） |

示例：

```
osu_replay_render map.osu replay.osr --out out.mp4
osu_replay_render map.osu replay.osr --png-dir frames --size 1280x720 --fps 30 --start 10000 --end 20000
```

帧时间轴与游戏内一致：60fps 时逐帧对应 lazer 的 FrameStability 游戏帧；
DT/HT 等 rate mod 的回放按真实游戏速度输出。渲染速度约 240fps（1080p
x264,瓶颈在 ffmpeg 编码；渲染管线本身约 0.6ms/帧,见下文「渲染流水线」）。

## 作为库使用（实时预览）

crate 同时提供 `[lib]`,渲染是**任意时刻 t 的纯函数**（`game::snapshot_at`
二分查找 + 光标插值），因此 seek/暂停/逐帧/倍速都是 O(1) 操作,天然适合
实时回放查看器:

```rust
use osu_replay_render::{build_atlas, draw, game, scene, surface};

// 一次性加载:判定引擎跑完整回放,得到逐帧快照时间轴。
let game = game::load("map.osu", "replay.osr")?;
let (atlas, bold, semibold) = build_atlas(None); // 可选背景图

// 窗口直渲:传 Win32 HWND,内部 1280x720 渲染后 letterbox 上屏。
let mut renderer = surface::SurfaceRenderer::new(1280, 720, &atlas, hwnd)?;
renderer.resize(w, h); // 窗口尺寸变化时重新配置 surface

let mut state = scene::SceneState::new(&game, 1280, 720);
let mut list = draw::DrawList::new();
let t = 12345.0; // 任意时刻,毫秒
let snap = game::snapshot_at(&game, t);
let assets = scene::Assets { atlas: &atlas, bold: &bold, semibold: &semibold };
list.clear();
state.build_frame(&game, &assets, &snap, &mut list);
list.finish();
renderer.render(&list, [0.055, 0.055, 0.075, 1.0]); // 呈现一帧
```

- `SurfaceRenderer`（`src/surface.rs`）：复用离屏管线渲染到内部纹理,再用
  letterbox blit 上屏 + `present(Fifo)` vsync;支持任意窗口尺寸,与宿主
  WebView UI 共存（OPP 中配合 `WS_EX_TRANSPARENT` 原生子窗口实现点击穿透）。
- 帧间无状态依赖:每帧独立由 t 决定,可乱序/跳跃求值,无需快进模拟。
- `duration = snapshots.last().time`、起点 `snapshots.first().time`,
  seek 时 clamp 到该区间即可。
- 离屏读回路径（`Renderer::render` / `render_deferred` + `read_oldest`
  三缓冲流水线）保持不变,CLI 与嵌入式逐帧导出均可用。

## 已实现（Argon）

- **圆**：四层渐变圆身（外填充/外渐变/内渐变/内填充）、白描边、combo
  数字（TorusPro Bold）、approach circle、命中动画复刻
  `ArgonMainCirclePiece.updateStateTransforms`：填充层 150ms OutQuint
  隐藏、数字 75ms 消失、外渐变延迟 12.5ms 变白（80ms）后线性淡出
  （150ms）、描边弹性收缩至 0.8×（400ms OutElasticHalf）+ 800ms 颜色
  渐变；色块闪光（FlashPiece 辉光，framework EdgeEffect `Hollow=false`
  数学：内部全亮、外侧二次衰减，半径 `OBJECT_RADIUS×0.6`）150ms
  OutQuint 弹入后 150ms 弹出——**色块远早于圈消失**；整块（描边圈）
  640ms（`fade_out_time×0.8`）OutQuad 淡出，圈最后消失；miss 100ms
  快速淡出。
- **滑条**：蛇形进入/退出（`ProgressAt` 镜像语义）、深色身体 + accent
  描边（20% 带宽）、滑条球（白环 + 渐变填充 + 方向箭头，入场
  200ms OutQuint 弹入；结束时按 `ArgonSliderBall` 源码**叠加 50ms
  OutQuint 额外淡出**（"intentionally pile on an extra FadeOut to make
  it happen much faster"），箭头同时收缩至 0.9×）、follow circle
  （2.4x 展开/释放/结束动画 + tick 脉冲）、tick 分数点、折返箭头
  （白色胶囊 + FontAwesome AngleDoubleRight 双箭头几何：两 chevron
  边缘相接不重叠、300ms 脉冲、首折返延迟淡入）、滑条头圆。尾部圆在
  Argon 皮肤中不可见（与 lazer 一致），身体收缩即尾动画。
- **Spinner**（按 `ArgonSpinnerDisc`/`SpinnerRotationTracker` 源码重写）：
  384u 大圆盘、25 个刻度标记（跟随**真实旋转的毫秒级阻尼**
  `Damp(0.99^elapsed_ms)` + 缓慢环境旋转；命中时额外 +180°）、弹出
  两阶段（0.5p-0.75p 到 0.3/0.2，p-1.5p 到 0.8/1.0）、顶部/底部圆弧
  （完成后 0.31→0.50 加粗，40ms 半衰期阻尼）、左右进度弧（静态背景
  完成时瞬时消失，进度弧 40ms 半衰期 + 青色辉光）、中心双环（tracking
  时 80→40 收缩，完成后冻结）、进度辉光填充（完成后每整圈脉冲闪烁）、
  SPM 计数（595ms 窗口，自首次 tracking 起淡入）、bonus 计数弹出
  （打满显示 MAX 2.8× 弹出）。
- **判定**：文字（GREAT/OK/MEH/MISS…，lazer Argon 单词风格、结果色、
  加色混合、miss 下落旋转）、环形爆炸粒子（按结果数量/距离缩放）、
  slider break 小圆点。
- **Follow points**：间距 32u 的双箭头，渐进滑入 + 淡出。
- **光标**：粉渐变环 + 内白环 + 中心点 + 青色辉光，按下弹性放大；
  光标轨迹（加色、(1-age)^4 衰减、300ms 生命）。
- **HUD**：楔形块、分数/准确率/连击计数器（argon-counter 官方纹理
  数字 + 线框背景 + ink 对齐度量）、数字滚动（250ms）、连击弹出/miss
  变红、血条（简化 HP 模拟 + 受伤闪红）、**UR 条**（水平置于屏幕底部
  居中，按 lazer `BarHitErrorMeter` 源码移植：刻度为**加色混合**判定
  色竖线（100ms 弹入至 0.6 后 5s 淡出收缩，最多 50 个）、判定色窗口
  引导线色轴（中心 Great 蓝向外 Ok 绿、Meh 黄，最外端渐隐；`--no-guides`
  可关闭，默认渲染）、Great 色中心圆标记、**EMA 均值小箭头**（指数
  移动平均 0.9/0.1，800ms OutQuint 滑动指向）、条上方实时 UR 数值；
  UR 事件集与 `ScoreProcessor.unstable_rate` 完全一致（`has_windows &&
  is_hit`，offset/rate），Welford 增量累计）。

## 资产

`assets/`(全部经 `include_bytes!` 内嵌进二进制,**运行时不依赖 CWD**,
可直接被其他程序作为库调用):
- `fonts/TorusPro-*.ttf` — 文字渲染（ab_glyph 启动时按 24/48/96 三档
  em 栅格化进图集）。
- `counter/argon-counter-*.png` — lazer 官方 HUD 计数器纹理数字
  （来自 [osu-resources](https://github.com/ppy/osu-resources)，MIT/CC-BY-NC 4.0）。
- `cursor/cursortrail.png` — 官方光标轨迹点。
- `cursor/cursor-smoke.png` — 官方烟迹粒子（64×64，来自 osu-resources；
  烟迹渲染实现后直接可用）。
- `spinner/spinner-glow.png` — 官方 spinner 侧弧辉光渐变条（1×107；
  lazer 中由 `ArgonSpinnerProgressArc.ProgressFill` 配合 SpinnerGlow
  shader 做径向采样——当前渲染器用加色 SDF 弧近似该辉光，此纹理留作
  后续实现对应 shader 时使用）。

> Argon/Argon-Pro 皮肤其余元素（圆、描边、辉光、滑条、spinner、光标
> 等）在 lazer 中全部为**程序化矢量绘制**（osu-resources 中除
> `argon-counter-*`、`approachcircle`、`repeat-edge-piece`、
> `cursortrail`、`cursor-smoke`、`spinner-glow` 外无 argon 纹理，
> `ring-glow`/`disc`/`number` 等仅用于 classic/legacy 皮肤，menu-cursor
> 仅菜单用），已在渲染器中以 SDF/图元逐一直接复刻，无可再提取的纹理
> 资产。

## 渲染架构

- wgpu 离屏（无窗口），DX12/Vulkan，4x MSAA；BGRA8 读回后喂给
  ffmpeg rawvideo 管道或写成 PNG;或经 `SurfaceRenderer` letterbox
  blit 到窗口 surface 直渲（present Fifo vsync）。
- 编码：BGRA 管道喂 ffmpeg。`nvenc` 以 bgr0 直喂 NVENC（硬件色彩转换，
  无 CPU swscale）；`x264`/`x265` 走 CPU swscale 转 yuv420p。
- 每帧 CPU 侧构建 `DrawList`：SDF 圆环/圆盘/辉光/胶囊/圆弧、纹理
  四边形、滑条身体带描边条带（MSAA 抗锯齿），按 alpha/加色混合分段
  绘制，绘制顺序复刻 `OsuPlayfield` 层级（spinner → follow points →
  判定爆炸 → 物件(早者在上) → 判定文字 → approach → 光标 → HUD）。
- 游戏状态：判定引擎跑完整回放后输出逐帧快照（光标插值位置、按键、
  slider tracking、spinner 旋转/进度）+ 判定事件时间轴，渲染端据此
  求值任意时刻的视觉状态（`game::snapshot_at` 支持任意输出帧率）。

## 渲染流水线（离屏 CLI 路径）

渲染与编码三级重叠，CPU 与 GPU 互不空等（`main.rs` 主循环 +
`render.rs` 的 `render_deferred`/`read_oldest_into`）：

1. **GPU 回读环形缓冲**（3 个 readback buffer）：每帧 `render_deferred`
   只提交不等待；积压达到深度 2 才 `read_oldest_into` 映射最老的一帧。
   CPU 建下一帧场景时 GPU 手里始终有已排队的渲染工作。
2. **独立写出线程 + 有界通道**（深度 3,天然背压）：帧数据经
   `SyncSender` 发给拥有 ffmpeg stdin 的写出线程,每帧单次
   `write_all`;编码慢时阻塞在 `send` 而不是拖慢渲染。
3. **帧缓冲复用**：写出线程用完的 `Vec<u8>`（1080p 约 8MB）经回传
   通道还回渲染线程,`read_oldest_into` 直接从 GPU 映射区拷入,
   每帧零大分配、单次拷贝。

实测（1080p60, x264 medium crf18, 1800 帧）：流水线化前 ~165fps,
之后 ~245fps,基本贴住 ffmpeg 单独编码的上限（~255fps）;渲染管线
本身（提交+回读）仅 ~0.6ms/帧。输出与流水线化前逐字节一致。
PNG 模式不走流水线（`Renderer::render` 同步路径）。

## 依赖

- Rust (`cargo build --release`)
- ffmpeg 在 PATH（`--out` 模式需要;库/surface 路径不需要）
- 判定引擎：`../osu-replay-judge`（path dependency，已加入逐帧快照、
  combo 信息输出与 `Engine::windows()` OD 窗口 getter 的修改）
- 作为库嵌入时:宿主窗口需提供原生窗口句柄(当前为 Win32 HWND;
  `raw-window-handle` 0.6)

## 开发

- 源码结构：`lib.rs`（库入口:图集构建/资产解码,全部贴图内嵌）、
  `main.rs`（CLI/编码管线）、`surface.rs`（窗口 surface 直渲 +
  letterbox blit,实时预览用）、`game.rs`（judge 输出 → 渲染
  视图 + UR/HP 时间轴）、`scene.rs`（Argon 皮肤全部视觉逻辑）、
  `hud.rs`（计数器/血条/UR 条）、`draw.rs`（SDF 图元/缓动/字体/图集）、
  `render.rs`（wgpu 离屏渲染器 + WGSL shader + readback 流水线）。
- 所有动画时序均以 osu!lazer 源码（`osu.Game.Rulesets.Osu/Skinning/Argon/`
  等）与 osu-framework（`Interpolation.Damp` 系列为**毫秒指数**语义）
  为准；调参前先对照源文件注释。
- 实际嵌入示例：[OPP](../OPP) 的 `src-tauri/src/live_render.rs`
  （Tauri 原生子窗口 + 播放线程 + seek/play/pause 命令,前端配合
  DOM 位置上报与可拖动进度条）。
