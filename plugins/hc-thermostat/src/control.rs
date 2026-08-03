//! Pure control helpers — aggregation, hysteresis, short-cycle lockout.
//!
//! Deliberately free of MQTT, state stores, and async so the control loop can
//! be exhaustively tested in isolation.

use chrono::{DateTime, Utc};

/// Aggregate readings using the named strategy. Returns `None` for empty input.
pub fn aggregate(values: &[f64], mode: &str) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    match mode {
        "min" => values
            .iter()
            .copied()
            .fold(None, |acc, v| Some(acc.map_or(v, |a: f64| a.min(v)))),
        "max" => values
            .iter()
            .copied()
            .fold(None, |acc, v| Some(acc.map_or(v, |a: f64| a.max(v)))),
        _ => Some(values.iter().sum::<f64>() / values.len() as f64),
    }
}

/// Compute the new `call_for` value given current temp, setpoint, hysteresis,
/// mode, and the prior call. Prior call is used to stay sticky within the
/// deadband (the hallmark of hysteresis).
pub fn compute_call_for(
    current: f64,
    setpoint: f64,
    hysteresis: f64,
    mode: &str,
    prev_call: &str,
) -> &'static str {
    let half = (hysteresis / 2.0).max(0.0);
    match mode {
        "heat" => {
            if prev_call == "heat" {
                if current < setpoint + half {
                    "heat"
                } else {
                    "idle"
                }
            } else if current < setpoint - half {
                "heat"
            } else {
                "idle"
            }
        }
        "cool" => {
            if prev_call == "cool" {
                if current > setpoint - half {
                    "cool"
                } else {
                    "idle"
                }
            } else if current > setpoint + half {
                "cool"
            } else {
                "idle"
            }
        }
        _ => "idle", // "off" and any unknown mode
    }
}

/// Seconds of lockout remaining before the actuator can flip from `prev_act`.
/// Returns 0 when no lockout is active.
pub fn lockout_remaining(
    prev_act: bool,
    min_on_secs: u64,
    min_off_secs: u64,
    last_change: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> u64 {
    let limit = if prev_act { min_on_secs } else { min_off_secs };
    if limit == 0 {
        return 0;
    }
    let Some(last) = last_change else {
        return 0;
    };
    let elapsed = (now - last).num_seconds().max(0) as u64;
    limit.saturating_sub(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_average() {
        assert_eq!(aggregate(&[68.0, 72.0], "average"), Some(70.0));
        assert_eq!(aggregate(&[70.0], "average"), Some(70.0));
    }

    #[test]
    fn aggregate_min_max() {
        assert_eq!(aggregate(&[68.0, 72.0, 65.0], "min"), Some(65.0));
        assert_eq!(aggregate(&[68.0, 72.0, 65.0], "max"), Some(72.0));
    }

    #[test]
    fn aggregate_empty_returns_none() {
        assert_eq!(aggregate(&[], "average"), None);
        assert_eq!(aggregate(&[], "min"), None);
        assert_eq!(aggregate(&[], "max"), None);
    }

    #[test]
    fn aggregate_unknown_falls_back_to_average() {
        assert_eq!(aggregate(&[68.0, 72.0], "mystery"), Some(70.0));
    }

    #[test]
    fn heat_mode_hysteresis() {
        assert_eq!(compute_call_for(70.0, 70.0, 2.0, "heat", "idle"), "idle");
        assert_eq!(compute_call_for(68.9, 70.0, 2.0, "heat", "idle"), "heat");
        assert_eq!(compute_call_for(69.5, 70.0, 2.0, "heat", "idle"), "idle");
        assert_eq!(compute_call_for(70.0, 70.0, 2.0, "heat", "heat"), "heat");
        assert_eq!(compute_call_for(70.9, 70.0, 2.0, "heat", "heat"), "heat");
        assert_eq!(compute_call_for(71.1, 70.0, 2.0, "heat", "heat"), "idle");
    }

    #[test]
    fn cool_mode_hysteresis() {
        assert_eq!(compute_call_for(70.0, 70.0, 2.0, "cool", "idle"), "idle");
        assert_eq!(compute_call_for(71.1, 70.0, 2.0, "cool", "idle"), "cool");
        assert_eq!(compute_call_for(70.9, 70.0, 2.0, "cool", "idle"), "idle");
        assert_eq!(compute_call_for(70.0, 70.0, 2.0, "cool", "cool"), "cool");
        assert_eq!(compute_call_for(69.1, 70.0, 2.0, "cool", "cool"), "cool");
        assert_eq!(compute_call_for(68.9, 70.0, 2.0, "cool", "cool"), "idle");
    }

    #[test]
    fn off_mode_never_calls() {
        assert_eq!(compute_call_for(50.0, 70.0, 2.0, "off", "heat"), "idle");
        assert_eq!(compute_call_for(95.0, 70.0, 2.0, "off", "cool"), "idle");
    }

    #[test]
    fn zero_hysteresis_is_bang_bang() {
        assert_eq!(compute_call_for(70.0, 70.0, 0.0, "heat", "idle"), "idle");
        assert_eq!(compute_call_for(69.99, 70.0, 0.0, "heat", "idle"), "heat");
        assert_eq!(compute_call_for(70.0, 70.0, 0.0, "heat", "heat"), "idle");
    }

    #[test]
    fn lockout_zero_limits_no_lockout() {
        let now = Utc::now();
        assert_eq!(lockout_remaining(true, 0, 0, Some(now), now), 0);
    }

    #[test]
    fn lockout_missing_timestamp_no_lockout() {
        let now = Utc::now();
        assert_eq!(lockout_remaining(true, 300, 0, None, now), 0);
    }

    #[test]
    fn lockout_remaining_on_phase() {
        let now = Utc::now();
        let last = now - chrono::Duration::seconds(120);
        assert_eq!(lockout_remaining(true, 300, 0, Some(last), now), 180);
    }

    #[test]
    fn lockout_remaining_off_phase() {
        let now = Utc::now();
        let last = now - chrono::Duration::seconds(30);
        assert_eq!(lockout_remaining(false, 0, 60, Some(last), now), 30);
    }

    #[test]
    fn lockout_expired_returns_zero() {
        let now = Utc::now();
        let last = now - chrono::Duration::seconds(400);
        assert_eq!(lockout_remaining(true, 300, 0, Some(last), now), 0);
    }
}
