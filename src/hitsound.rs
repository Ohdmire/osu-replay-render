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
//! - sliders play `NodeSamples[0]` on head hits, `NodeSamples[RepeatIndex+1]`
//!   on repeat hits, `TailSamples` (`NodeSamples[span]`) on the body
//!   judgement, `slidertick` on tick hits, and loop `sliderslide` (+
//!   `sliderwhistle` when the whistle flag is set) while tracked. The body
//!   samples are gated on the TAIL circle's own result
//!   (`DrawableSlider.PlaySamples`: `!TailCircle.SamplePlaysOnlyOnHit ||
//!   TailCircle.IsHit`, and `SamplePlaysOnlyOnHit` defaults to true), not on
//!   the slider's aggregate judgement;
//! - playback volume = `max(volume, 5)%` (`MINIMUM_SAMPLE_VOLUME`) times the
//!   game's three layers: the gameplay mixer (`OsuSetting.HitsoundVolume`,
//!   default 0.6), `VolumeEffect` (default 0.6) and `VolumeUniversal`
//!   (default 0.6). `Gameplay/combobreak` skips the gameplay mixer only.
//!   Stereo balance follows the playfield X (`PositionalHitsoundsLevel`
//!   0.8 → `round2(1.6 * (x/512 - 0.5))`, clamped to ±1 the way BASS clamps
//!   `BASS_ATTRIB_PAN` — see `balance`). Samples always play at their
//!   natural rate, under every mod — rate mods only compress the trigger
//!   times onto the wall timeline, the pitch never shifts. That is parity,
//!   not a deviation: lazer's `ModRateAdjust` resamples the *track* only
//!   (`ApplyToTrack`), while `ApplyToSample` is consumed by storyboard
//!   samples alone (`DrawableStoryboardSample`), so gameplay hitsounds do
//!   not follow the rate either — and Nightcore/Daycore likewise pitch the
//!   BGM, never the hit samples;
//! - a combo drop to zero plays `Gameplay/combobreak` (`ComboEffects`:
//!   old combo > 20, or the first break while `AlwaysPlayFirstComboBreak`
//!   is on — the default), at full volume, centered;
//! - spinners: the judgement plays the line samples, bonus revolutions
//!   fire `spinnerbonus` at the ACTUAL full-rotation moments (spins past the
//!   tick budget play `spinnerbonus-max`, whose LookupNames strip "-max"),
//!   and a looping `spinnerspin` runs while the disc audibly spins —
//!   synthesized with a phase accumulator at the lazer frequency curve
//!   (`20k/44.1k + progressUnclamped * 40k/44.1k`, capped `100k/44.1k`;
//!   `SpinnerFrequencyModulate` off pins it to the natural rate) under
//!   the tracking envelope (`VolumeTo(1, 300)` / `VolumeTo(0, 240)`).
//!   All three derive from the spinner's own `Samples`, which the decoder
//!   resolved at `GetEndTime() + 5ms` — so their bank/volume come from the
//!   point at the spinner's END, not its start. `spinnerbonus-max` is a plain
//!   `HitSampleInfo` built from the *normal* sample: it inherits bank, volume
//!   and suffix only (no file reference, no beatmap sample layer, and — not
//!   being a `LegacyHitSampleInfo` — never the layered mute);
//! - at most `SAMPLE_CONCURRENCY` (6) instances of one sample file sound at
//!   once (`OsuGameBase.SAMPLE_CONCURRENCY` → BASS `max` +
//!   `BassFlags.SampleOverrideLongestPlaying`): the 7th simultaneous play
//!   stops the longest-playing one. A sample owned by a single reused
//!   `SkinnableSound` instance (`spinnerbonus-max`, `combobreak`) has a cap of
//!   1, because `Play()` runs `Stop(); Play();`. See [`limit_voices`];
//! - a legacy skin with `LayeredHitSounds: 0` silences the layered hitnormal
//!   that was auto-attached to a hitsound set without the Normal flag
//!   (`LegacyHitSampleInfo.IsLayered` → `LegacySkinTransformer.GetSample`
//!   returns `SampleVirtual`), and the samples derived from it (`slidertick`,
//!   `sliderslide`, `spinnerspin`) inherit the mute. The mute belongs to the
//!   user-skin layer: the beatmap's own files are a separate skin source
//!   consulted BEFORE it, whose transformer never reads the user skin's
//!   setting, so a map-provided layered sample still plays;
//!
//! Samples come from the skin's `Gameplay/ArgonPro/` resource set
//! (`assets/sounds/ArgonPro`, embedded at compile time). Every osu!standard
//! gameplay lookup exists in that set, so the `ArgonProSkin.GetSample`
//! fallback chain never goes past it — including the slider sliding
//! loops (`sliderslide`/`sliderwhistle`), which the set ships as empty
//! PCM entries: ArgonPro plays NO sliding sounds. Node hit sounds
//! (head/repeat/tail) and `slidertick` have real samples and play.
//!
//! When a user skin is active (`--skin <dir>`) its own samples take
//! priority per element, with the ArgonPro set filling every slot the
//! skin leaves open (see [`SampleResolver`]).
//!
//! Beatmap-local sample files (the set folder's own
//! `soft-hitnormal.wav` / `normal-hitfinish2.wav` / …) sit ABOVE the
//! user skin, mirroring lazer's `LegacyBeatmapSkin` layer
//! (`BeatmapSkinProvidingContainer` → `LegacyBeatmapSkin.GetSample` →
//! user skin → default): a hit sample opts in via its custom sample
//! bank index (`UseBeatmapSamples` — the object's hitSample
//! `customIndex` when > 0, else the timing point's `sampleIndex`;
//! index ≥ 2 additionally pins the lookup to the suffixed name via
//! `UseCustomSampleBanks`, and a hitSample `filename` replaces the
//! hitnormal slot outright as `FileHitSampleInfo`). A file found in
//! the set folder is authoritative for its slot — including 0-byte
//! placeholders (deliberate silence), never falling through to the
//! skin. See [`BeatmapSampleStore`] and [`resolve_sample_bytes`].

use crate::game::{GameData, ObjKind};
use osu_parse::samples::{
    SampleBank as Bank, SampleBankInfo as BankInfo, SampleData, SampleObject as RawObj,
    SamplePoint,
};
use osu_replay_judge::process::NestedKind;
use osu_replay_judge::score::hit_result_ext;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
/// `OsuGameBase.SAMPLE_CONCURRENCY` → `ISample.PlaybackConcurrency`: how many
/// instances of one sample file may sound at the same time. The BASS handle is
/// loaded with `BassFlags.SampleOverrideLongestPlaying`, so the 7th
/// simultaneous play stops the instance that has been playing longest.
const SAMPLE_CONCURRENCY: usize = 6;


