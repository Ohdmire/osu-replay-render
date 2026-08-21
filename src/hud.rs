//! Argon HUD: wedge pieces, score/accuracy/combo counters (argon-counter
//! texture digits with wireframes), health bar, and rolling counter logic.

use crate::draw::{draw_ttf_text, ttf_measure, value_at, Blend, Colour, DrawList, Easing, Region};
use crate::game::{health_at, key_counts_at, key_state_at, GameData, KEY_ACTIONS};
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
///
/// Lazer's `ArgonCounterSpriteText` advances each glyph by its FULL
/// TEXTURE width minus 2 (`Spacing = (-2, 0)`) - the textures carry their
/// own side padding (digits: a 240-unit box with ~31-unit margins around a
/// 178-unit ink), which is what produces the airy digit spacing. Digits
/// are monospaced to '5' (`FixedWidthReferenceCharacter`) and centred in
/// that slot; the dot's ink sits near the box bottom, so baseline
/// alignment falls out of the box layout. This port lays out the same
/// way: slots are texture-relative, NOT ink-relative.
struct CounterDraw<'a> {
    atlas: &'a crate::draw::Atlas,
    /// Digit INK height in px (the visual digit height).
    digit_h: f32,
}

/// Argon counter digit metrics, in texture pixels (all textures are
/// TEX_BOX tall; digits and wireframes share the 240-wide slot box).
const TEX_BOX: f32 = 240.0;
const DIGIT_INK: f32 = 178.0;
/// `ArgonCounterSpriteText.Spacing = (-2, 0)`.
const COUNTER_SPACING: f32 = -2.0;

impl<'a> CounterDraw<'a> {
    fn region_for(c: char) -> Region {
        match c {
            '.' => Region::CounterDot,
            '%' => Region::CounterPercent,
            'x' | 'X' => Region::CounterX,
            _ => Region::CounterDigit(c as u8),
        }
    }

    /// Scale from texture px to screen px.
    fn k(&self) -> f32 {
        self.digit_h / DIGIT_INK
    }

    fn tex_w(&self, c: char) -> f32 {
        let r = self.atlas.region_rect(Self::region_for(c));
        r.x1 - r.x0
    }

    /// Layout slot for a char at `scale`: texture width - 2, digits
    /// monospaced to the '5' texture.
    fn slot_w(&self, c: char, scale: f32) -> f32 {
        let base = if c.is_ascii_digit() { self.tex_w('5') } else { self.tex_w(c) };
        (base + COUNTER_SPACING) * self.k() * scale
    }

    fn run_width(&self, text: &str, scale: f32) -> f32 {
        text.chars().map(|c| self.slot_w(c, scale)).sum()
    }

    /// Draws one glyph's full texture with its TOP-LEFT at (pen_x, top_y)
    /// (`scale` shrinks the glyph, e.g. accuracy decimals). Digits are
    /// centred in their monospaced slot. Returns the slot width.
    fn place_top(
        &self,
        list: &mut DrawList,
        region: Region,
        pen_x: f32,
        top_y: f32,
        scale: f32,
        colour: Colour,
        blend: Blend,
        centre_in_slot: bool,
    ) -> f32 {
        let rect = self.atlas.region_rect(region);
        let k = self.k() * scale;
        let tw = (rect.x1 - rect.x0) * k;
        let th = (rect.y1 - rect.y0) * k;
        let slot = if centre_in_slot {
            (self.tex_w('5') + COUNTER_SPACING) * self.k() * scale
        } else {
            tw - COUNTER_SPACING * self.k() * scale
        };
        let x = if centre_in_slot { pen_x + (slot - tw) * 0.5 } else { pen_x };
        let centre = [x + tw * 0.5, top_y + th * 0.5];
        crate::draw::DrawList::image(list, self.atlas, region, centre, [tw, th], 0.0, colour, blend);
        slot
    }

    /// Draw right-aligned with the run's slot box ending at `right_x`,
    /// texture TOP edges at `top_y` (FillFlow top-aligned components,
    /// like ArgonAccuracyCounter). Returns the total width.
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
        let total = self.run_width(text, scale);
        let mut pen = right_x - total;
        for c in text.chars() {
            let region = Self::region_for(c);
            let is_digit = c.is_ascii_digit();
            pen += self.place_top(list, region, pen, top_y, scale, colour, blend, is_digit);
        }
        total
    }

    /// Draw right-aligned so the run's slot box ends at `right_x`, texture
    /// box vertically centred at `cy` (score / combo counters). Returns
    /// the total width.
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
        let top_y = cy - self.k() * scale * TEX_BOX * 0.5;
        self.draw_top(list, text, right_x, top_y, scale, colour, blend)
    }
}

/// Per-key overlay animation state: tracks press/release edges so the
/// indicator slide and name-colour fades can run from the right moment.
struct KeyAnim {
    pressed: bool,
    press_t: f64,
    release_t: f64,
}

