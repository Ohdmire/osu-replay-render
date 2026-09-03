//! PP calculation via `rosu-pp` (the `pp-rework-202607` fork, a Rust port
//! of osu!lazer's difficulty/performance calculators).
//!
//! Two entry points:
//!
//! - [`calculate`]: the full-play PP (plus the map+mods FC max PP) - the
//!   numbers lazer's results screen would show.
//! - a **live PP timeline** built with `OsuGradualPerformance` ("逐个输入
//!   计算"): the judge timeline is walked in application order, each
//!   judgement folds into a running `OsuScoreState`, and every time a
//!   top-level object finishes judging the gradual calculator is advanced
//!   one object - exactly lazer's in-game `PerformancePointsCounter`
//!   behaviour (it recalculates on every `NewJudgement`). The result is
//!   `(time, pp)` pairs queryable at any moment, mirroring `score_events`.
//!
//! Judgement mapping onto `OsuHitResults` (lazer score; see
//! `InspectOsuPerformance::tick_hits` for what rosu-pp expects):
//!
//! | judgement | classic (stable score) | lazer score |
//! |---|---|---|
//! | circle / spinner final, `Great`/`Perfect` | n300 | n300 |
//! | circle / spinner final, `Ok`/`Meh`/`Miss` | n100/n50/misses | n100/n50/misses |
//! | slider head (lazer: windowed `Great`/`Ok`/`Meh`) | — (large-tick, not in acc) | n300/n100/n50 (heads are accuracy judgements only - they are NOT large ticks) |
//! | slider head miss | — | misses |
//! | repeat hit (`LargeTickHit`) | irrelevant | large_tick_hits |
//! | slider tail hit (`SliderTailHit`) | irrelevant | slider_end_hits (feeding 0 here makes rosu-pp treat every slider end as dropped, nerfing aim) |
//! | slider tick hit (`SmallTickHit`) | irrelevant | small_tick_hits |
//! | spinner bonus ticks / smax | skipped - no PP | skipped |
//!
//! The gradual calculator consumes objects in beatmap order, so advances
//! happen when the NEXT object in order finishes (a slider tail judged
//! after later circles delays their advance slightly; the score state is
//! always exact). Objects without judgements (0-spin spinners) are folded
//! in by the final `last()` advance.

use osu_replay_judge::engine::Engine;
use osu_replay_judge::score::HitResult;

/// Everything the renderer needs from the PP calculation.
pub struct PpData {
    /// Final playthrough PP (the last gradual value).
    pub pp: f64,
    /// FC max PP of the map+mods (`performance()` off the full attrs).
    pub pp_max: f64,
    /// Star rating of the map+mods (the results screen's
    /// `StarRatingDisplay` feed).
    pub stars: f64,
    /// Component breakdowns (achieved, FC max) for the results screen's
    /// `PerformanceBreakdownChart` (`GetAttributesForDisplay`).
    pub breakdown: PpBreakdown,
    pub breakdown_max: PpBreakdown,
    /// 结算页 Difficulty Graph 的三条曲线,均为逐物件时间序列
    /// `(物件 start_time, strain)`。speed/reading 是 `OsuStrains` 公开
    /// 字段里自带的逐物件序列;aim 依赖本地 rosu-pp patch
    /// (`Aim::into_object_difficulties`,条目数与 speed 一致,不一致
    /// 时视为未 patch 而留空)。条目 j 属于第 j+1 个顶层物件(首个
    /// 物件不产生条目)。单条为空时绘制端跳过。
    pub strain_aim_pts: Vec<(f64, f64)>,
    pub strain_speed_pts: Vec<(f64, f64)>,
    pub strain_reading_pts: Vec<(f64, f64)>,
    /// `(time, pp)` after each finished top-level object, in order - the
    /// live counter timeline ([`pp_at`] binary-searches it).
    pub events: Vec<(f64, f64)>,
}

/// PP components (`OsuPerformanceAttributes` minus the total).
#[derive(Clone, Copy, Debug, Default)]
pub struct PpBreakdown {
    pub total: f64,
    pub aim: f64,
    pub speed: f64,
    pub accuracy: f64,
    pub flashlight: f64,
    pub reading: f64,
}

impl PpBreakdown {
    fn of(a: &rosu_pp::osu::OsuPerformanceAttributes) -> PpBreakdown {
        PpBreakdown {
            total: a.pp,
            aim: a.pp_aim,
            speed: a.pp_speed,
            accuracy: a.pp_acc,
            flashlight: a.pp_flashlight,
            reading: a.pp_reading,
        }
    }
}

/// Folds one judgement into the running state. Judgements that carry no
/// PP weight (spinner bonus ticks, slider body ignores) are skipped.
fn fold(label: &str, result: HitResult, classic: bool, state: &mut rosu_pp::osu::OsuScoreState) {
    match label {
        "circle" | "spinner" => match result {
            HitResult::Great | HitResult::Perfect => state.hitresults.n300 += 1,
            HitResult::Ok => state.hitresults.n100 += 1,
            HitResult::Meh => state.hitresults.n50 += 1,
            HitResult::Miss => state.hitresults.misses += 1,
            _ => {}
        },
        // Lazer slider heads are windowed judgements that count toward
        // accuracy as plain n300/n100/n50 (NOT large ticks - rosu-pp's
        // n_large_ticks counts repeats only); classic heads are large-tick
        // (outside accuracy entirely).
        "head" if !classic => match result {
            HitResult::Great | HitResult::Perfect => state.hitresults.n300 += 1,
            HitResult::Ok => state.hitresults.n100 += 1,
            HitResult::Meh => state.hitresults.n50 += 1,
            HitResult::Miss => state.hitresults.misses += 1,
            _ => {}
        },
        // Repeats / tails / ticks: lazer counts them, classic ignores them.
        _ if !classic => match result {
            HitResult::LargeTickHit => state.hitresults.large_tick_hits += 1,
            HitResult::SliderTailHit => state.hitresults.slider_end_hits += 1,
            HitResult::SmallTickHit => state.hitresults.small_tick_hits += 1,
            _ => {}
        },
        _ => {}
    }
}