/// `SamplePointAt`: rightmost point with `time <= t`; before the first
/// point that point itself applies, else normal/100.
fn point_at(points: &[SamplePoint], t: f64) -> SamplePoint {
    if points.is_empty() {
        return SamplePoint { time: f64::NEG_INFINITY, bank: Bank::Normal, custom_index: 0, volume: 100 };
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
#[derive(Clone, Debug)]
struct HitSample {
    name: &'static str,
    bank: Bank,
    /// Merged custom sample bank index (`ApplyTo`: the object's
    /// hitSample/edgeSets index when > 0, else the timing point's
    /// `sampleIndex`). ≥1 opts into the beatmap's own sample files
    /// (`UseBeatmapSamples`), ≥2 additionally pins lookups to the
    /// suffixed name (`UseCustomSampleBanks`).
    custom: i32,
    volume: i32,
    /// The hitSample's explicit filename (`FileHitSampleInfo`): replaces
    /// the banked hitnormal slot with this beatmap-local file.
    filename: Option<String>,
    /// `ConvertHitObjectParser.LegacyHitSampleInfo.IsLayered`: the hitnormal
    /// auto-attached to a hitsound set that does NOT carry the Normal flag
    /// (`type != None && !type.HasFlag(Normal)`). `With()` keeps the flag, so
    /// the samples derived from that normal (`slidertick`, `sliderslide`,
    /// `spinnerspin`) inherit it. A legacy skin with `LayeredHitSounds: 0`
    /// replaces layered samples with `SampleVirtual`
    /// (`LegacySkinTransformer.GetSample`) — silence that ends the chain.
    /// Only the user-skin layer is affected; see [`SampleResolver::resolve`].
    layered: bool,
}

impl HitSample {
    /// How many instances of this sample may sound at once.
    ///
    /// `OsuGameBase.SAMPLE_CONCURRENCY` (6) is the per-file BASS limit. A
    /// sample owned by a single reused `SkinnableSound` instance is limited to
    /// 1 instead, because `SkinnableSound.Play()` runs `Stop(); Play();` on its
    /// one child and therefore cuts the previous playback:
    ///
    /// - `maxBonusSample` (`DrawableSpinner`, the `spinnerbonus-max` sample);
    /// - `ComboEffects.comboBreakSample` (`Gameplay/combobreak`).
    ///
    /// The two limits really live on different objects (a BASS sample handle vs
    /// a `SkinnableSound` instance). They are collapsed into one per-voice cap
    /// because each cap-1 owner also has a private file in practice; a skin
    /// that makes `spinnerbonus-max` fall back to `spinnerbonus` shares the
    /// 6-voice pool, which this model over-evicts (one play would stop every
    /// live bonus voice instead of the longest one).
    fn voice_cap(&self) -> usize {
        match self.name {
            "spinnerbonus-max" | "combobreak" => 1,
            _ => SAMPLE_CONCURRENCY,
        }
    }

    /// Whether the gameplay mixer's volume (`OsuSetting.HitsoundVolume`, bound
    /// to `DrawableRuleset.Audio`) applies. `ComboEffects` is added to the
    /// ruleset's `FrameStableComponents`, a sibling of that mixer, so
    /// `combobreak` bypasses it (it is still scaled by `VolumeEffect`).
    fn bypasses_gameplay_mixer(&self) -> bool {
        self.name == "combobreak"
    }
}

/// `LegacyHitSoundType`: None=0, Normal=1, Whistle=2, Finish=4, Clap=8.
/// `convertSoundType` marks the auto-attached hitnormal as layered exactly when
/// the set is non-empty and does not ask for the Normal sample itself.
fn is_layered(sound_type: u8) -> bool {
    sound_type != 0 && sound_type & 0b1 == 0
}

/// `convertSoundType` + `LegacySampleControlPoint.ApplyTo`: hitnormal
/// always, additions per flags; unspecified banks/volumes/custom index
/// inherit from the sample point active at the given time.
fn resolve_samples(sound_type: u8, info: &BankInfo, point: SamplePoint) -> Vec<HitSample> {
    let mut out = Vec::with_capacity(4);
    let custom = if info.custom_index > 0 { info.custom_index } else { point.custom_index };
    let volume = if info.volume > 0 { info.volume } else { point.volume };
    let layered = is_layered(sound_type);
    match info.filename.as_deref().filter(|f| !f.is_empty()) {
        // `FileHitSampleInfo`: custom index forced to 1 (beatmap-local,
        // never falls back through the suffixed chain), bank pinned to
        // normal; additions below still play. Never layered (`isLayered` keeps
        // its default `false`).
        Some(file) => out.push(HitSample {
            name: "hitnormal",
            bank: Bank::Normal,
            custom: 1,
            volume,
            filename: Some(file.to_string()),
            layered: false,
        }),
        None => out.push(HitSample {
            name: "hitnormal",
            bank: info.normal.unwrap_or(point.bank),
            custom,
            volume,
            filename: None,
            layered,
        }),
    }
    if sound_type & 0b100 != 0 {
        out.push(HitSample { name: "hitfinish", bank: info.additions.unwrap_or(point.bank), custom, volume, filename: None, layered: false });
    }
    if sound_type & 0b10 != 0 {
        out.push(HitSample { name: "hitwhistle", bank: info.additions.unwrap_or(point.bank), custom, volume, filename: None, layered: false });
    }
    if sound_type & 0b1000 != 0 {
        out.push(HitSample { name: "hitclap", bank: info.additions.unwrap_or(point.bank), custom, volume, filename: None, layered: false });
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

/// `DrawableHitObject.CalculateSamplePlaybackBalance`: the tracked playfield X
/// mapped to a stereo balance, already **clamped to [-1, 1]**.
///
/// The clamp is not cosmetic. lazer computes the same unclamped number and hands
/// it to BASS as `BASS_ATTRIB_PAN`, which clamps to [-1, 1] before the value can
/// reach the mix. Here the balance *is* the channel gain
/// (`gl = v * (1 - bal.max(0))`, `gr = v * (1 - (-bal).max(0))`), so an
/// out-of-range balance turns into a negative or >unity gain instead of a hard
/// pan. That is reachable with a degenerate beatmap position: an aspire
/// "infinite length" slider (`pixelLength = 2147483647`) makes `SliderPath`
/// scale its ~1.4 px path to ~1.5e9 playfield units, and `balance(1.5e9)` is
/// ~4.7e6 — a gain of ~-1.4e6 that slams the bus limiter into a full-scale
/// block (osu! beatmap 5839931, the 65-span sliders at 124.774 s / 125.074 s /
/// 125.374 s). Non-finite input maps to centre, like BASS treating an invalid
/// pan as 0.
fn balance(x: f32) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let b = POSITIONAL_HITSOUNDS_LEVEL * 2.0 * (x as f64 / 512.0 - 0.5);
    ((b * 100.0).round() / 100.0).clamp(-1.0, 1.0)
}

fn build_placements(game: &GameData, data: &SampleData, t0: f64, t_map_end: f64, resolver: &mut SampleResolver) -> Vec<Placement> {
    let mut out: Vec<Placement> = Vec::new();
    if game.objects.len() != data.objects.len() {
        // `SampleData::objects` is indexed POSITIONALLY against the beatmap's
        // hit-object list: row *i* describes object *i*. A parser that drops a
        // line on one side only therefore does not merely lose that object, it
        // shifts every later object onto the wrong samples — so bailing is the
        // only honest option. Warn loudly, because the alternative failure mode
        // is a map that is silently hitsound-free everywhere (`osu-parse`'s
        // per-line `MAX_COORDINATE_VALUE` drop did exactly this: 1713 judged
        // objects vs 2287 sample rows for beatmap 5839931).
        log::warn!(
            "hitsounds: beatmap parser and sample-table parser disagree ({} objects vs {} sample rows); \
             skipping every hitsound for this map",
            game.objects.len(),
            data.objects.len()
        );
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
                // Bonus revolutions (`SpinnerBonusTick`: Samples =
                // `CreateHitSampleInfo("spinnerbonus")`) fire at the ACTUAL
                // full-rotation moments. Plain `SpinnerTick`s carry no samples
                // in lazer (SmallBonus is silent); spins past the tick budget
                // play `spinnerbonus-max` (sound, no score — `maxBonusSample`;
                // its LookupNames tail falls back to "spinnerbonus" when a skin
                // has no -max file).
                //
                // All three reference samples derive from the spinner's own
                // `Samples`, which the decoder resolved at `GetEndTime() + 5`
                // (`applySamples`'s non-repeats branch; `GetPlayableBeatmap`
                // re-runs `ApplyDefaults` afterwards, so `CreateNestedHitObjects`
                // sees the applied list) — NOT the point at the start.
                let refs = spinner_refs(raw, data);
                for (oi, time, large) in &game.spinner_ticks {
                    if *oi == obj.index && *large && *time >= t0 && *time <= t_map_end {
                        out.push(Placement {
                            time: *time,
                            sample: HitSample { name: "spinnerbonus", ..refs.bonus() },
                            x: 256.0,
                            until: None,
                        });
                    }
                }
                for (oi, time) in &game.spinner_max_ticks {
                    if *oi == obj.index && *time >= t0 && *time <= t_map_end {
                        out.push(Placement {
                            time: *time,
                            // `SpinnerBonusMaxSampleInfo(HitObject.CreateHitSampleInfo())`
                            // is a plain `HitSampleInfo`: it copies the normal
                            // sample's bank, suffix and volume and nothing else.
                            // No file reference, `UseBeatmapSamples = false`, and
                            // not a `LegacyHitSampleInfo`, so the layered mute
                            // cannot reach it. `custom: 0` also drops the suffix,
                            // which matches the user legacy skin
                            // (`UseCustomSampleBanks = false` filters suffixed
                            // names out anyway).
                            sample: HitSample {
                                name: "spinnerbonus-max",
                                bank: refs.normal.bank,
                                custom: 0,
                                volume: refs.normal.volume,
                                filename: None,
                                layered: false,
                            },
                            x: 256.0,
                            until: None,
                        });
                    }
                }
            }
            ObjKind::Slider => {
                // Object-level samples resolve at StartTime + leniency + 1
                // and seed the tick + sliding sounds.
                let (obj_samples, obj_normal) = slider_object_samples(raw, obj, data);
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
                                sample: HitSample { name: "slidertick", ..obj_normal.clone() },
                                x: n.position[0],
                                until: None,
                            });
                        }
                        NestedKind::Repeat => {
                            // `repeat.Samples = GetNodeSamples(RepeatIndex + 1)`
                            // (`Slider.UpdateNestedSamples`), and `span_index`
                            // is the 0-based span the repeat closes — so the
                            // node index is one past it.
                            for s in node_samples(n.span_index + 1) {
                                out.push(Placement { time: t, sample: s, x: n.position[0], until: None });
                            }
                        }
                        NestedKind::Head | NestedKind::Tail => {}
                    }
                }

                // The body judgement plays `TailSamples`, gated on the TAIL
                // circle's own result (`DrawableSlider.PlaySamples`:
                // `!TailCircle.SamplePlaysOnlyOnHit || TailCircle.IsHit`;
                // `SamplePlaysOnlyOnHit` defaults to true and only
                // `OsuModClassic`'s "always play tail sample" relaxes it —
                // mods are out of scope here). The slider's own result is an
                // aggregate over every nested object (any nested hit → max),
                // so it may be a hit while the tail itself was missed; using
                // it as the gate wrongly plays the tail sample in that case.
                let tail = obj.nested.iter().find(|n| n.kind == NestedKind::Tail).and_then(|n| n.judged);
                if tail.is_some_and(|(_, r)| hit_result_ext::is_hit(r)) {
                    // `PlaySamples` runs at the slider's own judgement time
                    // (`HitStateUpdateTime`), applied once the tail has been
                    // judged and the end time has passed.
                    let t = obj.body_judged.map(|(t, _)| t).or(tail.map(|(t, _)| t));
                    if let Some(t) = t {
                        if t >= t0 && t <= t_map_end {
                            for s in node_samples(obj.span_count) {
                                out.push(Placement { time: t, sample: s, x: obj.end_position[0], until: None });
                            }
                        }
                    }
                }

                // Sliding loops while tracked (snapshots carry the engine's
                // tracking state at 60fps game frames, map timeline).
                // Shared sample construction + run scan with the live
                // loop-event collector (see collect_loop_events).
                let dbg = std::env::var("HITSOUND_DEBUG").is_ok();
                let runs = tracked_runs(game, obj);
                for sample in slide_loop_samples(&obj_samples, obj_normal, raw.sound_type) {
                    let len_ms = resolver.clip(&sample).map(|w| w.duration_ms()).unwrap_or(0.0);
                    if len_ms <= 0.0 {
                        continue;
                    }
                    for &(a, b) in &runs {
                        tile_loop(&mut out, obj, a, b, &sample, len_ms, game.rate, t0, t_map_end);
                    }
                }
                if dbg {
                    eprintln!(
                        "hitsound debug: slider #{} [{:.0}..{:.0}] head={:?} body={:?} nested={} runs={:?} whistle={}",
                        obj.index,
                        obj.start_time,
                        obj.end_time,
                        obj.head_judged.map(|(t, r)| (t as i64, format!("{:?}", r))),
                        obj.body_judged.map(|(t, r)| (t as i64, format!("{:?}", r))),
                        obj.nested.iter().filter(|n| n.judged.is_some()).count(),
                        runs,
                        raw.sound_type & 0b10 != 0,
                    );
                }
            }
        }
    }

    // Combo breaks (`ComboEffects`): when the score processor's combo
    // drops to zero the combobreak sample plays if the old combo was > 20,
    // or on the very first break (`AlwaysPlayFirstComboBreak`, default
    // on). Full volume, centered (a plain `SampleInfo`, no balance).
    // Plain SampleInfos carry no custom index and are not subject to the
    // UseBeatmapSamples gate — a beatmap-local combobreak.wav serves them.
    {
        let mut first_break = false;
        let mut prev_combo = 0;
        for e in &game.score_events {
            if e.combo == 0 && prev_combo != 0 && (prev_combo > 20 || !first_break) {
                first_break = true;
                if e.time >= t0 && e.time <= t_map_end {
                    out.push(Placement {
                        time: e.time,
                        sample: HitSample {
                            name: "combobreak",
                            bank: Bank::Normal,
                            custom: 0,
                            volume: 100,
                            filename: None,
                            layered: false,
                        },
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

/// Tiles a loop sample across the tracked interval [a, b] (map ms) so it
/// loops seamlessly at its natural rate on the wall timeline: each tile
/// rings for `len_ms` wall ms, i.e. `len_ms * rate` map ms apart.
/// Balance follows the ball position per tile.
fn tile_loop(
    out: &mut Vec<Placement>,
    obj: &crate::game::ObjView,
    a: f64,
    b: f64,
    sample: &HitSample,
    len_ms: f64,
    rate: f64,
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
            out.push(Placement { time: t, sample: sample.clone(), x, until: Some(b.min(t_map_end)) });
        }
        t += len_ms * rate;
    }
}

/// Object-level samples of a slider plus the hitnormal fallback that
/// seeds the sliding loops.
fn slider_object_samples(raw: &RawObj, obj: &crate::game::ObjView, data: &SampleData) -> (Vec<HitSample>, HitSample) {
    let head_point = point_at(&data.points, obj.start_time + CONTROL_POINT_LENIENCY + 1.0);
    let obj_samples = resolve_samples(raw.sound_type, &raw.bank, head_point);
    // `resolve_samples` always yields the hitnormal first; the fallback only
    // exists so the signature stays total.
    let obj_normal = obj_samples.iter().find(|s| s.name == "hitnormal").cloned().unwrap_or(HitSample {
        name: "hitnormal",
        bank: head_point.bank,
        custom: head_point.custom_index,
        volume: head_point.volume,
        filename: None,
        layered: is_layered(raw.sound_type),
    });
    (obj_samples, obj_normal)
}

/// Looping samples of a tracked slider: `sliderslide` always,
/// `sliderwhistle` when the object's whistle flag is set
/// (`CreateSlidingSamples`: the object's hitnormal/whistle sample with
/// the name swapped — custom index and filename carry over, and the whistle
/// is an addition, so it is never `IsLayered`).
fn slide_loop_samples(obj_samples: &[HitSample], obj_normal: HitSample, sound_type: u8) -> Vec<HitSample> {
    let mut out = vec![HitSample { name: "sliderslide", ..obj_normal }];
    if sound_type & 0b10 != 0 {
        out.push(HitSample {
            name: "sliderwhistle",
            bank: obj_samples.iter().find(|s| s.name == "hitwhistle").map(|s| s.bank).unwrap_or(obj_normal.bank),
            custom: obj_normal.custom,
            volume: obj_normal.volume,
            filename: None,
            layered: false,
        });
    }
    out
}

/// Tracking runs of a slider (map ms): consecutive tracked snapshots
/// extend a run to the next frame boundary, an untracked frame flushes.
fn tracked_runs(game: &GameData, obj: &crate::game::ObjView) -> Vec<(f64, f64)> {
    let mut runs: Vec<(f64, f64)> = Vec::new();
    let mut run: Option<(f64, f64)> = None;
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
                    runs.push((a, b));
                }
            }
        }
    }
    if let Some((a, b)) = run.take() {
        runs.push((a, b));
    }
    runs
}

// ---------------------------------------------------------------------------
// Live-playback loop events
// ---------------------------------------------------------------------------

/// A loop sound for live playback hosts (kira): `spinnerspin` /
/// `sliderslide` / `sliderwhistle`. The offline export synthesizes these
/// into the mix buffer; a live preview drives a looping handle per run
/// with the same lazer semantics.
#[derive(Clone, Debug)]
pub struct LoopSoundEvent {
    pub name: &'static str,
    pub bank: &'static str,
    /// Custom sample bank index merged per `ApplyTo` (object's own when
    /// > 0, else the timing point's): ≥1 resolves from the beatmap's own
    /// sample files, ≥2 adds the numeric lookup suffix.
    pub custom: i32,
    /// Explicit .osu hitSample filename on the reference sample
    /// (`FileHitSampleInfo.With` keeps the file when the name is swapped
    /// for the loop's) — set = the loop is that beatmap-local file.
    pub filename: Option<String>,
    /// Beatmap sample volume 0-100 (receiver applies `max(5)` and the
    /// Effect channel volume).
    pub volume: i32,
    /// `LegacyHitSampleInfo.IsLayered` of the source sample. A legacy skin
    /// with `LayeredHitSounds: 0` mutes layered samples, but only in the
    /// user-skin layer — a beatmap-local file still wins. Hosts that resolve
    /// samples themselves should pass this to
    /// [`resolve_sample_parts`] (and key their sample cache by it).
    pub layered: bool,
    /// Audible intervals (map ms, ascending); slider runs are exactly the
    /// tracking intervals, spinner runs carry the 300ms/240ms fade.
    pub runs: Vec<(f64, f64)>,
    pub control: LoopControl,
}

#[derive(Clone, Debug)]
pub enum LoopControl {
    /// Pan follows the slider ball (index into `GameData::objects`).
    Slider { object_index: usize },
    /// Playback rate follows spin progress (snapshot time → cumulative
    /// rotation degrees, plus the filled-spinner rotation count).
    Spin { rotation: Vec<(f64, f32)>, spins_required: f64 },
}

impl LoopSoundEvent {
    /// The sound-table key identifying this loop's sample.
    pub fn slot(&self) -> SampleSlot {
        match &self.filename {
            Some(filename) => SampleSlot::File { filename: filename.clone() },
            None => SampleSlot::Bank { bank: self.bank, name: self.name, custom: self.custom },
        }
    }

    /// Run audible at map time `t`, if any (spinner runs include their
    /// fade tail).
    pub fn run_at(&self, t: f64) -> Option<usize> {
        let i = self.runs.partition_point(|r| r.0 <= t).checked_sub(1)?;
        let (a, b) = self.runs[i];
        let end = match self.control {
            LoopControl::Spin { .. } => b + SPIN_FADE_OUT,
            LoopControl::Slider { .. } => b,
        };
        (t >= a && t <= end).then_some(i)
    }

    /// (playback rate, 0..1 amplitude (line volume × envelope), playfield
    /// X) at map time `t` inside run `ri`. Mirrors `synth_spin_loop`.
    pub fn params_at(&self, ri: usize, t: f64, freq_modulate: bool, game: &GameData) -> (f64, f64, f32) {
        let vol = self.volume.max(MINIMUM_SAMPLE_VOLUME) as f64 / 100.0;
        match &self.control {
            LoopControl::Slider { object_index } => {
                let obj = &game.objects[*object_index];
                let progress = if obj.duration > 0.0 { (t - obj.start_time) / obj.duration } else { 0.0 };
                (1.0, vol, obj.slider_ball_at(progress.clamp(0.0, 1.0))[0])
            }
            LoopControl::Spin { rotation, spins_required } => {
                let (a, b) = self.runs[ri];
                let env = if t <= b {
                    ((t - a) / SPIN_FADE_IN).clamp(0.0, 1.0)
                } else {
                    1.0 - (t - b) / SPIN_FADE_OUT
                };
                let progress = if *spins_required > 0.0 {
                    rotation_at(rotation, t) / 360.0 / *spins_required
                } else {
                    1.0
                };
                let rate = if freq_modulate {
                    (20_000.0 / 44_100.0 + progress * 40_000.0 / 44_100.0).min(100_000.0 / 44_100.0)
                } else {
                    1.0
                };
                (rate, vol * env.clamp(0.0, 1.0), 256.0)
            }
        }
    }
}

/// All loop sounds of a replay (sorted by first audibility); the loop
/// counterpart of [`collect_events`]. `data` is the sample-side data
/// parsed with the beatmap (`game.sample_data`).
pub fn collect_loop_events(game: &GameData, data: &SampleData) -> Vec<LoopSoundEvent> {
    let mut out = Vec::new();
    if game.objects.len() != data.objects.len() {
        return out; // parser mismatch; silence rather than desynced sounds
    }
    for (obj, raw) in game.objects.iter().zip(&data.objects) {
        match obj.kind {
            ObjKind::Slider => {
                let runs = tracked_runs(game, obj);
                if runs.is_empty() {
                    continue;
                }
                let (obj_samples, obj_normal) = slider_object_samples(raw, obj, &data);
                for sample in slide_loop_samples(&obj_samples, obj_normal, raw.sound_type) {
                    out.push(LoopSoundEvent {
                        name: sample.name,
                        bank: sample.bank.as_str(),
                        custom: sample.custom,
                        filename: sample.filename,
                        volume: sample.volume,
                        layered: sample.layered,
                        runs: runs.clone(),
                        control: LoopControl::Slider { object_index: obj.index },
                    });
                }
            }
            ObjKind::Spinner => {
                if let Some(lp) = spin_loop_for(game, &data, obj, raw) {
                    out.push(LoopSoundEvent {
                        name: "spinnerspin",
                        bank: lp.sample.bank.as_str(),
                        custom: lp.sample.custom,
                        filename: lp.sample.filename.clone(),
                        volume: lp.sample.volume,
                        layered: lp.sample.layered,
                        runs: lp.runs,
                        control: LoopControl::Spin { rotation: lp.rotation, spins_required: lp.spins_required },
                    });
                }
            }
            ObjKind::Circle => {}
        }
    }
    out.sort_by(|a, b| {
        let k = |e: &LoopSoundEvent| e.runs.first().map(|r| r.0).unwrap_or(f64::INFINITY);
        k(a).partial_cmp(&k(b)).unwrap()
    });
    out
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
/// `Gameplay/Argon/combobreak`. The spinner trio resolves from the Argon
/// level the same way (`spinnerspin` is the looping spin sound, the
/// bonus entries are the per-revolution rewards).
const COMBOBREAK: &[u8] = include_bytes!("../assets/sounds/Argon/combobreak.wav");
const SPINNERSPIN: &[u8] = include_bytes!("../assets/sounds/Argon/spinnerspin.wav");
const SPINNERBONUS: &[u8] = include_bytes!("../assets/sounds/Argon/spinnerbonus.wav");
const SPINNERBONUS_MAX: &[u8] = include_bytes!("../assets/sounds/Argon/spinnerbonus-max.wav");

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
fn sample_clip(bank: &str, name: &str) -> Option<Clip> {
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
        (_, "spinnerspin") => SPINNERSPIN,
        (_, "spinnerbonus") => SPINNERBONUS,
        (_, "spinnerbonus-max") => SPINNERBONUS_MAX,
        _ => return None,
    };
    decode_wav(bytes)
}

