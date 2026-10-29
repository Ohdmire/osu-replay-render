//! Note-lock "shake" animation (`DrawableExtensions.Shake` +
//! `ShakeContainer`).
//!
//! When a press is rejected by the hit policy (`ClickAction.Shake` —
//! clicking a circle that is still blocked by a previous object, or one
//! outside the 400ms miss window), lazer calls `Shake()` on the hit
//! object. `DrawableHitCircle` / `DrawableSlider` set
//! `ShakeContainer.ShakeDuration = 30`, and the framework extension runs a
//! fixed sequence of relative-free `MoveToX` transforms:
//!
//! ```text
//!  0ms     x = 0        (container rest position)
//!  0..15   x: 0 -> +8   Easing.OutSine
//! 15..45   x: +8 -> -8  Easing.InOutSine
//! 45..75   x: -8 -> +8  Easing.InOutSine
//! 75..105  x: +8 -> -8  Easing.InOutSine
//! 105..120 x: -8 -> 0   Easing.InSine
//! ```
//!
//! (`maximumLength` is null here, so all four swings play; the final
//! `MoveToX(0, 15, InSine)` brings the container back.) The magnitude is in
//! the object's local space; the caller multiplies by `ObjView::scale`
//! because lazer's shake container sits inside the CS-scaled container.
//!
//! A second shake while one is running truncates the remaining sequence:
//! framework `MoveToX` is absolute and the newly activated transform reads
//! the current X as its start value, so the new 0..15ms segment starts from
//! whatever offset the previous shake had reached.

use crate::draw::{Easing, value_at};

/// `ShakeContainer.ShakeDuration` set by `DrawableHitCircle` and
/// `DrawableSlider` (osu overrides the framework's 80ms default).
pub const SHAKE_DURATION: f64 = 30.0;

/// `DrawableExtensions.Shake` default `shakeMagnitude` (local pixels).
pub const SHAKE_MAGNITUDE: f64 = 8.0;

/// X offset of the shaken container at `t`, in object-local units.
///
/// `times` must be sorted ascending (one entry per shake start). Returns 0
/// before the first shake and after the last shake's 120ms sequence.
pub fn offset_at(times: &[f64], t: f64) -> f64 {
    debug_assert!(
        times.windows(2).all(|w| w[0] <= w[1]),
        "shake times must be sorted"
    );

    let Some(idx) = times.iter().rposition(|&start| start <= t) else {
        return 0.0;
    };

    // A shake that interrupts another starts from the interrupted X. This
    // recursion is bounded by the number of shakes inside the 120ms window
    // (a handful at gameplay input rates).
    let start_x = if idx == 0 {
        0.0
    } else {
        offset_at(&times[..idx], times[idx])
    };

    offset_from(times[idx], start_x, t)
}

/// One `Shake()` sequence starting at `start` with initial offset `start_x`.
fn offset_from(start: f64, start_x: f64, t: f64) -> f64 {
    const D: f64 = SHAKE_DURATION;
    let x = t - start;

    if x < D * 0.5 {
        // MoveToX(+8, 15, OutSine), starting from the interrupted offset.
        value_at(
            t,
            start,
            start + D * 0.5,
            start_x,
            SHAKE_MAGNITUDE,
            Easing::OutSine,
        )
    } else if x < D * 1.5 {
        // MoveToX(-8, 30, InOutSine)
        value_at(
            t,
            start + D * 0.5,
            start + D * 1.5,
            SHAKE_MAGNITUDE,
            -SHAKE_MAGNITUDE,
            Easing::InOutSine,
        )
    } else if x < D * 2.5 {
        // MoveToX(+8, 30, InOutSine)
        value_at(
            t,
            start + D * 1.5,
            start + D * 2.5,
            -SHAKE_MAGNITUDE,
            SHAKE_MAGNITUDE,
            Easing::InOutSine,
        )
    } else if x < D * 3.5 {
        // MoveToX(-8, 30, InOutSine)
        value_at(
            t,
            start + D * 2.5,
            start + D * 3.5,
            SHAKE_MAGNITUDE,
            -SHAKE_MAGNITUDE,
            Easing::InOutSine,
        )
    } else if x < D * 4.0 {
        // MoveToX(0, 15, InSine)
        value_at(
            t,
            start + D * 3.5,
            start + D * 4.0,
            -SHAKE_MAGNITUDE,
            0.0,
            Easing::InSine,
        )
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn no_shake_is_zero() {
        assert_eq!(offset_at(&[], 0.0), 0.0);
        assert_eq!(offset_at(&[100.0], 99.999), 0.0);
    }

    #[test]
    fn single_shake_waveform_matches_source() {
        let times = [1000.0];
        // Rest -> +8 over 15ms OutSine.
        assert!(close(offset_at(&times, 1000.0), 0.0));
        assert!(close(
            offset_at(&times, 1007.5),
            8.0 * (std::f64::consts::FRAC_PI_4).sin()
        ));
        assert!(close(offset_at(&times, 1015.0), 8.0));
        // +8 -> -8 over 30ms InOutSine; mid point is 0.
        assert!(close(offset_at(&times, 1030.0), 0.0));
        assert!(close(offset_at(&times, 1045.0), -8.0));
        // -8 -> +8.
        assert!(close(offset_at(&times, 1060.0), 0.0));
        assert!(close(offset_at(&times, 1075.0), 8.0));
        // +8 -> -8.
        assert!(close(offset_at(&times, 1090.0), 0.0));
        assert!(close(offset_at(&times, 1105.0), -8.0));
        // -8 -> 0 over 15ms InSine.
        assert!(close(
            offset_at(&times, 1112.5),
            -8.0 + 8.0 * (1.0 - std::f64::consts::FRAC_PI_4.cos())
        ));
        assert!(close(offset_at(&times, 1120.0), 0.0));
        assert_eq!(offset_at(&times, 1200.0), 0.0);
    }

    #[test]
    fn interrupting_shake_starts_from_current_offset() {
        // A second press at 1007.5, where the first shake is at +8*sin(pi/4).
        let times = [1000.0, 1007.5];
        let interrupted = 8.0 * (std::f64::consts::FRAC_PI_4).sin();
        assert!(close(offset_at(&times, 1007.5), interrupted));
        // Its 0..15ms segment runs from `interrupted` to +8.
        assert!(close(
            offset_at(&times, 1015.0),
            interrupted + (8.0 - interrupted) * (std::f64::consts::FRAC_PI_4).sin()
        ));
        assert!(close(offset_at(&times, 1022.5), 8.0));
        // The old sequence's -8 target at 1045 was truncated: the second
        // shake is only 3/4 through its +8 -> -8 swing there.
        assert!(close(
            offset_at(&times, 1045.0),
            8.0 - 16.0 * 0.8535533905932737
        ));
    }

    #[test]
    fn same_time_shakes_collapse_to_one() {
        let times = [1000.0, 1000.0];
        assert!(close(
            offset_at(&times, 1007.5),
            8.0 * (std::f64::consts::FRAC_PI_4).sin()
        ));
        assert!(close(offset_at(&times, 1015.0), 8.0));
    }
}
