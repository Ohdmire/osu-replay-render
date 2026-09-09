//! Flashbacklog-style PP timeout reproduction + fix verification.
//!
//! The PP pass in `src/pp.rs` walks `OsuGradualPerformance` one object at a
//! time, and each `next()` re-aggregates the full strain history (sorts +
//! scans in `DifficultyValues::eval`). That makes the whole timeline
//! quadratic in object count — invisible on normal maps, minutes on
//! Aspire-density maps like Flashbacklog [V]. The fix advances in adaptive
//! blocks (`nth`) so the expensive eval runs at most ~1024 times.
//!
//! Usage: `cargo run --release --example pp_bench -- <map.osu>`

use rosu_pp::{osu::OsuGradualPerformance, osu::OsuScoreState, Beatmap, Difficulty};

/// Tile the map `copies` times along the timeline: same density, same
/// patterns, N x the objects — an Aspire-length map without hand-writing
/// one.
fn stretch(map: &Beatmap, copies: usize) -> Beatmap {
    let mut m = map.clone();
    let base = map.hit_objects.clone();
    let span = base.last().unwrap().start_time - base.first().unwrap().start_time + 5000.0;

    let mut objs = Vec::with_capacity(base.len() * copies);
    for c in 0..copies {
        for h in &base {
            let mut h = h.clone();
            h.start_time += c as f64 * span;
            objs.push(h);
        }
    }
    objs.sort_by(|a, b| a.start_time.total_cmp(&b.start_time));
    m.hit_objects = objs;
    m
}

fn step(state: &mut OsuScoreState) {
    state.hitresults.n300 += 1;
    state.max_combo += 1;
}

/// Advance one object per call (the pre-fix behaviour).
fn run_per_object(map: &Beatmap) -> (Vec<f64>, std::time::Duration) {
    let mut gradual = OsuGradualPerformance::new(Difficulty::new(), map).unwrap();
    let mut state = OsuScoreState::new();
    let mut pps = Vec::new();
    let t = std::time::Instant::now();
    loop {
        step(&mut state);
        let Some(a) = gradual.next(state.clone()) else { break };
        pps.push(a.pp);
    }
    (pps, t.elapsed())
}

/// Advance `block` objects per `nth()` (the fix). Returns the pp at every
/// block boundary plus the duration.
fn run_blocked(map: &Beatmap, block: usize) -> (Vec<f64>, std::time::Duration) {
    let mut gradual = OsuGradualPerformance::new(Difficulty::new(), map).unwrap();
    let mut state = OsuScoreState::new();
    let n = map.hit_objects.len();
    let mut pps = Vec::new();
    let t = std::time::Instant::now();
    let mut consumed = 0usize;
    while consumed < n {
        let take = (n - consumed).min(block);
        for _ in 0..take {
            step(&mut state);
        }
        match gradual.nth(state.clone(), take - 1) {
            Some(a) => pps.push(a.pp),
            None => break,
        }
        consumed += take;
    }
    (pps, t.elapsed())
}

fn bench(map: &Beatmap, label: &str) {
    let n = map.hit_objects.len();

    // One-shot full difficulty calculation (the `calculate` + `strains`
    // calls in pp::calculate): expected linear.
    let t = std::time::Instant::now();
    let _ = Difficulty::new().calculate(map);
    println!("{label:>4}: {n:5} objects | one-shot difficulty: {:>10.1?} |", t.elapsed());

    let (per_obj, t1) = run_per_object(map);
    println!(
        "{label:>4}: {n:5} objects | per-object timeline: {:>10.1?} | {:.0} us/obj",
        t1,
        t1.as_micros() as f64 / n as f64,
    );

    let block = n.div_ceil(1024).max(1);
    let (blocked, t2) = run_blocked(map, block);
    // Correctness: every block boundary must match the per-object timeline
    // at the same object index (skills/state are identical there).
    let mut mismatches = 0usize;
    for (i, pp) in blocked.iter().enumerate() {
        let idx = ((i + 1) * block).min(n) - 1;
        if per_obj.get(idx).is_none_or(|p| p.to_bits() != pp.to_bits()) {
            mismatches += 1;
        }
    }
    let last_ok = per_obj.last().map(|p| p.to_bits()) == Some(blocked.last().copied().unwrap_or(0.0).to_bits());
    println!(
        "{label:>4}: {n:5} objects | blocked (b={block:<3}):   {:>10.1?} | {:.0} us/obj | speedup x{:.0} | mismatched boundaries: {mismatches}, final pp equal: {last_ok}",
        t2,
        t2.as_micros() as f64 / n as f64,
        t1.as_secs_f64() / t2.as_secs_f64().max(1e-9),
    );
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: pp_bench <map.osu>");
    let map = Beatmap::from_path(&path).unwrap();
    println!("base map: {path} ({} objects)", map.hit_objects.len());
    for copies in [8usize, 32] {
        bench(&stretch(&map, copies), &format!("x{copies}"));
    }
}
