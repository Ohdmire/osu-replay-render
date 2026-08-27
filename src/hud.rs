//! Argon HUD: wedge pieces, score/accuracy/combo counters (argon-counter
//! texture digits with wireframes), health bar, and rolling counter logic.
//!
//! When a user legacy skin is active (`--skin <dir>`), the four main HUD
//! pieces switch to the skin's own textures, ported from lazer's
//! `LegacyScoreCounter` / `LegacyAccuracyCounter` /
//! `LegacyDefaultComboCounter` / `LegacyHealthDisplay` /
//! `LegacyKeyCounterDisplay` (see `LegacyHud` below). The UR bar has no
//! legacy equivalent and always uses the argon implementation.

use crate::draw::{draw_ttf_text, ttf_measure, value_at, Atlas, Blend, Colour, DrawList, Easing, Region};
use crate::game::{health_at, key_counts_at, key_state_at, GameData, KEY_ACTIONS};
use crate::scene::{colour_for_result, draw_chevron, Assets, Mapper};
use crate::skin::{texture::LegacyFont, Skin, SkinTexture};

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
    /// Live PP counter (`ArgonPerformancePointsCounter`: int, 250ms roll).
    pp: Rolling,
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
    /// Force the Argon HUD even with a user legacy skin
    /// (`--argon-hud`; the legacy HUD is the default when a skin
    /// provides its pieces).
    pub argon_hud: bool,
    /// Resolved legacy-skin HUD pieces (built on first use while a
    /// legacy skin is active).
    legacy: Option<LegacyHud>,
    /// Legacy counters (proportional rolls) + combo/health/key anim state.
    l_score: PropRoll,
    l_acc: PropRoll,
    l_combo: LegacyCombo,
    l_health: LegacyHealth,
    l_keys: [LegacyKeyAnim; 3],
    /// Digit run height (HUD units) of the last legacy score draw - the
    /// accuracy counter sits below it (`LegacySkin`'s MainHUD container
    /// aligns the two).
    l_score_h: f32,
    /// Digit run height of the last legacy accuracy draw - the PP counter
    /// hangs below it.
    l_acc_h: f32,
}

