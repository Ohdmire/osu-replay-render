//! Argon HUD: wedge pieces, score/accuracy/combo counters (argon-counter
//! texture digits with wireframes), health bar, and rolling counter logic.

use crate::draw::{draw_ttf_text, value_at, Blend, Colour, DrawList, Easing, Region};
use crate::game::{health_at, GameData};
use crate::scene::{colour_for_result, draw_chevron, Assets, Mapper};

const WEDGE_COLOUR: u32 = 0x66CCFF;
const HEALTH_GLOW: [u8; 4] = [126, 215, 253, 128];

/// Rolling counter (250ms OutQuad, matching `RollingCounter`).
pub struct Rolling {
    pub display: f64,
    from: f64,
    to: f64,
    start: f64,
}

impl Rolling {
    pub fn new() -> Rolling {
        Rolling { display: 0.0, from: 0.0, to: 0.0, start: f64::NEG_INFINITY }
    }

    pub fn set(&mut self, value: f64, t: f64) {
        if value != self.to {
            self.from = self.display;
            self.to = value;
            self.start = t;
        }
    }

    pub fn update(&mut self, t: f64) {
        self.display = value_at(t, self.start, self.start + 250.0, self.from, self.to, Easing::OutQuad);
    }
}

/// A number rendered with the argon-counter texture digits.
struct CounterDraw<'a> {
    atlas: &'a crate::draw::Atlas,
    /// Digit ink height in px (integer digits).
    digit_h: f32,
    spacing: f32,
}

impl<'a> CounterDraw<'a> {
    fn region_for(c: char) -> Region {
        match c {
            '.' => Region::CounterDot,
            '%' => Region::CounterPercent,
            'x' | 'X' => Region::CounterX,
            _ => Region::CounterDigit(c as u8),
        }
    }

    /// Places one glyph so its ink box (from the alpha coverage bbox) is
    /// centred at `(cx, cy)` with ink height `h * scale`; returns the ink
    /// width.
    fn place(
        &self,
        list: &mut DrawList,
        c: char,
        cx: f32,
        cy: f32,
        h: f32,
        scale: f32,
        colour: Colour,
        blend: Blend,
    ) -> f32 {
        let region = Self::region_for(c);
        let rect = self.atlas.region_rect(region);
        let ink = self.atlas.ink(region);
        let tex_w = rect.x1 - rect.x0;
        let tex_h = rect.y1 - rect.y0;
        let ink_w = (ink[2] - ink[0]).max(1.0);
        let ink_h = (ink[3] - ink[1]).max(1.0);
        let target_h = h * scale;
        let s = target_h / ink_h;
        let img_w = tex_w * s;
        let img_h = tex_h * s;
        // Image position so that the ink centre lands on (cx, cy).
        let ink_cx = (ink[0] + ink[2]) * 0.5 * s;
        let ink_cy = (ink[1] + ink[3]) * 0.5 * s;
        let centre = [cx - ink_cx + img_w * 0.5, cy - ink_cy + img_h * 0.5];
        crate::draw::DrawList::image(list, self.atlas, region, centre, [img_w, img_h], 0.0, colour, blend);
        ink_w * s
    }

    /// Ink width of a char at `scale` (uses the digit_h ink calibration).
    /// Digits are monospaced to the width of '5' (lazer's
    /// `FixedWidthReferenceCharacter`), so rolling numbers never jitter.
    fn ink_w(&self, c: char, scale: f32) -> f32 {
        if c.is_ascii_digit() {
            return self.raw_ink_w('5', scale);
        }
        self.raw_ink_w(c, scale)
    }

    fn raw_ink_w(&self, c: char, scale: f32) -> f32 {
        let region = Self::region_for(c);
        let ink = self.atlas.ink(region);
        let w = (ink[2] - ink[0]).max(1.0);
        let h = (ink[3] - ink[1]).max(1.0);
        w * (self.digit_h * scale / h)
    }