// ---------------------------------------------------------------------------
// Sample resolution: beatmap files → user skin → embedded ArgonPro
// ---------------------------------------------------------------------------

/// Beatmap-local sample file access — the resource-store side of lazer's
/// `LegacyBeatmapSkin`. Given a lookup stem ("soft-hitnormal2"), the
/// implementation resolves the beatmap set's own file, trying the stable
/// extension order [`SAMPLE_EXTENSIONS`] (stem, `.wav`, `.mp3`, `.ogg`),
/// case-insensitively. A hit here is authoritative for the slot: 0-byte
/// files (deliberate silence) and undecodable files must NOT fall
/// through to the skin — mirrors the skin-layer policy downstream.
pub trait BeatmapSampleStore {
    fn sample_path(&self, stem: &str) -> Option<std::path::PathBuf>;
}

/// Stable's sample extension order (`Skin.RecycleSamples`: "osu-stable
/// performs audio lookups in order of wav -> mp3 -> ogg"; the bare stem
/// itself is tried first, as the framework's `GetFilenames` does).
pub const SAMPLE_EXTENSIONS: &[&str] = &["", "wav", "mp3", "ogg"];

/// `BeatmapSampleStore` over an unpacked beatmap set directory (stable
/// layout / .osz extraction / renderer `--map` folder). The directory is
/// scanned once into a lowercase filename → path map, so lookups are
/// case-insensitive like the game's stores.
pub struct DirectorySampleStore {
    by_name: HashMap<String, std::path::PathBuf>,
}

impl DirectorySampleStore {
    pub fn new(dir: &Path) -> DirectorySampleStore {
        let mut by_name = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                by_name.insert(name.to_lowercase(), path);
            }
        }
        DirectorySampleStore { by_name }
    }
}

impl BeatmapSampleStore for DirectorySampleStore {
    fn sample_path(&self, stem: &str) -> Option<std::path::PathBuf> {
        let stem = stem.replace('\\', "/").to_lowercase();
        let bare = stem.rsplit('/').next().unwrap_or(&stem).to_string();
        SAMPLE_EXTENSIONS
            .iter()
            .find_map(|ext| {
                let probe = if ext.is_empty() { bare.clone() } else { format!("{bare}.{ext}") };
                self.by_name.get(&probe).cloned()
            })
            .or_else(|| {
                // 声明名带子目录前缀而谱面集为平铺文件时的回退(同
                // VirtualFiles.resolve 的策略)。
                self.by_name.get(&bare).cloned()
            })
    }
}

/// 谱面层候选名 —— `HitSampleInfo.LookupNames` 在 `UseCustomSampleBanks`
/// (谱面层)后缀过滤后的形态:
/// - filename 槽(`FileHitSampleInfo.LookupNames`):[文件名, 去扩展名,
///   normal-hitnormal, hitnormal](bank 钉在 normal,`ApplyTo` 不覆盖);
/// - custom ≥ 2:先 `{bank}-{name}{N}`(后缀强制,不回退同 bank 的无后缀名),
///   再 `hitSample.Name`(lazer `getLegacyLookupNames` 末尾的通用名);
/// - custom = 1(含 filename 槽强制的 1):无后缀的 `{bank}-{name}` → `{name}`;
/// - custom ≤ 0:`UseBeatmapSamples` 为假,谱面层不参与(空表)。
///   combobreak 是无 bank 的 `SampleInfo`,不经过该门控,恒可查。
pub fn beatmap_lookup_names(bank: &str, name: &str, custom: i32, filename: Option<&str>) -> Vec<String> {
    if let Some(file) = filename.filter(|f| !f.is_empty()) {
        let mut out = vec![file.to_string()];
        // `Path.ChangeExtension(Filename, null)`:去掉最后一个扩展名。
        if let Some((stem, _)) = file.rsplit_once('.') {
            if !stem.is_empty() {
                out.push(stem.to_string());
            }
        }
        out.push("normal-hitnormal".to_string());
        out.push("hitnormal".to_string());
        return out;
    }
    if name == "combobreak" {
        return vec!["combobreak".to_string()];
    }
    if custom >= 2 {
        // 后缀过滤丢掉 `{bank}-{name}` / `{name}` 里不以组号结尾的项,
        // 然后无条件再查一次不带组号的音效名。
        vec![format!("{bank}-{name}{custom}"), name.to_string()]
    } else if custom == 1 {
        vec![format!("{bank}-{name}"), name.to_string()]
    } else {
        Vec::new()
    }
}

/// 皮肤层候选名(`getLegacyLookupNames` 剥后缀 + 通用名收尾)。File 槽
/// 无后缀不过滤,候选与 LookupNames 全集一致;combobreak 无 bank。
fn skin_lookup_names(bank: &str, name: &str, filename: Option<&str>) -> Vec<String> {
    if let Some(file) = filename.filter(|f| !f.is_empty()) {
        let mut out = vec![file.to_string()];
        if let Some((stem, _)) = file.rsplit_once('.') {
            if !stem.is_empty() {
                out.push(stem.to_string());
            }
        }
        out.push("normal-hitnormal".to_string());
        out.push("hitnormal".to_string());
        return out;
    }
    if name == "combobreak" {
        return vec!["Gameplay/combobreak".to_string()];
    }
    let mut names = vec![format!("Gameplay/{bank}-{name}"), format!("Gameplay/{name}")];
    // `SpinnerBonusMaxSampleInfo` strips "-max" back to the plain entry.
    if let Some(stripped) = name.strip_suffix("-max") {
        names.push(format!("Gameplay/{bank}-{stripped}"));
        names.push(format!("Gameplay/{stripped}"));
    }
    names
}

/// Does this skin turn `LayeredHitSounds` off?
///
/// `LegacySkinTransformer.GetSample` replaces a layered sample with
/// `SampleVirtual` only when the *legacy* skin resolves the setting to `false`
/// (absent means on). Hosts that decode samples themselves use this to apply
/// the same gate before [`resolve_sample_parts`].
pub fn mutes_layered_hit_sounds(skin: &dyn crate::skin::Skin) -> bool {
    skin.is_legacy()
        && skin
            .get_config(crate::skin::SkinLookup::LegacySetting(
                crate::skin::configuration::LegacySetting::LayeredHitSounds,
            ))
            .and_then(|v| v.as_bool())
            == Some(false)
}

/// Per-render hitsound resolver — lazer's gameplay sample chain:
/// beatmap-local files first (slots opted in via custom index ≥ 1 or an
/// explicit filename, `LegacyBeatmapSkin.GetSample`'s `UseBeatmapSamples`
/// gate), then the USER skin through lazer's `LegacySkin.GetSample` name
/// chain, then the embedded ArgonPro set filling every open slot —
/// elements MIX between layers, exactly one provider per lookup. A hit
/// layer is authoritative: a file that exists but fails to decode (or a
/// 0-byte placeholder, the map author's way of disabling a sound) is
/// silence, never a fall-through.
pub struct SampleResolver<'a> {
    skin: Option<&'a dyn crate::skin::Skin>,
    beatmap: Option<&'a dyn BeatmapSampleStore>,
    /// `OsuSkinConfiguration.SpinnerFrequencyModulate` (default true): the
    /// spinning loop's playback rate rises with spin progress.
    frequency_modulate: bool,
    /// `LegacySkinTransformer.GetSample`: with `LayeredHitSounds: 0` on a legacy
    /// skin, a layered hit sample is replaced by `SampleVirtual` — i.e. silence
    /// that terminates the chain (it does not fall through to ArgonPro).
    mute_layered: bool,
    /// Keyed by slot AND the layered flag: the same slot can be produced by a
    /// plain (`None`/Normal) set and by a layered one, and `LayeredHitSounds: 0`
    /// silences only the latter.
    cache: HashMap<(Bank, &'static str, i32, Option<String>, bool), Option<Arc<Clip>>>,
    /// Decoded clips deduplicated by the resource they came from, so two slots
    /// resolving to the same file share one `Arc` (and therefore one
    /// playback-concurrency pool).
    by_source: HashMap<String, Arc<Clip>>,
}

impl<'a> SampleResolver<'a> {
    pub fn new(skin: &'a dyn crate::skin::Skin) -> Self {
        Self::with_beatmap(skin, None)
    }

    /// The full chain: skin + beatmap-local sample files.
    pub fn with_beatmap(skin: &'a dyn crate::skin::Skin, beatmap: Option<&'a dyn BeatmapSampleStore>) -> Self {
        SampleResolver {
            skin: skin.is_legacy().then_some(skin),
            beatmap,
            frequency_modulate: skin
                .get_config(crate::skin::SkinLookup::Generic("SpinnerFrequencyModulate".into()))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            mute_layered: mutes_layered_hit_sounds(skin),
            cache: HashMap::new(),
            by_source: HashMap::new(),
        }
    }

    /// The embedded ArgonPro set alone (no user skin in scope).
    fn builtin() -> SampleResolver<'static> {
        SampleResolver {
            skin: None,
            beatmap: None,
            frequency_modulate: true,
            mute_layered: false,
            cache: HashMap::new(),
            by_source: HashMap::new(),
        }
    }

    /// Resolve one sample: beatmap file → user skin file → embedded
    /// ArgonPro entry → silent. Results (including negatives) are cached
    /// per (bank, name, custom, filename, layered).
    fn clip(&mut self, sample: &HitSample) -> Option<Arc<Clip>> {
        let key = (sample.bank, sample.name, sample.custom, sample.filename.clone(), sample.layered);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let clip = self.resolve(sample);
        // A muted layered sample is deliberate silence (`LayeredHitSounds: 0`),
        // not a missing slot — and only silence when no beatmap file claimed it.
        if clip.is_none() && !(sample.layered && self.mute_layered) {
            eprintln!(
                "hitsound warning: no sample for {}-{}{} in the beatmap, the skin or the default set, its hitsounds are silent",
                sample.bank.as_str(),
                sample.name,
                sample.custom.max(0)
            );
        }
        self.cache.insert(key, clip.clone());
        clip
    }

    /// Intern a decoded clip by its source, so identical files share one `Arc`.
    fn intern(&mut self, source: String, clip: Clip) -> Arc<Clip> {
        if let Some(existing) = self.by_source.get(&source) {
            return existing.clone();
        }
        let arc = Arc::new(clip);
        self.by_source.insert(source, arc.clone());
        arc
    }

    /// The lookup chain itself. The `LayeredHitSounds: 0` gate sits between
    /// the beatmap layer and the user skin: in lazer the beatmap skin is a
    /// source *above* the user skin (`BeatmapSkinProvidingContainer` wraps the
    /// ruleset chain), its transformer reads the *beatmap's* configuration
    /// (which this model does not load), and its `GetSample` returns the file
    /// before the user skin is ever asked — so a map-provided layered sample
    /// plays even when the user skin mutes layered sounds.
    fn resolve(&mut self, sample: &HitSample) -> Option<Arc<Clip>> {
        let bank = sample.bank.as_str();
        let filename = sample.filename.as_deref();
        let beatmap = self.beatmap;
        let skin = self.skin;
        let mut clip = None;
        // 1. Beatmap-local layer (`LegacyBeatmapSkin`): only slots opted
        //    in (custom ≥ 1 / filename / plain SampleInfo); a found file
        //    ends the whole chain, decode failure included.
        if let Some(store) = beatmap {
            for cand in beatmap_lookup_names(bank, sample.name, sample.custom, filename) {
                if let Some(path) = store.sample_path(&cand) {
                    clip = match decode_sample_file(&path) {
                        Ok(c) => Some(self.intern(format!("beatmap:{}", path.display()), c)),
                        Err(e) => {
                            eprintln!(
                                "hitsound: beatmap sample {} not decodable ({}), treated as disabled by the map",
                                path.display(),
                                e
                            );
                            None
                        }
                    };
                    break;
                }
            }
        }
        // 2. `LayeredHitSounds: 0` on the user's legacy skin replaces a layered
        //    sample with `SampleVirtual` — deliberate silence that ends the
        //    chain (no fall-through to the built-in set). The beatmap layer
        //    above already had its chance.
        if clip.is_none() && sample.layered && self.mute_layered {
            return None;
        }
        // 3. User skin (suffix stripped, File slots look up the raw name).
        if clip.is_none() {
            let mut names = skin_lookup_names(bank, sample.name, filename);
            if filename.is_none() && let Some(stripped) = sample.name.strip_suffix("-max") {
                names.extend(skin_lookup_names(bank, stripped, None));
            }
            let resolved = skin.and_then(|skin| names.iter().find_map(|n| skin.get_sample(n)));
            clip = match resolved {
                // 皮肤提供了该槽位 = 权威:解码失败(含 0 字节禁用占位)即静音,
                // 不回退内置默认
                Some(path) => match decode_sample_file(&path) {
                    Ok(c) => Some(self.intern(format!("skin:{}", path.display()), c)),
                    Err(e) => {
                        eprintln!(
                            "hitsound: skin sample {} not decodable ({}), treated as disabled by the skin",
                            path.display(),
                            e
                        );
                        None
                    }
                },
                // 皮肤未提供该槽位:内置默认。File 槽按 normal-hitnormal。
                // `spinnerbonus-max` 不剥回 `spinnerbonus`:Argon 的 MAX
                // 音效和普通 bonus 是两条不同的采样,剥回去会让转满之后
                // 继续响普通 bonus。皮肤查找链自己已经带了剥后缀的回退。
                None => {
                    let name = if filename.is_some() { "hitnormal" } else { sample.name };
                    let bank = if filename.is_some() { "normal" } else { bank };
                    match sample_clip(bank, name) {
                        Some(c) => Some(self.intern(format!("builtin:{bank}-{name}"), c)),
                        None => None,
                    }
                }
            };
        }
        clip
    }

    /// Distinct slots resolved so far (debug output).
    fn distinct(&self) -> usize {
        self.cache.len()
    }
}

