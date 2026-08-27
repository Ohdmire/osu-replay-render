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
    pub slider_distance: f64,
    pub span_count: usize,
    pub duration: f64,
    pub nested: Vec<NestedView>,
    pub spins_required: f64,
    /// Timeline events: head judgement (slider), body/final judgement.
    pub head_judged: Option<(f64, HitResult)>,
    pub body_judged: Option<(f64, HitResult)>,
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
        // Walk the polyline to the distance fraction.
        let target = progress.clamp(0.0, 1.0) as f32 * self.path_length();
        let pts = &self.slider_points;
        if pts.is_empty() {
            return self.position;
        }
        let mut acc = 0.0f32;
        for i in 0..pts.len() - 1 {
            let dx = pts[i + 1][0] - pts[i][0];
            let dy = pts[i + 1][1] - pts[i][1];
            let len = (dx * dx + dy * dy).sqrt();
            if acc + len >= target {
                let t = if len > 1e-6 { (target - acc) / len } else { 0.0 };
                return [
                    self.position[0] + pts[i][0] + dx * t,
                    self.position[1] + pts[i][1] + dy * t,
                ];
            }
            acc += len;
        }
        [
            self.position[0] + pts[pts.len() - 1][0],
            self.position[1] + pts[pts.len() - 1][1],
        ]
    }

    fn path_length(&self) -> f32 {
        let pts = &self.slider_points;
        let mut acc = 0.0f32;
        for i in 0..pts.len().saturating_sub(1) {
            let dx = pts[i + 1][0] - pts[i][0];
            let dy = pts[i + 1][1] - pts[i][1];
            acc += (dx * dx + dy * dy).sqrt();
        }
        acc
    }

    /// Sub-path (playfield coords, absolute) between path progress p0..p1.
    pub fn sub_path(&self, p0: f64, p1: f64) -> Vec<[f32; 2]> {
        let (mut p0, mut p1) = (p0.clamp(0.0, 1.0), p1.clamp(0.0, 1.0));
        if p0 > p1 {
            std::mem::swap(&mut p0, &mut p1);
        }
        let pts = &self.slider_points;
        if pts.len() < 2 {
            return Vec::new();
        }
        let total = self.path_length();
        if total <= 1e-6 {
            return Vec::new();
        }
        let d0 = (p0 as f32 * total) as f32;
        let d1 = (p1 as f32 * total) as f32;

        let mut out: Vec<[f32; 2]> = Vec::new();
        let mut acc = 0.0f32;
        let mut started = false;
        for i in 0..pts.len() - 1 {
            let dx = pts[i + 1][0] - pts[i][0];
            let dy = pts[i + 1][1] - pts[i][1];
            let len = (dx * dx + dy * dy).sqrt();
            let seg_start = acc;
            let seg_end = acc + len;

            let lerp_at = |d: f32| -> [f32; 2] {
                let t = if len > 1e-6 { ((d - seg_start) / len).clamp(0.0, 1.0) } else { 0.0 };
                [pts[i][0] + dx * t, pts[i][1] + dy * t]
            };

            if !started && seg_end >= d0 {
                out.push(lerp_at(d0));
                started = true;
            }
            if started && seg_end <= d1 {
                out.push([pts[i + 1][0], pts[i + 1][1]]);
            } else if started {
                out.push(lerp_at(d1));
                break;
            }
            acc = seg_end;
        }
        if out.len() == 1 {
            out.push(out[0]);
        }
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
    pub final_score: i64,
    pub final_classic_score: i64,
    pub final_max_combo: i32,
    pub final_accuracy: f64,
    /// Autoplay mod (beatmap preview): hide the score/accuracy/combo
    /// counters and the UR bar — the numbers are all perfect and carry no
    /// information about the beatmap.
    pub autoplay: bool,
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
/// 音频与视频都覆盖完整前奏。首个快照时间 <= 0 时无需补齐。
fn with_lead_in(mut snapshots: Vec<FrameSnap>) -> Vec<FrameSnap> {
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
            t += 1000.0 / 60.0;
        }
        lead.extend(snapshots);
        snapshots = lead;
    }
    snapshots
}

pub fn load(map_path: &str, replay_path: &str) -> Result<GameData, String> {
    let content = std::fs::read_to_string(map_path).map_err(|e| format!("cannot read beatmap: {}", e))?;
    let map = beatmap::decode(&content)?;
    let rep = replay::decode_file(replay_path, map.version)?;

    let is_legacy_score = (rep.header.version as i64) < 30_000_000;
    let classic = is_legacy_score;
    let mods = Mods::from_legacy(rep.header.mods, classic)?;
    let difficulty = process::apply_difficulty_mods(map.difficulty, mods.hard_rock, mods.easy);
    let processed = process::process(&map, difficulty, classic, mods.hard_rock);

    let mut engine = Engine::new(processed, &mods);
    engine.run(&rep.frames);

    let mut data = build(mods, classic, map.combo_colours, &engine)?;
    data.player = rep.header.player_name.clone();
    Ok(data)
}