    /// Draw right-aligned with the ink TOP edge at `top_y` (FillFlow
    /// top-aligned components, like ArgonAccuracyCounter). Returns total
    /// ink width.
    fn draw_top(
        &self,
        list: &mut DrawList,
        text: &str,
        right_x: f32,
        top_y: f32,
        scale: f32,
        colour: Colour,
        blend: Blend,
    ) -> f32 {
        // Slot widths use ink_w (digits monospaced to '5'), so the run never
        // jitters and lines up with the wireframes.
        let mut widths = Vec::with_capacity(text.len());
        let mut total = 0.0f32;
        for c in text.chars() {
            let w = self.ink_w(c, scale);
            widths.push(w);
            total += w + self.spacing * scale;
        }
        if !text.is_empty() {
            total -= self.spacing * scale;
        }

        let mut cx = right_x - total;
        for (i, c) in text.chars().enumerate() {
            // Centre the glyph's ink box on the slot centre, ink top at top_y.
            let region = Self::region_for(c);
            let rect = self.atlas.region_rect(region);
            let ink = self.atlas.ink(region);
            let tex_w = rect.x1 - rect.x0;
            let tex_h = rect.y1 - rect.y0;
            let ink_h = (ink[3] - ink[1]).max(1.0);
            let target_h = self.digit_h * scale;
            let k = target_h / ink_h;
            let img_w = tex_w * k;
            let img_h = tex_h * k;
            let slot_cx = cx + widths[i] * 0.5;
            let raw_w = (ink[2] - ink[0]).max(1.0) * k;
            let centre = [
                slot_cx - raw_w * 0.5 - ink[0] * k + img_w * 0.5,
                top_y - ink[1] * k + img_h * 0.5,
            ];
            crate::draw::DrawList::image(list, self.atlas, region, centre, [img_w, img_h], 0.0, colour, blend);
            cx += widths[i] + self.spacing * scale;
        }
        total
    }

    /// Draw right-aligned so the ink right edge is at `right_x`, ink
    /// vertically centred at `cy`. `scale` shrinks a glyph (accuracy
    /// decimals). Returns the ink width.
    fn draw_right(
        &self,
        list: &mut DrawList,
        text: &str,
        right_x: f32,
        cy: f32,
        scale: f32,
        colour: Colour,
        blend: Blend,
    ) -> f32 {
        // Total advance = sum of ink widths + spacing. Digits are monospaced
        // to '5' (lazer's FixedWidthReferenceCharacter), so the run keeps a
        // fixed width as digits roll and lines up with the wireframes.
        let mut widths = Vec::with_capacity(text.len());
        let mut total = 0.0f32;
        for c in text.chars() {
            let w = self.ink_w(c, scale);
            widths.push(w);
            total += w + self.spacing * scale;
        }
        total -= self.spacing * scale;

        let mut cx = right_x - total;
        for (i, c) in text.chars().enumerate() {
            let w = self.place(list, c, cx + widths[i] * 0.5, cy, self.digit_h, scale, colour, blend);
            let _ = w;
            cx += widths[i] + self.spacing * scale;
        }
        total
    }
}

pub struct HudState {
    score: Rolling,
    acc: Rolling,
    combo_display: f64,
    combo_scale_anim: Option<(f64, f64, f64, f64, Easing)>,
    /// Live combo scale, advanced every frame.
    combo_scale_now: f64,
    last_combo: i32,
    last_combo_time: f64,
    was_miss: bool,
    health_flash: Option<f64>,
    last_health: f64,
    classic_score: bool,
    /// UR bar (`BarHitErrorMeter` port): time of the first timed hit (starts
    /// the axis growth / marker / arrow appear animations).
    ur_first_t: Option<f64>,
    /// Exponential moving average of hit offsets (`floatingAverage`, 0.9/0.1).
    ur_ema: f64,
    /// Arrow slide animation (start, from ms, to ms), 800ms OutQuint.
    ur_arrow_anim: Option<(f64, f64, f64)>,
    /// Number of ur_events consumed so far.
    ur_processed: usize,
    /// Whether the UR bar's window guide lines (colour axis) render.
    pub ur_guides: bool,
}