/// PP for the replay the engine judged, plus the live timeline. `None`
/// when rosu-pp cannot parse the map (rendering continues without the
/// numbers).
pub fn calculate(map_path: &str, mods_bits: u32, classic: bool, engine: &Engine) -> Option<PpData> {
    let map = rosu_pp::Beatmap::from_path(map_path).ok()?;
    map.check_suspicion().ok()?;

    let difficulty = rosu_pp::Difficulty::new()
        .lazer(!classic)
        .mods(rosu_pp::model::mods::rosu_mods::GameModsLegacy::from_bits(mods_bits));

    // FC max PP off the full difficulty attributes.
    let attrs = match difficulty.clone().calculate(&map) {
        rosu_pp::any::DifficultyAttributes::Osu(attrs) => attrs,
        _ => return None,
    };
    // 难度-时间图:一次 strains() 全算完。speed/reading 逐物件(条目 j
    // 属于第 j+1 个顶层物件),t 直接取谱面物件 start_time;aim 依赖
    // 本地 rosu-patch(逐物件化),条目数与 speed 一致才用 —— 未 patch
    // 时 aim 是按值降序、无时间戳的滚动峰值,画了就是错的数据,宁可
    // 留空(flashlight 仍是滚动峰值,不用)。数量万一不齐就 zip 截断,
    // 曲线缺角但不清空。
    let (strain_aim_pts, strain_speed_pts, strain_reading_pts) = match difficulty.clone().strains(&map) {
        rosu_pp::any::Strains::Osu(s) => {
            let per_object = |values: &[f64]| {
                values
                    .iter()
                    .zip(map.hit_objects.iter().skip(1))
                    .map(|(&v, h)| (h.start_time, v))
                    .collect()
            };
            // 仅当 aim 与 speed 条目数一致(=本地 patch 生效)才映射。
            let aim = if s.aim.len() == s.speed.len() {
                per_object(&s.aim)
            } else {
                Vec::new()
            };
            (aim, per_object(&s.speed), per_object(&s.reading))
        }
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    let max_pp = rosu_pp::osu::OsuPerformance::new(attrs.clone())
        .lazer(!classic)
        .mods(rosu_pp::model::mods::rosu_mods::GameModsLegacy::from_bits(mods_bits))
        .calculate()
        .ok()?;
    let breakdown_max = PpBreakdown::of(&max_pp);
    let max_pp = max_pp.pp();

    // Live timeline: advance the gradual calculator one object each time
    // the next object (in beatmap order) finishes judging.
    let mut gradual = rosu_pp::osu::OsuGradualPerformance::new(difficulty, &map).ok()?;
    let mut state = rosu_pp::osu::OsuScoreState::new();

    let n_objects = engine.objects().len();
    // Judgements remaining per object (index == beatmap order).
    let mut remaining = vec![0usize; n_objects];
    for e in &engine.timeline {
        if e.label == "smax" || e.label.starts_with("stick") {
            continue; // spinner bonus: no PP
        }
        if let Some(slot) = remaining.get_mut(e.object_index) {
            *slot += 1;
        }
    }

    let mut events: Vec<(f64, f64)> = Vec::new();
    let mut next_obj = 0usize;
    let mut max_combo = 0i32;
    // Keep the pair timeline monotonic (`pp_at` binary-searches it): late
    // judgements carry earlier object times, same as `score_events`.
    let mut last_t = f64::NEG_INFINITY;
    // The last gradual result (the score's own performance attributes).
    let mut last_attrs: Option<rosu_pp::osu::OsuPerformanceAttributes> = None;

    for e in &engine.timeline {
        if e.label == "smax" || e.label.starts_with("stick") {
            continue;
        }
        fold(&e.label, e.result, classic, &mut state);
        max_combo = max_combo.max(e.combo);
        state.max_combo = max_combo.max(0) as u32;
        if let Some(slot) = remaining.get_mut(e.object_index) {
            *slot = slot.saturating_sub(1);
        }

        // Advance every object that has fully judged, in order.
        while next_obj < n_objects && remaining[next_obj] == 0 {
            if let Some(attrs) = gradual.next(state.clone()) {
                last_t = last_t.max(e.time);
                events.push((last_t, attrs.pp));
                last_attrs = Some(attrs);
            }
            next_obj += 1;
        }
    }

    // Objects that never judged (e.g. 0-spin spinners): fold them in at
    // the last judgement's time.
    if gradual.len() > 0 {
        if let Some(attrs) = gradual.last(state.clone()) {
            let t = engine.timeline.last().map(|e| e.time).unwrap_or(0.0);
            last_t = last_t.max(t);
            events.push((last_t, attrs.pp));
            last_attrs = Some(attrs);
        }
    }

    let pp = events.last().map(|&(_, pp)| pp).unwrap_or(0.0);
    let breakdown = last_attrs.as_ref().map(PpBreakdown::of).unwrap_or_default();

    Some(PpData { pp, pp_max: max_pp, stars: attrs.stars, breakdown, breakdown_max, strain_aim_pts, strain_speed_pts, strain_reading_pts, events })
}

/// Live PP at time `t` (latest event at/before `t`; 0.0 before the first).
pub fn pp_at(events: &[(f64, f64)], t: f64) -> f64 {
    match events.partition_point(|e| e.0 <= t) {
        0 => 0.0,
        n => events[n - 1].1,
    }
}