/// Bytes-level resolution for live-playback hosts that own their audio
/// engine (decode the returned WAV themselves): the lazer chain with the
/// beatmap-local layer enabled — beatmap set files (slots opted in via
/// custom index ≥ 1 or an explicit filename) first, then the user skin
/// under the same mix policy (skin WAVs pass through untouched, mp3/ogg
/// decode in-process via symphonia), else the embedded ArgonPro entry.
/// A hit layer is authoritative: 0-byte placeholders and read/decode
/// failures mean silence, never a fall-through to the next layer.
///
/// `layered` is `HitsoundEvent::layered` / `LoopSoundEvent::layered`: a
/// legacy skin with `LayeredHitSounds: 0` silences those samples unless a
/// beatmap file claims the slot first. `SampleSlot::File` is never layered
/// (`FileHitSampleInfo` leaves `IsLayered` at `false`).
pub fn resolve_sample_bytes(
    slot: &SampleSlot,
    layered: bool,
    beatmap: Option<&dyn BeatmapSampleStore>,
    skin: &dyn crate::skin::Skin,
) -> Option<Vec<u8>> {
    match slot {
        SampleSlot::File { filename } => {
            resolve_sample_parts("normal", "hitnormal", 1, Some(filename), false, beatmap, skin)
        }
        SampleSlot::Bank { bank, name, custom } => {
            resolve_sample_parts(bank, name, *custom, None, layered, beatmap, skin)
        }
    }
}

/// [`resolve_sample_bytes`] 的参数形态(事件字段直接传入,免构造)。
pub fn resolve_sample_parts(
    bank: &str,
    name: &str,
    custom: i32,
    filename: Option<&str>,
    layered: bool,
    beatmap: Option<&dyn BeatmapSampleStore>,
    skin: &dyn crate::skin::Skin,
) -> Option<Vec<u8>> {
    // 1. Beatmap-local files (`LegacyBeatmapSkin` layer). The beatmap skin is
    //    a source above the user skin and never consults its
    //    `LayeredHitSounds`, so it wins before the gate below.
    if let Some(store) = beatmap {
        for cand in beatmap_lookup_names(bank, name, custom, filename) {
            if let Some(path) = store.sample_path(&cand) {
                return sample_file_bytes(&path);
            }
        }
    }
    // 2. `LayeredHitSounds: 0` on the user's legacy skin: `SampleVirtual`
    //    (`LegacySkinTransformer.GetSample`), i.e. silence that ends the
    //    chain — no fall-through to the built-in set.
    if layered && mutes_layered_hit_sounds(skin) {
        return None;
    }
    // 3. User skin (suffix stripped).
    if skin.is_legacy() {
        let mut names = skin_lookup_names(bank, name, filename);
        if filename.is_none() && let Some(stripped) = name.strip_suffix("-max") {
            names.extend(skin_lookup_names(bank, stripped, None));
        }
        if let Some(path) = names.iter().find_map(|n| skin.get_sample(n)) {
            // 皮肤提供了该采样槽位 = 权威。0 字节占位是皮肤作者**有意禁用**
            // 该音效的惯用手法:读出/解码失败直接静音,绝不回退内置默认
            //(调用方对空字节/None 都按缺失处理,天然静音)。
            return sample_file_bytes(&path);
        }
    }
    // 4. 皮肤未提供该槽位(或非 legacy 皮肤):回退内置默认(File 槽按
    // normal-hitnormal)。`spinnerbonus-max` 保留原名,MAX 音效和普通
    // bonus 是两条采样。
    let name = if filename.is_some() { "hitnormal" } else { name };
    let bank = if filename.is_some() { "normal" } else { bank };
    sample_bytes(bank, name).map(|b| b.to_vec())
}

/// 采样文件 → WAV 字节:wav 原样透传(0 字节占位 = Some(empty) → 静音),
/// mp3/ogg/flac 进程内 symphonia 解码(失败 None = 静音)。
fn sample_file_bytes(path: &Path) -> Option<Vec<u8>> {
    let is_wav = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    if is_wav {
        std::fs::read(path).ok()
    } else {
        decode_wav_44k(path)
    }
}

/// Bytes-level resolution without the beatmap-local layer (user skin →
/// embedded ArgonPro) — the pre-beatmap-samples behavior, kept for hosts
/// that resolve before a beatmap store exists. `layered` has the same
/// meaning as in [`resolve_sample_parts`].
pub fn resolve_sample_wav(
    bank: &str,
    name: &str,
    layered: bool,
    skin: &dyn crate::skin::Skin,
) -> Option<Vec<u8>> {
    resolve_sample_parts(bank, name, 0, None, layered, None, skin)
}

/// Decode a skin sample file: WAV in-process ([`decode_wav`]); anything
/// else (mp3/ogg/flac/…) in-process via symphonia — no external binary.
fn decode_sample_file(path: &Path) -> Result<Clip, String> {
    let is_wav = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    if is_wav {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        return decode_wav(&bytes).ok_or_else(|| "not a decodable RIFF/PCM wav".to_string());
    }
    decode_pcm_stereo44k(path).ok_or_else(|| "symphonia decode failed".to_string())
}

/// 进程内解码任意支持的压缩音频(mp3/ogg/vorbis/flac/…)→ 交错立体声
/// f32 @44.1k(`Clip` 口径,替代原 `ffmpeg -f s16le -ar 44100 -ac 2`
/// 子进程)。重采样为线性 —— 打击音效是短样本,质量足够;失败返回
/// None(调用方按缺失静音,不回退)。
fn decode_pcm_stereo44k(path: &Path) -> Option<Clip> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?
        .format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;
    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(SAMPLE_RATE);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(2)
        .max(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .ok()?;

    let mut raw: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break, // EOF / 不可恢复的容器错误
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // 坏包跳过继续(截断文件尾巴常见),其余错误终止
            Err(SymError::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        raw.extend_from_slice(buf.samples());
    }
    if raw.is_empty() {
        return None;
    }
    let data = to_stereo_44k(&raw, channels, src_rate);
    Some(Clip { data })
}

/// 任意声道布局 → 立体声,再线性重采样到 44.1k。
fn to_stereo_44k(interleaved: &[f32], channels: usize, src_rate: u32) -> Vec<f32> {
    let stereo: Vec<f32> = match channels {
        1 => interleaved.iter().flat_map(|s| [*s, *s]).collect(),
        2 => interleaved.to_vec(),
        // >2 声道取前两声道(打击音效不会有环绕混音语义)
        _ => interleaved
            .chunks_exact(channels)
            .flat_map(|c| [c[0], c[1]])
            .collect(),
    };
    if src_rate == SAMPLE_RATE || stereo.len() < 2 {
        return stereo;
    }
    let in_frames = stereo.len() / 2;
    let out_frames = ((in_frames as f64) * (SAMPLE_RATE as f64 / src_rate as f64)).floor() as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 * (src_rate as f64 / SAMPLE_RATE as f64);
        let i0 = (pos.floor() as usize).min(in_frames - 1);
        let i1 = (i0 + 1).min(in_frames - 1);
        let t = pos - i0 as f64;
        for ch in 0..2 {
            let a = stereo[i0 * 2 + ch] as f64;
            let b = stereo[i1 * 2 + ch] as f64;
            out.push((a + (b - a) * t) as f32);
        }
    }
    out
}

/// 进程内解码任意支持的音频 → 44.1k 立体声 wav(PCM16)字节。直播放
/// 宿主(kira `StaticSoundData::from_cursor`)直接吃;失败返回 None。
pub fn decode_wav_44k(path: &Path) -> Option<Vec<u8>> {
    decode_pcm_stereo44k(path).map(|clip| encode_wav(&clip.data, false))
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

/// Minimal RIFF PCM decoder (16/24-bit, plain or WAVE_FORMAT_EXTENSIBLE)
/// with linear resampling to the output rate; mono sources are duplicated
/// to both channels.
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
                let mut audio_format = rd(body);
                let channels = rd(body + 2);
                let rate = u32::from_le_bytes([bytes[body + 4], bytes[body + 5], bytes[body + 6], bytes[body + 7]]);
                let bits = rd(body + 14);
                // WAVE_FORMAT_EXTENSIBLE: the real tag is the SubFormat
                // GUID's Data1 (1 = PCM) at offset 24. The shipped
                // soft-hitnormal is such a file.
                if audio_format == 0xFFFE && size >= 40 {
                    let sub = u32::from_le_bytes([
                        bytes[body + 24], bytes[body + 25], bytes[body + 26], bytes[body + 27],
                    ]);
                    audio_format = sub as u16;
                }
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
    if format != 1 || (bits != 16 && bits != 24) || channels == 0 || channels > 2 || rate == 0 {
        return None;
    }
    let width = bits as usize / 8;
    let frames = size / width / channels;
    let src = |f: usize, c: usize| -> f32 {
        let off = body + (f * channels + c) * width;
        if bits == 16 {
            rd(off) as i16 as f32 / 32768.0
        } else {
            // 3 bytes LE, sign-extended to i32.
            let v = ((rd(off) as u32) | ((rd(off + 1) as u32) << 8) | ((*bytes.get(off + 2).unwrap_or(&0) as u32) << 16)) << 8;
            (v as i32 >> 8) as f32 / 8388608.0
        }
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

/// Mixes one placement into `buf` (interleaved stereo f32) at the clip's
/// natural rate — samples never pitch-shift with rate mods, DT/HT only
/// move the trigger times. `until_sec` truncates loop tiles where lazer
/// would stop the looping sample.
fn place(buf: &mut [f32], clip: &Clip, start_sec: f64, gl: f32, gr: f32, until_sec: Option<f64>) {
    let frames = clip.data.len() / 2;
    if frames == 0 {
        return;
    }
    let dst_start = (start_sec * SAMPLE_RATE as f64).round() as isize;
    let mut dst_len = frames;
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
        buf[dst * 2] += clip.data[n * 2] * gl;
        buf[dst * 2 + 1] += clip.data[n * 2 + 1] * gr;
    }
}

/// A one-shot hitsound event on the map timeline, for live preview
/// playback (fire when the playhead crosses `time`). Loop sounds
/// (sliderslide/sliderwhistle/spinnerspin) are separate: see
/// [`collect_loop_events`].
#[derive(Clone, Debug)]
pub struct HitsoundEvent {
    /// Map time in ms (judgement time).
    pub time: f64,
    /// Sample name: "hitnormal"/"hitwhistle"/"hitfinish"/"hitclap"/
    /// "slidertick"/"spinnerbonus"/"spinnerbonus-max"/"combobreak".
    pub name: &'static str,
    /// Sample bank: "normal"/"soft"/"drum".
    pub bank: &'static str,
    /// Custom sample bank index (`ApplyTo` merge): ≥1 = resolve from the
    /// beatmap's own sample files, ≥2 = `{bank}-{name}{N}` suffix lookup.
    pub custom: i32,
    /// Explicit .osu hitSample filename override (`FileHitSampleInfo`):
    /// when set, the sample is this beatmap-local file and the banked
    /// chain does not apply.
    pub filename: Option<String>,
    /// Beatmap sample volume 0-100 (apply `max(5)` and the Effect
    /// channel volume on the receiver's side).
    pub volume: i32,
    /// `LegacyHitSampleInfo.IsLayered` of the sample. A legacy skin with
    /// `LayeredHitSounds: 0` mutes layered samples in the user-skin layer
    /// only — a beatmap-local file still wins. Hosts that resolve samples
    /// themselves pass this to [`resolve_sample_parts`] and key their sample
    /// cache by it (lazer includes `IsLayered` in `LegacyHitSampleInfo`
    /// equality for the same pooling reason).
    pub layered: bool,
    /// Playfield X for stereo balance.
    ///
    /// Nominally 0..512, but **not guaranteed to be in range**: a degenerate
    /// slider (`pixelLength = 2147483647`, aspire "infinite length") resolves
    /// its repeat positions through a path scaled to ~1.5e9 units, and a host
    /// that maps this straight to a channel gain would get a negative/oversized
    /// gain instead of a hard pan (see `balance`, which clamps like BASS's
    /// `BASS_ATTRIB_PAN`). Hosts must clamp too — Aria's `osu_panning` does.
    pub pan_x: f32,
}

/// Sample-slot identity for downstream sound tables: everything needed
/// to run the lazer lookup chain (`HitSampleInfo.LookupNames`) minus the
/// per-hit volume/pan and the `layered` flag. The flag only changes the
/// result when a legacy skin turns `LayeredHitSounds` off, so hosts that
/// cache resolved samples should key by the slot **and** the event's
/// `layered` bit (see [`resolve_sample_parts`]).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SampleSlot {
    /// A banked gameplay sample: bank × name × custom sample bank index
    /// (0/1 = unsuffixed lookup, ≥2 = `{bank}-{name}{N}`).
    Bank { bank: &'static str, name: &'static str, custom: i32 },
    /// An explicit .osu hitSample filename (`FileHitSampleInfo`).
    File { filename: String },
}

impl HitsoundEvent {
    /// The sound-table key identifying this event's sample.
    pub fn slot(&self) -> SampleSlot {
        match &self.filename {
            Some(filename) => SampleSlot::File { filename: filename.clone() },
            None => SampleSlot::Bank { bank: self.bank, name: self.name, custom: self.custom },
        }
    }
}

// ---------------------------------------------------------------------------
// Spinner spinning loop (pitch-modulated, phase-accumulator synth)
// ---------------------------------------------------------------------------

/// `DrawableSpinner`'s loop fires on `IsSpinning` — the rotation outpaces
/// the damped display by >10°, which in steady state means a sustained
/// rate above ~100°/s (10° / -ln(0.99)) inside the spinnable window.
const SPIN_ACTIVE_RATE: f64 = 0.1005; // deg per ms
/// Damp stickiness: a pause shorter than this keeps the loop alive.
const SPIN_RUN_MERGE_GAP: f64 = 100.0;
/// `updateSpinningSample`: VolumeTo(1, 300) on start, VolumeTo(0, 240) on
/// stop.
const SPIN_FADE_IN: f64 = 300.0;
const SPIN_FADE_OUT: f64 = 240.0;

/// One spinner's looping `spinnerspin` descriptor. Not a [`Placement`]:
/// the loop plays at a continuously VARYING rate (frequency modulation),
/// which fixed-rate one-shots cannot express — `render_spin_loops`
/// synthesizes it directly with a phase accumulator.
struct SpinLoop {
    /// `referenceSample.With("spinnerspin")`: the spinner's normal sample's
    /// bank/volume with the name swapped.
    sample: HitSample,
    /// Audibly-spinning intervals (map ms).
    runs: Vec<(f64, f64)>,
    /// (snapshot time, score-side total rotation) across the spinner, for
    /// the modulation curve.
    rotation: Vec<(f64, f32)>,
    spins_required: f64,
}