impl HudState {
    pub fn new() -> HudState {
        HudState {
            score: Rolling::new(),
            acc: Rolling::new(),
            combo_display: 0.0,
            combo_scale_anim: None,
            combo_scale_now: 1.0,
            last_combo: 0,
            last_combo_time: f64::NEG_INFINITY,
            was_miss: false,
            health_flash: None,
            last_health: 1.0,
            classic_score: false,
            ur_first_t: None,
            ur_ema: 0.0,
            ur_arrow_anim: None,
            ur_processed: 0,
            ur_guides: true,
        }
    }

    pub fn use_classic_score(&mut self) {
        self.classic_score = true;
    }

    pub fn draw(
        &mut self,
        game: &GameData,
        assets: &Assets,
        list: &mut DrawList,
        m: &Mapper,
        t: f64,
    ) {
        // Latest score state at/before t (full judgement timeline).
        let mut score = 0i64;
        let mut combo = 0i32;
        let mut accuracy = 1.0f64;
        for ev in &game.score_events {
            if ev.time > t {
                break;
            }
            score = if self.classic_score { ev.classic_score } else { ev.score };
            combo = ev.combo;
            accuracy = ev.accuracy;
        }

        // --- Score counter --------------------------------------------------
        self.score.set(score as f64, t);
        self.score.update(t);

        // Wedge pieces (380x72, shear 0.8, #66CCFF 0->0.25 gradient, two
        // pieces offset by (4,5), positioned at (-50,15) virtual).
        draw_wedge(list, m, [-50.0, 15.0]);
        draw_wedge(list, m, [-50.0 + 4.0, 15.0 + 5.0]);

        // Score digits right edge at virtual (250, 55): centre y = 55 + h/2.
        let cd = CounterDraw { atlas: assets.atlas, digit_h: 36.0 * m.virt, spacing: -2.0 * m.virt };
        let right = m.virt([250.0, 0.0])[0];
        let cy = m.virt([0.0, 55.0 + 20.0])[1];
        let score_text = format!("{}", self.score.display.round() as i64);
        // Wireframe background: fixed digit count.
        let wire_digits = (game.final_score.max(game.final_classic_score)).to_string().len().max(8);
        draw_wireframe_run(list, assets.atlas, right, cy, wire_digits, m.virt);
        cd.draw_right(list, &score_text, right, cy, 1.0, Colour::WHITE, Blend::Alpha);

        // --- Accuracy counter --------------------------------------------------
        // Exact ArgonAccuracyCounter layout: a horizontal FillFlow of
        // [whole (full)], [".##" scale 0.5, margin-top 4], ["%" (full)],
        // all TOP-aligned; anchored TopRight at virtual (1024-20, 20).
        self.acc.set(accuracy * 100.0, t);
        self.acc.update(t);
        let acc_cd = CounterDraw { atlas: assets.atlas, digit_h: 36.0 * m.virt, spacing: -2.0 * m.virt };
        let acc_right = m.virt([1024.0 - 20.0, 0.0])[0];
        let acc_top = m.virt([0.0, 20.0])[1];
        // Component-local margins (fraction margin-top 4) scale with the
        // digit calibration (36 local units = digit_h).
        let unit = acc_cd.digit_h / 36.0;

        let acc_val = self.acc.display;
        let whole = acc_val.trunc();
        let frac = ((acc_val - whole) * 100.0).round();
        let whole_s = format!("{}", whole as i64);
        let frac_s = format!(".{:02}", frac as i64);

        // Widths, then place left-to-right ending at acc_right.
        let w_pct: f32 = "%".chars().map(|c| acc_cd.ink_w(c, 1.0) - acc_cd.spacing).sum::<f32>() + acc_cd.spacing;
        let w_frac: f32 = frac_s.chars().map(|c| acc_cd.ink_w(c, 0.5) + acc_cd.spacing * 0.5).sum::<f32>() - acc_cd.spacing * 0.5;

        let pct_right = acc_right;
        let frac_right = pct_right - w_pct;
        let whole_right = frac_right - w_frac;

        acc_cd.draw_top(list, "%", pct_right, acc_top, 1.0, Colour::WHITE, Blend::Alpha);
        acc_cd.draw_top(list, &frac_s, frac_right, acc_top + 4.0 * unit, 0.5, Colour::WHITE, Blend::Alpha);
        acc_cd.draw_top(list, &whole_s, whole_right, acc_top, 1.0, Colour::WHITE, Blend::Alpha);

        // --- Combo counter (bottom-left, scale 1.3) --------------------------------
        // ArgonComboCounter: newScale = clamp(current * (increase ? 1.1 :
        // 0.8), 0.6, 1.4), then ScaleTo(1, 500/2000, OutQuint). `current` is
        // the LIVE scale at the moment of the change.
        if combo != self.last_combo {
            let increase = combo > self.last_combo;
            let was_miss = self.last_combo > 1 && combo == 0;
            let new_scale = (self.combo_scale_now * if increase { 1.1 } else { 0.8 }).clamp(0.6, 1.4);
            let dur = if was_miss { 2000.0 } else { 500.0 };
            self.combo_scale_anim = Some((t, t + dur, new_scale, 1.0, Easing::OutQuint));
            self.was_miss = was_miss;
            self.last_combo = combo;
            self.last_combo_time = t;
        }
        let combo_scale = match self.combo_scale_anim {
            Some((a, b, from, to, e)) => value_at(t, a, b, from, to, e),
            None => 1.0,
        };
        self.combo_scale_now = combo_scale;
        // Combo number roll: instant is fine (lazer rolls 250ms too).
        self.combo_display = lerp_to(self.combo_display, combo as f64, 0.3);

        if combo > 0 {
            let combo_cd = CounterDraw { atlas: assets.atlas, digit_h: 25.0 * 1.3 * m.virt, spacing: -2.0 * 1.3 * m.virt };
            let base = m.virt([36.0, 768.0 - 66.0]);
            let cy = base[1] - 26.0 * 1.3 * m.virt;
            let text = format!("{}x", self.combo_display.round() as i64);
            let flash = self.was_miss && t < self.last_combo_time + 800.0;
            let col = if flash {
                let f = value_at(t, self.last_combo_time, self.last_combo_time + 800.0, 1.0, 0.0, Easing::OutQuint) as f32;
                Colour::lerp(Colour::WHITE, Colour::from_hex(0xFF0000), f)
            } else {
                Colour::WHITE
            };
            // Left-anchored: measure with the same monospaced slot widths
            // that draw_right places glyphs with.
            let mut width = 0.0f32;
            for c in text.chars() {
                width += combo_cd.ink_w(c, combo_scale as f32) + combo_cd.spacing;
            }
            width -= combo_cd.spacing;
            combo_cd.draw_right(list, &text, base[0] + width, cy, combo_scale as f32, col, Blend::Alpha);
        }

        // --- Health bar ------------------------------------------------------------
        let health = health_at(game, t);
        if health < self.last_health - 1e-6 {
            self.health_flash = Some(t);
        }
        self.last_health = health;
        draw_health(list, m, health, t, self.health_flash);

        // --- Unstable rate bar (skin style, bottom centre) ------------------------
        self.draw_ur_bar(game, assets, list, m, t);
    }

