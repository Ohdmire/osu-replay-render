//! Game state construction: turns the judge crate's outputs (processed
//! beatmap + engine run) into render-friendly views.

use crate::draw::Colour;
use osu_replay_judge::engine::Engine;

pub use osu_replay_judge::engine::FrameSnap;
use osu_replay_judge::mods::Mods;
use osu_replay_judge::process::{NestedKind, ProcKind, ProcObject};
use osu_replay_judge::score::HitResult;
use osu_replay_judge::{beatmap, process, replay};

/// Argon skin combo colours (`ArgonSkin.CustomComboColours`), used when the
/// beatmap defines none. Indexed by `ComboIndex % len` (1-based, so the
/// first combo is GREEN - lazer matches the standard green-blue-red-yellow
/// progression from slot 1).
pub const ARGON_COMBO_COLOURS: [u32; 6] = [0xF17400, 0x00F135, 0x0052F1, 0xF10000, 0xE8EB00, 0x5C00F1];

/// `SkinConfiguration.DefaultComboColours` - the classic default-skin
/// colours. Only used by skins WITHOUT custom combo colours (Argon has
/// them, so these never apply here); kept for parity with lazer.
#[allow(dead_code)]
pub const DEFAULT_COMBO_COLOURS: [u32; 4] = [0xFFC000, 0x00CA00, 0x127CFF, 0xF21839];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjKind {
    Circle,
    Slider,
    Spinner,
}

#[derive(Clone, Copy, Debug)]
pub struct NestedView {
    pub kind: NestedKind,
    pub time: f64,
    /// Playfield position (includes slider position + stack).
    pub position: [f32; 2],
    pub span_index: usize,
    pub path_progress: f64,
    /// Judgement from the timeline (time, result), if any.
    pub judged: Option<(f64, HitResult)>,
}

#[derive(Clone)]
pub struct ObjView {
    pub index: usize,
    pub kind: ObjKind,
    pub start_time: f64,
    pub end_time: f64,
    /// Stacked start position (playfield coords).
    pub position: [f32; 2],
    /// Stacked end position (slider tail / circle).
    pub end_position: [f32; 2],
    pub radius: f32,
    pub scale: f32,
    pub preempt: f64,
    pub fade_in: f64,
    /// Whether this object starts a new combo (`IHasCombo.NewCombo`).
    pub new_combo: bool,
    pub colour: Colour,
    /// `IHasComboInformation.ComboIndexWithOffsets` - the index skin combo
    /// colour lookups use (`SkinComboColourLookup.ColourIndex`).
    pub combo_colour_index: u32,
    pub number: u32,
    /// Slider: full piecewise-linear path in playfield coords (relative to
    /// the slider position, i.e. ready to add to `position`).
    pub slider_points: Vec<[f32; 2]>,
    /// Cumulative arc length of `slider_points` (same length; `[0] == 0`).
    /// Lets `position_at_path` / `sub_path` binary-search instead of walking
    /// the whole path on every call.
    pub slider_cum: Vec<f32>,
    pub slider_len: f32,
    /// Slider body path, built once: `slider_points` with pathological
    /// "scribble" paths (length >> bounding box, e.g. seven thousand points
    /// zig-zagging inside 2 px) simplified away. Empty when the exact path is
    /// already small enough (the body then slices `slider_points` directly).
    pub body_points: Vec<[f32; 2]>,
    /// Cumulative arc length of `body_points` (empty together with it).
    pub body_cum: Vec<f32>,
    /// Arc length of the effective body path (`slider_len` when not
    /// simplified).
    pub body_len: f32,
    pub slider_distance: f64,
    pub span_count: usize,
    pub duration: f64,
    pub nested: Vec<NestedView>,
    pub spins_required: f64,
    /// `Spinner.MaximumBonusSpins`: extra spins that award bonus score
    /// after `SpinsRequired + 2`. Drives the Argon bonus counter.
    pub maximum_bonus_spins: i64,
    /// Timeline events: head judgement (slider), body/final judgement.
    pub head_judged: Option<(f64, HitResult)>,
    pub body_judged: Option<(f64, HitResult)>,
    /// Press times at which the note lock rejected a click on this object
    /// (`ClickAction.Shake`). Sorted; drives [`crate::shake`].
    pub shake_times: Vec<f64>,
}

impl ObjView {
    /// Position of the slider ball at overall progress [0..1] (playfield
    /// coords, includes stack).
    pub fn slider_ball_at(&self, progress: f64) -> [f32; 2] {
        let sc = self.span_count as f64;
        let mut p = (progress * sc) % 1.0;
        let span = (progress * sc) as i32;
        if span % 2 == 1 {
            p = 1.0 - p;
        }
        self.position_at_path(p)
    }