impl HudState {
    pub fn new() -> HudState {
        HudState {
            score: Rolling::new(),
            acc: Rolling::new(),
            pp: Rolling::new(),
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
            argon_hud: false,
            legacy: None,
            l_score: PropRoll::new(0.0),
            l_acc: PropRoll::new(1.0),
            l_combo: LegacyCombo::new(),
            l_health: LegacyHealth::new(),
            l_keys: [LegacyKeyAnim::new(), LegacyKeyAnim::new(), LegacyKeyAnim::new()],
            l_score_h: 0.0,
            l_acc_h: 0.0,
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

        // Legacy-skin HUD pieces, built once per render (missing pieces
        // keep their argon fallbacks per element).
        let use_legacy = !self.argon_hud && assets.skin.is_legacy();
        if use_legacy && self.legacy.is_none() {
            self.legacy = Some(LegacyHud::resolve(assets.skin));
        }
        let legacy_score = use_legacy && self.legacy.as_ref().is_some_and(|l| l.score.is_some());
        let legacy_acc = use_legacy && self.legacy.as_ref().is_some_and(|l| l.score.is_some());
        let legacy_combo = use_legacy && self.legacy.as_ref().is_some_and(|l| l.combo.is_some());
        let legacy_health = use_legacy && self.legacy.as_ref().is_some_and(|l| l.health_bg.is_some() && l.health_fill.is_some());
        let legacy_keys = use_legacy && self.legacy.as_ref().is_some_and(|l| l.input_bg.is_some() || l.input_key.is_some());

        // --- Score counter --------------------------------------------------
        self.score.set(score as f64, t);
        self.score.update(t);

        if legacy_score {
            self.draw_legacy_score(assets, list, m, score, t);
        } else {
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
        }

        // --- Accuracy counter --------------------------------------------------
        // Exact ArgonAccuracyCounter layout: a horizontal FillFlow of
        // [whole (full)], [".##" scale 0.5, margin-top 4], ["%" (full)],
        // all TOP-aligned; anchored TopRight at virtual (1024-20, 20).
        self.acc.set(accuracy * 100.0, t);
        self.acc.update(t);
        if legacy_acc {
            self.draw_legacy_accuracy(assets, list, m, accuracy, t);
        } else {
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
        }

        // --- PP counter (`ArgonPerformancePointsCounter` style) -----------------
        // Live PP off the gradual timeline, rounded to int (`Math.Round`
        // AwayFromZero) with the same 250ms roll as the other counters.
        // Shown for EVERY skin (deliberate deviation: lazer's legacy-skin
        // MainHUD container ships no PP counter) - under legacy skins it
        // hangs below the legacy accuracy run instead of the argon one.
        if !game.pp_events.is_empty() {
            let pp = crate::pp::pp_at(&game.pp_events, t);
            self.pp.set(pp.round(), t);
            self.pp.update(t);

            let cd = CounterDraw { atlas: assets.atlas, digit_h: 22.0 * m.virt };
            // Below the accuracy counter: (accuracy.X, accuracy.Y +
            // accuracy.DrawHeight + 10) with TopRight anchors. Placement
            // follows whichever accuracy counter was actually DRAWN
            // (`legacy_acc`), not the skin type - a legacy skin without
            // score digits falls back to the argon accuracy, and the
            // legacy run heights would still be 0.
            let (right, top) = if legacy_acc {
                (m.virt([1024.0 - 17.0, 0.0])[0], m.virt([0.0, self.l_score_h + 9.0 + self.l_acc_h + 10.0])[1])
            } else {
                (m.virt([1024.0 - 20.0, 0.0])[0], m.virt([0.0, 20.0 + 36.0 + 10.0])[1])
            };
            let text = format!("{}", self.pp.display.round() as i64);
            let wire = text.len().max(3); // `Math.Max(3, digits)`
            draw_wireframe_run(list, assets.atlas, right, top + cd.k() * TEX_BOX * 0.5, wire, m.virt);
            cd.draw_right(list, &text, right, top + cd.k() * TEX_BOX * 0.5, 1.0, Colour::WHITE, Blend::Alpha);

            // "PP" label (Torus Bold 12, Blue0), 2.5 left of the digits,
            // top-aligned with the digit boxes.
            let label_size = 12.0 * m.virt;
            let (lw, _ltop, _lbottom) = ttf_measure(assets.bold, "PP", label_size, 0.0);
            let label_right = right - cd.run_width(&text, 1.0) - 2.5 * m.virt;
            draw_ttf_text(
                list,
                assets.atlas,
                assets.bold,
                true,
                "PP",
                [label_right - lw * 0.5, top + 4.0 * m.virt],
                label_size,
                Colour::from_hex(0x99DDFF),
                0.0,
                Blend::Alpha,
            );
        }

        // --- Combo counter (bottom-left, scale 1.3) --------------------------------
        // ArgonComboCounter: newScale = clamp(current * (increase ? 1.1 :
        // 0.8), 0.6, 1.4), then ScaleTo(1, 500/2000, OutQuint). `current` is
        // the LIVE scale at the moment of the change.
        if legacy_combo {
            self.l_combo.update(combo, t);
            self.draw_legacy_combo(assets, list, m, t);
        } else if combo != self.last_combo {
            let increase = combo > self.last_combo;
            let was_miss = self.last_combo > 1 && combo == 0;
            let new_scale = (self.combo_scale_now * if increase { 1.1 } else { 0.8 }).clamp(0.6, 1.4);
            let dur = if was_miss { 2000.0 } else { 500.0 };
            self.combo_scale_anim = Some((t, t + dur, new_scale, 1.0, Easing::OutQuint));
            self.was_miss = was_miss;
            self.last_combo = combo;
            self.last_combo_time = t;
        }
        if !legacy_combo {
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
        }

        // --- Health bar ------------------------------------------------------------
        let health = health_at(game, t);
        if legacy_health {
            self.draw_legacy_health(assets, list, m, health, t);
        } else {
            if health < self.last_health - 1e-6 {
                self.health_flash = Some(t);
            }
            self.last_health = health;
            draw_health(list, m, health, t, self.health_flash);
        }

        // --- Unstable rate bar (skin style, bottom centre) ------------------------
        if self.ur_bar {
            self.draw_ur_bar(game, assets, list, m, t);
        }

        // --- Key overlay (Z/X/C tap display) ---------------------------------------
        if self.key_overlay && std::env::var("NO_KEYS").is_err() {
            if legacy_keys {
                self.draw_legacy_keys(game, assets, list, m, t);
            } else {
                self.draw_key_overlay(game, assets, list, m, t);
            }
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

// ---------------------------------------------------------------------------
// Legacy-skin HUD (`--skin <dir>`): ports of the lazer components the
// gameplay HUD is made of when a user skin provides them.
//
// | lazer source | port |
// |---|---|
// | `Skinning/LegacyScoreCounter.cs` (TopRight, 0.96, margin 10, Score font, FixedWidth, "000000"/"00000000") | [`LegacyHud::score`] + [`HudState::draw_legacy_score`] |
// | `Skinning/LegacyAccuracyCounter.cs` (TopRight, 0.576, margins 9/17, below the score, FormatAccuracy) | [`HudState::draw_legacy_accuracy`] |
// | `Skinning/LegacyDefaultComboCounter.cs` (BottomLeft, 1.28, margin 10, "{n}x", pop-out bursts + proportional break roll) | [`LegacyCombo`] + [`HudState::draw_legacy_combo`] |
// | `Skinning/LegacyHealthDisplay.cs` (scorebar-bg/colour/marker/ki*, old vs new style) | [`LegacyHealth`] + [`HudState::draw_legacy_health`] |
// | `Skinning/LegacyKeyCounter(Display).cs` (`inputoverlay-*`, right-centre column) | [`HudState::draw_legacy_keys`] |
// | `Skinning/LegacySpriteText.cs` (`{prefix}-{glyph}` textures, -overlap spacing, FixedWidth '5') | [`DigitFont`] |
//
// All coordinates are lazer's 1024x768 HUD units (the same stretched
// virtual space `Mapper::virt` maps); `LegacySkin.STABLE_MAGIC_SCALE_FACTOR`
// (1.6) constants from the source are pre-multiplied into the literals.
// ---------------------------------------------------------------------------

/// Proportional rolling counter (`RollingCounter.IsRollingProportional`):
/// the roll duration is the size of the change, with the counter's own
/// easing (`LegacyScoreCounter` 1000·|Δscore| ms Out;
/// `PercentageCounter` 375·|Δacc|·100 ms Out). The FIRST value seen is
/// snapped in without a roll - lazer's `RollingCounter` sets
/// `DisplayedCount` directly on the initial bind, and a cold seek into a
/// finished score must not spend a minute rolling up from zero.
struct PropRoll {
    display: f64,
    from: f64,
    to: f64,
    start: f64,
    dur: f64,
    init: bool,
}

impl PropRoll {
    fn new(initial: f64) -> PropRoll {
        PropRoll { display: initial, from: initial, to: initial, start: f64::NEG_INFINITY, dur: 0.0, init: false }
    }

    fn set(&mut self, value: f64, t: f64, per_unit: f64) {
        if value != self.to {
            if !self.init {
                // Initial bind: snap without a roll (from == to ⇒ dur 0).
                self.from = value;
                self.display = value;
                self.init = true;
            } else {
                self.from = self.display;
            }
            self.to = value;
            self.start = t;
            self.dur = (value - self.from).abs() * per_unit;
        }
    }

    fn update(&mut self, t: f64) {
        self.display = value_at(t, self.start, self.start + self.dur, self.from, self.to, Easing::Out);
    }
}

/// A legacy sprite font (`LegacySpriteText` + `LegacyGlyphStore`):
/// `{prefix}-0..9` digits plus the punctuation lookups (`comma`, `dot`,
/// `percent`, plain `x`). `FixedWidth` digits all advance by the '5'
/// texture's width (`FixedWidthReferenceCharacter`), everything else by
/// its own width; every advance loses `overlap` (`Spacing = -overlap`).
struct DigitFont {
    digits: [Option<SkinTexture>; 10],
    overlap: f32,
    dot: Option<SkinTexture>,
    percent: Option<SkinTexture>,
    comma: Option<SkinTexture>,
    x: Option<SkinTexture>,
}

impl DigitFont {
    /// Full font or nothing (`LegacySkinExtensions.HasFont` requires at
    /// least `{prefix}-0`): the score/accuracy/combo counters demand all
    /// ten digits so big numbers never grow gaps. `partial` fonts (the
    /// key-overlay `scoreentry` counts) skip missing glyphs instead -
    /// plenty of skins ship only a handful of entry digits.
    fn resolve(skin: &crate::skin::ResolvedSkin, font: LegacyFont) -> Option<DigitFont> {
        Self::resolve_inner(skin, font, true)
    }

    fn resolve_partial(skin: &crate::skin::ResolvedSkin, font: LegacyFont) -> Option<DigitFont> {
        Self::resolve_inner(skin, font, false)
    }

    fn resolve_inner(skin: &crate::skin::ResolvedSkin, font: LegacyFont, require_full: bool) -> Option<DigitFont> {
        let prefix = crate::skin::get_font_prefix(skin, font);
        let mut digits: [Option<SkinTexture>; 10] = Default::default();
        let mut any = false;
        for (i, slot) in digits.iter_mut().enumerate() {
            *slot = skin.legacy_texture(&format!("{}-{}", prefix, i));
            any |= slot.is_some();
        }
        if !any || (require_full && digits.iter().any(|d| d.is_none())) {
            return None;
        }
        let punct = |name: &str| skin.legacy_texture(&format!("{}-{}", prefix, name));
        Some(DigitFont {
            digits,
            overlap: crate::skin::get_font_overlap(skin, font),
            dot: punct("dot"),
            percent: punct("percent"),
            comma: punct("comma"),
            x: punct("x"),
        })
    }

    fn glyph(&self, c: char) -> Option<&SkinTexture> {
        match c {
            '0'..='9' => self.digits[c as usize - '0' as usize].as_ref(),
            '.' => self.dot.as_ref(),
            '%' => self.percent.as_ref(),
            ',' => self.comma.as_ref(),
            'x' | 'X' => self.x.as_ref(),
            _ => None,
        }
    }

    fn tex_w(&self, c: char) -> f32 {
        self.glyph(c).map(|t| t.display_width()).unwrap_or(0.0)
    }

    /// Glyph advance in font units (texture px ÷ @2x).
    fn advance(&self, c: char, fixed_width: bool) -> f32 {
        let w = if fixed_width && c.is_ascii_digit() { self.tex_w('5') } else { self.tex_w(c) };
        (w - self.overlap).max(0.0)
    }

    fn run_width(&self, text: &str, fixed_width: bool) -> f32 {
        text.chars().map(|c| self.advance(c, fixed_width)).sum()
    }

    /// Draws the run with its LAST glyph's advance slot ending at
    /// `right_x` (Origin TopRight), glyph boxes bottom-aligned on
    /// `baseline_y` (`UseFullGlyphHeight = false` bottom-baseline layout).
    /// `k` converts font units to screen px (component scale included).
    /// Returns the run's total advance in font units.
    #[allow(clippy::too_many_arguments)]
    fn draw_right(
        &self,
        list: &mut DrawList,
        atlas: &Atlas,
        text: &str,
        right_x: f32,
        baseline_y: f32,
        k: f32,
        colour: Colour,
        blend: Blend,
    ) -> f32 {
        let total = self.run_width(text, true);
        let mut pen = right_x - total * k;
        for c in text.chars() {
            if let Some(tex) = self.glyph(c) {
                let w = tex.display_width() * k;
                let h = tex.display_height() * k;
                list.image(atlas, tex.region, [pen + w * 0.5, baseline_y - h * 0.5], [w, h], 0.0, colour, blend);
            }
            pen += self.advance(c, true) * k;
        }
        total
    }

    /// Same run left-aligned at `left_x`.
    #[allow(clippy::too_many_arguments)]
    fn draw_left(
        &self,
        list: &mut DrawList,
        atlas: &Atlas,
        text: &str,
        left_x: f32,
        baseline_y: f32,
        k: f32,
        colour: Colour,
        blend: Blend,
        fixed_width: bool,
    ) -> f32 {
        let mut pen = left_x;
        for c in text.chars() {
            if let Some(tex) = self.glyph(c) {
                let w = tex.display_width() * k;
                let h = tex.display_height() * k;
                list.image(atlas, tex.region, [pen + w * 0.5, baseline_y - h * 0.5], [w, h], 0.0, colour, blend);
            }
            pen += self.advance(c, fixed_width) * k;
        }
        self.run_width(text, fixed_width)
    }

    fn max_digit_h(&self) -> f32 {
        self.digits.iter().filter_map(|d| d.as_ref()).map(|d| d.display_height()).fold(0.0, f32::max)
    }
}

/// All resolved legacy HUD pieces; element slots stay `None` when the
/// skin doesn't provide them (the argon fallback takes over that piece).
struct LegacyHud {
    score: Option<DigitFont>,
    combo: Option<DigitFont>,
    entry: Option<DigitFont>,
    health_bg: Option<SkinTexture>,
    health_fill: Option<crate::skin::SkinAnimation>,
    health_marker: Option<SkinTexture>,
    health_ki: Option<SkinTexture>,
    health_kidanger: Option<SkinTexture>,
    health_kidanger2: Option<SkinTexture>,
    input_bg: Option<SkinTexture>,
    input_key: Option<SkinTexture>,
    /// `[Colours] InputOverlayText` (?? black, `LegacyKeyCounterDisplay`).
    input_text_colour: Colour,
    /// `LegacyHealthDisplay.isNewStyle`: a `scorebar-marker` provider
    /// switches the bar to the new-style fill position + HP tint.
    health_new_style: bool,
}

impl LegacyHud {
    fn resolve(skin: &crate::skin::ResolvedSkin) -> LegacyHud {
        let tex = |name: &str| skin.legacy_texture(name);
        let anim = |name: &str| {
            skin.legacy_skin()
                .and_then(|l| crate::skin::get_animation(l, name, true, true, true, "-"))
        };
        let input_text_colour = skin
            .get_config(crate::skin::SkinLookup::CustomColour(crate::skin::SkinCustomColourLookup(
                "InputOverlayText".to_string(),
            )))
            .and_then(|v| v.as_colour())
            .unwrap_or_else(|| Colour::from_hex(0x000000));
        let health_bg = tex("scorebar-bg");
        LegacyHud {
            score: DigitFont::resolve(skin, LegacyFont::Score),
            combo: DigitFont::resolve(skin, LegacyFont::Combo),
            entry: DigitFont::resolve_partial(skin, LegacyFont::ScoreEntry),
            health_bg,
            health_fill: if health_bg.is_some() { anim("scorebar-colour") } else { None },
            health_marker: tex("scorebar-marker"),
            health_ki: tex("scorebar-ki"),
            health_kidanger: tex("scorebar-kidanger"),
            health_kidanger2: tex("scorebar-kidanger2"),
            input_bg: tex("inputoverlay-background"),
            input_key: tex("inputoverlay-key"),
            input_text_colour,
            health_new_style: health_bg.is_some() && tex("scorebar-marker").is_some(),
        }
    }
}

/// `LegacyDefaultComboCounter` state: the stepped `DisplayedCount` with
/// its delayed +1 queue, the additive pop-out burst, the small pulse on
/// the displayed text, and the break roll + fade.
struct LegacyCombo {
    prev: i32,
    displayed: f64,
    /// Scheduled `scheduledPopOutSmall` fire times (each steps the
    /// displayed count +1 toward `target`).
    steps: Vec<f64>,
    target: i32,
    visible: bool,
    alpha: f64,
    alpha_anim: Option<(f64, f64, f64)>, // (start, from, to) 100ms linear
    roll: Option<(f64, f64, f64, f64)>,  // (start, from, to, dur) linear
    big_pop: Option<(f64, i32)>,         // (burst time, burst text value)
    small_pop: Option<f64>,
}

impl LegacyCombo {
    fn new() -> LegacyCombo {
        LegacyCombo {
            prev: 0,
            displayed: 0.0,
            steps: Vec::new(),
            target: 0,
            visible: false,
            alpha: 0.0,
            alpha_anim: None,
            roll: None,
            big_pop: None,
            small_pop: None,
        }
    }

    fn update(&mut self, current: i32, t: f64) {
        if current != self.prev {
            if current == 0 && self.prev > 0 {
                // onCountRolling: fade out (100ms) + proportional roll to 0
                // (`difference * 20ms`, linear `TransformTo`).
                self.alpha_anim = Some((t, self.alpha, 0.0));
                self.roll = Some((t, self.displayed, 0.0, self.displayed * 20.0));
                self.steps.clear();
                self.big_pop = None;
                self.small_pop = None;
            } else if current == self.prev + 1 {
                // onCountIncrement: big additive pop-out now, the displayed
                // value steps up 160ms later (big_pop_out_duration - 140).
                self.big_pop = Some((t, current));
                self.small_pop = Some(t);
                self.steps.push(t + 160.0);
                self.target = current;
                self.visible = true;
                self.alpha_anim = Some((t, self.alpha, 1.0));
                self.roll = None;
            } else {
                // onCountChange: jump.
                self.displayed = current as f64;
                self.target = current;
                self.steps.clear();
                self.roll = None;
                self.big_pop = None;
                self.small_pop = None;
                self.visible = current > 0;
                self.alpha_anim = Some((t, self.alpha, if current > 0 { 1.0 } else { 0.0 }));
            }
            self.prev = current;
        }

        // Fire due steps (`scheduledPopOutSmall`, guarded by
        // `DisplayedCount < currentValue`).
        let mut fired = false;
        while self.steps.first().is_some_and(|&s| s <= t) {
            self.steps.remove(0);
            if self.displayed < self.target as f64 {
                self.displayed += 1.0;
                fired = true;
            }
        }
        let _ = fired;

        if let Some((start, from, to, dur)) = self.roll {
            self.displayed = value_at(t, start, start + dur, from, to, Easing::Linear);
        }
        if let Some((start, from, to)) = self.alpha_anim {
            self.alpha = value_at(t, start, start + 100.0, from, to, Easing::Linear);
        }
    }

    /// Displayed-text scale (`transformPopOutSmall`: 1 → 1.1 → 1 over
    /// 50+50ms In/Out).
    fn small_scale(&self, t: f64) -> f64 {
        match self.small_pop {
            Some(s) => {
                let x = t - s;
                if x < 0.0 || x > 100.0 {
                    1.0
                } else if x < 50.0 {
                    value_at(x, 0.0, 50.0, 1.0, 1.1, Easing::In)
                } else {
                    value_at(x, 50.0, 100.0, 1.1, 1.0, Easing::Out)
                }
            }
            None => 1.0,
        }
    }
}

/// `LegacyHealthDisplay` state: smoothed fill width + marker flash/bulge.
struct LegacyHealth {
    fill_w: f64,
    prev_hp: f64,
    bulge: Option<f64>,
    flash: Option<(f64, bool)>,
    last_t: f64,
}

impl LegacyHealth {
    fn new() -> LegacyHealth {
        LegacyHealth { fill_w: 0.0, prev_hp: 1.0, bulge: None, flash: None, last_t: f64::NEG_INFINITY }
    }
}

struct LegacyKeyAnim {
    pressed: bool,
    press_t: f64,
    release_t: f64,
    /// `activatedOnce`: after the first press the key NAME is swapped for
    /// the cumulative press count permanently.
    activated: bool,
}

impl LegacyKeyAnim {
    fn new() -> LegacyKeyAnim {
        LegacyKeyAnim { pressed: false, press_t: -1e12, release_t: -1e12, activated: false }
    }
}

/// `LegacyUtils.InterpolateNonLinear` / `LegacyHealthDisplay.getFillColour`:
/// white above half, darkening toward black at low HP, then into red in
/// the danger zone.
fn legacy_fill_colour(hp: f64) -> Colour {
    if hp < 0.2 {
        let f = ((0.2 - hp) / 0.2) as f32;
        Colour::lerp(Colour::from_hex(0x000000), Colour::from_hex(0xFF0000), f)
    } else if hp < 0.5 {
        let f = ((0.5 - hp) / 0.5) as f32;
        Colour::lerp(Colour::WHITE, Colour::from_hex(0x000000), f)
    } else {
        Colour::WHITE
    }
}

impl HudState {
    /// `LegacyScoreCounter`: TopRight origin TopRight, Scale 0.96, margin
    /// horizontal 10; score digits, FixedWidth, zero-padded to 6
    /// (standardised) / 8 (classic) digits (`GameplayScoreCounter`).
    fn draw_legacy_score(&mut self, assets: &Assets, list: &mut DrawList, m: &Mapper, score: i64, t: f64) {
        self.l_score.set(score as f64, t, 1000.0);
        self.l_score.update(t);
        let value = self.l_score.display.round() as i64;
        let classic = self.classic_score;

        let Some(font) = self.legacy.as_ref().and_then(|l| l.score.as_ref()) else { return };

        let k = 0.96 * m.virt;
        let digits = if classic { 8 } else { 6 };
        let text = format!("{:0width$}", value, width = digits);

        let right = m.virt([1024.0 - 10.0, 0.0])[0];
        // Glyph boxes bottom-aligned on the run's baseline; the run top is
        // flush with the screen top edge (margin vertical 0).
        let baseline = font.max_digit_h() * k;
        font.draw_right(list, assets.atlas, &text, right, baseline, k, Colour::WHITE, Blend::Alpha);
        self.l_score_h = font.max_digit_h() * 0.96;
    }

    /// `LegacyAccuracyCounter`: TopRight, Scale 0.6·0.96, margins
    /// vertical 9 / horizontal 17; its Y is pinned below the score run by
    /// the MainHUD container callback. Text = `FormatAccuracy` ("0.00%",
    /// floored to 4 decimals so a 89.9999% never rounds up to 90%).
    fn draw_legacy_accuracy(&mut self, assets: &Assets, list: &mut DrawList, m: &Mapper, accuracy: f64, t: f64) {
        // PercentageCounter rolls the FRACTION with |Δ|·375ms·100 duration.
        self.l_acc.set(accuracy, t, 37500.0);
        self.l_acc.update(t);
        let floored = (self.l_acc.display * 10_000.0).floor() / 10_000.0;
        let text = format!("{:.2}%", floored * 100.0);
        let top_units = self.l_score_h + 9.0;

        let Some(font) = self.legacy.as_ref().and_then(|l| l.score.as_ref()) else { return };

        let k = 0.6 * 0.96 * m.virt;
        // Remember the run height so the PP counter can hang below it.
        self.l_acc_h = font.max_digit_h() * 0.6 * 0.96;
        let right = m.virt([1024.0 - 17.0, 0.0])[0];
        let baseline = m.virt([0.0, top_units])[1] + font.max_digit_h() * k;
        font.draw_right(list, assets.atlas, &text, right, baseline, k, Colour::WHITE, Blend::Alpha);
    }

    /// `LegacyDefaultComboCounter`: BottomLeft + margin 10, Scale 1.28,
    /// Combo font, "{n}x". The additive pop-out burst (`transformPopOut`)
    /// fires behind every increment; the displayed value pulses
    /// (`transformPopOutSmall`) and steps up 160ms later.
    fn draw_legacy_combo(&mut self, assets: &Assets, list: &mut DrawList, m: &Mapper, t: f64) {
        let combo = &self.l_combo;
        let (big_pop, small_scale, visible, alpha, displayed, roll_active) =
            (combo.big_pop, combo.small_scale(t) as f32, combo.visible, combo.alpha as f32, combo.displayed.round() as i64, combo.roll.is_some());

        let Some(font) = self.legacy.as_ref().and_then(|l| l.combo.as_ref()) else { return };

        let v = m.virt;
        let k = 1.28 * v;
        let left = 10.0 * v;
        let baseline = m.virt([0.0, 768.0 - 10.0])[1];
        let text = |value: i32| format!("{}x", value);

        // Additive pop-out behind (`popOutCount`): text of the burst value,
        // scale 1.56 → 1 and alpha .6 → 0 over 300ms, linear.
        if let Some((start, value)) = big_pop {
            let x = t - start;
            if x < 300.0 {
                let s = value_at(x, 0.0, 300.0, 1.56, 1.0, Easing::Linear) as f32;
                let a = value_at(x, 0.0, 300.0, 0.6, 0.0, Easing::Linear) as f32;
                if a > 0.004 {
                    font.draw_left(
                        list,
                        assets.atlas,
                        &text(value),
                        left,
                        baseline,
                        k * s,
                        Colour::WHITE.opacity(a),
                        Blend::Additive,
                        false,
                    );
                }
            }
        }

        if visible && alpha > 0.004 {
            if displayed > 0 || roll_active {
                font.draw_left(
                    list,
                    assets.atlas,
                    &text(displayed as i32),
                    left,
                    baseline,
                    k * small_scale,
                    Colour::WHITE.opacity(alpha),
                    Blend::Alpha,
                    false,
                );
            }
        }
    }

    /// `LegacyHealthDisplay`: scorebar-bg at the screen top-left corner,
    /// the fill (masked by HP, smoothed 200ms OutQuint) inset (3,10)·1.6
    /// old-style / (7.5,7.8)·1.6 new-style, and the marker riding the
    /// fill's leading edge. New style tints fill+marker by HP and blends
    /// the marker additively above half; old style swaps
    /// ki/kidanger/kidanger2 at 0.5/0.2. Damage flashes the marker
    /// (additive burst, 120ms Out); gains bulge it (1.2 → 0.8, 150ms).
    fn draw_legacy_health(&mut self, assets: &Assets, list: &mut DrawList, m: &Mapper, hp: f64, t: f64) {
        // Snapshot the skin pieces (cheap: textures are Copy handles, the
        // animation a small frame vec) so the health state can be mutated
        // below without aliasing `self.legacy`.
        let (bg, fill_anim, new_style, marker_new, ki, kidanger, kidanger2) = {
            let Some(hud) = self.legacy.as_ref() else { return };
            match (hud.health_bg, hud.health_fill.clone()) {
                (Some(bg), Some(fill)) => (
                    bg,
                    fill,
                    hud.health_new_style,
                    hud.health_marker,
                    hud.health_ki,
                    hud.health_kidanger,
                    hud.health_kidanger2,
                ),
                _ => return,
            }
        };

        let v = m.virt;
        let state = &mut self.l_health;

        // Health-change edges (`HealthChanged(increase)` → Bulge / Flash).
        if hp < state.prev_hp - 1e-9 {
            state.flash = Some((t, hp >= 0.5));
        } else if hp > state.prev_hp + 1e-9 {
            state.bulge = Some(t);
        }
        state.prev_hp = hp;

        // Background sprite, top-left of the screen.
        let bg_w = bg.display_width() * v;
        let bg_h = bg.display_height() * v;
        list.image(assets.atlas, bg.region, [bg_w * 0.5, bg_h * 0.5], [bg_w, bg_h], 0.0, Colour::WHITE, Blend::Alpha);

        // Fill: animated frame (`LegacyFill`'s scorebar-colour animation),
        // masked to the smoothed HP width. The per-frame width tween is
        // `Interpolation.ValueAt(ElapsedFrameTime clamped 200, width,
        // hp·max, 0, 200, OutQuint)`.
        let (off_x, off_y) = if new_style { (7.5 * 1.6, 7.8 * 1.6) } else { (3.0 * 1.6, 10.0 * 1.6) };
        let frame = fill_anim.frame_at(t);
        let max_w = frame.display_width() as f64;
        let fill_h = frame.display_height() * v;
        let dt = if state.last_t.is_finite() && t > state.last_t { (t - state.last_t).clamp(0.0, 200.0) } else { 200.0 };
        state.fill_w = value_at(dt, 0.0, 200.0, state.fill_w, hp * max_w, Easing::OutQuint);
        state.last_t = t;
        let frac = if max_w > 0.0 { (state.fill_w / max_w).clamp(0.0, 1.0) } else { 0.0 };

        let fill_x = off_x * v;
        let fill_y = off_y * v;
        let fill_w = max_w as f32 * v * frac as f32;
        if fill_w > 0.5 {
            let tint = if new_style { legacy_fill_colour(hp) } else { Colour::WHITE };
            list.image_sub(
                assets.atlas,
                frame.region,
                [fill_x + fill_w * 0.5, fill_y + fill_h * 0.5],
                [fill_w, fill_h],
                0.0,
                tint,
                Blend::Alpha,
                0.0,
                0.0,
                frac as f32,
                1.0,
            );
        }

        // Marker: rides the fill's leading edge (right-middle for the new
        // style, right-top line for the old one).
        let marker_x = fill_x + fill_w;
        let marker_y = if new_style { fill_y + fill_h * 0.5 } else { fill_y };

        // Bulge animation factor (`Main.ScaleTo(1.2)` then → 0.8, 150ms).
        let bulge_s = match state.bulge {
            Some(b) => {
                let x = t - b;
                if x < 0.0 || x > 150.0 {
                    1.0
                } else {
                    value_at(x, 0.0, 150.0, 1.2, 0.8, Easing::Linear) as f32
                }
            }
            None => 1.0,
        };

        // Pick the marker texture + tint + blend mode for this style/HP.
        let (marker, tint, blend) = if new_style {
            let tint = legacy_fill_colour(hp);
            let blend = if hp >= 0.5 { Blend::Additive } else { Blend::Alpha };
            (marker_new, tint, blend)
        } else {
            let tex = if hp < 0.2 {
                kidanger2.or(kidanger).or(ki)
            } else if hp < 0.5 {
                kidanger.or(ki)
            } else {
                ki.or(kidanger)
            };
            (tex, Colour::WHITE, Blend::Alpha)
        };

        if let Some(marker) = marker {
            let mw = marker.display_width() * v * bulge_s;
            let mh = marker.display_height() * v * bulge_s;
            list.image(assets.atlas, marker.region, [marker_x, marker_y], [mw, mh], 0.0, tint, blend);

            // Flash: additive copy of the current marker texture bursting
            // out (scale → 2 epic / 1.6, FadeOutFromOne, 120ms Out).
            if let Some((start, epic)) = state.flash {
                let x = t - start;
                if x < 120.0 {
                    let a = value_at(x, 0.0, 120.0, 1.0, 0.0, Easing::Out) as f32;
                    let s = value_at(x, 0.0, 120.0, 1.0, if epic { 2.0 } else { 1.6 }, Easing::Out) as f32;
                    let ew = marker.display_width() * v * s;
                    let eh = marker.display_height() * v * s;
                    list.image(
                        assets.atlas,
                        marker.region,
                        [marker_x, marker_y],
                        [ew, eh],
                        0.0,
                        tint.opacity(a),
                        Blend::Additive,
                    );
                }
            }
        }
    }

    /// `LegacyKeyCounterDisplay`: a vertical column of `inputoverlay-key`
    /// boxes hanging off the right edge, centred vertically
    /// (`CentreRight` anchor, origin TopRight, Position (0, -40)·1.6),
    /// with the `inputoverlay-background` strip rotated 90° behind them.
    /// Each 46-unit box shows the key NAME until its first press, then
    /// the cumulative press count (`ScoreEntry` font), tinted
    /// `InputOverlayText`; the box squashes to 0.75 while pressed and the
    /// first two keys light up #ffde00, the third #f8009e.
    fn draw_legacy_keys(&mut self, game: &GameData, assets: &Assets, list: &mut DrawList, m: &Mapper, t: f64) {
        let v = m.virt;

        const BOX: f32 = 46.0;
        const SPACING: f32 = 1.8;

        let state = key_state_at(game, t);
        let counts = key_counts_at(game, t);

        // Phase 1: advance the press animations / activation flags.
        let mut rows: [(bool, f32, bool, u32); 3] = [(false, 1.0, false, 0); 3];
        for k in 0..3 {
            let anim = &mut self.l_keys[k];
            if state[k] != anim.pressed {
                if state[k] {
                    anim.press_t = t;
                    anim.activated = true;
                } else {
                    anim.release_t = t;
                }
                anim.pressed = state[k];
            }
            // Container squash while pressed (160ms Out both ways).
            let press_s = if anim.pressed {
                value_at(t, anim.press_t, anim.press_t + 160.0, 1.0, 0.75, Easing::Out)
            } else {
                value_at(t, anim.release_t, anim.release_t + 160.0, 0.75, 1.0, Easing::Out)
            } as f32;
            rows[k] = (anim.pressed, press_s, anim.activated, counts[k]);
        }

        // Phase 2: draw with the skin borrow.
        let Some(hud) = self.legacy.as_ref() else { return };
        let text_colour = hud.input_text_colour;

        // Display container: CentreRight anchor, TopRight origin, (0, -64).
        let tr = [m.virt([1024.0, 0.0])[0], m.screen_h * 0.5 - 64.0 * v];

        // Background strip: Sprite anchored TopRight/origin TopLeft,
        // Scale (1.05, 1), Rotation 90 - scale applies first, then the
        // rotation about the (unrotated) top-left corner, so the quad
        // hangs DOWN from the container's top-right edge.
        if let Some(bg) = hud.input_bg.as_ref() {
            let w = bg.display_width() * 1.05 * v;
            let h = bg.display_height() * v;
            // Unrotated half-extents about the pivot corner; +90° maps
            // (x, y) → (-y, x), placing the quad left-below the pivot.
            let centre = [tr[0] - h * 0.5, tr[1] + w * 0.5];
            list.image(assets.atlas, bg.region, centre, [h, w], 90.0, Colour::WHITE, Blend::Alpha);
        }

        // Key flow: TopRight anchor, X -1.5, Y +7, vertical, spacing 1.8.
        let flow_tr = [tr[0] - 1.5 * v, tr[1] + 7.0 * v];

        for k in 0..3 {
            let (pressed, press_s, activated, count) = rows[k];
            let row_top = flow_tr[1] + (k as f32) * (BOX + SPACING) * v;
            let centre = [flow_tr[0] - BOX * 0.5 * v, row_top + BOX * 0.5 * v];

            // Key sprite (native display size, centred in the 46-box),
            // tinted with the active colour while pressed.
            if let Some(key) = hud.input_key.as_ref() {
                let kw = key.display_width() * v * press_s;
                let kh = key.display_height() * v * press_s;
                let active = if k < 2 { Colour::from_hex(0xFFDE00) } else { Colour::from_hex(0xF8009E) };
                let tint = if pressed { active } else { Colour::WHITE };
                list.image(assets.atlas, key.region, centre, [kw, kh], 0.0, tint, Blend::Alpha);
            }

            // Overlay text: key name until the first press, then the
            // cumulative count (`ScoreEntry` digits), centred in the box.
            // Deviation: lazer blanks the box when the skin ships no
            // entry digits (`LegacyGlyphStore` finds nothing); we keep the
            // key NAME instead of losing the count display entirely.
            let show_name = !activated || hud.entry.is_none();
            if !show_name {
                let font = hud.entry.as_ref().unwrap();
                let text = format!("{}", count);
                let run_w = font.run_width(&text, false) * v;
                let baseline = centre[1] + font.max_digit_h() * 0.5 * v;
                font.draw_left(list, assets.atlas, &text, centre[0] - run_w * 0.5, baseline, v, text_colour, Blend::Alpha, false);
            } else {
                let size = 20.0 * v;
                draw_ttf_text(
                    list,
                    assets.atlas,
                    assets.semibold,
                    false,
                    KEY_ACTIONS[k],
                    [centre[0], centre[1]],
                    size,
                    text_colour,
                    0.0,
                    Blend::Alpha,
                );
            }
        }
    }
}