    /// Skin-style unstable-rate bar, horizontal at the bottom centre of the
    /// screen, drawn per lazer's `BarHitErrorMeter` (rotated 90 degrees):
    /// a judgement-coloured window axis with the outermost (meh) band fading
    /// out, additive judgement line ticks (0.6 alpha, 100ms pop-in, 5s fade
    /// while shrinking), a Great-coloured centre circle marker and the
    /// moving-average chevron arrow (EMA 0.9/0.1, 800ms OutQuint slides).
    fn draw_ur_bar(&mut self, game: &GameData, assets: &Assets, list: &mut DrawList, m: &Mapper, t: f64) {
        let n = game.ur_events.partition_point(|e| e.time <= t);
        if n == 0 {
            return; // no timed hits yet
        }
        let last = game.ur_events[n - 1];
        let (great, ok, meh) = game.hit_windows;
        let meh = meh.max(1.0);

        // Consume new judgements: update the floating average and retarget
        // the arrow (`OnNewJudgement`: arrow.MoveToY(..., 800, OutQuint)).
        if n > self.ur_processed {
            for e in &game.ur_events[self.ur_processed..n] {
                self.ur_ema = self.ur_ema * 0.9 + e.offset * 0.1;
            }
            let newest = game.ur_events[n - 1].time;
            let from = self.ur_arrow_ms_at(newest);
            self.ur_arrow_anim = Some((newest, from, self.ur_ema));
            self.ur_processed = n;
            if self.ur_first_t.is_none() {
                self.ur_first_t = Some(game.ur_events[0].time);
            }
        }
        let ft = self.ur_first_t.unwrap_or(t);

        // Virtual-space layout (lazer HUD units: strip 14, spine 2, chevron
        // 8, centre marker 8, tick thickness 4, half width spans the meh
        // window).
        let centre = m.virt([512.0, 736.0]);
        let cy = centre[1];
        let half_w = 230.0 * m.virt;
        let px = |ms: f64, scale: f32| -> f32 { (ms / meh).clamp(-1.0, 1.0) as f32 * half_w * scale };

        // Axis growth (ResizeHeightTo(1, 800, OutQuint) from the first hit)
        // and fade-in (FadeTo(1, 500, OutQuint)).
        let grow = value_at(t, ft, ft + 800.0, 0.0, 1.0, Easing::OutQuint) as f32;
        let axis_a = value_at(t, ft, ft + 500.0, 0.0, 1.0, Easing::OutQuint) as f32;
        let spine_r = 1.0 * m.virt; // bar_width 2
        fn band(list: &mut DrawList, cx: f32, cy: f32, x0: f32, x1: f32, r: f32, col: Colour) {
            list.capsule(
                [cx + x0, cy],
                [cx + x1, cy],
                r,
                col,
                Blend::Alpha,
            );
        }

        // Colour axis per side: Great band at the centre, then Ok, then Meh
        // (solid 80% + gradient to transparent for the outer fifth -
        // `createColourBar` requireGradient).
        let col_great = colour_for_result(osu_replay_judge::score::HitResult::Great);
        let col_ok = colour_for_result(osu_replay_judge::score::HitResult::Ok);
        let col_meh = colour_for_result(osu_replay_judge::score::HitResult::Meh);

        // Colour axis per side: Great band at the centre, then Ok, then Meh
        // (solid 80% + gradient to transparent for the outer fifth -
        // `createColourBar` requireGradient). Skipped entirely with
        // `--no-guides`.
        if self.ur_guides {
            for side in [-1.0f32, 1.0] {
                let (g, o, mm) = (
                    side * px(great, grow),
                    side * px(ok, grow),
                    side * px(meh, grow),
                );
                band(list, centre[0], cy, 0.0, g, spine_r, col_great.opacity(axis_a));
                band(list, centre[0], cy, g, o, spine_r, col_ok.opacity(axis_a));
                // meh band: solid part then fading tail.
                let split = o + (mm - o) * 0.8;
                band(list, centre[0], cy, o, split, spine_r, col_meh.opacity(axis_a));
                let fade_a = col_meh.opacity(axis_a);
                let fade_b = col_meh.opacity(0.0);
                let y0 = cy - spine_r;
                let y1 = cy + spine_r;
                let pts = if side < 0.0 {
                    [[centre[0] + mm, y0], [centre[0] + split, y0], [centre[0] + split, y1], [centre[0] + mm, y1]]
                } else {
                    [[centre[0] + split, y0], [centre[0] + mm, y0], [centre[0] + mm, y1], [centre[0] + split, y1]]
                };
                list.quad_gradient(&pts, [fade_a, fade_a, fade_b, fade_b], Blend::Alpha);
            }
        }

        // Centre marker (Circle style): Great-coloured disc behind the ticks,
        // darkened half-size disc in front; pops in with an elastic scale.
        let marker_a = value_at(t, ft, ft + 500.0, 0.0, 1.0, Easing::OutQuint) as f32;
        let marker_s = value_at(t, ft, ft + 1000.0, 0.0, 1.0, Easing::OutElasticHalf) as f32;
        let outer_r = 4.0 * m.virt * marker_s; // centre_marker_size 8
        if marker_a > 0.003 && outer_r > 0.1 {
            list.disc(centre, outer_r, col_great.opacity(marker_a), col_great.opacity(marker_a), Blend::Alpha);
        }

        // Judgement line ticks (`JudgementLine`): additive, judgement colour,
        // fade to 0.6 over 100ms then out over 5000ms while shrinking across.
        let start = n.saturating_sub(50); // max_concurrent_judgements
        for e in &game.ur_events[start..n] {
            let x = t - e.time;
            if x < 0.0 || x > 5100.0 {
                continue;
            }
            let (a, wf) = if x < 100.0 {
                (0.6 * value_at(x, 0.0, 100.0, 0.0, 1.0, Easing::OutQuint) as f32,
                 value_at(x, 0.0, 100.0, 0.0, 1.0, Easing::OutQuint) as f32)
            } else {
                (0.6 * value_at(x, 100.0, 5100.0, 1.0, 0.0, Easing::Linear) as f32,
                 value_at(x, 100.0, 5100.0, 1.0, 0.0, Easing::InQuint) as f32)
            };
            if a <= 0.004 {
                continue;
            }
            let tx = centre[0] + px(e.offset, 1.0);
            let half_len = 7.0 * m.virt * wf.max(0.0); // judgement_line_width 14
            let tick_r = 2.0 * m.virt; // JudgementLineThickness 4
            list.capsule(
                [tx, cy - half_len],
                [tx, cy + half_len],
                tick_r,
                colour_for_result(e.result).opacity(a),
                Blend::Additive,
            );
        }

        // Centre marker front disc (Depth.MinValue - over the ticks).
        if marker_a > 0.003 && outer_r > 0.1 {
            list.disc(centre, outer_r * 0.5, col_great.darken(0.3).opacity(marker_a), col_great.darken(0.3).opacity(marker_a), Blend::Alpha);
        }

        // Moving-average chevron arrow (`arrowContainer`: delayed 450ms,
        // fades in 250ms; slides 800ms OutQuint to each new EMA).
        let arrow_a = value_at(t, ft + 450.0, ft + 700.0, 0.0, 1.0, Easing::OutQuint) as f32;
        if arrow_a > 0.003 {
            let ms = self.ur_arrow_ms_at(t);
            let ax = centre[0] + px(ms, 1.0);
            let ay = cy + 13.0 * m.virt; // below the strip, pointing up at it
            draw_chevron(
                list,
                [ax, ay],
                -90.0,
                8.0 * m.virt,
                1.6 * m.virt,
                Colour::WHITE.opacity(arrow_a),
                Colour::WHITE.opacity(arrow_a),
                Blend::Alpha,
            );
        }

        // Early/late labels at the ends (`recreateLabels`, text style).
        let label_a = value_at(t, ft, ft + 500.0, 0.0, 1.0, Easing::Linear) as f32 * 0.5;
        if label_a > 0.004 {
            let ey = cy;
            draw_ttf_text(list, assets.atlas, assets.semibold, false, "EARLY", [centre[0] - half_w - 30.0 * m.virt, ey], 10.0 * m.virt, Colour::WHITE.opacity(label_a), 1.0 * m.virt, Blend::Alpha);
            draw_ttf_text(list, assets.atlas, assets.semibold, false, "LATE", [centre[0] + half_w + 24.0 * m.virt, ey], 10.0 * m.virt, Colour::WHITE.opacity(label_a), 1.0 * m.virt, Blend::Alpha);
        }

        // Live UR value above the bar.
        let text = format!("UR {}", last.ur.round() as i64);
        draw_ttf_text(
            list,
            assets.atlas,
            assets.semibold,
            false,
            &text,
            [centre[0], cy - 26.0 * m.virt],
            22.0 * m.virt,
            Colour::WHITE.opacity(0.95),
            2.0 * m.virt,
            Blend::Alpha,
        );
    }