/// The spinner's own sample list (`HitObject.Samples` AFTER `applySamples`).
struct SpinnerRefs {
    /// `Samples[0]` — the hitnormal (or the `FileHitSampleInfo` slot).
    normal: HitSample,
    /// First non-hitnormal entry, when the line carries Whistle/Finish/Clap.
    addition: Option<HitSample>,
}

impl SpinnerRefs {
    /// `CreateHitSampleInfo("spinnerbonus")`: the first addition if the set has
    /// one (additions are never `IsLayered`), else the normal.
    fn bonus(&self) -> HitSample {
        self.addition.clone().unwrap_or_else(|| self.normal.clone())
    }
}

/// Resolve a spinner's reference samples the way lazer does.
///
/// `CreateSpinningSamples()` / `CreateHitSampleInfo("spinnerbonus")` /
/// `SpinnerBonusMaxSampleInfo` all derive from `HitObject.Samples`, and
/// `LegacyBeatmapDecoder.applySamples` applies the sample point at
/// `GetEndTime() + LENIENCY` to non-repeats objects. `GetPlayableBeatmap` then
/// re-runs `ApplyDefaults`, so `CreateNestedHitObjects` (where the bonus ticks
/// take their snapshot) sees the applied list. The point at the spinner's
/// START is therefore the wrong one.
fn spinner_refs(raw: &RawObj, data: &SampleData) -> SpinnerRefs {
    let point = point_at(&data.points, raw.end_time + CONTROL_POINT_LENIENCY);
    let mut samples = resolve_samples(raw.sound_type, &raw.bank, point).into_iter();
    let normal = samples.next().unwrap_or_else(|| HitSample {
        name: "hitnormal",
        bank: point.bank,
        custom: point.custom_index,
        volume: point.volume,
        filename: None,
        layered: is_layered(raw.sound_type),
    });
    let addition = samples.find(|s| s.name != "hitnormal");
    SpinnerRefs { normal, addition }
}

fn collect_spin_loops(game: &GameData, data: &SampleData) -> Vec<SpinLoop> {
    let mut loops = Vec::new();
    if game.objects.len() != data.objects.len() {
        return loops;
    }
    for (obj, raw) in game.objects.iter().zip(&data.objects) {
        if obj.kind != ObjKind::Spinner {
            continue;
        }
        if let Some(lp) = spin_loop_for(game, data, obj, raw) {
            loops.push(lp);
        }
    }
    loops
}

/// One spinner's loop descriptor (rotation timeline + audibly-spinning
/// runs with the 100ms stickiness merge).
fn spin_loop_for(game: &GameData, data: &SampleData, obj: &crate::game::ObjView, raw: &RawObj) -> Option<SpinLoop> {
    let mut rotation = Vec::new();
    let mut runs: Vec<(f64, f64)> = Vec::new();
    let mut open: Option<f64> = None;
    let mut last_run_end = f64::NEG_INFINITY;
    let mut prev: Option<(f64, f32)> = None;
    for (si, snap) in game.snapshots.iter().enumerate() {
        let Some(sp) = snap.spinners.iter().find(|sp| sp.object_index == obj.index) else {
            continue;
        };
        rotation.push((snap.time, sp.total_rotation));
        let active = match prev {
            Some((pt, prot)) => {
                let dt = snap.time - pt;
                // Spinnable window only; movement above the threshold.
                snap.time >= obj.start_time
                    && snap.time < obj.end_time
                    && dt > 0.0
                    && (sp.total_rotation - prot).abs() as f64 / dt > SPIN_ACTIVE_RATE
            }
            None => false,
        };
        // The run extends at least to the NEXT snapshot (the active
        // judgement was made over the interval ending here).
        let cur_end = game.snapshots.get(si + 1).map(|s| s.time).unwrap_or(snap.time);
        match (&mut open, active) {
            (None, true) => {
                if snap.time - last_run_end > SPIN_RUN_MERGE_GAP {
                    open = Some(snap.time);
                } else if let Some(last) = runs.last_mut() {
                    // Re-activate within the stickiness gap: extend the
                    // previous run instead of starting a new fade-in.
                    last.1 = cur_end.max(last.1);
                    open = Some(last.0);
                } else {
                    open = Some(snap.time);
                }
            }
            (Some(_), true) => {
                if let Some(last) = runs.last_mut() {
                    last.1 = cur_end.max(last.1);
                }
            }
            (Some(start), false) => {
                runs.push((*start, snap.time));
                last_run_end = snap.time;
                open = None;
            }
            (None, false) => {}
        }
        prev = Some((snap.time, sp.total_rotation));
    }
    if let Some(start) = open {
        runs.push((start, obj.end_time));
    }
    if runs.is_empty() {
        return None;
    }
    Some(SpinLoop {
        // `referenceSample.With("spinnerspin")`: the spinner's own (applied)
        // normal sample with the name swapped; a filename reference keeps its
        // file. `applySamples` resolved that sample at `GetEndTime() + 5`, not
        // at the start time.
        sample: HitSample { name: "spinnerspin", ..spinner_refs(raw, data).normal },
        runs,
        rotation,
        spins_required: obj.spins_required,
    })
}

/// Linear-interpolated score-side rotation at a map time.
fn rotation_at(rot: &[(f64, f32)], t: f64) -> f64 {
    match rot.binary_search_by(|(time, _)| time.partial_cmp(&t).unwrap()) {
        Ok(i) => rot[i].1 as f64,
        Err(0) => rot.first().map(|r| r.1 as f64).unwrap_or(0.0),
        Err(i) => {
            let (t0, r0) = rot[i - 1];
            match rot.get(i) {
                Some(&(t1, r1)) => {
                    if t1 > t0 {
                        r0 as f64 + (r1 - r0) as f64 * (t - t0) / (t1 - t0)
                    } else {
                        r0 as f64
                    }
                }
                None => r0 as f64,
            }
        }
    }
}

/// Synthesize the spinning loops straight into the mix buffer (clip
/// resolution via the resolver, then the pure [`synth_spin_loop`]).
///
/// `mixer_gain` is the gameplay mixer layer (`OsuSetting.HitsoundVolume`):
/// `spinningSample` lives inside `DrawableSpinner`, i.e. under
/// `DrawableRuleset.Audio`, so the spin loop takes it exactly like the
/// one-shot voices do.
fn render_spin_loops(
    buf: &mut [f32],
    loops: &[SpinLoop],
    resolver: &mut SampleResolver,
    t0: f64,
    rate: f64,
    mixer_gain: f32,
) {
    for lp in loops {
        let Some(clip) = resolver.clip(&lp.sample) else { continue };
        synth_spin_loop(buf, &clip.data, lp, resolver.frequency_modulate, t0, rate, mixer_gain);
    }
}

/// The read phase advances by the modulation ratio each output frame and
/// wraps at the clip length (seamless: the sample is authored to loop),
/// gain = line volume × gameplay mixer × the tracking envelope.
/// `progressUnclamped` keeps rising past 1.0, capped at the
/// `100_000/44_100` frequency ceiling.
fn synth_spin_loop(
    buf: &mut [f32],
    data: &[f32],
    lp: &SpinLoop,
    freq_modulate: bool,
    t0: f64,
    rate: f64,
    mixer_gain: f32,
) {
    let frames = data.len() / 2;
    if frames == 0 {
        return;
    }
    let vol = lp.sample.volume.max(MINIMUM_SAMPLE_VOLUME) as f32 / 100.0;
    for &(a, b) in &lp.runs {
        let frame_at = |map_ms: f64| ((map_ms - t0) / rate / 1000.0 * SAMPLE_RATE as f64).round() as i64;
        let fa = frame_at(a);
        let fe = frame_at(b + SPIN_FADE_OUT);
        let mut phase = 0.0f64;
        let buf_frames = (buf.len() / 2) as i64;
        let mut i = fa.max(0);
        while i < fe.min(buf_frames) {
            let map_t = t0 + i as f64 / SAMPLE_RATE as f64 * rate * 1000.0;
            let env = if map_t <= b {
                ((map_t - a) / SPIN_FADE_IN).clamp(0.0, 1.0)
            } else {
                1.0 - (map_t - b) / SPIN_FADE_OUT
            };
            if env > 0.0 {
                let progress = if lp.spins_required > 0.0 {
                    rotation_at(&lp.rotation, map_t) / 360.0 / lp.spins_required
                } else {
                    1.0
                };
                let ratio = if freq_modulate {
                    (20_000.0 / 44_100.0 + progress * 40_000.0 / 44_100.0).min(100_000.0 / 44_100.0)
                } else {
                    1.0
                };
                phase += ratio;
                let p0 = (phase as usize) % frames;
                let p1 = (p0 + 1) % frames;
                let frac = (phase - phase.floor()) as f32;
                let l = data[p0 * 2] + (data[p1 * 2] - data[p0 * 2]) * frac;
                let r = data[p0 * 2 + 1] + (data[p1 * 2 + 1] - data[p0 * 2 + 1]) * frac;
                let g = vol * mixer_gain * env as f32;
                buf[(i as usize) * 2] += l * g;
                buf[(i as usize) * 2 + 1] += r * g;
            }
            i += 1;
        }
    }
}


