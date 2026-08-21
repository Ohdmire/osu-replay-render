//! Offline hitsound track synthesis with lazer gameplay-audio parity.
//!
//! The playable audio of an osu! map is BGM plus per-judgement samples:
//! each sample only fires when the judgement is a HIT (lazer
//! `DrawableHitObject.UpdateState`: `newState == ArmedState.Hit` →
//! `PlaySamples()`), at the judgement time, with volume/bank resolved from
//! the .osu sample data (`ConvertHitObjectParser` + `applySamples`):
//!
//! - every `[TimingPoints]` line carries a sample point (bank + volume);
//!   the point active at an object decides bank/volume the object itself
//!   does not specify (`LegacySampleControlPoint.ApplyTo`);
//! - non-repeating objects use the point at `EndTime + 5ms`, slider heads
//!   at `StartTime + 6ms`, slider node *i* at `StartTime + i*span + 5ms`
//!   (`CONTROL_POINT_LENIENCY` = 5);
//! - sliders play `NodeSamples[0]` on head hits, `NodeSamples[i]` on
//!   repeat hits, `TailSamples` (`NodeSamples[span]`) on the body
//!   judgement, `slidertick` on tick hits, and loop `sliderslide` (+
//!   `sliderwhistle` when the whistle flag is set) while tracked;
//! - playback volume = `max(volume, 5)%` (`MINIMUM_SAMPLE_VOLUME`),
//!   stereo balance follows the playfield X (`PositionalHitsoundsLevel`
//!   0.8 → `round2(1.6 * (x/512 - 0.5))`), and rate mods pitch samples up
//!   like the rest of lazer's gameplay audio mixer;
//! - a combo drop to zero plays `Gameplay/combobreak` (`ComboEffects`:
//!   old combo > 20, or the first break while `AlwaysPlayFirstComboBreak`
//!   is on — the default), at full volume, centered.
//!
//! Samples come from the skin's `Gameplay/ArgonPro/` resource set
//! (`assets/sounds/ArgonPro`, embedded at compile time). Every osu!standard
//! gameplay lookup exists in that set, so the `ArgonProSkin.GetSample`
//! fallback chain never goes past it — including the slider sliding
//! loops (`sliderslide`/`sliderwhistle`), which the set ships as empty
//! PCM entries: ArgonPro plays NO sliding sounds. Node hit sounds
//! (head/repeat/tail) and `slidertick` have real samples and play.

use crate::game::{GameData, ObjKind};
use osu_replay_judge::process::NestedKind;
use osu_replay_judge::score::hit_result_ext;

/// Output sample rate (Hz). All shipped skin samples are 44.1k; foreign
/// rates are linearly resampled on decode.
pub const SAMPLE_RATE: u32 = 44_100;

/// `DrawableHitObject.MINIMUM_SAMPLE_VOLUME`.
const MINIMUM_SAMPLE_VOLUME: i32 = 5;
/// `LegacyBeatmapDecoder.CONTROL_POINT_LENIENCY`.
const CONTROL_POINT_LENIENCY: f64 = 5.0;
/// Lazer default `PositionalHitsoundsLevel` (0.8) doubled, see
/// `CalculateSamplePlaybackBalance`.
const POSITIONAL_HITSOUNDS_LEVEL: f64 = 0.8;

// ---------------------------------------------------------------------------
// .osu sample-side parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bank {
    Normal,
    Soft,
    Drum,
}