    /// Displayed arrow position (ms offset) at time `t`.
    fn ur_arrow_ms_at(&self, t: f64) -> f64 {
        match self.ur_arrow_anim {
            Some((a, from, to)) => value_at(t, a, a + 800.0, from, to, Easing::OutQuint),
            None => 0.0,
        }
    }
}

fn lerp_to(current: f64, target: f64, factor: f64) -> f64 {
    current + (target - current) * factor
}

/// The wireframe segments behind score digits (ink-aligned).
fn draw_wireframe_run(
    list: &mut DrawList,
    atlas: &crate::draw::Atlas,
    right: f32,
    cy: f32,
    digits: usize,
    virt: f32,
) {
    // Slot width = digit '5' ink width (the framework monospaces both the
    // wireframe and digits to the same reference), so the digits land
    // exactly inside the wireframe boxes.
    let digit5 = atlas.ink(Region::CounterDigit(b'5'));
    let d_ink_w = (digit5[2] - digit5[0]).max(1.0);
    let d_ink_h = (digit5[3] - digit5[1]).max(1.0);
    let h = 36.0 * virt;
    let w = d_ink_w * (h / d_ink_h);
    let region = Region::CounterWireframes;
    let rect = atlas.region_rect(region);
    let ink = atlas.ink(region);
    let ink_h = (ink[3] - ink[1]).max(1.0);
    let s = h / ink_h;
    let spacing = -2.0 * virt;
    let img_w = (rect.x1 - rect.x0) * s;
    let img_h = (rect.y1 - rect.y0) * s;
    let ink_cx = (ink[0] + ink[2]) * 0.5 * s;
    let ink_cy = (ink[1] + ink[3]) * 0.5 * s;
    let mut x = right;
    for _ in 0..digits {
        let cx = x - w * 0.5;
        let centre = [cx - ink_cx + img_w * 0.5, cy - ink_cy + img_h * 0.5];
        crate::draw::DrawList::image(
            list,
            atlas,
            region,
            centre,
            [img_w, img_h],
            0.0,
            Colour::WHITE.opacity(0.25),
            Blend::Alpha,
        );
        x -= w + spacing;
    }
}