/// Beatmap preview with the Autoplay mod: frames come from the local port
/// of lazer's `OsuAutoGenerator` instead of a recorded .osr, so no replay
/// file is needed. The engine then judges the generated frames like any
/// other replay — every judgement/HP/combo/UR readout is real.
pub fn load_autoplay(map_path: &str) -> Result<GameData, String> {
    let content = std::fs::read_to_string(map_path).map_err(|e| format!("cannot read beatmap: {}", e))?;
    let map = beatmap::decode(&content)?;

    // Lazer autoplay scores: no rate/visibility mods, standardised scoring.
    let mods = Mods::from_legacy(0, false)?;
    let classic = false;
    let difficulty = process::apply_difficulty_mods(map.difficulty, false, false);
    let processed = process::process(&map, difficulty, classic, false);

    let frames = crate::autoplay::AutoGenerator::new(&processed.objects, difficulty.ar as f64).generate();

    let mut engine = Engine::new(processed, &mods);
    engine.run(&frames);

    let mut data = build(mods, classic, map.combo_colours, &engine)?;
    // lazer's autoplay attribution; HUD counters stay hidden (see
    // GameData::autoplay).
    data.player = "osu!".to_string();
    data.autoplay = true;
    Ok(data)
}

fn build(
    mods: Mods,
    classic: bool,
    map_colours: Vec<[u8; 3]>,
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

        let (kind, slider_points, span_count, duration, nested_raw, spins_required) = match &obj.kind {
            ProcKind::Circle => (ObjKind::Circle, Vec::new(), 0, 0.0, None, 0.0),
            ProcKind::Spinner { spins_required, .. } => {
                (ObjKind::Spinner, Vec::new(), 0, obj.end_time - obj.start_time, None, *spins_required as f64)
            }
            ProcKind::Slider { path, nested, span_count, duration, .. } => {
                let pts: Vec<[f32; 2]> = path
                    .calculated_path()
                    .iter()
                    .map(|p| [p.x, p.y])
                    .collect();
                (ObjKind::Slider, pts, *span_count, *duration, Some(nested), 0.0)
            }
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
            slider_distance: match &obj.kind {
                ProcKind::Slider { path, .. } => path.distance(),
                _ => 0.0,
            },
            span_count,
            duration,
            nested,
            spins_required,
            head_judged: None,
            body_judged: None,
        });
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

    // Timeline -> per-object judgement states + display events.
    let mut events: Vec<EventView> = Vec::new();
    let mut spinner_ticks: Vec<(usize, f64, bool)> = Vec::new();
    let mut spinner_max_ticks: Vec<(usize, f64)> = Vec::new();
    for entry in &engine.timeline {
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

    Ok(GameData {
        score_events,
        ur_events,
        hit_windows,
        objects,
        events,
        snapshots: with_lead_in(engine.snapshots.clone()),
        combo_colours,
        has_beatmap_colours: !map_colours.is_empty(),
        rate: mods.rate,
        nightcore: mods.nightcore,
        classic,
        health,
        spinner_ticks,
        spinner_max_ticks,
        key_events,
        player: String::new(),
        final_score: engine.score.total_score(),
        final_classic_score: engine.score.classic_display_score(),
        final_max_combo: engine.score.highest_combo,
        final_accuracy: engine.score.accuracy(),
        autoplay: false,
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
/// an installed legacy skin's colours (custom, or the default stable
/// fallback) always override the beatmap's, like lazer with
/// "Beatmap skins" off.
///
/// Re-entrant: every call re-maps the whole palette from the base one
/// `build()` stored (`combo_colours`: beatmap `[Colours]` or the argon
/// fallback), so hosts can call it again after a skin swap or a flag
/// toggle without the previous mapping sticking.
pub fn apply_skin_combo_colours(game: &mut GameData, skin: &crate::skin::ResolvedSkin, force: bool) {
    use crate::skin::Skin as _;
    let skin_colours = skin
        .get_config(crate::skin::SkinLookup::GlobalColour(
            crate::skin::GlobalSkinColours::ComboColours,
        ))
        .and_then(|v| match v {
            crate::skin::SkinValue::ComboColours(c) => Some(c),
            _ => None,
        })
        .filter(|c| !c.is_empty() && skin.is_legacy());
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