    pub fn position_at_path(&self, progress: f64) -> [f32; 2] {
        let pts = &self.slider_points;
        if pts.len() < 2 || self.slider_len <= 1e-6 {
            return self.position;
        }
        let target = progress.clamp(0.0, 1.0) as f32 * self.slider_len;
        let i = self
            .slider_cum
            .partition_point(|&c| c <= target)
            .saturating_sub(1)
            .min(pts.len() - 2);
        let seg_start = self.slider_cum[i];
        let seg_end = self.slider_cum[i + 1];
        let t = if seg_end > seg_start {
            ((target - seg_start) / (seg_end - seg_start)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (a, b) = (pts[i], pts[i + 1]);
        [
            self.position[0] + a[0] + (b[0] - a[0]) * t,
            self.position[1] + a[1] + (b[1] - a[1]) * t,
        ]
    }

    /// Sub-path (playfield coords, absolute) between path progress p0..p1.
    /// Binary search on the cached arc lengths; only the points inside the
    /// range are copied.
    pub fn sub_path(&self, p0: f64, p1: f64) -> Vec<[f32; 2]> {
        let (mut p0, mut p1) = (p0.clamp(0.0, 1.0), p1.clamp(0.0, 1.0));
        if p0 > p1 {
            std::mem::swap(&mut p0, &mut p1);
        }
        let pts = &self.slider_points;
        if pts.len() < 2 || self.slider_len <= 1e-6 {
            return Vec::new();
        }
        let d0 = p0 as f32 * self.slider_len;
        let d1 = p1 as f32 * self.slider_len;

        let last = pts.len() - 2;
        let i0 = self.slider_cum.partition_point(|&c| c <= d0).saturating_sub(1).min(last);
        // `d0 == d1` exactly on a vertex leaves `i1` one segment *before*
        // `i0`: `cum[k]` belongs to segment k for `i0` (`c <= d0`), but the
        // strict `c < d1` scan stops at k-1. Clamp so the capacity/loop below
        // can never underflow.
        let i1 = self
            .slider_cum
            .partition_point(|&c| c < d1)
            .saturating_sub(1)
            .min(last)
            .max(i0);

        let lerp_at = |i: usize, d: f32| -> [f32; 2] {
            let seg_start = self.slider_cum[i];
            let seg_end = self.slider_cum[i + 1];
            let t = if seg_end > seg_start {
                ((d - seg_start) / (seg_end - seg_start)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let a = pts[i];
            let b = pts[i + 1];
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
        };

        let mut out: Vec<[f32; 2]> = Vec::with_capacity(i1 - i0 + 2);
        out.push(lerp_at(i0, d0));
        for i in i0..=i1 {
            let seg_end = self.slider_cum[i + 1];
            if seg_end < d1 {
                out.push(pts[i + 1]);
            }
        }
        out.push(lerp_at(i1, d1));

        // Drop consecutive duplicates: zero-length segments degenerate the
        // stroke strip (path_to_progress can emit the boundary vertex twice).
        let mut dedup: Vec<[f32; 2]> = Vec::with_capacity(out.len());
        for p in out {
            if let Some(last) = dedup.last() {
                if (last[0] - p[0]).abs() < 0.05 && (last[1] - p[1]).abs() < 0.05 {
                    continue;
                }
            }
            dedup.push(p);
        }
        if dedup.len() == 1 {
            dedup.push(dedup[0]);
        }
        dedup.into_iter().map(|p| [self.position[0] + p[0], self.position[1] + p[1]]).collect()
    }

    /// Slider body capsules for path progress `p0..p1` (absolute playfield
    /// coords). Slices the body path cached at build time instead of walking
    /// the exact path (`sub_path`) every frame: two binary searches plus a
    /// handful of lerps, so degenerate paths cost nothing per frame.
    pub fn body_segments(&self, p0: f64, p1: f64) -> Vec<([f32; 2], [f32; 2])> {
        let (pts, cum, total) = self.body_path();
        if pts.len() < 2 || total <= 1e-6 {
            return Vec::new();
        }

        let (mut p0, mut p1) = (p0.clamp(0.0, 1.0), p1.clamp(0.0, 1.0));
        if p0 > p1 {
            std::mem::swap(&mut p0, &mut p1);
        }

        let d0 = p0 as f32 * total;
        let d1 = p1 as f32 * total;

        let last = pts.len() - 2;
        let i0 = cum.partition_point(|&c| c <= d0).saturating_sub(1).min(last);
        // Same vertex-boundary guard as `sub_path`.
        let i1 = cum
            .partition_point(|&c| c < d1)
            .saturating_sub(1)
            .min(last)
            .max(i0);

        let mut out = Vec::with_capacity(i1 - i0 + 1);
        for i in i0..=i1 {
            let seg_start = cum[i];
            let seg_end = cum[i + 1];
            if seg_end <= seg_start {
                continue;
            }
            let a = lerp_segment(pts, cum, i, d0.max(seg_start));
            let b = lerp_segment(pts, cum, i, d1.min(seg_end));
            out.push((
                [self.position[0] + a[0], self.position[1] + a[1]],
                [self.position[0] + b[0], self.position[1] + b[1]],
            ));
        }

        if out.is_empty() {
            // Zero-length slice (snake just started): emit a point capsule so
            // the body still shows a dot at the head.
            let a = lerp_segment(pts, cum, i0, d0);
            let p = [self.position[0] + a[0], self.position[1] + a[1]];
            out.push((p, p));
        }

        out
    }

    /// Effective body path: the simplified one when it exists, otherwise the
    /// exact slider path with its cached arc lengths.
    fn body_path(&self) -> (&[[f32; 2]], &[f32], f32) {
        if self.body_points.is_empty() {
            (&self.slider_points, &self.slider_cum, self.slider_len)
        } else {
            (&self.body_points, &self.body_cum, self.body_len)
        }
    }
}

/// Linear interpolation on segment `i` of a cached polyline at arc length
/// `distance`.
fn lerp_segment(pts: &[[f32; 2]], cum: &[f32], i: usize, distance: f32) -> [f32; 2] {
    let seg_start = cum[i];
    let seg_end = cum[i + 1];
    let t = if seg_end > seg_start {
        ((distance - seg_start) / (seg_end - seg_start)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let a = pts[i];
    let b = pts[i + 1];
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Upper bound for the cached body path. Normal sliders are far below this;
/// only pathological (Aspire) paths exceed it. It also bounds the per-frame
/// capsule count and the RDP work, so loading can never degrade into a
/// quadratic simplification.
const MAX_BODY_POINTS: usize = 4096;
/// Paths that stay inside this box while walking many times their extent are
/// "scribbles" (the classic Aspire pattern). At screen resolution their shape
/// carries no information, so a handful of chords is enough.
const SCRIBBLE_EXTENT: f32 = 8.0;
const SCRIBBLE_POINTS: usize = 96;

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    dx * dx + dy * dy
}

/// Builds the cached body path: drop consecutive duplicates, then simplify
/// paths that are either huge or "scribbles".
///
/// `body_cum` keeps the **exact** arc length of every kept vertex (measured
/// on the unsimplified path). Simplifying shortens the polyline, and mapping
/// snake progress by the shortened length made the body trail/lead the ball
/// on complex sliders, so the body's distance axis stays the exact one.
///
/// The RDP input is capped by a uniform arc-length pre-pass: without it a
/// 160k-point path made `simplify_rdp` quadratic (Flashbacklog's 1:07 slider
/// hung the loader for minutes).
fn build_body_path(points: &[[f32; 2]]) -> (Vec<[f32; 2]>, Vec<f32>) {
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(points.len());
    for &p in points {
        if let Some(l) = pts.last() {
            if dist2(*l, p) < 1e-6 {
                continue;
            }
        }
        pts.push(p);
    }
    if pts.len() < 8 {
        return (Vec::new(), Vec::new());
    }

    let cum = cumulative(&pts);
    let total = *cum.last().unwrap();

    // Bounding box plus the indices of its extreme points: a scribble's
    // silhouette is defined by those, and the uniform pre-pass alone can skip
    // them (e.g. every sample landing on the "high" leg of a 2 px zig-zag).
    let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
    let mut ext = [0usize; 4];
    for (i, p) in pts.iter().enumerate() {
        if p[0] < min[0] {
            min[0] = p[0];
            ext[0] = i;
        }
        if p[0] > max[0] {
            max[0] = p[0];
            ext[1] = i;
        }
        if p[1] < min[1] {
            min[1] = p[1];
            ext[2] = i;
        }
        if p[1] > max[1] {
            max[1] = p[1];
            ext[3] = i;
        }
    }
    let extent = (max[0] - min[0]).max(max[1] - min[1]);

    let scribble = extent <= SCRIBBLE_EXTENT && total > 16.0 * extent.max(1.0);
    if !scribble && pts.len() <= MAX_BODY_POINTS {
        return (Vec::new(), Vec::new());
    }

    let limit = if scribble { SCRIBBLE_POINTS } else { MAX_BODY_POINTS };
    let mut sub: Vec<usize> = if pts.len() > limit {
        uniform_indices(&cum, limit)
    } else {
        (0..pts.len()).collect()
    };
    if scribble {
        sub.extend_from_slice(&ext);
        sub.sort_unstable();
        sub.dedup();
    }
    let sub_pts: Vec<[f32; 2]> = sub.iter().map(|&i| pts[i]).collect();

    let eps = if scribble { (extent * 0.4).clamp(0.4, 2.0) } else { 0.25 };
    let kept = simplify_rdp_indices(&sub_pts, eps);

    let idx: Vec<usize> = if kept.len() < sub.len() {
        kept.iter().map(|&j| sub[j]).collect()
    } else {
        sub
    };
    if idx.len() >= pts.len() {
        return (Vec::new(), Vec::new());
    }

    let body_points = idx.iter().map(|&i| pts[i]).collect();
    let body_cum = idx.iter().map(|&i| cum[i]).collect();
    (body_points, body_cum)
}

/// Prefix sums of segment lengths (`cum[0] == 0`, `cum[i]` is the arc length
/// up to point `i`).
fn cumulative(points: &[[f32; 2]]) -> Vec<f32> {
    let mut cum = Vec::with_capacity(points.len());
    let mut len = 0.0f32;
    if !points.is_empty() {
        cum.push(0.0);
    }
    for i in 1..points.len() {
        len += dist2(points[i - 1], points[i]).sqrt();
        cum.push(len);
    }
    cum
}

/// Evenly spaced (by arc length) vertex indices, endpoints included. Caps the
/// RDP input so its worst case stays linear-ish instead of quadratic.
fn uniform_indices(cum: &[f32], limit: usize) -> Vec<usize> {
    let n = cum.len();
    if n <= 2 || limit < 3 {
        return vec![0, n.saturating_sub(1)];
    }
    let total = cum[n - 1];
    let step = (total / (limit - 1) as f32).max(1e-6);
    let mut out = Vec::with_capacity(limit);
    out.push(0);
    let mut next = step;
    for i in 1..n - 1 {
        if cum[i] >= next {
            out.push(i);
            next = cum[i] + step;
        }
    }
    out.push(n - 1);
    out
}

/// Iterative Ramer-Douglas-Peucker (a recursive implementation would blow the
/// stack on long paths). Returns the indices of the kept vertices.
fn simplify_rdp_indices(points: &[[f32; 2]], eps: f32) -> Vec<usize> {
    if points.len() < 3 {
        return (0..points.len()).collect();
    }

    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;

    let mut stack = vec![(0usize, points.len() - 1)];
    let eps2 = eps * eps;

    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let a = points[first];
        let b = points[last];
        let ab = dist2(a, b);

        let mut best = -1.0f32;
        let mut best_index = first;
        for (i, p) in points.iter().enumerate().take(last).skip(first + 1) {
            let d2 = if ab <= 1e-12 {
                dist2(a, *p)
            } else {
                let t = (((p[0] - a[0]) * (b[0] - a[0]) + (p[1] - a[1]) * (b[1] - a[1])) / ab)
                    .clamp(0.0, 1.0);
                let q = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                dist2(q, *p)
            };
            if d2 > best {
                best = d2;
                best_index = i;
            }
        }

        if best > eps2 {
            keep[best_index] = true;
            stack.push((first, best_index));
            stack.push((best_index, last));
        }
    }

    (0..points.len()).filter(|&i| keep[i]).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JudgementDisplay {
    /// Judgement text ("GREAT", "MISS", ...) at the event position.
    Text,
    /// Small additive dot (slider tick miss).
    TickMiss,
    /// No display.
    None,
}

#[derive(Clone)]
pub struct EventView {
    pub time: f64,
    /// Playfield position.
    pub position: [f32; 2],
    pub result: HitResult,
    pub display: JudgementDisplay,
    /// Object scale (judgements are scaled by CS).
    pub scale: f32,
    pub combo: i32,
    pub score: i64,
    pub classic_score: i64,
    pub accuracy: f64,
}

/// Full timeline score state (every judgement, not just displayed ones).
#[derive(Clone, Copy)]
pub struct ScoreEvent {
    pub time: f64,
    pub combo: i32,
    pub score: i64,
    pub classic_score: i64,
    pub accuracy: f64,
}

/// One timed hit for the unstable-rate bar: the same event set the engine
/// counts toward UR (`has_windows && is_hit`), with the cumulative UR and
/// mean offset after including it.
#[derive(Clone, Copy)]
pub struct UrEvent {
    /// Judgement time (raw, ms).
    pub time: f64,
    /// Hit error in real ms (`time_offset / gameplay_rate`).
    pub offset: f64,
    pub result: HitResult,
    /// Cumulative UR (Welford, `10 * stddev`) after this hit.
    pub ur: f64,
    /// Cumulative mean offset (ms) after this hit.
    pub mean: f64,
}

/// Cumulative key-tap counts after each press edge, for the key overlay
/// (lazer `InputCountController`: one count per `Activate`, i.e. per rising
/// edge). Index order matches `KEY_ACTIONS`: Z (K1), X (K2), C (smoke).
#[derive(Clone, Copy)]
pub struct KeyCountEvent {
    pub time: f64,
    /// Cumulative counts [z, x, c] after the edge at `time`.
    pub counts: [u32; 3],
}

/// Keys shown in the key overlay, in display order. osu! default bindings:
/// Z = K1 (left), X = K2 (right), C = smoke.
pub const KEY_ACTIONS: [&str; 3] = ["Z", "X", "C"];

/// One circle-judgement hit with the spatial data the results screen's
/// `AccuracyHeatmap` needs (lazer `HitEvent` for `HitCircle`s, with
/// `LastHitObject` and `CursorPositionAtHit`).
#[derive(Clone, Copy, Debug)]
pub struct ResultsHitEvent {
    /// Judgement time (gameplay clock ms).
    pub time: f64,
    /// Raw hit offset (gameplay clock ms, `HitEvent.TimeOffset`).
    pub offset: f64,
    pub result: HitResult,
    /// Stacked playfield position of the circle that was hit.
    pub pos: [f32; 2],
    /// Stacked position of the previous circle (None on the first).
    pub last_pos: Option<[f32; 2]>,
    /// Cursor position (playfield coords) at the hit — lazer
    /// `ClosestPressPosition`: the press closest to the object centre
    /// between spawn and judgement, at the replay frame's exact cursor.
    pub cursor: [f32; 2],
    /// Circle radius in playfield units (CS-scaled `OBJECT_RADIUS`).
    pub radius: f32,
}

/// Beatmap `[Metadata]` (results-screen header): lazer prefers the
/// romanised `Title`/`Artist` with the unicode variants as fallback.
#[derive(Clone, Debug, Default)]
pub struct MapMeta {
    pub title: String,
    pub artist: String,
    /// Difficulty name (`Version`).
    pub version: String,
    pub creator: String,
}

impl MapMeta {
    /// From the parsed `[Metadata]`: lazer prefers the romanised
    /// `Title`/`Artist` with the unicode variants as fallback.
    pub fn from_metadata(m: &osu_parse::beatmap::BeatmapMetadata) -> MapMeta {
        let pick = |romanised: &str, unicode: &str| {
            if romanised.is_empty() { unicode.to_string() } else { romanised.to_string() }
        };
        MapMeta {
            title: pick(&m.title, &m.title_unicode),
            artist: pick(&m.artist, &m.artist_unicode),
            version: m.version.clone(),
            creator: m.creator.clone(),
        }
    }
}

pub struct GameData {
    pub score_events: Vec<ScoreEvent>,
    /// Timed hits for the unstable-rate bar (UR-relevant events only).
    pub ur_events: Vec<UrEvent>,
    /// OD hit windows (great, ok, meh) in ms.
    pub hit_windows: (f64, f64, f64),
    pub objects: Vec<ObjView>,
    pub events: Vec<EventView>,
    pub snapshots: Vec<FrameSnap>,
    /// Base combo palette from `build()`: the beatmap's `[Colours]` when
    /// present, else the argon fallback. `apply_skin_combo_colours`
    /// re-maps object colours from this, so it must stay pristine.
    pub combo_colours: Vec<Colour>,
    /// Whether the beatmap shipped its own `[Colours]` (lazer: the
    /// `LegacyBeatmapSkin` lookup hits, so the user skin's combo colours
    /// are never reached).
    pub has_beatmap_colours: bool,
    /// `[Colours]` entries that are not `Combo1..8` (`SliderBorder`,
    /// `SliderTrackOverride`, `SliderBall`, `SpinnerBackground`, ...) from
    /// `osu_parse::beatmap::Beatmap::custom_colours`. The legacy skin cache
    /// gives them priority over the user skin (lazer
    /// `BeatmapSkinProvidingContainer`); `--skin-colours` clears them.
    pub custom_colours: Vec<(String, [u8; 3])>,
    /// `[Events]` break periods `(start, end)` in gameplay-clock ms —
    /// the break-time source for the scene's break background
    /// lightening (lazer `BreakTracker`).
    pub breaks: Vec<(f64, f64)>,
    pub rate: f64,
    /// Nightcore mod (rate 1.5 like DT, but the export keeps the game's
    /// pitch-up on the BGM — nightcore without the pitch isn't nightcore).
    /// Hitsounds stay at their natural rate regardless.
    pub nightcore: bool,
    pub classic: bool,
    /// Health processor output from the judge crate
    /// (`HealthProcessor` + `DrainingHealthProcessor` +
    /// `OsuHealthProcessor`): drain rate/window, no-drain break periods
    /// and the per-judgement HP curve. Query via [`health_at`].
    pub health: osu_replay_judge::health::HealthInfo,
    /// Spinner bonus ticks: (object index, time, large_bonus).
    pub spinner_ticks: Vec<(usize, f64, bool)>,
    /// Spins past the tick limit (lazer `maxBonusSample`: sound, no
    /// score): (object index, time).
    pub spinner_max_ticks: Vec<(usize, f64)>,
    /// Key-tap count timeline for the key overlay (rising edges only).
    pub key_events: Vec<KeyCountEvent>,
    pub player: String,
    /// When the score was set: raw Windows FILETIME ticks from the .osr
    /// header (None for autoplay previews).
    pub played_at_ticks: Option<u64>,
    pub final_score: i64,
    pub final_classic_score: i64,
    pub final_max_combo: i32,
    pub final_accuracy: f64,
    /// Final judgement counts (`ScoreProcessor.Statistics`) and the
    /// autoplay-simulated maximums (`MaximumStatistics`) - the results
    /// screen's `HitResultStatistic` feed.
    pub final_statistics: Vec<(HitResult, i32)>,
    pub final_maximum_statistics: Vec<(HitResult, i32)>,
    /// Maximum achievable combo (`ScoreInfo.GetMaximumAchievableCombo`).
    pub max_combo_achievable: i32,
    /// The replay's mods (acronyms / rates for the results ModDisplay).
    pub mods: Mods,
    /// Beatmap metadata (`[Metadata]`) for the results screen header.
    pub map_meta: MapMeta,
    /// `[General] AudioFilename` (beatmap's own BGM, relative to the map).
    pub map_audio: Option<String>,
    /// `[Events]` background image filename (relative to the map).
    pub map_background: Option<String>,
    /// `[General] LetterboxInBreaks`.
    pub letterbox_in_breaks: bool,
    /// `.osu` sample-side data (banks/volumes/hit samples) parsed with the
    /// beatmap; consumed by the hitsound synthesis.
    pub sample_data: osu_parse::samples::SampleData,
    /// Star rating of the map+mods (rosu-pp); NaN when unavailable.
    pub stars: f64,
    /// Performance points of the judged replay (`rosu-pp`,
    /// pp-rework-202607 port of lazer's calculator) and the FC max PP of
    /// the map+mods. `NaN` when rosu-pp could not parse the map.
    pub pp: f64,
    pub pp_max: f64,
    /// PP component breakdowns (achieved, FC) for the results screen's
    /// `PerformanceBreakdownChart`. `None` when PP is unavailable.
    pub pp_breakdown: Option<(crate::pp::PpBreakdown, crate::pp::PpBreakdown)>,
    /// 结算页 Difficulty Graph 三条曲线,均为逐物件时间序列
    /// `(物件 start_time, strain)`:aim(依赖本地 rosu-pp patch)、
    /// speed、reading。PP 不可用时为空;aim 在未 patch 环境下为空。
    pub strain_aim_pts: Vec<(f64, f64)>,
    pub strain_speed_pts: Vec<(f64, f64)>,
    pub strain_reading_pts: Vec<(f64, f64)>,
    /// Circle hit events for the results statistics (`AccuracyHeatmap`:
    /// hits + misses; the `Timing Distribution` graph / `UnstableRate` /
    /// `AverageHitError` use the hits only).
    pub results_hit_events: Vec<ResultsHitEvent>,
    /// Live PP timeline (`OsuGradualPerformance`, advanced once per fully
    /// judged object): `(time, pp)` pairs - the in-game PP counter feed
    /// (`pp::pp_at` queries it). Empty when PP is unavailable.
    pub pp_events: Vec<(f64, f64)>,
    /// Judgement times of every negative-increase miss (`Miss`,
    /// `LargeTickMiss`, `SmallTickMiss` - `ArgonHealthDisplay`'s
    /// `onNewJudgement` miss condition; `IgnoreMiss` carries no health
    /// delta and is excluded). Feeds the health bar's red miss display.
    pub miss_times: Vec<f64>,
    /// Hidden mod (`OsuModHidden`, visual only — HD changes no judgement).
    /// Objects fade out before their hit time; approach circles are hidden
    /// except on the first object.
    pub hidden: bool,
    /// Beatmap index of the first non-spinner object
    /// (`IsFirstAdjustableObject`): under HD it keeps its approach circle
    /// (`IncreaseFirstObjectVisibility`, default on).
    pub hd_first_object: usize,
}

/// 完整渲染:判定引擎的快照从「首物件时间 − 抢先量」才开始,谱面前奏
/// 无 note 的空段没有快照。这里按 60fps 游戏帧节奏补齐 [0, 首个快照)
/// 的 idle 快照(光标静止取首快照位置、无按键),使时间轴从 0 开始,
/// 音频与视频都覆盖完整前奏。首个快照时间 <= 0 时无需补齐。补齐间隔
/// 与引擎的快照节奏一致(1000/60 × rate 游戏时钟毫秒/帧),否则速率
/// 模组下前奏段与正片的回放速度会不一致(60fps 导出走快照 1:1)。
fn with_lead_in(mut snapshots: Vec<FrameSnap>, rate: f64) -> Vec<FrameSnap> {
    if snapshots.first().is_some_and(|s| s.time > 0.0) {
        let first = snapshots[0].clone();
        let mut lead: Vec<FrameSnap> = Vec::new();
        let mut t = 0.0;
        while t < first.time {
            lead.push(FrameSnap {
                time: t,
                cursor: first.cursor,
                left: false,
                right: false,
                smoke: false,
                sliders: Vec::new(),
                spinners: Vec::new(),
            });
            t += 1000.0 / 60.0 * rate;
        }
        lead.extend(snapshots);
        snapshots = lead;
    }
    snapshots
}

/// [`load`] with the rosu-pp pass skippable (`with_pp: false` — hosts that
/// won't consume PP/stars data skip the calculation for load time; the
/// results screen then carries no PP/graph data). Decide by whether PP data
/// is wanted at all (the host's PP-display option, a results screen, a
/// later backfill), NOT by HUD visibility — HUD is a pure display toggle
/// over precomputed data.
pub fn load_with_pp(map_path: &str, replay_path: &str, with_pp: bool) -> Result<GameData, String> {    let content = std::fs::read_to_string(map_path).map_err(|e| format!("cannot read beatmap {map_path}: {e}"))?;
    let mut map = beatmap::decode(&content)?;
    let rep = replay::decode_file(replay_path, map.version)?;

    let is_legacy_score = (rep.header.version as i64) < 30_000_000;
    let classic = is_legacy_score;
    let mods = Mods::from_legacy(rep.header.mods, classic)?;
    let difficulty = process::apply_difficulty_mods(map.difficulty, mods.hard_rock, mods.easy);
    let processed = process::process(&map, difficulty, classic, mods.hard_rock);
    let breaks = processed.breaks.clone();

    let mut engine = Engine::new(processed, &mods);
    engine.run(&rep.frames);

    let mut data = build(mods, classic, map.combo_colours, map.custom_colours.clone(), MapMeta::from_metadata(&map.metadata), &engine)?;
    data.breaks = breaks;
    data.player = rep.header.player_name.clone();
    data.played_at_ticks = Some(rep.header.timestamp);
    // Beatmap extras for the assembly stage (audio/background/hitsounds):
    // parsed once here instead of re-reading the .osu downstream.
    data.map_audio = map.general.audio_filename.clone();
    data.map_background = map.background.clone();
    data.letterbox_in_breaks = map.general.letterbox_in_breaks.unwrap_or(false);
    data.sample_data = std::mem::take(&mut map.sample_data);
    if with_pp {
        if let Some(pp) = crate::pp::calculate(content.as_bytes(), rep.header.mods, classic, &engine) {
            data.pp = pp.pp;
            data.pp_max = pp.pp_max;
            data.pp_events = pp.events;
            data.stars = pp.stars;
            data.pp_breakdown = Some((pp.breakdown, pp.breakdown_max));
            data.strain_aim_pts = pp.strain_aim_pts;
            data.strain_speed_pts = pp.strain_speed_pts;
            data.strain_reading_pts = pp.strain_reading_pts;
        }
    }
    Ok(data)
}

/// [`load_with_pp`] with the rosu-pp pass always on (previous behaviour).
pub fn load(map_path: &str, replay_path: &str) -> Result<GameData, String> {
    load_with_pp(map_path, replay_path, true)
}

/// Beatmap preview with the Autoplay mod: frames come from the local port
/// of lazer's `OsuAutoGenerator` instead of a recorded .osr, so no replay
/// file is needed. The engine then judges the generated frames like any
/// other replay — every judgement/HP/combo/UR readout is real.
/// `mods_bits`: legacy mod bits selected by the host (only HD/HR/EZ/DT/HT/
/// NC are honoured — the rest is masked away). HR/EZ re-tune CS/AR/OD/HP
/// and HR mirrors objects; DT/NC/HT set the gameplay rate (snapshot
/// cadence, hit windows, HUD mod display, PP/stars all follow); HD adds
/// its visuals on top of the host's own `hidden` flag.
/// `hidden`: host-side HD visual switch (same effect as the HD bit).
/// `with_pp`: skip the rosu-pp pass (stars/PP timeline) when the host won't
/// consume PP data — decide by the host's PP-display option (or a later
/// backfill re-run), NOT by HUD visibility; HUD is a pure display toggle.
pub fn load_autoplay(map_path: &str, mods_bits: u32, hidden: bool, with_pp: bool) -> Result<GameData, String> {
    let content = std::fs::read_to_string(map_path).map_err(|e| format!("cannot read beatmap {map_path}: {e}"))?;
    load_autoplay_content(&content, mods_bits, hidden, with_pp)
}

/// Bits kept from `mods_bits` — everything the wallpaper host can select.
/// AUTO/RX/AP/SO are rejected by `Mods::from_legacy` and must not arrive.
const AUTOPLAY_MODS_MASK: u32 = osu_replay_judge::mods::legacy_bits::HIDDEN
    | osu_replay_judge::mods::legacy_bits::HARDROCK
    | osu_replay_judge::mods::legacy_bits::EASY
    | osu_replay_judge::mods::legacy_bits::DOUBLETIME
    | osu_replay_judge::mods::legacy_bits::HALFTIME
    | osu_replay_judge::mods::legacy_bits::NIGHTCORE;

/// [`load_autoplay`] for zero-copy hosts (osu!lazer content-addressed
/// storage): the beatmap text is supplied by the host, which reads it
/// straight from its blob store — no real beatmap directory required.
pub fn load_autoplay_content(osu_text: &str, mods_bits: u32, hidden: bool, with_pp: bool) -> Result<GameData, String> {
    let mut map = beatmap::decode(osu_text)?;

    // Lazer autoplay scores: standardised scoring. Difficulty mods and rate
    // come from the host's selection; everything downstream (generator AR,
    // engine windows, snapshot cadence, mod display, PP) follows `mods`
    // exactly as it does for a recorded replay.
    let mods = Mods::from_legacy(mods_bits & AUTOPLAY_MODS_MASK, false)?;
    let classic = false;
    let difficulty = process::apply_difficulty_mods(map.difficulty, mods.hard_rock, mods.easy);
    let processed = process::process(&map, difficulty, classic, mods.hard_rock);

    // The generator works entirely in gameplay-clock ms (object times and
    // preempt are map-time values), so rate mods need no adjustments here.
    let frames = crate::autoplay::AutoGenerator::new(&processed.objects, difficulty.ar as f64).generate();
    let breaks = processed.breaks.clone();

    let mut engine = Engine::new(processed, &mods);
    engine.run(&frames);

    let mut data = build(mods.clone(), classic, map.combo_colours, map.custom_colours.clone(), MapMeta::from_metadata(&map.metadata), &engine)?;
    data.breaks = breaks;
    // lazer's autoplay attribution.
    data.player = "osu!".to_string();
    data.map_audio = map.general.audio_filename.clone();
    data.map_background = map.background.clone();
    data.letterbox_in_breaks = map.general.letterbox_in_breaks.unwrap_or(false);
    data.sample_data = std::mem::take(&mut map.sample_data);
    // HD 视觉(纯装饰:不改判定/PP;mod 位与宿主开关取或)
    data.hidden = hidden || mods.hidden;
    if with_pp {
        if let Some(pp) = crate::pp::calculate(osu_text.as_bytes(), mods_bits & AUTOPLAY_MODS_MASK, classic, &engine) {
            data.pp = pp.pp;
            data.pp_max = pp.pp_max;
            data.pp_events = pp.events;
            data.stars = pp.stars;
            data.pp_breakdown = Some((pp.breakdown, pp.breakdown_max));
            data.strain_aim_pts = pp.strain_aim_pts;
            data.strain_speed_pts = pp.strain_speed_pts;
            data.strain_reading_pts = pp.strain_reading_pts;
        }
    }
    Ok(data)
}

pub fn build(
    mods: Mods,
    classic: bool,
    map_colours: Vec<[u8; 3]>,
    map_custom_colours: Vec<(String, [u8; 3])>,
    map_meta: MapMeta,
    engine: &Engine,
) -> Result<GameData, String> {
    let engine_objects: &[ProcObject] = engine.objects();

    let combo_colours: Vec<Colour> = if !map_colours.is_empty() {
        map_colours.iter().map(|&c| Colour::from_bytes(c)).collect()
    } else {
        ARGON_COMBO_COLOURS.iter().map(|&c| Colour::from_hex(c)).collect()
    };

    let mut objects: Vec<ObjView> = Vec::with_capacity(engine_objects.len());
    for (i, obj) in engine_objects.iter().enumerate() {
        let stack = obj.stack_offset();
        let pos = [obj.position.x + stack.x, obj.position.y + stack.y];
        let end_pos = [obj.end_position.x + stack.x, obj.end_position.y + stack.y];

        let (kind, slider_points, span_count, duration, nested_raw, spins_required, maximum_bonus_spins) = match &obj.kind {
            ProcKind::Circle => (ObjKind::Circle, Vec::new(), 0, 0.0, None, 0.0, 0),
            ProcKind::Spinner { spins_required, maximum_bonus_spins, .. } => {
                (ObjKind::Spinner, Vec::new(), 0, obj.end_time - obj.start_time, None, *spins_required as f64, *maximum_bonus_spins as i64)
            }
            ProcKind::Slider { path, nested, span_count, duration, .. } => {
                let pts: Vec<[f32; 2]> = path
                    .calculated_path()
                    .iter()
                    .map(|p| [p.x, p.y])
                    .collect();
                (ObjKind::Slider, pts, *span_count, *duration, Some(nested), 0.0, 0)
            }
        };

        let slider_cum = cumulative(&slider_points);
        let slider_len = slider_cum.last().copied().unwrap_or(0.0);
        let (body_points, body_cum) = build_body_path(&slider_points);
        let body_len = if body_points.is_empty() {
            slider_len
        } else {
            body_cum.last().copied().unwrap_or(0.0)
        };

        let mut nested: Vec<NestedView> = Vec::new();
        if let Some(nl) = nested_raw {
            for n in nl {
                let position = match &obj.kind {
                    ProcKind::Slider { path, .. } => {
                        let p = path.position_at(n.path_progress.clamp(0.0, 1.0));
                        [pos[0] + p.x, pos[1] + p.y]
                    }
                    _ => pos,
                };
                nested.push(NestedView {
                    kind: n.kind,
                    time: n.time,
                    position,
                    span_index: n.span_index,
                    path_progress: n.path_progress,
                    judged: None,
                });
            }
        }

        objects.push(ObjView {
            index: i,
            kind,
            start_time: obj.start_time,
            end_time: obj.end_time,
            position: pos,
            end_position: end_pos,
            radius: obj.radius,
            scale: obj.scale,
            preempt: obj.time_preempt,
            fade_in: 400.0 * (obj.time_preempt / 450.0).min(1.0),
            new_combo: obj.new_combo,
            colour: if !map_colours.is_empty() {
                // Beatmap [Colours] (LegacyBeatmapSkin.GetComboColour):
                // indexed by ComboIndexWithOffsets % len - no "-1".
                Colour::from_bytes(
                    map_colours[(obj.combo_index_with_offsets as usize) % map_colours.len()],
                )
            } else {
                // Argon skin custom colours (ArgonSkin.getComboColour):
                // indexed by ComboIndex % len. For .osu beatmaps the combo
                // offsets are always zero, so this equals ComboIndex.
                Colour::from_hex(
                    ARGON_COMBO_COLOURS[(obj.combo_index_with_offsets as usize) % ARGON_COMBO_COLOURS.len()],
                )
            },
            number: obj.index_in_current_combo + 1,
            combo_colour_index: obj.combo_index_with_offsets as u32,
            slider_points,
            slider_cum,
            slider_len,
            body_points,
            body_cum,
            body_len,
            slider_distance: match &obj.kind {
                ProcKind::Slider { path, .. } => path.distance(),
                _ => 0.0,
            },
            span_count,
            duration,
            nested,
            spins_required,
            maximum_bonus_spins,
            head_judged: None,
            body_judged: None,
            shake_times: Vec::new(),
        });
    }

    // Note-lock shakes: rejected presses shake their target hit object.
    // A slider-head press shakes two containers (`DrawableSliderHead.Shake`
    // calls both its own and `DrawableSlider.Shake`), but both start at the
    // same time, so one list drives both offsets.
    for shake in &engine.shakes {
        if let Some(obj) = objects.get_mut(shake.object_index) {
            obj.shake_times.push(shake.time);
        }
    }
    for obj in &mut objects {
        obj.shake_times.sort_by(|a, b| a.total_cmp(b));
    }

    // Full score state timeline.
    let score_events: Vec<ScoreEvent> = engine
        .timeline
        .iter()
        .map(|e| ScoreEvent {
            time: e.time,
            combo: e.combo,
            score: e.score,
            classic_score: e.classic_score,
            accuracy: e.accuracy,
        })
        .collect();

    // The timeline is in judgement APPLICATION order, and a hit applied
    // late (a slow slider tail judged after the next object's miss) keeps
    // its earlier object time - HUD timelines scan these with break /
    // partition_point, which assume monotonic time. Clamp each event to
    // run at or after its predecessor so causal order and time order
    // agree.
    let mut last = f64::NEG_INFINITY;
    let mut score_events = score_events;
    for e in &mut score_events {
        e.time = e.time.max(last);
        last = e.time;
    }

    // UR-relevant hits with cumulative Welford statistics - exactly the
    // event set `ScoreProcessor.unstable_rate` counts (has_windows && hit).
    let mut ur_events: Vec<UrEvent> = Vec::new();
    {
        let mut count = 0u64;
        let mut mean = 0.0f64;
        let mut sum_of_squares = 0.0f64;
        for e in &engine.score.hit_events {
            if !e.has_windows || !osu_replay_judge::score::hit_result_ext::is_hit(e.result) {
                continue;
            }
            count += 1;
            let v = e.time_offset / e.gameplay_rate;
            let next_mean = mean + (v - mean) / count as f64;
            sum_of_squares += (v - mean) * (v - next_mean);
            mean = next_mean;
            ur_events.push(UrEvent {
                time: e.raw_time,
                offset: v,
                result: e.result,
                ur: 10.0 * (sum_of_squares / count as f64).sqrt(),
                mean,
            });
        }
    }
    let w = engine.windows();
    let hit_windows = (w.great, w.ok, w.meh);

    // Same monotonic-time clamp for the UR bar's binary search.
    let mut last_ur = f64::NEG_INFINITY;
    for e in &mut ur_events {
        e.time = e.time.max(last_ur);
        last_ur = e.time;
    }

    // Timeline -> per-object judgement states + display events.
    let mut events: Vec<EventView> = Vec::new();
    let mut miss_times: Vec<f64> = Vec::new();
    let mut spinner_ticks: Vec<(usize, f64, bool)> = Vec::new();
    let mut spinner_max_ticks: Vec<(usize, f64)> = Vec::new();
    for entry in &engine.timeline {
        if matches!(entry.result, HitResult::Miss | HitResult::LargeTickMiss | HitResult::SmallTickMiss) {
            miss_times.push(entry.time);
        }
        let obj = &mut objects[entry.object_index];
        let label = entry.label.as_str();

        let nested_idx: Option<usize> = if let Some(rest) = label.strip_prefix("tick") {
            rest.parse::<usize>().ok()
        } else if let Some(rest) = label.strip_prefix("repeat") {
            rest.parse::<usize>().ok()
        } else {
            None
        };

        match label {
            "circle" => obj.body_judged = Some((entry.time, entry.result)),
            "head" => obj.head_judged = Some((entry.time, entry.result)),
            "slider" | "spinner" => obj.body_judged = Some((entry.time, entry.result)),
            "stick" => {
                spinner_ticks.push((entry.object_index, entry.time, entry.result == HitResult::LargeBonus));
            }
            "smax" => {
                spinner_max_ticks.push((entry.object_index, entry.time));
            }
            _ => {}
        }

        if let Some(ni) = nested_idx {
            if let Some(n) = obj.nested.get_mut(ni) {
                n.judged = Some((entry.time, entry.result));
            }
        }

        // Decide display.
        let position = match label {
            "circle" => obj.position,
            "head" => obj.position,
            "slider" => obj.end_position,
            "spinner" => obj.position,
            "tick" | "repeat" | "tail" if nested_idx.is_some() => obj
                .nested
                .get(nested_idx.unwrap())
                .map(|n| n.position)
                .unwrap_or(obj.position),
            _ => obj.position,
        };

        let (display, scale) = match label {
            "circle" | "spinner" => (JudgementDisplay::Text, obj.scale),
            "head" => {
                if classic {
                    // Classic slider heads are LargeTickHit/Miss.
                    if entry.result == HitResult::LargeTickMiss {
                        (JudgementDisplay::TickMiss, obj.scale)
                    } else {
                        (JudgementDisplay::None, obj.scale)
                    }
                } else {
                    (JudgementDisplay::Text, obj.scale)
                }
            }
            "slider" => {
                if classic {
                    (JudgementDisplay::Text, obj.scale)
                } else if entry.result == HitResult::IgnoreMiss {
                    (JudgementDisplay::TickMiss, obj.scale)
                } else {
                    (JudgementDisplay::None, obj.scale)
                }
            }
            "tick" | "repeat" => {
                if entry.result == HitResult::LargeTickMiss {
                    (JudgementDisplay::TickMiss, obj.scale)
                } else {
                    (JudgementDisplay::None, obj.scale)
                }
            }
            _ => (JudgementDisplay::None, obj.scale),
        };

        if display != JudgementDisplay::None {
            events.push(EventView {
                time: entry.time,
                position,
                result: entry.result,
                display,
                scale,
                combo: entry.combo,
                score: entry.score,
                classic_score: entry.classic_score,
                accuracy: entry.accuracy,
            });
        }
    }

    // Health: the judge engine's `HealthProcessor` pass (drain rate solved
    // off the perfect-play simulation, per-judgement HP curve with the
    // combo-end bonus and break no-drain periods).
    let health = engine.health.clone();

    // Key overlay counts: rising edges of [left, right, smoke] across the
    // visual snapshots (the stream the overlay itself displays).
    let mut key_events: Vec<KeyCountEvent> = Vec::new();
    {
        let mut counts = [0u32; 3];
        let mut prev = [false; 3];
        for s in &engine.snapshots {
            let cur = [s.left, s.right, s.smoke];
            for k in 0..3 {
                if cur[k] && !prev[k] {
                    counts[k] += 1;
                }
            }
            if cur != prev {
                key_events.push(KeyCountEvent { time: s.time, counts });
                prev = cur;
            }
        }
    }

    let hd_first_object = objects
        .iter()
        .find(|o| o.kind != ObjKind::Spinner)
        .map(|o| o.index)
        .unwrap_or(0);

    // Circle hit events for the results statistics (lazer `HitEvent` for
    // `HitCircle`s incl. slider heads — `HitObject is HitCircle &&
    // !(SliderTailCircle)` — hits AND misses). The hit position is lazer's
    // `HitReceptor.ClosestPressPosition`: the press closest to the object
    // centre among all presses from spawn to judgement, recorded with the
    // replay frame's exact cursor; entries without a press are skipped
    // (lazer `Position == null`). Misses feed only the heatmap (their
    // `MissPoint` x-marks); the timing graph / UR keep hits only.
    let presses = &engine.presses;
    let mut results_hit_events: Vec<ResultsHitEvent> = Vec::new();
    {
        // `ScoreProcessor`'s `LastHitObject` chain: the previously judged
        // object of ANY kind, advanced on every judgement (misses and
        // nested slider results included). Its `StackedEndPosition` is the
        // heatmap's movement start: circles/heads/spinners at their
        // centre, the slider body and tail at the slider end, ticks and
        // repeats at their path position (labels carry the nested index).
        let judged_end = |obj: &ObjView, label: &str| -> [f32; 2] {
            match label {
                "tail" | "slider" => obj.end_position,
                l if l.starts_with("tick") || l.starts_with("repeat") => {
                    let digits = l.find(|c: char| c.is_ascii_digit()).unwrap_or(l.len());
                    let idx = l[digits..].parse::<usize>().unwrap_or(0);
                    obj.nested.get(idx).map(|n| n.position).unwrap_or(obj.position)
                }
                _ => obj.position,
            }
        };
        let mut last_end: Option<[f32; 2]> = None;
        for entry in &engine.timeline {
            let obj = &objects[entry.object_index];
            let this_end = judged_end(obj, &entry.label);
            let is_circle = matches!(entry.label.as_str(), "circle" | "head");
            if is_circle {
                // Press window = the receptor's lifetime (spawn ..=
                // judgement, +2ms slop), any button. Classic-mode heads
                // judge as `LargeTickHit`/`LargeTickMiss`; the heatmap has
                // no result filter in lazer, so any result with a press
                // lands a point (in-circle green / outside red x).
                let t0 = obj.start_time - obj.preempt;
                let cursor = presses
                    .iter()
                    .filter(|(t, _)| *t >= t0 && *t <= entry.time + 2.0)
                    .map(|(_, p)| ((p.x - obj.position[0]).hypot(p.y - obj.position[1]), [p.x, p.y]))
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, p)| p);
                if let Some(cursor) = cursor {
                    results_hit_events.push(ResultsHitEvent {
                        time: entry.time,
                        offset: entry.time_offset,
                        result: entry.result,
                        pos: obj.position,
                        last_pos: last_end,
                        cursor,
                        radius: obj.radius,
                    });
                }
            }
            last_end = Some(this_end);
        }
    }

    Ok(GameData {
        score_events,
        ur_events,
        hit_windows,
        objects,
        events,
        snapshots: with_lead_in(engine.snapshots.clone(), mods.rate),
        combo_colours,
        has_beatmap_colours: !map_colours.is_empty(),
        custom_colours: map_custom_colours,
        breaks: Vec::new(),
        rate: mods.rate,
        nightcore: mods.nightcore,
        classic,
        health,
        spinner_ticks,
        spinner_max_ticks,
        key_events,
        player: String::new(),
        played_at_ticks: None,
        final_score: engine.score.total_score(),
        final_classic_score: engine.score.classic_display_score(),
        final_max_combo: engine.score.highest_combo,
        final_accuracy: engine.score.accuracy(),
        final_statistics: engine.score.statistics.clone(),
        final_maximum_statistics: engine.score.maximum_statistics.clone(),
        // `ScoreInfo.GetMaximumAchievableCombo`: the sum of the maximum
        // statistics over the combo-affecting results. Clamped to the
        // achieved combo (the invariant lazer maintains by construction —
        // the judge's autoplay simulation can disagree with its own
        // playthrough by a few nested judgements).
        max_combo_achievable: (engine
            .score
            .maximum_statistics
            .iter()
            .filter(|(r, _)| {
                matches!(
                    r,
                    HitResult::Miss
                        | HitResult::Meh
                        | HitResult::Ok
                        | HitResult::Good
                        | HitResult::Great
                        | HitResult::Perfect
                        | HitResult::LargeTickHit
                        | HitResult::LargeTickMiss
                        | HitResult::SliderTailHit
                )
            })
            .map(|&(_, c)| c)
            .sum::<i32>())
        .max(engine.score.highest_combo),
        mods: mods.clone(),
        map_meta,
        map_audio: None,
        map_background: None,
        letterbox_in_breaks: false,
        sample_data: Default::default(),
        stars: f64::NAN,
        pp: f64::NAN,
        pp_max: f64::NAN,
        pp_breakdown: None,
        strain_aim_pts: Vec::new(),
        strain_speed_pts: Vec::new(),
        strain_reading_pts: Vec::new(),
        results_hit_events,
        pp_events: Vec::new(),
        miss_times,
        hidden: mods.hidden,
        hd_first_object,
    })
}

/// Synthesizes a snapshot at an arbitrary time: cursor linearly interpolated
/// between the surrounding game frames, button/tracking state from the last
/// frame at or before `t`.
pub fn snapshot_at(game: &GameData, t: f64) -> FrameSnap {
    let snaps = &game.snapshots;
    assert!(!snaps.is_empty(), "no snapshots");

    // Rightmost snapshot with time <= t.
    let mut lo = 0usize;
    let mut hi = snaps.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if snaps[mid].time <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let idx = lo.saturating_sub(1);

    let mut snap = snaps[idx].clone();
    if let Some(next) = snaps.get(idx + 1) {
        let span = next.time - snap.time;
        if span > 0.0 && t > snap.time {
            let f = ((t - snap.time) / span).clamp(0.0, 1.0);
            snap.cursor.x = snap.cursor.x + (next.cursor.x - snap.cursor.x) * f as f32;
            snap.cursor.y = snap.cursor.y + (next.cursor.y - snap.cursor.y) * f as f32;
        }
    }
    snap.time = t;
    snap
}

/// Key overlay states at time `t`: [z, x, c], taken from the last snapshot
/// at or before `t` (same rule as `snapshot_at`'s button state).
pub fn key_state_at(game: &GameData, t: f64) -> [bool; 3] {
    let snaps = &game.snapshots;
    let idx = snaps.partition_point(|s| s.time <= t).saturating_sub(1);
    match snaps.get(idx) {
        Some(s) => [s.left, s.right, s.smoke],
        None => [false, false, false],
    }
}

/// Cumulative key-tap counts at time `t` (lazer `ActivationCount`).
pub fn key_counts_at(game: &GameData, t: f64) -> [u32; 3] {
    let n = game.key_events.partition_point(|e| e.time <= t);
    if n == 0 { [0, 0, 0] } else { game.key_events[n - 1].counts }
}

/// Health at time `t` (judge `HealthProcessor` reconstruction: latest
/// judgement's clamped HP minus the linear drain, skipping break periods).
pub fn health_at(game: &GameData, t: f64) -> f64 {
    osu_replay_judge::health::health_at(&game.health, t)
}

/// Re-map object colours through a loaded skin's combo colours
/// (`SkinComboColourLookup`). Default (`force == false`, lazer's
/// "Beatmap skins" player setting on): the beatmap's `[Colours]` win —
/// `LegacyBeatmapSkin` answers the lookup first — and the skin's
/// colours only apply when the beatmap ships none (its
/// `AllowDefaultComboColoursFallback` is false, so the lookup falls
/// through to the user skin). `force == true` is the stable behaviour:
/// the current skin's colours - user legacy custom (or the default
/// stable fallback) or the builtin argon six - always override the
/// beatmap's, like lazer with "Beatmap colours" off (the beatmap skin's
/// colour lookup is skipped, and `ArgonSkin.GetConfig` answers combo
/// colour lookups with no legacy gate).
///
/// Re-entrant: every call re-maps the whole palette from the base one
/// `build()` stored (`combo_colours`: beatmap `[Colours]` or the argon
/// fallback), so hosts can call it again after a skin swap or a flag
/// toggle without the previous mapping sticking.
pub fn apply_skin_combo_colours(game: &mut GameData, skin: &crate::skin::ResolvedSkin, force: bool) {
    use crate::skin::Skin as _;
    // `--skin-colours` is the renderer's "Beatmap colours: off": combo AND
    // custom `[Colours]` (SliderBorder etc.) both fall back to the user skin.
    if force {
        game.custom_colours.clear();
    }
    let skin_colours = skin
        .get_config(crate::skin::SkinLookup::GlobalColour(
            crate::skin::GlobalSkinColours::ComboColours,
        ))
        .and_then(|v| match v {
            crate::skin::SkinValue::ComboColours(c) => Some(c),
            _ => None,
        })
        .filter(|c| !c.is_empty());
    let palette = match skin_colours {
        Some(c) if force || !game.has_beatmap_colours => c,
        _ => game.combo_colours.clone(),
    };
    if palette.is_empty() {
        return;
    }
    for obj in &mut game.objects {
        obj.colour = palette[(obj.combo_colour_index as usize) % palette.len()];
    }
}


#[cfg(test)]
mod ur_check_tests {
    use super::*;

    /// autoplay 会话的 UR 事件表必须非空( HUD 的 UR 条依赖它);
    /// 偏移全 0(SS 自动)但事件本身存在。
    #[test]
    fn ur_events_autoplay() {
        let map = "osu file format v14
                   
[General]
Mode: 0
                   
[Difficulty]
HPDrainRate:5
CircleSize:4
OverallDifficulty:8
ApproachRate:8
SliderMultiplier:1.8
SliderTickRate:1
                   
[TimingPoints]
500,400,4,2,0,45,1,0
                   
[HitObjects]
                   100,100,1000,1,0,0:0:0:0:0
                   200,100,1200,1,0,0:0:0:0:0
                   256,192,2000,12,0,3500,0:0:0:0:0
";
        let dir = std::env::temp_dir().join(format!("orr_ur_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ur.osu");
        std::fs::write(&path, map).unwrap();
        let game = load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();
        eprintln!("[ur-check] ur_events = {}", game.ur_events.len());
        assert!(!game.ur_events.is_empty(), "autoplay UR 事件表为空 —— HUD UR 条不会绘制");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod combo_colour_tests {
    use super::*;
    use crate::skin::{load_skin, GlobalSkinColours, Skin, SkinLookup, SkinValue};

    /// 内置 Argon(无用户皮肤)时 force(`--skin-colours` / osu-player
    /// "强制皮肤色")也必须生效:lazer 的 `ArgonSkin.GetConfig` 对 combo
    /// 色无条件应答(自定义六色),"Beatmap colours" 关掉后谱面
    /// `[Colours]` 被跳过、皮肤色赢。回归:`apply_skin_combo_colours`
    /// 曾用 `is_legacy()` 把 Argon 挡掉,且 `ArgonSkin` 漏设
    /// `CustomComboColours`(会答出经典默认四色)。
    #[test]
    fn argon_forced_skin_colours_override_beatmap_colours() {
        let map = "osu file format v14

[General]
Mode: 0

[Colours]
Combo1 : 255,255,255
Combo2 : 1,2,3

[Difficulty]
HPDrainRate:5
CircleSize:4
OverallDifficulty:8
ApproachRate:8
SliderMultiplier:1.8
SliderTickRate:1

[TimingPoints]
500,400,4,2,0,45,1,0

[HitObjects]
100,100,1000,1,0,0:0:0:0:0
200,100,1200,1,0,0:0:0:0:0
";
        let mut game = load_autoplay_content(map, 0, false, false).unwrap();
        assert!(game.has_beatmap_colours, "[Colours] 未被解析,测试前提不成立");

        let skin = load_skin(None).unwrap();
        // 皮肤层:Argon 答出自定义六色,而不是经典默认四色。
        let value = skin
            .get_config(SkinLookup::GlobalColour(GlobalSkinColours::ComboColours))
            .expect("argon skin must answer ComboColours");
        let colours = match value {
            SkinValue::ComboColours(c) => c,
            _ => panic!("expected SkinValue::ComboColours"),
        };
        assert_eq!(colours.len(), 6, "ArgonSkin 须答出六色,得到 {colours:?}");

        // force=false:谱面色赢(lazer "Beatmap colours" 开)。
        let beatmap_colour = game.objects[0].colour;
        apply_skin_combo_colours(&mut game, &skin, false);
        assert_eq!(game.objects[0].colour, beatmap_colour);

        // force=true:Argon 六色覆盖谱面色。
        apply_skin_combo_colours(&mut game, &skin, true);
        let expected = Colour::from_hex(
            ARGON_COMBO_COLOURS[(game.objects[0].combo_colour_index as usize) % ARGON_COMBO_COLOURS.len()],
        );
        assert_eq!(game.objects[0].colour, expected);
        assert_ne!(game.objects[0].colour, beatmap_colour);
    }

    #[test]
    fn scribble_body_path_collapses_to_its_silhouette() {
        // 3000 points oscillating inside 2 px - the XNOR intro slider shape.
        // The body must keep the 2 px silhouette (3 points: 430, 428, 430)
        // instead of tessellating thousands of overlapping capsules.
        let mut pts = Vec::new();
        for i in 0..=3000 {
            pts.push([0.0, if i % 2 == 0 { 430.0 } else { 428.0 }]);
        }

        let (body, cum) = build_body_path(&pts);
        let len = cum.last().copied().unwrap_or(0.0);

        assert!(body.len() <= 4, "body points = {}", body.len());
        assert_eq!(body.len(), cum.len());
        // `cum` holds the *exact* arc length (2 px * 3000 legs) so the body's
        // snake maps onto the same distance axis as the ball; the simplified
        // silhouette itself is only 4 px long, checked through the points.
        assert!((len - 6000.0).abs() < 1.0, "exact len = {len}");
        assert_eq!(cum[0], 0.0);
        assert!(cum.windows(2).all(|w| w[1] >= w[0]));
        let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
        for p in &body {
            min_y = min_y.min(p[1]);
            max_y = max_y.max(p[1]);
        }
        assert!((max_y - min_y - 2.0).abs() < 0.01);
    }

    #[test]
    fn long_normal_body_path_keeps_subpixel_shape() {
        // A 20k-point sine must simplify, keep both endpoints and stay within
        // 0.3 px of the original polyline.
        let pts: Vec<[f32; 2]> = (0..20_000)
            .map(|i| {
                let x = i as f32 * 0.01;
                [x, (x * 0.05).sin() * 40.0]
            })
            .collect();

        let (body, cum) = build_body_path(&pts);

        assert!(body.len() < pts.len(), "not simplified: {}", body.len());
        assert!(body.len() > 2);
        assert_eq!(body.len(), cum.len());
        assert_eq!(body[0], pts[0]);
        assert_eq!(*body.last().unwrap(), *pts.last().unwrap());

        let mut worst = 0.0f32;
        let mut j = 0usize;
        for p in &pts {
            while j + 2 < body.len() && body[j + 1][0] < p[0] {
                j += 1;
            }
            let a = body[j];
            let b = body[j + 1];
            let ab = [b[0] - a[0], b[1] - a[1]];
            let len2 = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if len2 > 1e-9 {
                (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let q = [a[0] + ab[0] * t, a[1] + ab[1] * t];
            worst = worst.max(dist2(q, *p).sqrt());
        }
        assert!(worst < 0.3, "worst deviation = {worst}");
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn slider_view(points: Vec<[f32; 2]>) -> ObjView {
        let slider_cum = cumulative(&points);
        let slider_len = slider_cum.last().copied().unwrap_or(0.0);
        let (body_points, body_cum) = build_body_path(&points);
        let body_len = if body_points.is_empty() {
            slider_len
        } else {
            body_cum.last().copied().unwrap_or(0.0)
        };
        ObjView {
            index: 0,
            kind: ObjKind::Slider,
            start_time: 0.0,
            end_time: 1000.0,
            position: [0.0, 0.0],
            end_position: [0.0, 0.0],
            radius: 36.0,
            scale: 1.0,
            preempt: 450.0,
            fade_in: 400.0,
            new_combo: true,
            colour: Colour::WHITE,
            combo_colour_index: 0,
            number: 1,
            slider_points: points,
            slider_cum,
            slider_len,
            body_points,
            body_cum,
            body_len,
            slider_distance: slider_len as f64,
            span_count: 1,
            duration: 1000.0,
            nested: Vec::new(),
            spins_required: 0.0,
            maximum_bonus_spins: 0,
            head_judged: None,
            body_judged: None,
            shake_times: Vec::new(),
        }
    }

    /// Straight linear walk over the polyline: the reference the cached
    /// prefix-sum + binary-search implementation must match.
    fn linear_reference(points: &[[f32; 2]], progress: f32) -> [f32; 2] {
        let mut lengths = Vec::with_capacity(points.len().saturating_sub(1));
        let mut total = 0.0f32;
        for w in points.windows(2) {
            let l = dist2(w[0], w[1]).sqrt();
            lengths.push(l);
            total += l;
        }
        let target = progress.clamp(0.0, 1.0) * total;
        let mut acc = 0.0f32;
        for (i, l) in lengths.iter().enumerate() {
            if acc + l >= target {
                let t = if *l > 1e-6 { (target - acc) / l } else { 0.0 };
                return [
                    points[i][0] + (points[i + 1][0] - points[i][0]) * t,
                    points[i][1] + (points[i + 1][1] - points[i][1]) * t,
                ];
            }
            acc += l;
        }
        *points.last().unwrap()
    }

    fn wavy_path(n: usize) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let x = i as f32 * 2.0;
                [x, (x * 0.05).sin() * 40.0 + (x * 0.013).cos() * 15.0]
            })
            .collect()
    }

    #[test]
    fn position_at_path_matches_linear_reference() {
        let points = wavy_path(400);
        let view = slider_view(points.clone());

        for k in 0..=400 {
            let progress = k as f32 / 400.0;
            let got = view.position_at_path(progress as f64);
            let want = linear_reference(&points, progress);
            assert!(
                (got[0] - want[0]).abs() < 1e-2 && (got[1] - want[1]).abs() < 1e-2,
                "p={progress}: got {got:?} want {want:?}"
            );
        }
    }

    #[test]
    fn sub_path_hits_both_ends_and_clips() {
        let points = wavy_path(200);
        let view = slider_view(points.clone());

        let full = view.sub_path(0.0, 1.0);
        assert!(full.len() >= 2);
        let first = full[0];
        let last = *full.last().unwrap();
        assert!((first[0] - points[0][0]).abs() < 1e-2 && (first[1] - points[0][1]).abs() < 1e-2);
        assert!(
            (last[0] - points.last().unwrap()[0]).abs() < 1e-2
                && (last[1] - points.last().unwrap()[1]).abs() < 1e-2
        );

        // A narrow slice stays inside the full path's bounding range.
        let mid = view.sub_path(0.4, 0.6);
        assert!(mid.len() >= 2);
        let d0 = view.position_at_path(0.4);
        let d1 = view.position_at_path(0.6);
        assert!((mid[0][0] - d0[0]).abs() < 1e-2 && (mid[0][1] - d0[1]).abs() < 1e-2);
        let m_last = *mid.last().unwrap();
        assert!((m_last[0] - d1[0]).abs() < 1e-2 && (m_last[1] - d1[1]).abs() < 1e-2);

        // Reversed range swaps instead of returning empty.
        let rev = view.sub_path(0.6, 0.4);
        assert_eq!(rev.len(), mid.len());
    }

    #[test]
    fn zero_length_sub_path_on_a_vertex_does_not_underflow() {
        // cum = [0, 100, 200]: p = 0.5 lands exactly on the middle vertex, so
        // the `c <= d0` and `c < d1` scans disagree by one segment.
        let view = slider_view(vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0]]);

        let p = view.sub_path(0.5, 0.5);
        assert_eq!(p.len(), 2);
        assert!((p[0][0] - 100.0).abs() < 1e-3 && (p[0][1] - 0.0).abs() < 1e-3);

        let caps = view.body_segments(0.5, 0.5);
        assert_eq!(caps.len(), 1);
        assert!((caps[0].0[0] - 100.0).abs() < 1e-3);

        // Degenerate ends (snake start / full body) stay safe too.
        assert!(!view.sub_path(0.0, 0.0).is_empty());
        assert!(!view.sub_path(1.0, 1.0).is_empty());
        assert!(!view.body_segments(0.0, 0.0).is_empty());
        assert!(!view.body_segments(1.0, 1.0).is_empty());
    }

    #[test]
    fn body_segments_cover_the_full_path() {
        let points = wavy_path(64);
        let view = slider_view(points.clone());

        let capsules = view.body_segments(0.0, 1.0);
        assert_eq!(capsules.len(), points.len() - 1);
        assert!((capsules[0].0[0] - points[0][0]).abs() < 1e-2);
        let tail = capsules.last().unwrap().1;
        assert!((tail[0] - points.last().unwrap()[0]).abs() < 1e-2);

        // Snaking slice: every capsule stays inside [i0, i1].
        let partial = view.body_segments(0.25, 0.75);
        assert!(partial.len() < capsules.len());
        assert!(!partial.is_empty());
    }

    #[test]
    fn body_tip_follows_the_ball_on_a_simplified_path() {
        // Simplification shortens the polyline; mapping snake progress over
        // the shortened length put the body behind the ball (the Flashbacklog
        // 1:07 lag). The body must stay on the ball's exact-distance axis.
        let points: Vec<[f32; 2]> = (0..20_000)
            .map(|i| {
                let x = i as f32 * 0.01;
                [x, (x * 0.05).sin() * 40.0]
            })
            .collect();
        let view = slider_view(points.clone());
        assert!(!view.body_points.is_empty(), "test premise: path must be simplified");

        let mut worst = 0.0f32;
        for k in 0..=200 {
            let p = k as f32 / 200.0;
            let ball = view.position_at_path(p as f64);
            let caps = view.body_segments(p as f64, p as f64);
            assert!(!caps.is_empty());
            let tip = caps[0].0;
            worst = worst.max((tip[0] - ball[0]).abs().max((tip[1] - ball[1]).abs()));
        }
        assert!(worst < 0.5, "body tip vs ball = {worst}");
    }

    #[test]
    fn scribble_body_segments_are_few() {
        let mut points = Vec::new();
        for i in 0..=5000 {
            points.push([0.0, if i % 2 == 0 { 430.0 } else { 428.0 }]);
        }
        let view = slider_view(points);

        let capsules = view.body_segments(0.0, 1.0);
        assert!(capsules.len() <= 4, "capsules = {}", capsules.len());
    }
}