fn draw_wedge(list: &mut DrawList, m: &Mapper, top_left_virtual: [f32; 2]) {
    let w = 380.0;
    let h = 72.0;
    let shear = 0.8;
    let v = |x: f32, y: f32| -> [f32; 2] { m.virt([top_left_virtual[0] + x + shear * y, top_left_virtual[1] + y]) };
    let pts = [v(0.0, 0.0), v(w, 0.0), v(w, h), v(0.0, h)];

    let top = Colour::from_hex(WEDGE_COLOUR).opacity(0.0);
    let bottom = Colour::from_hex(WEDGE_COLOUR).opacity(0.25);
    list.quad_gradient(&pts, [top, top, bottom, bottom], Blend::Alpha);
}

fn draw_health(list: &mut DrawList, m: &Mapper, health: f64, t: f64, flash: Option<f64>) {
    // Position: TopLeft (50, 20), width 300, bar height 30 (virtual).
    let left = m.virt([50.0, 20.0 + 10.0]);
    let right = m.virt([50.0 + 300.0, 20.0 + 10.0]);
    let radius = 10.0 * m.virt;

    // Glow bar (additive, slightly ahead of health).
    let glow_col = Colour::rgba_bytes(HEALTH_GLOW[0], HEALTH_GLOW[1], HEALTH_GLOW[2], 110);
    let glow_extent = (health + 0.02).clamp(0.0, 1.0) as f32;
    let glow_right = [left[0] + (right[0] - left[0]) * glow_extent, left[1]];
    list.capsule(left, glow_right, radius * 1.6, glow_col.opacity(0.35), Blend::Additive);

    // Main bar (white, additive).
    let main_right = [left[0] + (right[0] - left[0]) * health as f32, left[1]];
    list.capsule(left, main_right, radius, Colour::WHITE.opacity(0.9), Blend::Additive);
    list.capsule(left, main_right, radius * 0.5, Colour::WHITE, Blend::Additive);

    // Damage flash: red tint fading out.
    if let Some(ft) = flash {
        let x = t - ft;
        if x < 1100.0 {
            let a = value_at(x, 0.0, 1100.0, 1.0, 0.0, Easing::OutQuint) as f32;
            let red = Colour::from_hex(0xFF6060).opacity(a * 0.6);
            list.capsule(left, right, radius, red, Blend::Additive);
        }
    }
}