impl Bank {
    /// `(LegacySampleBank)int`; 0 (None) and invalid values mean "not
    /// specified" for object-level banks, and Normal for timing points.
    fn from_legacy(v: i64) -> Option<Bank> {
        match v {
            1 => Some(Bank::Normal),
            2 => Some(Bank::Soft),
            3 => Some(Bank::Drum),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Bank::Normal => "normal",
            Bank::Soft => "soft",
            Bank::Drum => "drum",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SamplePoint {
    time: f64,
    bank: Bank,
    volume: i32,
}

/// `ConvertHitObjectParser.SampleBankInfo`: banks/volume read from a
/// hitobject's trailing hitSample (or a slider's edgeSets entry).
#[derive(Clone, Default, Debug)]
struct BankInfo {
    /// Bank for hitnormal; `None` = inherit from the control point.
    normal: Option<Bank>,
    /// Bank for additions; `None` = same as `normal`.
    additions: Option<Bank>,
    /// 0-100; 0 = inherit from the control point.
    volume: i32,
}

struct RawObj {
    start_time: f64,
    /// Circle: start time; spinner: end time; slider: unused (node sample
    /// points are resolved from the processed object's duration).
    end_time: f64,
    /// Slider only: the .osu repeat field (span count).
    span_count: usize,
    /// HitSound bitmask: 2 whistle, 4 finish, 8 clap.
    sound_type: u8,
    bank: BankInfo,
    /// Per-node (head, repeats..., tail) sound types / banks.
    node_types: Vec<u8>,
    node_banks: Vec<BankInfo>,
}

struct SampleData {
    points: Vec<SamplePoint>,
    objects: Vec<RawObj>,
}

/// `readCustomSampleBanks`. `banks_only` mirrors the slider object-level
/// call, where the trailing hitSample contributes banks but no volume.
fn read_custom_sample_banks(s: &str, info: &mut BankInfo, banks_only: bool) {
    let split: Vec<&str> = s.split(':').collect();
    let parse = |v: Option<&&str>| -> i64 {
        v.and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0)
    };
    info.normal = Bank::from_legacy(parse(split.first()));
    let add = Bank::from_legacy(parse(split.get(1)));
    info.additions = add.or(info.normal);
    if !banks_only && split.len() > 3 {
        info.volume = parse(split.get(3)).max(0) as i32;
    }
}

fn parse_sample_data(content: &str) -> SampleData {
    let mut default_bank = Bank::Normal;
    let mut default_volume = 100i32;
    let mut points: Vec<SamplePoint> = Vec::new();
    let mut objects: Vec<RawObj> = Vec::new();
    let mut section = "";

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }

        match section {
            "General" => {
                let mut pair = line.splitn(2, ':');
                let key = pair.next().unwrap_or("").trim();
                let value = pair.next().unwrap_or("").trim();
                match key {
                    "SampleSet" => {
                        default_bank = match value.to_ascii_lowercase().as_str() {
                            "soft" => Bank::Soft,
                            "drum" => Bank::Drum,
                            _ => Bank::Normal, // normal + none + numeric 0
                        };
                    }
                    "SampleVolume" => {
                        default_volume = value.trim().parse().unwrap_or(default_volume);
                    }
                    _ => {}
                }
            }
            "TimingPoints" => {
                let split: Vec<&str> = line.split(',').collect();
                let time = split.first().and_then(|s| s.trim().parse::<f64>().ok());
                let Some(time) = time else { continue };
                // Fields 4-6: sampleSet, sampleSetIndex, volume. Missing
                // fields fall back to the [General] defaults.
                let bank = split
                    .get(3)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .and_then(Bank::from_legacy)
                    .unwrap_or(default_bank);
                let volume = split
                    .get(5)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(default_volume as i64)
                    .clamp(0, 100) as i32;
                let point = SamplePoint { time, bank, volume };
                // Same-time lines: the last one wins (control point groups
                // replace, non-redundant later additions override).
                if points.last().map(|p| p.time == time).unwrap_or(false) {
                    points.pop();
                }
                points.push(point);
            }
            "HitObjects" => {
                let split: Vec<&str> = line.split(',').collect();
                if split.len() < 5 {
                    continue;
                }
                let start_time = split[2].trim().parse::<f64>().unwrap_or(0.0);
                let obj_type = split[3].trim().parse::<i64>().unwrap_or(0);
                let sound_type = split[4].trim().parse::<i64>().unwrap_or(0).max(0) as u8;
                let mut bank = BankInfo::default();

                if obj_type & 1 != 0 {
                    // Circle: x,y,time,type,hitSound,hitSample
                    if let Some(s) = split.get(5) {
                        read_custom_sample_banks(s, &mut bank, false);
                    }
                    objects.push(RawObj {
                        start_time,
                        end_time: start_time,
                        span_count: 0,
                        sound_type,
                        bank,
                        node_types: Vec::new(),
                        node_banks: Vec::new(),
                    });
                } else if obj_type & 2 != 0 {
                    // Slider: ...,path,repeats,pixelLength,edgeSounds,
                    // edgeSets,hitSample (hitSample is banks-only).
                    if let Some(s) = split.get(10) {
                        read_custom_sample_banks(s, &mut bank, true);
                    }
                    let span_count = split.get(5 + 1).and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                    let nodes = span_count + 1;
                    let mut node_banks = vec![bank.clone(); nodes];
                    if let Some(sets) = split.get(9).filter(|s| !s.is_empty()) {
                        for (i, set) in sets.split('|').enumerate() {
                            if i >= nodes {
                                break;
                            }
                            read_custom_sample_banks(set, &mut node_banks[i], false);
                        }
                    }
                    let mut node_types = vec![sound_type; nodes];
                    if let Some(adds) = split.get(8).filter(|s| !s.is_empty()) {
                        for (i, add) in adds.split('|').enumerate() {
                            if i >= nodes {
                                break;
                            }
                            if let Ok(v) = add.trim().parse::<i64>() {
                                node_types[i] = v.max(0) as u8;
                            }
                        }
                    }
                    objects.push(RawObj {
                        start_time,
                        end_time: start_time,
                        span_count,
                        sound_type,
                        bank,
                        node_types,
                        node_banks,
                    });
                } else if obj_type & 8 != 0 {
                    // Spinner: x,y,time,type,hitSound,endTime,hitSample
                    let end_time = split.get(5).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(start_time);
                    if let Some(s) = split.get(6) {
                        read_custom_sample_banks(s, &mut bank, false);
                    }
                    objects.push(RawObj {
                        start_time,
                        end_time: end_time.max(start_time),
                        span_count: 0,
                        sound_type,
                        bank,
                        node_types: Vec::new(),
                        node_banks: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    // Match the decoder's ordering so indices line up with the processed
    // objects: stable sort by start time.
    objects.sort_by(|a, b| a.start_time.partial_cmp(&b.start_time).unwrap());
    SampleData { points, objects }
}

/// `SamplePointAt`: rightmost point with `time <= t`; before the first
/// point that point itself applies, else normal/100.
fn point_at(points: &[SamplePoint], t: f64) -> SamplePoint {
    if points.is_empty() {
        return SamplePoint { time: f64::NEG_INFINITY, bank: Bank::Normal, volume: 100 };
    }
    let mut lo = 0usize;
    let mut hi = points.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if points[mid].time <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 { points[0] } else { points[lo - 1] }
}

/// A fully resolved playback sample.
#[derive(Clone, Copy, Debug)]
struct HitSample {
    name: &'static str,
    bank: Bank,
    volume: i32,
}

/// `convertSoundType` + `LegacySampleControlPoint.ApplyTo`: hitnormal
/// always, additions per flags; unspecified banks/volumes inherit from the
/// sample point active at the given time.
fn resolve_samples(sound_type: u8, info: &BankInfo, point: SamplePoint) -> Vec<HitSample> {
    let mut out = Vec::with_capacity(4);
    let mut push = |name: &'static str, bank: Option<Bank>| {
        let bank = bank.unwrap_or(point.bank);
        let volume = if info.volume > 0 { info.volume } else { point.volume };
        out.push(HitSample { name, bank, volume });
    };
    push("hitnormal", info.normal);
    if sound_type & 0b100 != 0 {
        push("hitfinish", info.additions);
    }
    if sound_type & 0b10 != 0 {
        push("hitwhistle", info.additions);
    }
    if sound_type & 0b1000 != 0 {
        push("hitclap", info.additions);
    }
    out
}

// ---------------------------------------------------------------------------
// Event assembly (map timeline, ms)
// ---------------------------------------------------------------------------

/// One scheduled sample placement: judgement time plus the playfield X for
/// stereo balance. Loop sounds are pre-expanded into placements at their
/// natural tile length, cut at the tracking end (`until`) — lazer stops
/// the looping sample the moment tracking breaks.
struct Placement {
    time: f64,
    sample: HitSample,
    /// Playfield X in osu coordinates (0..512).
    x: f32,
    /// Map-time cutoff (ms) for loop tiles; `None` lets the sample ring
    /// out naturally.
    until: Option<f64>,
}

fn balance(x: f32) -> f64 {
    let b = POSITIONAL_HITSOUNDS_LEVEL * 2.0 * (x as f64 / 512.0 - 0.5);
    (b * 100.0).round() / 100.0
}

fn build_placements(game: &GameData, data: &SampleData, t0: f64, t_map_end: f64) -> Vec<Placement> {
    let mut out: Vec<Placement> = Vec::new();
    if game.objects.len() != data.objects.len() {
        return out; // parser mismatch; silence rather than desynced sounds
    }

    for (obj, raw) in game.objects.iter().zip(&data.objects) {
        match obj.kind {
            ObjKind::Circle => {
                if let Some((t, r)) = obj.body_judged {
                    if hit_result_ext::is_hit(r) && t >= t0 && t <= t_map_end {
                        let point = point_at(&data.points, obj.end_time + CONTROL_POINT_LENIENCY);
                        for s in resolve_samples(raw.sound_type, &raw.bank, point) {
                            out.push(Placement { time: t, sample: s, x: obj.position[0], until: None });
                        }
                    }
                }
            }
            ObjKind::Spinner => {
                if let Some((t, r)) = obj.body_judged {
                    if hit_result_ext::is_hit(r) && t >= t0 && t <= t_map_end {
                        let point = point_at(&data.points, raw.end_time + CONTROL_POINT_LENIENCY);
                        for s in resolve_samples(raw.sound_type, &raw.bank, point) {
                            out.push(Placement { time: t, sample: s, x: obj.position[0], until: None });
                        }
                    }
                }
            }
            ObjKind::Slider => {
                // Object-level samples resolve at StartTime + leniency + 1
                // and seed the tick + sliding sounds.
                let head_point = point_at(&data.points, obj.start_time + CONTROL_POINT_LENIENCY + 1.0);
                let obj_samples = resolve_samples(raw.sound_type, &raw.bank, head_point);
                let obj_normal = obj_samples.iter().find(|s| s.name == "hitnormal").copied().unwrap_or(HitSample {
                    name: "hitnormal",
                    bank: head_point.bank,
                    volume: head_point.volume,
                });
                let span_duration = if obj.span_count > 0 { obj.duration / obj.span_count as f64 } else { 0.0 };

                let node_samples = |i: usize| -> Vec<HitSample> {
                    let ty = raw.node_types.get(i).copied().unwrap_or(raw.sound_type);
                    let info = raw.node_banks.get(i).unwrap_or(&raw.bank);
                    let point = point_at(
                        &data.points,
                        obj.start_time + i as f64 * span_duration + CONTROL_POINT_LENIENCY,
                    );
                    resolve_samples(ty, info, point)
                };

                // Head (NodeSamples[0]).
                if let Some((t, r)) = obj.head_judged {
                    if hit_result_ext::is_hit(r) && t >= t0 && t <= t_map_end {
                        for s in node_samples(0) {
                            out.push(Placement { time: t, sample: s, x: obj.position[0], until: None });
                        }
                    }
                }

                for n in &obj.nested {
                    let Some((t, r)) = n.judged else { continue };
                    if !hit_result_ext::is_hit(r) || t < t0 || t > t_map_end {
                        continue;
                    }
                    match n.kind {
                        NestedKind::Tick => {
                            out.push(Placement {
                                time: t,
                                sample: HitSample { name: "slidertick", ..obj_normal },
                                x: n.position[0],
                                until: None,
                            });
                        }
                        NestedKind::Repeat => {
                            // RepeatIndex + 1 == span index of the repeat.
                            for s in node_samples(n.span_index) {
                                out.push(Placement { time: t, sample: s, x: n.position[0], until: None });
                            }
                        }
                        NestedKind::Head | NestedKind::Tail => {}
                    }
                }

                // Slider body judgement plays the tail samples.
                if let Some((t, r)) = obj.body_judged {
                    if hit_result_ext::is_hit(r) && t >= t0 && t <= t_map_end {
                        for s in node_samples(obj.span_count) {
                            out.push(Placement { time: t, sample: s, x: obj.end_position[0], until: None });
                        }
                    }
                }

                // Sliding loops while tracked (snapshots carry the engine's
                // tracking state at 60fps game frames, map timeline).
                let slide = HitSample { name: "sliderslide", ..obj_normal };
                let whistle = (raw.sound_type & 0b10 != 0)
                    .then(|| HitSample {
                        name: "sliderwhistle",
                        bank: obj_samples.iter().find(|s| s.name == "hitwhistle").map(|s| s.bank).unwrap_or(obj_normal.bank),
                        volume: obj_normal.volume,
                    });
                let slide_len_ms = sample_clip(slide).map(|w| w.duration_ms()).unwrap_or(0.0);
                let whistle_len_ms = whistle
                    .and_then(|w| sample_clip(w))
                    .map(|w| w.duration_ms())
                    .unwrap_or(0.0);
                if slide_len_ms > 0.0 || whistle_len_ms > 0.0 {
                    let dbg = std::env::var("HITSOUND_DEBUG").is_ok();
                    let mut run: Option<(f64, f64)> = None; // [start, next-untracked time]
                    for (i, snap) in game.snapshots.iter().enumerate() {
                        let tracked = snap
                            .sliders
                            .iter()
                            .any(|(idx, tr)| *idx == obj.index && *tr)
                            && snap.time >= obj.start_time
                            && snap.time <= obj.end_time;
                        match (&mut run, tracked) {
                            (Some((_, end)), true) => {
                                let next = game.snapshots.get(i + 1).map(|s| s.time).unwrap_or(snap.time);
                                *end = next.max(snap.time);
                            }
                            (None, true) => {
                                let next = game.snapshots.get(i + 1).map(|s| s.time).unwrap_or(snap.time);
                                run = Some((snap.time, next.max(snap.time)));
                            }
                            _ => {
                                if let Some((a, b)) = run.take() {
                                    tile_loop(&mut out, obj, a, b, slide, slide_len_ms, t0, t_map_end);
                                    if let Some(w) = whistle {
                                        tile_loop(&mut out, obj, a, b, w, whistle_len_ms, t0, t_map_end);
                                    }
                                }
                            }
                        }
                    }
                    if let Some((a, b)) = run.take() {
                        tile_loop(&mut out, obj, a, b, slide, slide_len_ms, t0, t_map_end);
                        if let Some(w) = whistle {
                            tile_loop(&mut out, obj, a, b, w, whistle_len_ms, t0, t_map_end);
                        }
                    }
                    if dbg {
                        let tracked_frames = game
                            .snapshots
                            .iter()
                            .filter(|s| s.sliders.iter().any(|(i, tr)| *i == obj.index && *tr))
                            .count();
                        eprintln!(
                            "hitsound debug: slider #{} [{:.0}..{:.0}] head={:?} body={:?} nested={} slide_len={:.0} whistle={} tracked_frames={}",
                            obj.index,
                            obj.start_time,
                            obj.end_time,
                            obj.head_judged.map(|(t, r)| (t as i64, format!("{:?}", r))),
                            obj.body_judged.map(|(t, r)| (t as i64, format!("{:?}", r))),
                            obj.nested.iter().filter(|n| n.judged.is_some()).count(),
                            slide_len_ms,
                            whistle.is_some(),
                            tracked_frames
                        );
                    }
                }
            }
        }
    }

    // Combo breaks (`ComboEffects`): when the score processor's combo
    // drops to zero the combobreak sample plays if the old combo was > 20,
    // or on the very first break (`AlwaysPlayFirstComboBreak`, default
    // on). Full volume, centered (a plain `SampleInfo`, no balance).
    {
        let mut first_break = false;
        let mut prev_combo = 0;
        for e in &game.score_events {
            if e.combo == 0 && prev_combo != 0 && (prev_combo > 20 || !first_break) {
                first_break = true;
                if e.time >= t0 && e.time <= t_map_end {
                    out.push(Placement {
                        time: e.time,
                        sample: HitSample { name: "combobreak", bank: Bank::Normal, volume: 100 },
                        x: 256.0,
                        until: None,
                    });
                }
            }
            prev_combo = e.combo;
        }
    }
    out
}

/// Tiles a loop sample across the tracked interval [a, b] (map ms) at its
/// natural length; balance follows the ball position per tile.
fn tile_loop(
    out: &mut Vec<Placement>,
    obj: &crate::game::ObjView,
    a: f64,
    b: f64,
    sample: HitSample,
    len_ms: f64,
    t0: f64,
    t_map_end: f64,
) {
    if len_ms <= 0.0 {
        return;
    }
    let mut t = a;
    while t < b {
        if t >= t0 && t <= t_map_end {
            let progress = if obj.duration > 0.0 { (t - obj.start_time) / obj.duration } else { 0.0 };
            let x = obj.slider_ball_at(progress.clamp(0.0, 1.0))[0];
            out.push(Placement { time: t, sample, x, until: Some(b.min(t_map_end)) });
        }
        t += len_ms;
    }
}

// ---------------------------------------------------------------------------
// Skin samples (embedded ArgonPro hitsounds)
// ---------------------------------------------------------------------------

macro_rules! wav {
    ($file:literal) => {
        include_bytes!(concat!("../assets/sounds/ArgonPro/", $file, ".wav"))
    };
}

/// `Gameplay/combobreak` (a `SampleInfo`, no bank): not present in the
/// ArgonPro set at all, so the `ArgonProSkin.GetSample` chain falls to
/// `Gameplay/Argon/combobreak`.
const COMBOBREAK: &[u8] = include_bytes!("../assets/sounds/Argon/combobreak.wav");

/// `ArgonProSkin.GetSample` (osu.Game/Skinning/ArgonProSkin.cs) resolves
/// each `HitSampleInfo.LookupNames` entry through the skin's own samples
/// (none embedded), then `Gameplay/ArgonPro/`, then `Gameplay/Argon/`,
/// then the plain `Gameplay/` set. Every banked osu!standard gameplay
/// lookup EXISTS in the ArgonPro set — a present resource ends the chain,
/// even when it decodes to no PCM: the set's `sliderslide`/
/// `sliderwhistle` entries are empty (muted), so ArgonPro plays no
/// sliding sounds and the loop tiles degrade to silence instead of
/// falling through to the Argon copies. Bank-less lookups the ArgonPro
/// set doesn't carry (combobreak) resolve from the Argon level.
///
/// LookupNames: "Gameplay/{Bank}-{Name}{Suffix}" → "Gameplay/{Bank}-{Name}".
/// Suffix lookups need custom sample banks (index ≥ 2), which the
/// embedded set doesn't provide, so only the plain bank form applies.
fn sample_clip(sample: HitSample) -> Option<Clip> {
    let bytes: &[u8] = match (sample.bank.as_str(), sample.name) {
        ("normal", "hitnormal") => wav!("normal-hitnormal"),
        ("normal", "hitwhistle") => wav!("normal-hitwhistle"),
        ("normal", "hitfinish") => wav!("normal-hitfinish"),
        ("normal", "hitclap") => wav!("normal-hitclap"),
        ("normal", "slidertick") => wav!("normal-slidertick"),
        ("normal", "sliderslide") => wav!("normal-sliderslide"),
        ("normal", "sliderwhistle") => wav!("normal-sliderwhistle"),
        ("soft", "hitnormal") => wav!("soft-hitnormal"),
        ("soft", "hitwhistle") => wav!("soft-hitwhistle"),
        ("soft", "hitfinish") => wav!("soft-hitfinish"),
        ("soft", "hitclap") => wav!("soft-hitclap"),
        ("soft", "slidertick") => wav!("soft-slidertick"),
        ("soft", "sliderslide") => wav!("soft-sliderslide"),
        ("soft", "sliderwhistle") => wav!("soft-sliderwhistle"),
        ("drum", "hitnormal") => wav!("drum-hitnormal"),
        ("drum", "hitwhistle") => wav!("drum-hitwhistle"),
        ("drum", "hitfinish") => wav!("drum-hitfinish"),
        ("drum", "hitclap") => wav!("drum-hitclap"),
        ("drum", "slidertick") => wav!("drum-slidertick"),
        ("drum", "sliderslide") => wav!("drum-sliderslide"),
        ("drum", "sliderwhistle") => wav!("drum-sliderwhistle"),
        (_, "combobreak") => COMBOBREAK,
        _ => return None,
    };
    decode_wav(bytes)
}

// ---------------------------------------------------------------------------
// WAV decode / mix / encode
// ---------------------------------------------------------------------------

/// Decoded clip: interleaved stereo f32 at `SAMPLE_RATE`.
struct Clip {
    data: Vec<f32>,
}

impl Clip {
    fn duration_ms(&self) -> f64 {
        self.data.len() as f64 / 2.0 / SAMPLE_RATE as f64 * 1000.0
    }
}

/// Minimal RIFF/PCM16 decoder with linear resampling to the output rate;
/// mono sources are duplicated to both channels.
fn decode_wav(bytes: &[u8]) -> Option<Clip> {
    let rd = |i: usize| -> u16 {
        u16::from_le_bytes([*bytes.get(i).unwrap_or(&0), *bytes.get(i + 1).unwrap_or(&0)])
    };
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (audio format, channels, rate, bits)
    let mut data: Option<(usize, usize)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let body = pos + 8;
        if body + size > bytes.len() {
            break;
        }
        match id {
            b"fmt " => {
                let audio_format = rd(body);
                let channels = rd(body + 2);
                let rate = u32::from_le_bytes([bytes[body + 4], bytes[body + 5], bytes[body + 6], bytes[body + 7]]);
                let bits = rd(body + 14);
                fmt = Some((audio_format, channels, rate, bits));
            }
            b"data" => data = Some((body, size)),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let (format, channels, rate, bits) = fmt?;
    let (body, size) = data?;
    let channels = channels as usize;
    if format != 1 || bits != 16 || channels == 0 || channels > 2 || rate == 0 {
        return None;
    }
    let frames = size / 2 / channels;
    let src = |f: usize, c: usize| -> f32 {
        let v = rd(body + (f * channels + c) * 2) as i16;
        v as f32 / 32768.0
    };
    let mut data = Vec::with_capacity(frames * 2);
    if rate == SAMPLE_RATE {
        for f in 0..frames {
            let l = src(f, 0);
            let r = if channels == 2 { src(f, 1) } else { l };
            data.push(l);
            data.push(r);
        }
    } else {
        let out_frames = ((frames as f64) * (SAMPLE_RATE as f64 / rate as f64)).round() as usize;
        for f in 0..out_frames {
            let t = f as f64 * rate as f64 / SAMPLE_RATE as f64;
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            let j = (i + 1).min(frames.saturating_sub(1));
            let l = src(i, 0) * (1.0 - frac) + src(j, 0) * frac;
            let r = if channels == 2 {
                src(i, 1) * (1.0 - frac) + src(j, 1) * frac
            } else {
                l
            };
            data.push(l);
            data.push(r);
        }
    }
    Some(Clip { data })
}

/// Mixes one placement into `buf` (interleaved stereo f32). `speed`
/// resamples the clip (rate mods pitch samples up, like lazer's gameplay
/// audio adjustments); `until_sec` truncates loop tiles where lazer would
/// stop the looping sample.
fn place(buf: &mut [f32], clip: &Clip, start_sec: f64, speed: f64, gl: f32, gr: f32, until_sec: Option<f64>) {
    let frames = clip.data.len() / 2;
    if frames == 0 || speed <= 0.0 {
        return;
    }
    let dst_start = (start_sec * SAMPLE_RATE as f64).round() as isize;
    let mut dst_len = (frames as f64 / speed).ceil() as usize;
    if let Some(until) = until_sec {
        let cut = ((until * SAMPLE_RATE as f64).round() as isize - dst_start).max(0) as usize;
        dst_len = dst_len.min(cut);
    }
    for n in 0..dst_len {
        let dst = dst_start + n as isize;
        if dst < 0 {
            continue;
        }
        let dst = dst as usize;
        if dst * 2 + 1 >= buf.len() {
            break;
        }
        let t = n as f64 * speed;
        let i = t.floor() as usize;
        if i >= frames {
            break;
        }
        let frac = (t - i as f64) as f32;
        let j = (i + 1).min(frames - 1);
        let l = clip.data[i * 2] * (1.0 - frac) + clip.data[j * 2] * frac;
        let r = clip.data[i * 2 + 1] * (1.0 - frac) + clip.data[j * 2 + 1] * frac;
        buf[dst * 2] += l * gl;
        buf[dst * 2 + 1] += r * gr;
    }
}

/// Renders the hitsound track for the exported range and encodes it as a
/// PCM16 stereo WAV. `t0` is the first output frame's map time,
/// `wall_secs` the output video's duration in seconds; the track spans
/// exactly that wall window so it muxes 1:1 with the video.
/// `master_gain` scales the whole bus (`--hitsounds-volume`).
///
/// Loudness follows the game's defaults: samples play at their authored
/// level (beatmap volume × the sample's mastering, Effect channel 1.0),
/// no bus normalization. Stacked hits sum in float and the encoder's
/// soft limiter replaces the DAC clipping the game would do.
/// A one-shot hitsound event on the map timeline, for live preview
/// playback (fire when the playhead crosses `time`).
#[derive(Clone, Copy, Debug)]
pub struct HitsoundEvent {
    /// Map time in ms (judgement time).
    pub time: f64,
    /// Sample name: "hitnormal"/"hitwhistle"/"hitfinish"/"hitclap"/
    /// "slidertick"/"combobreak" (loops are not exposed: ArgonPro mutes
    /// them).
    pub name: &'static str,
    /// Sample bank: "normal"/"soft"/"drum".
    pub bank: &'static str,
    /// Beatmap sample volume 0-100 (apply `max(5)` and the Effect
    /// channel volume on the receiver's side).
    pub volume: i32,
    /// Playfield X (0..512) for stereo balance.
    pub pan_x: f32,
}

/// All one-shot hitsound events of a replay (sorted by time, loops
/// excluded). `map_content` is the raw .osu the game data was loaded
/// from. Volume/bank resolution follows the same lazer semantics as the
/// offline track.
pub fn collect_events(game: &GameData, map_content: &str) -> Vec<HitsoundEvent> {
    let data = parse_sample_data(map_content);
    let mut events: Vec<HitsoundEvent> = build_placements(game, &data, f64::NEG_INFINITY, f64::INFINITY)
        .into_iter()
        .filter(|p| p.until.is_none())
        .map(|p| HitsoundEvent {
            time: p.time,
            name: p.sample.name,
            bank: p.sample.bank.as_str(),
            volume: p.sample.volume,
            pan_x: p.x,
        })
        .collect();
    events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap());
    events
}

/// Embedded skin sample bytes for a (bank, name) pair (ArgonPro set,
/// combobreak from the Argon set per the lookup chain). PCM WAV, for the
/// caller's own decoder.
pub fn sample_bytes(bank: &str, name: &str) -> Option<&'static [u8]> {
    let bytes: &[u8] = match (bank, name) {
        ("normal", "hitnormal") => wav!("normal-hitnormal"),
        ("normal", "hitwhistle") => wav!("normal-hitwhistle"),
        ("normal", "hitfinish") => wav!("normal-hitfinish"),
        ("normal", "hitclap") => wav!("normal-hitclap"),
        ("normal", "slidertick") => wav!("normal-slidertick"),
        ("normal", "sliderslide") => wav!("normal-sliderslide"),
        ("normal", "sliderwhistle") => wav!("normal-sliderwhistle"),
        ("soft", "hitnormal") => wav!("soft-hitnormal"),
        ("soft", "hitwhistle") => wav!("soft-hitwhistle"),
        ("soft", "hitfinish") => wav!("soft-hitfinish"),
        ("soft", "hitclap") => wav!("soft-hitclap"),
        ("soft", "slidertick") => wav!("soft-slidertick"),
        ("soft", "sliderslide") => wav!("soft-sliderslide"),
        ("soft", "sliderwhistle") => wav!("soft-sliderwhistle"),
        ("drum", "hitnormal") => wav!("drum-hitnormal"),
        ("drum", "hitwhistle") => wav!("drum-hitwhistle"),
        ("drum", "hitfinish") => wav!("drum-hitfinish"),
        ("drum", "hitclap") => wav!("drum-hitclap"),
        ("drum", "slidertick") => wav!("drum-slidertick"),
        ("drum", "sliderslide") => wav!("drum-sliderslide"),
        ("drum", "sliderwhistle") => wav!("drum-sliderwhistle"),
        (_, "combobreak") => COMBOBREAK,
        _ => return None,
    };
    Some(bytes)
}

pub fn render_track_wav(game: &GameData, map_content: &str, t0: f64, wall_secs: f64, rate: f64, master_gain: f32) -> Vec<u8> {
    let data = parse_sample_data(map_content);
    let t_map_end = t0 + wall_secs * rate * 1000.0;
    let placements = build_placements(game, &data, t0, t_map_end);
    if std::env::var("HITSOUND_DEBUG").is_ok() {
        eprintln!("hitsound debug: parsed {} objects (game {}), {} points, {} placements in [{},{}]",
            data.objects.len(), game.objects.len(), data.points.len(), placements.len(), t0, t_map_end);
    }

    let total = (wall_secs.max(0.0) * SAMPLE_RATE as f64).round() as usize;
    let mut buf = vec![0.0f32; total * 2];

    // Decode each distinct (bank, name) once.
    let mut cache: Vec<(HitSample, Clip)> = Vec::new();
    for p in &placements {
        if cache.iter().all(|(s, _)| s.name != p.sample.name || s.bank != p.sample.bank) {
            if let Some(clip) = sample_clip(p.sample) {
                cache.push((p.sample, clip));
            }
        }
    }

    for p in &placements {
        let Some((_, clip)) = cache.iter().find(|(s, _)| s.name == p.sample.name && s.bank == p.sample.bank) else {
            continue;
        };
        let volume = p.sample.volume.max(MINIMUM_SAMPLE_VOLUME) as f32 / 100.0;
        let bal = balance(p.x) as f32;
        let gl = volume * (1.0 - bal.max(0.0));
        let gr = volume * (1.0 - (-bal).max(0.0));
        let wall = (p.time - t0) / rate / 1000.0;
        let until = p.until.map(|u| (u - t0) / rate / 1000.0);
        place(&mut buf, clip, wall, rate, gl, gr, until);
    }

    // `--hitsounds-volume` master gain (game default: Effect 1.0).
    if (master_gain - 1.0).abs() > 1e-6 {
        for v in &mut buf {
            *v *= master_gain;
        }
    }

    if std::env::var("HITSOUND_DEBUG").is_ok() {
        let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        eprintln!("hitsound debug: {} clips decoded, buffer peak {:.4}", cache.len(), peak);
        let mut names: Vec<(usize, &str)> = Vec::new();
        for p in &placements {
            match names.iter_mut().find(|(_, n)| *n == p.sample.name) {
                Some((c, _)) => *c += 1,
                None => names.push((1, p.sample.name)),
            }
        }
        eprintln!("hitsound debug: {:?}", names);
        for p in placements.iter().take(6) {
            eprintln!("hitsound debug: t={:.0} {} {} vol={} x={:.0}", p.time, p.sample.bank.as_str(), p.sample.name, p.sample.volume, p.x);
        }
    }

    encode_wav(&buf)
}

/// Soft-knee limiter (tanh above the threshold). The game's BASS mixer
/// sums channels in float and only clips at the DAC; stacking samples
/// must not hard-clip inside the track, or dense sections turn into
/// rail-slamming squares that bury the BGM once summed again in ffmpeg.
fn soft_limit(x: f32) -> f32 {
    const T: f32 = 0.7;
    let a = x.abs();
    if a <= T {
        x
    } else {
        let t = (a - T) / (1.0 - T);
        let limited = T + (1.0 - T) * t.tanh();
        limited.copysign(x)
    }
}

fn encode_wav(interleaved: &[f32]) -> Vec<u8> {
    let frames = interleaved.len() / 2;
    let data_len = frames * 4;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.reserve(data_len);
    for v in interleaved {
        let s = soft_limit(*v);
        out.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
    }
    out
}
