//! Landing on the values a thermostat accepts.
//!
//! Mysa lists every setpoint a thermostat takes, per mode
//! (`GET /v1/capabilities`), and refuses anything between two of them with
//! `400 invalid_request` rather than rounding. A person reads in °F, a rule
//! may compute 21.3 °C, and the app's − and + move a degree Fahrenheit at a
//! time, so every number this plugin sends is first moved onto the list.

/// The listed value nearest `wanted`. `None` when nothing is listed.
pub fn nearest(wanted: f64, allowed: &[f64]) -> Option<f64> {
    allowed
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .min_by(|a, b| {
            (a - wanted)
                .abs()
                .total_cmp(&(b - wanted).abs())
                // A tie goes to the lower value: never warmer than asked.
                .then(a.total_cmp(b))
        })
}

/// The listed value for `wanted`, moving away from `current` when the nearest
/// would be no change at all.
///
/// The app's + from 70 °F asks for 71 °F (21.67 °C). On a half-degree list the
/// nearest is 21.5 °C, which is fine; but on a whole-degree list, 21.11 °C
/// (70 °F) asked from 21 °C snaps straight back to 21. Without this, a press
/// would do nothing, and a person presses again.
pub fn toward(wanted: f64, current: Option<f64>, allowed: &[f64]) -> Option<f64> {
    let near = nearest(wanted, allowed)?;
    let Some(cur) = current else {
        return Some(near);
    };
    if (near - cur).abs() > 1e-6 || (wanted - cur).abs() < 1e-6 {
        return Some(near);
    }
    let mut sorted: Vec<f64> = allowed.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    let next = if wanted > cur {
        sorted.iter().copied().find(|v| *v > cur + 1e-6)
    } else {
        sorted.iter().copied().rev().find(|v| *v < cur - 1e-6)
    };
    Some(next.unwrap_or(near))
}

/// The nearest listed integer, for fan speeds and swing positions.
pub fn nearest_int(wanted: i64, allowed: &[i64]) -> Option<i64> {
    allowed
        .iter()
        .copied()
        .min_by_key(|v| ((v - wanted).abs(), *v))
}

/// The range and step a client should draw a control with: the ends of the
/// list and its smallest gap.
pub fn range_of(allowed: &[f64]) -> Option<(f64, f64, f64)> {
    let mut sorted: Vec<f64> = allowed.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    sorted.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let (first, last) = (*sorted.first()?, *sorted.last()?);
    let step = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(f64::INFINITY, f64::min);
    let step = if step.is_finite() && step > 0.0 {
        (step * 100.0).round() / 100.0
    } else {
        0.5
    };
    Some((first, last, step))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn halves() -> Vec<f64> {
        (10..=60).map(|h| h as f64 / 2.0).collect()
    }

    #[test]
    fn a_value_between_steps_lands_on_the_nearest_never_between() {
        assert_eq!(nearest(21.11, &halves()), Some(21.0));
        assert_eq!(nearest(21.67, &halves()), Some(21.5));
        assert_eq!(nearest(21.25, &halves()), Some(21.0), "a tie goes down");
        assert_eq!(
            nearest(40.0, &halves()),
            Some(30.0),
            "past the end: the end"
        );
        assert_eq!(nearest(21.0, &[]), None);
    }

    #[test]
    fn a_press_always_moves_when_there_is_somewhere_to_go() {
        let whole: Vec<f64> = (16..=30).map(f64::from).collect();
        // + from 21 °C asks for 70 °F + 1 = 21.67: nearest 22, a move.
        assert_eq!(toward(21.67, Some(21.0), &whole), Some(22.0));
        // − from 21 °C as 70 °F − 1 = 69 °F = 20.56: nearest 21, no move: so 20.
        assert_eq!(toward(20.56, Some(21.0), &whole), Some(20.0));
        // + that already lands somewhere new is left alone.
        assert_eq!(toward(21.67, Some(21.0), &halves()), Some(21.5));
        // At the top, there is nowhere further.
        assert_eq!(toward(31.0, Some(30.0), &whole), Some(30.0));
        // Asking for exactly the current value is that value.
        assert_eq!(toward(21.0, Some(21.0), &whole), Some(21.0));
    }

    #[test]
    fn ints_snap_too() {
        assert_eq!(nearest_int(5, &[1, 2, 3]), Some(3));
        assert_eq!(nearest_int(2, &[1, 3]), Some(1));
        assert_eq!(nearest_int(2, &[]), None);
    }

    #[test]
    fn the_range_is_the_lists_ends_and_its_smallest_gap() {
        assert_eq!(range_of(&halves()), Some((5.0, 30.0, 0.5)));
        assert_eq!(range_of(&[16.0, 17.0, 18.0]), Some((16.0, 18.0, 1.0)));
        assert_eq!(range_of(&[20.0]), Some((20.0, 20.0, 0.5)));
        assert_eq!(range_of(&[]), None);
    }
}