/// All one-shot hitsound events of a replay (sorted by time, loop
/// sounds excluded — those are [`collect_loop_events`]). `data` is the
/// sample-side data parsed with the beatmap (`game.sample_data`).
/// Volume/bank resolution follows the same lazer semantics as the
/// offline track.
pub fn collect_events(game: &GameData, data: &SampleData) -> Vec<HitsoundEvent> {
    let mut events: Vec<HitsoundEvent> = build_placements(game, data, f64::NEG_INFINITY, f64::INFINITY, &mut SampleResolver::builtin())
        .into_iter()
        .filter(|p| p.until.is_none())
        .map(|p| HitsoundEvent {
            time: p.time,
            name: p.sample.name,
            bank: p.sample.bank.as_str(),
            custom: p.sample.custom,
            filename: p.sample.filename,
            volume: p.sample.volume,
            layered: p.sample.layered,
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
        (_, "spinnerspin") => SPINNERSPIN,
        (_, "spinnerbonus") => SPINNERBONUS,
        (_, "spinnerbonus-max") => SPINNERBONUS_MAX,
        _ => return None,
    };
    Some(bytes)
}

/// Renders the hitsound track for the exported range and encodes it as a
/// PCM16 stereo WAV. `t0` is the first output frame's map time,
/// `wall_secs` the output video's duration in seconds; the track spans
/// exactly that wall window so it muxes 1:1 with the video.
///
/// `bus_gain` scales the whole bus (`--hitsounds-volume`, the game's
/// `VolumeEffect`); `mixer_gain` is the gameplay mixer layer
/// (`OsuSetting.HitsoundVolume`, default 0.6) that `DrawableRuleset.Audio`
/// applies to the playfield's samples. `Gameplay/combobreak` bypasses that
/// mixer (`ComboEffects` sits beside it), so it takes `bus_gain` alone.
///
/// Loudness follows the game's defaults: samples play at their authored
/// level (beatmap volume × the sample's mastering, no bus normalization),
/// stacked hits are capped by the per-sample concurrency limit
/// (see [`limit_voices`]) and the encoder's soft limiter replaces the DAC
/// clipping the game would do.
pub fn render_track_wav(
    game: &GameData,
    data: &SampleData,
    t0: f64,
    wall_secs: f64,
    rate: f64,
    bus_gain: f32,
    mixer_gain: f32,
    skin: &dyn crate::skin::Skin,
) -> Vec<u8> {
    render_track(game, data, t0, wall_secs, rate, bus_gain, mixer_gain, true, skin, None)
}

/// [`render_track_wav`] with the beatmap-local sample layer enabled: the
/// set folder's own sample files take priority per slot over the skin
/// (lazer `LegacyBeatmapSkin`). Pass `None` to skip the layer.
pub fn render_track_wav_with_beatmap(
    game: &GameData,
    data: &SampleData,
    t0: f64,
    wall_secs: f64,
    rate: f64,
    bus_gain: f32,
    mixer_gain: f32,
    skin: &dyn crate::skin::Skin,
    beatmap: Option<&dyn BeatmapSampleStore>,
) -> Vec<u8> {
    render_track(game, data, t0, wall_secs, rate, bus_gain, mixer_gain, true, skin, beatmap)
}

/// `render_track_wav` without the bus soft limiter: the sum stays
/// linear and only true `>= 1.0` overs saturate at the PCM16
/// conversion. `scale` is PCM headroom the caller divides back out
/// before applying its own channel/master chain — for mixes that clip
/// exactly once at the end (live-render parity). A tanh knee on this
/// bus ducked dense stacks 2-4 dB and bent every stack peak; the
/// float-sum mix only needs the headroom so PCM16 can carry stacks
/// above unity.
pub fn render_track_wav_linear(
    game: &GameData,
    data: &SampleData,
    t0: f64,
    wall_secs: f64,
    rate: f64,
    scale: f32,
    mixer_gain: f32,
    skin: &dyn crate::skin::Skin,
) -> Vec<u8> {
    render_track(game, data, t0, wall_secs, rate, scale, mixer_gain, false, skin, None)
}

/// [`render_track_wav_linear`] with the beatmap-local sample layer
/// enabled (see [`render_track_wav_with_beatmap`]).
pub fn render_track_wav_linear_with_beatmap(
    game: &GameData,
    data: &SampleData,
    t0: f64,
    wall_secs: f64,
    rate: f64,
    scale: f32,
    mixer_gain: f32,
    skin: &dyn crate::skin::Skin,
    beatmap: Option<&dyn BeatmapSampleStore>,
) -> Vec<u8> {
    render_track(game, data, t0, wall_secs, rate, scale, mixer_gain, false, skin, beatmap)
}

/// One `PlaySamples` burst for storyboard `HitSound` triggers.
/// Looping slide samples are omitted: lazer fires the trigger from the
/// hit-object sample list, not from each `sliderslide` tile.
pub struct TriggerSampleGroup {
    pub time_ms: f64,
    pub samples: Vec<osu_parse::storyboard::timeline::PlayedHit>,
}

pub fn trigger_sample_groups(game: &GameData) -> Vec<TriggerSampleGroup> {
    let mut resolver = SampleResolver::builtin();
    let placements = build_placements(game, &game.sample_data, f64::NEG_INFINITY, f64::INFINITY, &mut resolver);
    let mut groups: Vec<TriggerSampleGroup> = Vec::new();
    for p in placements {
        if p.until.is_some() || !p.sample.name.starts_with("hit") {
            continue;
        }
        let suffix = (p.sample.custom >= 2).then(|| p.sample.custom.to_string());
        let hit = osu_parse::storyboard::timeline::PlayedHit {
            name: p.sample.name.to_string(),
            bank: p.sample.bank.as_str().to_string(),
            suffix,
        };
        let key = (p.time * 1000.0).round() as i64;
        if let Some(g) = groups.last_mut() {
            if (g.time_ms * 1000.0).round() as i64 == key {
                g.samples.push(hit);
                continue;
            }
        }
        groups.push(TriggerSampleGroup { time_ms: p.time, samples: vec![hit] });
    }
    groups
}

/// One scheduled sample playback, after clip resolution: everything
/// [`place`] needs plus the per-sample concurrency bookkeeping.
struct MixVoice {
    clip: Arc<Clip>,
    /// Start of the sample on the WALL timeline (seconds).
    start: f64,
    /// Hard cutoff (loop tiles, or a steal by a newer voice).
    until: Option<f64>,
    gl: f32,
    gr: f32,
    /// Identity of the resolved resource: the pool this voice competes in.
    key: usize,
    /// How many voices of this pool may sound at once
    /// (see [`HitSample::voice_cap`]).
    cap: usize,
}

impl MixVoice {
    /// The wall second this voice stops sounding: its natural length, unless
    /// truncated by `until`.
    fn ends_at(&self) -> f64 {
        let natural = self.start + self.clip.duration_ms() / 1000.0;
        match self.until {
            Some(u) => natural.min(u),
            None => natural,
        }
    }
}

/// Apply `ISample.PlaybackConcurrency` the way the game's BASS mixer does.
///
/// `OsuGameBase` sets the sample store's concurrency to [`SAMPLE_CONCURRENCY`]
/// (6) and `SampleBassFactory` loads every sample with
/// `BassFlags.SampleOverrideLongestPlaying`: while six instances of one sample
/// are sounding, the seventh stops the one that has been playing longest. A
/// `cap` of 1 (a single reused `SkinnableSound` instance) therefore models
/// `Stop(); Play();` — each retrigger cuts the previous playback.
///
/// Loop tiles are skipped: lazer holds ONE looping channel per slider/spinner
/// for its whole run, while this track expands loops into consecutive tiles, so
/// counting tiles as competing voices would make a single loop steal from
/// itself.
fn limit_voices(voices: &mut [MixVoice]) {
    // One-shots only, in start order: BASS evicts by "longest playing", which
    // among simultaneously-live instances is the earliest start.
    let mut order: Vec<usize> = (0..voices.len()).filter(|i| voices[*i].until.is_none()).collect();
    order.sort_by(|a, b| voices[*a].start.total_cmp(&voices[*b].start));

    let mut active: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in order {
        let start = voices[idx].start;
        let cap = voices[idx].cap;
        let list = active.entry(voices[idx].key).or_default();
        list.retain(|&i| voices[i].ends_at() > start);
        while list.len() >= cap {
            let oldest = list.remove(0);
            // The evicted instance is cut at the instant the new one starts.
            voices[oldest].until = Some(start);
        }
        list.push(idx);
    }
}

fn render_track(
    game: &GameData,
    data: &SampleData,
    t0: f64,
    wall_secs: f64,
    rate: f64,
    bus_gain: f32,
    mixer_gain: f32,
    limit: bool,
    skin: &dyn crate::skin::Skin,
    beatmap: Option<&dyn BeatmapSampleStore>,
) -> Vec<u8> {
    let t_map_end = t0 + wall_secs * rate * 1000.0;
    let mut resolver = SampleResolver::with_beatmap(skin, beatmap);
    let placements = build_placements(game, &data, t0, t_map_end, &mut resolver);
    if std::env::var("HITSOUND_DEBUG").is_ok() {
        eprintln!("hitsound debug: parsed {} objects (game {}), {} points, {} placements in [{},{}]",
            data.objects.len(), game.objects.len(), data.points.len(), placements.len(), t0, t_map_end);
    }

    let total = (wall_secs.max(0.0) * SAMPLE_RATE as f64).round() as usize;
    let mut buf = vec![0.0f32; total * 2];

    // Resolve every placement first: the concurrency cap needs each voice's
    // natural length before anything is mixed. Each distinct slot decodes once;
    // a sample that fails to decode warns once and drops only its own
    // placements (a silent `continue` used to hide a 24-bit asset mismatch).
    let mut voices: Vec<MixVoice> = Vec::with_capacity(placements.len());
    for p in &placements {
        let Some(clip) = resolver.clip(&p.sample) else {
            continue;
        };
        let volume = p.sample.volume.max(MINIMUM_SAMPLE_VOLUME) as f32 / 100.0;
        // `DrawableRuleset.Audio` volume, bypassed by `ComboEffects`.
        let mixer = if p.sample.bypasses_gameplay_mixer() { 1.0 } else { mixer_gain };
        let bal = balance(p.x) as f32;
        let gl = volume * mixer * (1.0 - bal.max(0.0));
        let gr = volume * mixer * (1.0 - (-bal).max(0.0));
        let start = (p.time - t0) / rate / 1000.0;
        let until = p.until.map(|u| (u - t0) / rate / 1000.0);
        voices.push(MixVoice {
            key: Arc::as_ptr(&clip) as usize,
            clip,
            start,
            until,
            gl,
            gr,
            cap: p.sample.voice_cap(),
        });
    }
    let loop_tiles = voices.iter().filter(|v| v.until.is_some()).count();
    limit_voices(&mut voices);
    if std::env::var("HITSOUND_DEBUG").is_ok() {
        let cut = voices.iter().filter(|v| v.until.is_some()).count();
        eprintln!(
            "hitsound debug: {} voices, {} loop tiles, {} cut by the concurrency limit",
            voices.len(),
            loop_tiles,
            cut.saturating_sub(loop_tiles),
        );
    }
    for v in &voices {
        place(&mut buf, &v.clip, v.start, v.gl, v.gr, v.until);
    }

    // The spinner spinning loops (pitch-modulated, phase-accumulator
    // synthesis — see `render_spin_loops`).
    let loops = collect_spin_loops(game, &data);
    if std::env::var("HITSOUND_DEBUG").is_ok() {
        for lp in &loops {
            eprintln!(
                "hitsound debug: spin loop bank={} vol={} spins_req={} runs={:?} rot[0..3]={:?} rot[-1]={:?}",
                lp.sample.bank.as_str(),
                lp.sample.volume,
                lp.spins_required,
                lp.runs,
                &lp.rotation[..lp.rotation.len().min(3)],
                lp.rotation.last()
            );
        }
    }
    render_spin_loops(&mut buf, &loops, &mut resolver, t0, rate, mixer_gain);

    // Bus gain (`--hitsounds-volume`, the game's `VolumeEffect`), or the
    // linear variant's PCM headroom. The gameplay mixer layer is already in
    // each voice (`mixer_gain`).
    if (bus_gain - 1.0).abs() > 1e-6 {
        for v in &mut buf {
            *v *= bus_gain;
        }
    }

    if std::env::var("HITSOUND_DEBUG").is_ok() {
        let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        eprintln!("hitsound debug: {} clips decoded, buffer peak {:.4}", resolver.distinct(), peak);
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

    encode_wav(&buf, limit)
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

fn encode_wav(interleaved: &[f32], limit: bool) -> Vec<u8> {
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
        // f32 -> i16 `as` casts saturate: the linear variant's only clip.
        let s = if limit { soft_limit(*v) } else { *v };
        out.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod zero_byte_skin_tests {
    use super::*;

    /// 皮肤用 0 字节占位禁用某音效:resolve 不得回退内置默认
    ///(0 字节 = Some(empty) 或 None,二者下游都静音)。
    struct StubSkin {
        conf: crate::skin::SkinConfiguration,
        sample: std::path::PathBuf,
    }
    impl crate::skin::Skin for StubSkin {
        fn name(&self) -> &str { "stub" }
        fn configuration(&self) -> &crate::skin::SkinConfiguration { &self.conf }
        fn is_legacy(&self) -> bool { true }
        fn get_texture(&self, _: &str) -> Option<crate::skin::SkinTexture> { None }
        fn get_config(&self, _: crate::skin::SkinLookup) -> Option<crate::skin::SkinValue> { None }
        fn get_sample(&self, _: &str) -> Option<std::path::PathBuf> { Some(self.sample.clone()) }
    }

    #[test]
    fn zero_byte_skin_sample_disables_instead_of_fallback() {
        let dir = std::env::temp_dir().join(format!("orr_zero_sample_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("normal-hitnormal.wav");
        std::fs::write(&wav, b"").unwrap(); // 0 字节占位
        let skin = StubSkin { conf: Default::default(), sample: wav };
        let resolved = resolve_sample_wav("normal", "hitnormal", false, &skin);
        match resolved {
            Some(bytes) => assert!(bytes.is_empty(), "0 字节占位应原样返回空字节,而不是内置默认"),
            None => {} // 非 wav/读取失败路径:同样表示静音
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 皮肤完全没有该槽位:仍回退内置默认(ArgonPro 内嵌采样非空)。
    struct MissingSkin { conf: crate::skin::SkinConfiguration }
    impl crate::skin::Skin for MissingSkin {
        fn name(&self) -> &str { "missing" }
        fn configuration(&self) -> &crate::skin::SkinConfiguration { &self.conf }
        fn is_legacy(&self) -> bool { true }
        fn get_texture(&self, _: &str) -> Option<crate::skin::SkinTexture> { None }
        fn get_config(&self, _: crate::skin::SkinLookup) -> Option<crate::skin::SkinValue> { None }
        fn get_sample(&self, _: &str) -> Option<std::path::PathBuf> { None }
    }

    #[test]
    fn missing_slot_still_falls_back_to_builtin() {
        let skin = MissingSkin { conf: Default::default() };
        let resolved = resolve_sample_wav("normal", "hitnormal", false, &skin);
        assert!(resolved.is_some_and(|b| !b.is_empty()), "未提供槽位应回退内置非空采样");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `soft-hitnormal` is the set's only 24-bit
    /// WAVE_FORMAT_EXTENSIBLE file; the decoder must carry it (it was
    /// silently dropped before, muting every soft-bank hit).
    #[test]
    fn decodes_24bit_extensible_wav() {
        let clip = decode_wav(wav!("soft-hitnormal")).expect("24-bit EXTENSIBLE wav decodes");
        assert!((930.0..950.0).contains(&clip.duration_ms()));
        let peak = clip.data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.5, "decoded content should be loud, peak={peak}");
    }

    #[test]
    fn decodes_16bit_wav() {
        let clip = decode_wav(wav!("normal-hitnormal")).expect("16-bit wav decodes");
        assert!(clip.duration_ms() > 500.0);
        assert!(!clip.data.is_empty());
    }

    /// The API-level regression: soft hitnormal resolves to a playable clip.
    #[test]
    fn soft_hitnormal_resolves_to_clip() {
        assert!(sample_clip("soft", "hitnormal").is_some());
    }

    /// Samples land sample-for-sample at their natural rate — rate mods
    /// move trigger times only, never the playback speed/pitch — and
    /// `until` truncates loop tiles at the wall-time cutoff.
    #[test]
    fn place_copies_at_natural_rate() {
        // 100-frame ramp, one channel duplicated to stereo.
        let clip = Clip { data: (0..100).flat_map(|i| [i as f32 / 100.0; 2]).collect() };
        let mut buf = vec![0.0f32; 30000 * 2];
        place(&mut buf, &clip, 0.5, 1.0, 1.0, None);
        for f in 0..100 {
            assert_eq!(buf[(22050 + f) * 2], clip.data[f * 2]);
        }
        assert_eq!(buf[(22050 + 100) * 2], 0.0, "must not ring past the natural length");

        let mut cut = vec![0.0f32; 30000 * 2];
        place(&mut cut, &clip, 0.5, 1.0, 1.0, Some(0.5 + 50.0 / SAMPLE_RATE as f64));
        assert_eq!(cut[(22050 + 49) * 2], clip.data[49 * 2]);
        assert_eq!(cut[(22050 + 50) * 2], 0.0, "cut at until");
    }

    /// A user skin sample takes priority over the embedded set, missing
    /// slots mix with it, and lookups nothing provides are silent.
    #[test]
    fn resolver_mixes_skin_with_builtin() {
        let dir = std::env::temp_dir().join(format!("osr_hitsound_skin_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 50ms of near-silence - the builtin hitwhistle is much longer,
        // so which provider answered is observable via the duration.
        let short = encode_wav(&vec![0.01f32; 4410], false);
        std::fs::write(dir.join("normal-hitwhistle.wav"), &short).unwrap();
        // Bank-less universal name: "hitclap" with no bank prefix.
        std::fs::write(dir.join("hitclap.wav"), &short).unwrap();
        std::fs::write(dir.join("skin.ini"), "[General]\nVersion: 2.5\n").unwrap();

        let skin = crate::skin::load_skin(Some(&dir)).unwrap();
        assert!(skin.is_legacy());
        let mut resolver = SampleResolver::new(&skin);

        let whistled = resolver.clip(&HitSample { name: "hitwhistle", bank: Bank::Normal, custom: 0, volume: 100, filename: None, layered: false }).unwrap();
        assert!((45.0..55.0).contains(&whistled.duration_ms()), "skin file wins, got {}ms", whistled.duration_ms());
        let builtin_whistle = sample_clip("normal", "hitwhistle").unwrap();
        assert!(builtin_whistle.duration_ms() > 100.0);

        // The skin has no drum-hitfinish: the embedded ArgonPro copy fills it.
        let finish = resolver.clip(&HitSample { name: "hitfinish", bank: Bank::Drum, custom: 0, volume: 100, filename: None, layered: false }).unwrap();
        assert_eq!(finish.duration_ms(), sample_clip("drum", "hitfinish").unwrap().duration_ms());

        // Universal bank-less file serves every bank's hitclap (lazer's
        // `Gameplay/{Name}` + raw-name tail of `getLegacyLookupNames`).
        let clap = resolver.clip(&HitSample { name: "hitclap", bank: Bank::Soft, custom: 0, volume: 100, filename: None, layered: false }).unwrap();
        assert!((45.0..55.0).contains(&clap.duration_ms()));

        // Nothing provides this: silent, and cached as such.
        assert!(resolver.clip(&HitSample { name: "nosuchsound", bank: Bank::Soft, custom: 0, volume: 100, filename: None, layered: false }).is_none());
        assert!(resolver.clip(&HitSample { name: "nosuchsound", bank: Bank::Soft, custom: 0, volume: 100, filename: None, layered: false }).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `SampleResolver::new` on the builtin skin (no `--skin`) keeps the
    /// embedded set as the only provider.
    #[test]
    fn resolver_builtin_skin_uses_embedded_set() {
        let skin = crate::skin::load_skin(None).unwrap();
        let mut resolver = SampleResolver::new(&skin);
        let clip = resolver.clip(&HitSample { name: "hitnormal", bank: Bank::Normal, custom: 0, volume: 100, filename: None, layered: false }).unwrap();
        let builtin = sample_clip("normal", "hitnormal").unwrap();
        assert_eq!(clip.duration_ms(), builtin.duration_ms());
    }

    /// `LayeredHitSounds: 0` on a legacy skin silences only the layered
    /// hitnormal (`LegacySkinTransformer.GetSample` → `SampleVirtual`), and the
    /// cache must keep that decision separate from the same slot played
    /// non-layered.
    #[test]
    fn layered_hit_sounds_setting_mutes_only_layered_samples() {
        let dir = std::env::temp_dir().join(format!("osr_layered_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("skin.ini"), "[General]\nVersion: 2.5\nLayeredHitSounds: 0\n").unwrap();

        let skin = crate::skin::load_skin(Some(&dir)).unwrap();
        assert!(skin.is_legacy());
        let plain = HitSample { name: "hitnormal", bank: Bank::Soft, custom: 0, volume: 100, filename: None, layered: false };
        let layered = HitSample { layered: true, ..plain.clone() };

        // Plain first, layered second: the cached plain result must not leak.
        let mut resolver = SampleResolver::new(&skin);
        assert!(resolver.clip(&plain).is_some(), "plain hitnormal still plays");
        assert!(resolver.clip(&layered).is_none(), "layered hitnormal is silenced");

        // Layered first: the plain slot must still resolve afterwards.
        let mut resolver = SampleResolver::new(&skin);
        assert!(resolver.clip(&layered).is_none());
        assert!(resolver.clip(&plain).is_some(), "silencing is not cached across the layered flag");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The layered mute belongs to the user-skin layer: a beatmap file is
    /// served by the beatmap skin, which sits above the user skin and never
    /// reads its `LayeredHitSounds`. The beatmap layer must win before the
    /// gate, even for a layered sample.
    #[test]
    fn layered_mute_does_not_apply_to_beatmap_samples() {
        let skin_dir = std::env::temp_dir().join(format!("osr_layered_bm_skin_{}", std::process::id()));
        let map_dir = std::env::temp_dir().join(format!("osr_layered_bm_map_{}", std::process::id()));
        for d in [&skin_dir, &map_dir] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(skin_dir.join("skin.ini"), "[General]\nVersion: 2.5\nLayeredHitSounds: 0\n").unwrap();
        // ~11ms marker: distinguishable from anything the skin/builtin serve.
        let marker = encode_wav(&vec![0.01f32; 500], false);
        std::fs::write(map_dir.join("soft-hitnormal.wav"), &marker).unwrap();

        let skin = crate::skin::load_skin(Some(&skin_dir)).unwrap();
        let store = DirectorySampleStore::new(&map_dir);

        // custom=1 opts the beatmap layer in: the map file wins over the mute.
        let from_map = resolve_sample_parts("soft", "hitnormal", 1, None, true, Some(&store), &skin)
            .expect("map-provided layered sample still plays");
        assert!(from_map == marker, "the beatmap file should be returned untouched");

        // custom=0 keeps the beatmap layer out, so the user-skin mute applies.
        assert!(
            resolve_sample_parts("soft", "hitnormal", 0, None, true, Some(&store), &skin).is_none(),
            "layered sample falls to the user skin, which mutes it"
        );

        std::fs::remove_dir_all(&skin_dir).ok();
        std::fs::remove_dir_all(&map_dir).ok();
    }

    /// The spinner trio resolves from the embedded Argon level (the
    /// ArgonPro set carries no spinner entries — lazer's chain falls
    /// through the same way).
    #[test]
    fn spinner_samples_resolve_from_argon_layer() {
        for name in ["spinnerspin", "spinnerbonus", "spinnerbonus-max"] {
            assert!(sample_clip("normal", name).is_some(), "{name} missing from the embedded set");
        }
    }

    /// `--hitsounds-mixer-volume` (the gameplay mixer layer) scales the whole
    /// track linearly; `--hitsounds-volume` (`bus_gain`) multiplies on top.
    #[test]
    fn mixer_gain_scales_the_track() {
        let map = "osu file format v14\n\
                   \n[General]\nMode: 0\n\
                   \n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:8\nApproachRate:8\nSliderMultiplier:1.8\nSliderTickRate:1\n\
                   \n[TimingPoints]\n500,400,4,2,0,40,1,0\n\
                   \n[HitObjects]\n\
                   100,100,1000,1,0,0:0:0:0:\n";
        let dir = std::env::temp_dir().join(format!("osr_mix_gain_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gain.osu");
        std::fs::write(&path, map).unwrap();
        let game = crate::game::load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();
        let skin = crate::skin::load_skin(None).unwrap();

        let peak = |wav: &[u8]| -> f32 {
            wav[44..]
                .chunks_exact(2)
                .map(|c| (i16::from_le_bytes([c[0], c[1]]) as f32 / 32767.0).abs())
                .fold(0.0, f32::max)
        };
        // Linear variant: no soft limiter, so the gain relation is exact.
        let full = render_track_wav_linear(&game, &game.sample_data, 900.0, 0.5, 1.0, 1.0, 1.0, &skin);
        let half = render_track_wav_linear(&game, &game.sample_data, 900.0, 0.5, 1.0, 1.0, 0.5, &skin);
        let quarter = render_track_wav_linear(&game, &game.sample_data, 900.0, 0.5, 1.0, 0.5, 0.5, &skin);
        let p_full = peak(&full);
        assert!(p_full > 0.05, "the hit is audible, peak={p_full}");
        assert!((peak(&half) / p_full - 0.5).abs() < 0.02, "mixer layer halves the track");
        assert!((peak(&quarter) / p_full - 0.25).abs() < 0.02, "bus gain multiplies on top");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `collect_loop_events` on an autoplay one-slider-one-spinner map.
    #[test]
    fn collect_loop_events_autoplay() {
        // The trailing `hitSample` field is the filename: `0:0:0:0:` leaves it
        // empty (banked sample), while `0:0:0:0:0` would be
        // `FileHitSampleInfo("0")` in lazer — a file lookup that resolves to the
        // default set instead of the timing point's bank.
        let map = "osu file format v14\n\
                   \n[General]\nMode: 0\n\
                   \n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:8\nApproachRate:8\nSliderMultiplier:1.8\nSliderTickRate:1\n\
                   \n[TimingPoints]\n500,400,4,2,0,40,1,0\n\
                   \n[HitObjects]\n\
                   100,100,1000,2,0,B|300:100,1,200,0,0:0:0:0:0,0:0:0:0:0\n\
                   256,192,2000,12,0,3500,0:0:0:0:\n";
        let dir = std::env::temp_dir().join(format!("osr_loop_events_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loops.osu");
        std::fs::write(&path, map).unwrap();
        let game = crate::game::load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();

        let events = collect_loop_events(&game, &osu_parse::samples::parse(map));
        let slide = events.iter().find(|e| e.name == "sliderslide").expect("sliderslide event");
        let spin = events.iter().find(|e| e.name == "spinnerspin").expect("spinnerspin event");
        assert_eq!(events.len(), 2, "no whistle flag on the slider: slide + spin only");

        // Both resolve bank/volume from the timing point active at the
        // object (soft, 40%).
        for e in [&slide, &spin] {
            assert_eq!(e.bank, "soft");
            assert_eq!(e.volume, 40);
        }

        // Slide: exactly the tracking interval, hard start/stop.
        assert_eq!(slide.runs.len(), 1);
        let (a, b) = slide.runs[0];
        assert!((995.0..1020.0).contains(&a), "slide run starts at the slider head, got {a}");
        assert!(b > a + 100.0, "slide run spans the slider body, got [{a},{b}]");
        assert!(slide.run_at(a).is_some() && slide.run_at(b).is_some());
        assert!(slide.run_at(a - 1.0).is_none());
        assert!(slide.run_at(b + 1.0).is_none(), "no fade tail on slide runs");
        let (rate, amp, x) = slide.params_at(0, a + (b - a) / 2.0, true, &game);
        assert_eq!(rate, 1.0);
        assert!((amp - 0.4).abs() < 1e-9, "slide amplitude = line volume, got {amp}");
        assert!((100.0..=300.0).contains(&x), "slide pan follows the ball, got {x}");

        // Spin: run inside the spinnable window [2000, 3500].
        assert!(!spin.runs.is_empty());
        let (a, b) = spin.runs[0];
        assert!(a >= 2000.0 && a < 2150.0, "spin run starts after the spinner opens, got {a}");
        assert!(b > 3000.0 && b <= 3500.0 + 1000.0 / 60.0 + 1.0, "spin run ends at the spinner close (+≤1 frame), got {b}");
        assert!(spin.run_at(b + SPIN_FADE_OUT).is_some(), "fade tail keeps the run audible");
        assert!(spin.run_at(b + SPIN_FADE_OUT + 1.0).is_none());

        // Envelope: silent at the run start, line volume after the 300ms
        // fade-in; playback rate starts below 1 (frequency modulation at
        // zero progress) and rises with spin progress.
        let (_, amp0, _) = spin.params_at(0, a, true, &game);
        assert!(amp0 < 0.01, "fade-in starts silent, got {amp0}");
        let (_, amp1, _) = spin.params_at(0, a + SPIN_FADE_IN, true, &game);
        assert!((amp1 - 0.4).abs() < 1e-9, "envelope reaches line volume, got {amp1}");
        let (r0, _, _) = spin.params_at(0, a + SPIN_FADE_IN, true, &game);
        let (r1, _, _) = spin.params_at(0, b, true, &game);
        assert!(r0 < 1.0, "modulated rate starts below natural, got {r0}");
        assert!(r1 > r0, "rate rises with progress: {r0} -> {r1}");
        let (rn, _, _) = spin.params_at(0, a + SPIN_FADE_IN, false, &game);
        assert_eq!(rn, 1.0, "SpinnerFrequencyModulate off pins the natural rate");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `spinnerspin`/`spinnerbonus`/`spinnerbonus-max` inherit bank/volume from
    /// the sample point at `GetEndTime() + 5ms` (`applySamples`'s non-repeats
    /// branch), not from the point at the spinner's start.
    #[test]
    fn spinner_reference_samples_use_the_end_point() {
        let map = "osu file format v14\n\
                   \n[General]\nMode: 0\n\
                   \n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:8\nApproachRate:8\nSliderMultiplier:1.8\nSliderTickRate:1\n\
                   \n[TimingPoints]\n500,400,4,2,0,40,1,0\n1500,400,4,3,0,70,1,0\n\
                   \n[HitObjects]\n\
                   256,192,1000,12,0,3000,0:0:0:0:\n";
        let dir = std::env::temp_dir().join(format!("osr_spin_endpoint_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spin.osu");
        std::fs::write(&path, map).unwrap();
        let game = crate::game::load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();

        let events = collect_loop_events(&game, &osu_parse::samples::parse(map));
        let spin = events.iter().find(|e| e.name == "spinnerspin").expect("spinnerspin event");
        // The spinner starts under the soft/40 point and ends under drum/70;
        // `Samples` (and therefore the loop) is resolved at the END.
        assert_eq!(spin.bank, "drum");
        assert_eq!(spin.volume, 70);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `spinnerbonus-max` is `SpinnerBonusMaxSampleInfo(HitObject.CreateHitSampleInfo())`
    /// — a plain `HitSampleInfo` carrying only the normal sample's bank, suffix
    /// and volume. It must not inherit the spinner line's filename, must not opt
    /// into the beatmap sample layer, and (not being a `LegacyHitSampleInfo`)
    /// must never be layered, even when the spinner's own sample is.
    #[test]
    fn spinner_bonus_max_is_a_plain_sample() {
        let map = "osu file format v14\n\
                   \n[General]\nMode: 0\n\
                   \n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:8\nApproachRate:8\nSliderMultiplier:1.8\nSliderTickRate:1\n\
                   \n[TimingPoints]\n500,400,4,2,0,40,1,0\n\
                   \n[HitObjects]\n\
                   256,192,1000,12,0,9000,0:0:2:70:spin.wav\n";
        let dir = std::env::temp_dir().join(format!("osr_spin_max_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spinmax.osu");
        std::fs::write(&path, map).unwrap();
        let game = crate::game::load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();

        let events = collect_events(&game, &game.sample_data);
        let max: Vec<_> = events.iter().filter(|e| e.name == "spinnerbonus-max").collect();
        assert!(!max.is_empty(), "autoplay must spin past the bonus budget for this test input");
        for e in max {
            assert_eq!(e.bank, "normal");
            assert_eq!(e.volume, 70, "volume comes from the normal sample");
            assert_eq!(e.custom, 0, "plain HitSampleInfo: no custom-bank suffix");
            assert_eq!(e.filename, None, "plain HitSampleInfo: no file reference");
            assert!(!e.layered, "plain HitSampleInfo: never layered");
        }
        // The plain bonus sample does keep the file reference (`With` preserves
        // the `FileHitSampleInfo`), so the two slots differ on purpose.
        assert!(
            events.iter().filter(|e| e.name == "spinnerbonus").any(|e| e.filename.is_some()),
            "FileHitSampleInfo.With keeps the file for spinnerbonus"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `Sample.PlaybackConcurrency`: 6 at a time per sample file, and a single
    /// reused `SkinnableSound` instance (`cap == 1`) cuts on every retrigger.
    #[test]
    fn voice_limit_caps_and_steals_longest_playing() {
        let clip = Arc::new(decode_wav(wav!("normal-hitnormal")).unwrap());
        let dur = clip.duration_ms() / 1000.0;
        let voice = |start: f64, key: usize, cap: usize| MixVoice {
            clip: clip.clone(),
            start,
            until: None,
            gl: 1.0,
            gr: 1.0,
            key,
            cap,
        };

        // Seven instances of one sample at the same instant: the seventh play
        // evicts the longest-playing (the first) and cuts it immediately.
        let mut voices: Vec<MixVoice> =
            (0..7).map(|_| voice(0.0, 1, SAMPLE_CONCURRENCY)).collect();
        limit_voices(&mut voices);
        assert_eq!(voices[0].until, Some(0.0), "voice 0 is stolen at 0s");
        assert!(voices[1..].iter().all(|v| v.until.is_none()), "voices 1..6 keep playing");

        // cap 1: each retrigger cuts the previous playback.
        let mut voices: Vec<MixVoice> = (0..3).map(|i| voice(i as f64 * 0.01, 2, 1)).collect();
        limit_voices(&mut voices);
        assert_eq!(voices[0].until, Some(0.01));
        assert_eq!(voices[1].until, Some(0.02));
        assert_eq!(voices[2].until, None);

        // A voice that already ended holds no slot.
        let mut voices = vec![voice(0.0, 3, 1), voice(dur + 0.1, 3, 1)];
        limit_voices(&mut voices);
        assert_eq!(voices[0].until, None, "finished playback is not stolen");
        assert_eq!(voices[1].until, None);

        // Different resources never compete (separate BASS handles).
        let mut voices = vec![voice(0.0, 4, 1), voice(0.01, 5, 1)];
        limit_voices(&mut voices);
        assert!(voices.iter().all(|v| v.until.is_none()));

        // Loop tiles keep their own cutoff (and never steal from themselves).
        let mut voices: Vec<MixVoice> = (0..8)
            .map(|i| {
                let mut v = voice(i as f64 * 0.01, 6, SAMPLE_CONCURRENCY);
                v.until = Some(i as f64 * 0.01 + 0.005);
                v
            })
            .collect();
        limit_voices(&mut voices);
        for (i, v) in voices.iter().enumerate() {
            assert_eq!(v.until, Some(i as f64 * 0.01 + 0.005), "loop tile {i} untouched");
        }
    }

    /// The stereo balance is clamped to [-1, 1] like BASS's `BASS_ATTRIB_PAN`:
    /// out-of-range positions (degenerate aspire sliders scale their path to
    /// ~1.5e9 units) must hard-pan, never turn into a negative/oversized gain.
    #[test]
    fn balance_clamps_like_bass_pan() {
        assert!((balance(0.0) + 0.8).abs() < 1e-9, "left edge");
        assert!((balance(512.0) - 0.8).abs() < 1e-9, "right edge");
        assert_eq!(balance(256.0), 0.0, "centre");
        for x in [1518500608.0f32, -1518500608.0, 1.0e30, -1.0e30, f32::INFINITY, f32::NEG_INFINITY] {
            let b = balance(x);
            assert!((-1.0..=1.0).contains(&b), "balance({x}) = {b} must stay within [-1, 1]");
        }
        assert_eq!(balance(f32::NAN), 0.0, "non-finite position → centre");
        // A clamped balance can never produce a negative gain.
        for x in [1518500608.0f32, -1518500608.0] {
            let bal = balance(x) as f32;
            assert!(1.0 - bal.max(0.0) >= 0.0 && 1.0 - (-bal).max(0.0) >= 0.0);
        }
    }

    /// `LegacyHitSoundType` → `IsLayered`: a non-empty set without the Normal
    /// flag attaches a layered hitnormal; None is a plain normal.
    #[test]
    fn layered_flag_matches_convert_sound_type() {
        assert!(!is_layered(0), "None: plain normal sample");
        assert!(!is_layered(1), "normal flag: not layered");
        assert!(is_layered(2), "whistle only: layered hitnormal");
        assert!(is_layered(4 | 8), "finish+clap: layered hitnormal");
        assert!(!is_layered(1 | 2), "normal+whistle: not layered");
        // The pooled-sample caps.
        let base = HitSample { name: "hitnormal", bank: Bank::Normal, custom: 0, volume: 100, filename: None, layered: false };
        assert_eq!(base.voice_cap(), SAMPLE_CONCURRENCY);
        assert_eq!(HitSample { name: "spinnerbonus", ..base.clone() }.voice_cap(), SAMPLE_CONCURRENCY);
        assert_eq!(HitSample { name: "spinnerbonus-max", ..base.clone() }.voice_cap(), 1);
        assert_eq!(HitSample { name: "combobreak", ..base.clone() }.voice_cap(), 1);
        assert!(HitSample { name: "combobreak", ..base.clone() }.bypasses_gameplay_mixer());
        assert!(!base.bypasses_gameplay_mixer());
    }

    /// A 200Hz sine through the spin-loop synth: with modulation the
    /// ratio at zero progress is `20_000/44_100` (pitch drops to ~91Hz),
    /// with `SpinnerFrequencyModulate` off it stays at the natural rate,
    /// and the envelope gates the run (300ms in / 240ms out).
    #[test]
    fn spin_loop_modulates_pitch_and_envelopes() {
        let sr = SAMPLE_RATE as usize;
        let clip: Vec<f32> = (0..sr / 2)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr as f32).sin();
                [v, v]
            })
            .collect();
        // Constant rotation rate: 144 deg/s -> progress 0.4/s with
        // spins_required = 1 (stays under the 2.27x frequency cap).
        let lp = SpinLoop {
            sample: HitSample { name: "spinnerspin", bank: Bank::Normal, custom: 0, volume: 100, filename: None, layered: false },
            runs: vec![(100.0, 900.0)],
            rotation: (0..=20).map(|i| (i as f64 * 100.0, i as f32 * 14.4)).collect(),
            spins_required: 1.0,
        };
        let zcr = |buf: &[f32], a: usize, b: usize| {
            let mut c = 0;
            for i in a..b {
                if (buf[i * 2] < 0.0) != (buf[(i + 1) * 2] < 0.0) {
                    c += 1;
                }
            }
            c as f64 / ((b - a) as f64 / sr as f64)
        };

        // 2s of stereo buffer; `vi(ms)` is the VALUE index of a frame.
        let vi = |ms: usize| ms * sr / 1000 * 2;
        let mut buf = vec![0.0f32; vi(2000)];
        synth_spin_loop(&mut buf, &clip, &lp, false, 0.0, 1.0, 1.0);
        // Natural rate inside the run; silence before it and after the
        // fade tail (900ms + 240ms).
        assert!((395.0..405.0).contains(&zcr(&buf, sr / 2, sr * 8 / 10)), "natural rate (2 crossings/period)");
        assert!(buf[..vi(50)].iter().all(|v| *v == 0.0), "silent before the run");
        assert!(buf[vi(1200)..].iter().all(|v| *v == 0.0), "silent after the fade tail");
        // Full gain well inside the run (fade-in done by 400ms).
        let peak = buf[vi(600)..vi(800)].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.95, "envelope reaches unity, peak={peak}");

        // The gameplay mixer layer scales the loop like every other hitsound
        // voice (`spinningSample` sits under `DrawableRuleset.Audio`).
        let mut buf = vec![0.0f32; vi(2000)];
        synth_spin_loop(&mut buf, &clip, &lp, false, 0.0, 1.0, 0.5);
        let half = buf[vi(600)..vi(800)].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((half / peak - 0.5).abs() < 0.01, "mixer halves the loop, peak={half}");

        // Modulated: over [300ms, 500ms] the progress spans 0.12..0.2 ->
        // ratio 0.562..0.635 -> ~225-254 crossings/s.
        let mut buf = vec![0.0f32; vi(2000)];
        synth_spin_loop(&mut buf, &clip, &lp, true, 0.0, 1.0, 1.0);
        let z = zcr(&buf, sr * 3 / 10, sr / 2);
        assert!((220.0..260.0).contains(&z), "modulated pitch ~0.6x, got {z}");
        // Later window runs faster: [700ms, 900ms] -> ratio ~0.71..0.78.
        let z2 = zcr(&buf, sr * 7 / 10, sr * 9 / 10);
        assert!(z2 > z + 30.0, "pitch rises with progress: {z} -> {z2}");
    }
}

#[cfg(test)]
mod beatmap_layer_tests {
    use super::*;

    /// 谱面层候选名:`UseCustomSampleBanks` 后缀强制(≥2 只留带后缀名)、
    /// custom=1 无后缀、custom=0 不参与、filename 槽与 combobreak 的特例。
    #[test]
    fn beatmap_lookup_names_follow_lazer_chain() {
        assert_eq!(
            beatmap_lookup_names("soft", "hitnormal", 2, None),
            vec!["soft-hitnormal2".to_string(), "hitnormal".to_string()],
            "custom ≥ 2:带组号优先,再查通用名"
        );
        assert_eq!(
            beatmap_lookup_names("soft", "hitnormal", 1, None),
            vec!["soft-hitnormal".to_string(), "hitnormal".to_string()]
        );
        assert!(beatmap_lookup_names("soft", "hitnormal", 0, None).is_empty(), "custom=0:UseBeatmapSamples 为假,谱面层不参与");
        assert_eq!(beatmap_lookup_names("normal", "combobreak", 0, None), vec!["combobreak".to_string()]);
        assert_eq!(
            beatmap_lookup_names("normal", "hitnormal", 1, Some("tickle.mp3")),
            vec!["tickle.mp3".to_string(), "tickle".to_string(), "normal-hitnormal".to_string(), "hitnormal".to_string()]
        );
    }

    /// 层级优先级:谱面文件(参与的槽位)> 用户皮肤 > 内置;命中即权威
    /// (0 字节 = 静音,不跨层回退);custom=0 的谱面文件被无视。
    #[test]
    fn beatmap_files_layer_over_skin_and_builtin() {
        let dir = std::env::temp_dir().join(format!("orr_beatmap_samples_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 45ms / 70ms 近静音:时长即可区分每一层提供的字节。
        let short = encode_wav(&vec![0.01f32; 993], false); // ~45ms
        let mid = encode_wav(&vec![0.01f32; 1546], false); // ~70ms
        std::fs::write(dir.join("soft-hitnormal2.wav"), &short).unwrap();
        std::fs::write(dir.join("soft-hitnormal.wav"), &mid).unwrap();
        std::fs::write(dir.join("soft-hitclap2.wav"), b"").unwrap(); // 0 字节占位
        let universal = encode_wav(&vec![0.01f32; 5000], false); // ~57ms
        std::fs::write(dir.join("hitclap.wav"), &universal).unwrap();
        std::fs::write(dir.join("tickle.wav"), &short).unwrap();
        let store = DirectorySampleStore::new(&dir);

        // 皮肤:提供自己的 soft-hitnormal / soft-hitclap。
        let skin_dir = std::env::temp_dir().join(format!("orr_beatmap_skin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&skin_dir);
        std::fs::create_dir_all(&skin_dir).unwrap();
        let skin_hit = encode_wav(&vec![0.02f32; 3085], false); // ~140ms,与上面都不同
        std::fs::write(skin_dir.join("soft-hitnormal.wav"), &skin_hit).unwrap();
        std::fs::write(skin_dir.join("soft-hitclap.wav"), &skin_hit).unwrap();
        std::fs::write(skin_dir.join("skin.ini"), "[General]\nVersion: 2.5\n").unwrap();
        let skin = crate::skin::load_skin(Some(&skin_dir)).unwrap();

        let ms = |bytes: &Vec<u8>| decode_wav(bytes).map(|c| c.duration_ms()).unwrap_or(0.0);

        // custom=2:谱面 soft-hitnormal2(45ms)压过皮肤的无后缀文件。
        let r = resolve_sample_parts("soft", "hitnormal", 2, None, false, Some(&store), &skin).unwrap();
        assert!((10.0..12.0).contains(&ms(&r)), "custom=2 应命中谱面带后缀文件: {:.0}ms", ms(&r));

        // custom=1:谱面无后缀 soft-hitnormal(70ms)压过皮肤(140ms)。
        let r = resolve_sample_parts("soft", "hitnormal", 1, None, false, Some(&store), &skin).unwrap();
        assert!((16.0..19.0).contains(&ms(&r)), "custom=1 应命中谱面无后缀文件: {:.0}ms", ms(&r));

        // custom=0:谱面文件不参与,皮肤(140ms)接管。
        let r = resolve_sample_parts("soft", "hitnormal", 0, None, false, Some(&store), &skin).unwrap();
        assert!((34.0..36.0).contains(&ms(&r)), "custom=0 应由皮肤提供: {:.0}ms", ms(&r));

        // custom=3:谱面没有 soft-hitnormal3 → 皮肤剥后缀接管(140ms)。
        let r = resolve_sample_parts("soft", "hitnormal", 3, None, false, Some(&store), &skin).unwrap();
        assert!((34.0..36.0).contains(&ms(&r)), "缺失的后缀文件应剥后缀落到皮肤: {:.0}ms", ms(&r));

        // 0 字节谱面占位 = 权威静音(不回退皮肤/内置,也不做通用名二次查找)。
        let r = resolve_sample_parts("soft", "hitclap", 2, None, false, Some(&store), &skin);
        assert!(r.map(|b| b.is_empty()).unwrap_or(false), "0 字节占位应静音");

        // 组号文件缺失:谱面里的通用名 hitclap 优先于皮肤。
        let r = resolve_sample_parts("soft", "hitclap", 4, None, false, Some(&store), &skin).unwrap();
        assert!((54.0..60.0).contains(&ms(&r)), "组号缺失应命中谱面通用名: {:.0}ms", ms(&r));

        // filename 槽:谱面 tickle.wav 直接命中。
        let r = resolve_sample_parts("normal", "hitnormal", 1, Some("tickle.wav"), false, Some(&store), &skin).unwrap();
        assert!((10.0..12.0).contains(&ms(&r)), "filename 槽应命中谱面文件: {:.0}ms", ms(&r));

        // filename 槽缺失:剥扩展名也 miss → 内置 normal-hitnormal(非空)。
        let r = resolve_sample_parts("normal", "hitnormal", 1, Some("nosuch.ogg"), false, Some(&store), &skin);
        assert!(r.is_some_and(|b| !b.is_empty()), "filename 缺失应回退内置 hitnormal");

        // 无皮肤参与的非 legacy 内置皮肤同样工作(store 单独在场)。
        let argon = crate::skin::load_skin(None).unwrap();
        let r = resolve_sample_parts("soft", "hitnormal", 2, None, false, Some(&store), &argon).unwrap();
        assert!((10.0..12.0).contains(&ms(&r)));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&skin_dir).ok();
    }

    /// 解析 → 事件:timing point sampleIndex 与对象 hitSample 的
    /// customIndex 合并(ApplyTo 语义),filename 槽以 File 形态出现在
    /// 事件里。
    #[test]
    fn collect_events_carries_custom_index_and_filename() {
        let map = "osu file format v14\n\
                   \n[General]\nMode: 0\n\
                   \n[Difficulty]\nHPDrainRate:5\nCircleSize:4\nOverallDifficulty:8\nApproachRate:8\nSliderMultiplier:1.8\nSliderTickRate:1\n\
                   \n[TimingPoints]\n500,400,4,2,2,45,1,0\n\
                   \n[HitObjects]\n\
                   100,100,1000,1,0,0:0:2:60:\n\
                   200,100,1200,1,0,0:0:0:80:tickle.wav\n";
        let dir = std::env::temp_dir().join(format!("orr_custom_idx_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom.osu");
        std::fs::write(&path, map).unwrap();
        let game = crate::game::load_autoplay(path.to_str().unwrap(), 0, false, false).unwrap();

        let events = collect_events(&game, &game.sample_data);
        let first = events.iter().find(|e| e.time == 1000.0).expect("object 1 judged");
        assert_eq!(first.bank, "soft", "bank 继承 timing point(sampleSet=2)");
        assert_eq!(first.custom, 2, "对象 customIndex=2 覆盖点级");
        assert_eq!(first.volume, 60, "对象 volume 覆盖点级");
        assert_eq!(first.slot(), SampleSlot::Bank { bank: "soft", name: "hitnormal", custom: 2 });

        let second = events.iter().find(|e| e.time == 1200.0).expect("object 2 judged");
        assert_eq!(second.custom, 1, "filename 槽强制 custom=1");
        assert_eq!(second.volume, 80);
        assert_eq!(second.slot(), SampleSlot::File { filename: "tickle.wav".to_string() });
        std::fs::remove_dir_all(&dir).ok();
    }
}