impl KeyAnim {
    fn new() -> KeyAnim {
        // Finite far-past: value_at clamps to the eased end value.
        KeyAnim { pressed: false, press_t: -1e12, release_t: -1e12 }
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
    /// Whether the whole UR bar (ticks/marker/arrow/number) renders.
    pub ur_bar: bool,
    /// Whether the UR bar's window guide lines (colour axis) render
    /// (only visible when `ur_bar` is on).
    pub ur_guides: bool,
    /// Key overlay (Z/X/C tap display, lazer `ArgonKeyCounterDisplay`).
    pub key_overlay: bool,
    /// Press/release animation state per key (order matches KEY_ACTIONS).
    keys: [KeyAnim; 3],
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
            ur_bar: true,
            ur_guides: true,
            key_overlay: true,
            keys: [KeyAnim::new(), KeyAnim::new(), KeyAnim::new()],
        }
    }

    pub fn use_classic_score(&mut self) {
        self.classic_score = true;
    }

    /// 实时预览用:双向切换经典分/standardised 计分显示。
    pub fn set_classic_score(&mut self, enabled: bool) {
        self.classic_score = enabled;
    }

    pub fn draw(
        &mut self,
        game: &GameData,
        assets: &Assets,
        list: &mut DrawList,
        m: &Mapper,
        t: f64,
    ) {
        // Autoplay (beatmap preview): score/accuracy/combo, the UR bar and
        // the health bar are all perfect by construction and say nothing
        // about the beatmap — hide the entire HUD.
        if game.autoplay {
            return;
        }

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
        let cd = CounterDraw { atlas: assets.atlas, digit_h: 36.0 * m.virt };
        // Lazer aligns the glyph BOX right edge at virtual x=250
        // (`score.Position = (components_x_offset + 200, ...)` with Origin
        // TopRight); the digit textures carry a ~32/240 right margin, so
        // the visible ink edge reads ~6 units left of it. Nudge the whole
        // assembly (wireframes included) right by that margin so the ink
        // right edge lands on 250.
        let right = m.virt([250.0, 0.0])[0] + 32.0 * cd.k();
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
        let acc_cd = CounterDraw { atlas: assets.atlas, digit_h: 36.0 * m.virt };
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

        // Widths (texture-slot based), then place left-to-right ending at
        // acc_right.
        let w_pct = acc_cd.run_width("%", 1.0);
        let w_frac = acc_cd.run_width(&frac_s, 0.5);

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
            let combo_cd = CounterDraw { atlas: assets.atlas, digit_h: 25.0 * 1.3 * m.virt };
            let base = m.virt([36.0, 768.0 - 66.0]);
            // ArgonSkin.cs: combo 组件 BottomLeft + Position(36, -66),数字贴
            // 组件盒底(240 贴图盒的 ink 底边距 ~31/240,scale 1.3 后数字 ink
            // 底 ≈ -66 线上方 ~7 单位) —— 即与 key overlay 数字同一水平线。
            let cy = base[1] - 18.0 * 1.3 * m.virt;
            let text = format!("{}x", self.combo_display.round() as i64);
            let flash = self.was_miss && t < self.last_combo_time + 800.0;
            let col = if flash {
                let f = value_at(t, self.last_combo_time, self.last_combo_time + 800.0, 1.0, 0.0, Easing::OutQuint) as f32;
                Colour::lerp(Colour::WHITE, Colour::from_hex(0xFF0000), f)
            } else {
                Colour::WHITE
            };
            // Left-anchored: measure with the same slot widths draw_right
            // places glyphs with.
            let width = combo_cd.run_width(&text, combo_scale as f32);
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
        if self.ur_bar {
            self.draw_ur_bar(game, assets, list, m, t);
        }

        // --- Key overlay (Z/X/C tap display) ---------------------------------------
        if self.key_overlay && std::env::var("NO_KEYS").is_err() {
            self.draw_key_overlay(game, assets, list, m, t);
        }
    }

    /// `ArgonKeyCounterDisplay` port: a horizontal row of key counters at
    /// the bottom-right of the screen, laid out per `ArgonSkin.cs` (Argon-Pro
    /// inherits it): BottomRight anchor, Position (-36, -66) — right margin
    /// 36 (hit_error_offset_width 26 + padding 10), bottom edge 66 above the
    /// screen bottom (padding*2 + song progress height), i.e. the SAME
    /// horizontal line as the combo counter (also -66).
    /// Each `ArgonKeyCounter` box is 52.5 x 45 (35/30 Figma units * the 1.5 eyeballed scale factor) showing
    /// the key letter (Torus Bold 15, `OsuColour.Blue0`), the cumulative
    /// press count (Torus Bold 21) and a top indicator line (4.5 tall, alpha
    /// 0.5 idle) that brightens over 10ms and slides down 4 units over 60ms
    /// OutQuint while held, easing back over 250ms OutQuart on release.
    fn draw_key_overlay(&mut self, game: &GameData, assets: &Assets, list: &mut DrawList, m: &Mapper, t: f64) {
        const COUNTER_W: f32 = 52.5;
        const COUNTER_H: f32 = 45.0;
        const SPACING: f32 = 2.0;
        const LINE_H: f32 = 4.5;
        const PRESS_OFFSET: f32 = 4.0;
        const NAME_SIZE: f32 = 15.0;
        const COUNT_SIZE: f32 = 21.0;

        let state = key_state_at(game, t);
        let counts = key_counts_at(game, t);
        let blue0 = Colour::from_hex(0x99DDFF);

        let total_w = 3.0 * COUNTER_W + 2.0 * SPACING;
        // ArgonSkin.cs 布局: BottomRight + Position(-(hit_error_offset_width
        // + padding), -(padding*2 + song_progress_offset_height)) = (-36, -66)
        // —— 右边距 36,底边与 combo 同一水平线(combo 也是 -66)。
        let x1 = m.virt([1024.0 - 36.0, 0.0])[0];
        let y1 = m.virt([0.0, 768.0 - 66.0])[1];
        let x0 = x1 - total_w * m.virt;
        let y0 = y1 - COUNTER_H * m.virt;

        for k in 0..3 {
            let anim = &mut self.keys[k];
            if state[k] != anim.pressed {
                if state[k] {
                    anim.press_t = t;
                } else {
                    anim.release_t = t;
                }
                anim.pressed = state[k];
            }

            let cx0 = x0 + (k as f32 * (COUNTER_W + SPACING)) * m.virt;

            // Indicator line: brighten + slide down while held, ease back
            // on release.
            let (alpha, y_off) = if anim.pressed {
                (
                    value_at(t, anim.press_t, anim.press_t + 10.0, 0.5, 1.0, Easing::OutQuint),
                    value_at(t, anim.press_t, anim.press_t + 60.0, 0.0, 1.0, Easing::OutQuint),
                )
            } else {
                (
                    value_at(t, anim.release_t, anim.release_t + 250.0, 1.0, 0.5, Easing::OutQuart),
                    value_at(t, anim.release_t, anim.release_t + 250.0, 1.0, 0.0, Easing::OutQuart),
                )
            };
            let r = LINE_H * 0.5 * m.virt;
            let cy = y0 + r + y_off as f32 * m.virt;
            list.capsule(
                [cx0 + r, cy],
                [cx0 + COUNTER_W * m.virt - r, cy],
                r,
                Colour::WHITE.opacity(alpha as f32),
                Blend::Alpha,
            );

            // Key name: Blue0 -> white over 10ms on press, back over 200ms.
            let f = if anim.pressed {
                value_at(t, anim.press_t, anim.press_t + 10.0, 0.0, 1.0, Easing::OutQuint)
            } else {
                value_at(t, anim.release_t, anim.release_t + 200.0, 1.0, 0.0, Easing::OutQuart)
            };
            let col = Colour::lerp(blue0, Colour::WHITE, f as f32);
            let size = NAME_SIZE * m.virt;
            let (w, top, bottom) = ttf_measure(assets.bold, KEY_ACTIONS[k], size, 0.0);
            let ink_top = y0 + (LINE_H + PRESS_OFFSET) * m.virt;
            draw_ttf_text(
                list,
                assets.atlas,
                assets.bold,
                true,
                KEY_ACTIONS[k],
                [cx0 + w * 0.5, ink_top + (bottom - top) * 0.5],
                size,
                col,
                0.0,
                Blend::Alpha,
            );

            // Cumulative press count, bottom-left of the box: lazer anchors
            // the TEXT LAYOUT bottom at the box bottom (countText
            // BottomLeft), so the digit ink sits a font-descent (~5.5 units
            // at Torus 21) above it — the same line the combo digits end on.
            let text = format!("{}", counts[k]);
            let size = COUNT_SIZE * m.virt;
            let (w, top, bottom) = ttf_measure(assets.bold, &text, size, 0.0);
            let ink_bottom = y0 + COUNTER_H * m.virt - 5.5 * m.virt;
            draw_ttf_text(
                list,
                assets.atlas,
                assets.bold,
                true,
                &text,
                [cx0 + w * 0.5, ink_bottom - (bottom - top) * 0.5],
                size,
                Colour::WHITE,
                0.0,
                Blend::Alpha,
            );
        }
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
                8.0 * 0.094 * m.virt, // FA ChevronRight stroke: 48/512 of the box
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

/// The wireframe segments behind the score digits, laid out with the same
/// texture-slot widths as the digits themselves (the wireframe texture
/// shares the digits' 240-unit box, so slots line up exactly).
fn draw_wireframe_run(
    list: &mut DrawList,
    atlas: &crate::draw::Atlas,
    right: f32,
    cy: f32,
    digits: usize,
    virt: f32,
) {
    let cd = CounterDraw { atlas, digit_h: 36.0 * virt };
    let top_y = cy - cd.k() * TEX_BOX * 0.5;
    let slot = cd.slot_w('5', 1.0);
    let mut pen = right - slot * digits as f32;
    for _ in 0..digits {
        pen += cd.place_top(
            list,
            Region::CounterWireframes,
            pen,
            top_y,
            1.0,
            Colour::WHITE.opacity(0.25),
            Blend::Alpha,
            true,
        );
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
