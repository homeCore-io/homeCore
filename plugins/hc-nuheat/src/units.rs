//! Temperature, in the three scales this plugin has to keep straight.
//!
//! NuHeat's API carries temperatures as **integer hundredths of a degree
//! Celsius**: the documented hold example sends `"setPointTemp": 3000`, which
//! is 30.00 °C — the top of a floor thermostat's range, and 86 °F, the number
//! the NuHeat app shows as its maximum. Everything else in the API uses the
//! same units, `currentTemperature` included.
//!
//! homeCore publishes and accepts **degrees Celsius as a float**, because that
//! is what every other plugin in this repository does and what the `thermostat`
//! dashboard widget binds to. The operator may prefer Fahrenheit, which is a
//! display concern their client handles — with one exception, noted on
//! [`Scale`]: the *config file* lets an operator write limits in whichever
//! scale they think in, since a floor-heating maximum is the sort of number
//! people know as "82 °F" and not as "27.8 °C".
//!
//! ## Why the range check exists
//!
//! The unit scale is inferred from NuHeat's documented example rather than
//! stated outright in their reference — the prose says "1/10 °C" in one place
//! and every worked example contradicts it. If that inference is ever wrong,
//! being wrong by a factor of ten is the failure mode, and it is silent: a
//! plugin that reads `2100` as 210 °C publishes a plausible-looking number
//! that no rule threshold will ever match. [`decode_celsius`] therefore
//! rejects a decoded reading outside [`PLAUSIBLE_C`] instead of publishing it,
//! and the caller raises a notice. A visibly broken plugin beats a quietly
//! wrong one.

use std::fmt;

/// The NuHeat wire unit: hundredths of a degree Celsius.
const HUNDREDTHS_PER_DEGREE: f64 = 100.0;

/// What a floor thermostat could believably be reading, in °C.
///
/// Deliberately wide — this is a scale-error tripwire, not a validity check on
/// the room. A slab sensor in an unheated garage in winter and one in a sunroom
/// in August both have to fit inside it; only a factor-of-ten mistake does not.
pub const PLAUSIBLE_C: std::ops::RangeInclusive<f64> = -30.0..=70.0;

/// The range NuHeat's own thermostats accept as a setpoint, in °C.
///
/// 5 °C to 30 °C, matching the app. A setpoint outside this is clamped rather
/// than sent: the API would reject it, and a rejected write leaves the UI
/// showing a temperature the floor never adopted.
pub const SETPOINT_MIN_C: f64 = 5.0;
pub const SETPOINT_MAX_C: f64 = 30.0;

/// Which scale a number in the *config file* is written in.
///
/// Only ever applies to operator-authored limits. Wire values are always
/// hundredths of °C and published values are always °C, neither of which this
/// touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    #[default]
    Celsius,
    Fahrenheit,
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Celsius => "celsius",
            Self::Fahrenheit => "fahrenheit",
        })
    }
}

impl Scale {
    /// Convert a config-authored number in this scale to °C.
    pub fn to_celsius(self, value: f64) -> f64 {
        match self {
            Self::Celsius => value,
            Self::Fahrenheit => f_to_c(value),
        }
    }
}

pub fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

/// A NuHeat wire temperature → °C, or `None` if the result is implausible.
///
/// `None` means "do not publish this and say something is wrong" — see the
/// module note. It is not the same as a thermostat having no reading, which
/// arrives as an absent field and never reaches here.
pub fn decode_celsius(wire: i64) -> Option<f64> {
    let c = wire as f64 / HUNDREDTHS_PER_DEGREE;
    PLAUSIBLE_C.contains(&c).then_some(round_hundredths(c))
}

/// What this plugin will actually ask a thermostat for: the hardware's own
/// range, narrowed by whatever the operator configured.
///
/// The operator half matters more than it looks. A NuHeat will happily drive a
/// slab to 30 °C, and floor coverings do not survive that — engineered hardwood
/// is generally rated to about 27 °C. The thermostat enforces nothing, so
/// without a limit here a rule with a units mistake in it damages the floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SetpointLimits {
    pub min_c: f64,
    pub max_c: f64,
}

impl Default for SetpointLimits {
    fn default() -> Self {
        Self {
            min_c: SETPOINT_MIN_C,
            max_c: SETPOINT_MAX_C,
        }
    }
}

impl SetpointLimits {
    /// Configured limits, intersected with the hardware's.
    ///
    /// Intersected, not replaced: a config asking for 40 °C does not widen what
    /// the thermostat accepts, it just stops narrowing it. And a pair given the
    /// wrong way round — a minimum above the maximum, which is what a typo
    /// looks like — collapses to the hardware range rather than to an empty one
    /// that would clamp every setpoint to a single value.
    pub fn new(min_c: Option<f64>, max_c: Option<f64>) -> Self {
        let min = min_c.unwrap_or(SETPOINT_MIN_C).max(SETPOINT_MIN_C);
        let max = max_c.unwrap_or(SETPOINT_MAX_C).min(SETPOINT_MAX_C);
        if min > max {
            return Self::default();
        }
        Self {
            min_c: min,
            max_c: max,
        }
    }

    /// Whether a requested setpoint has to be pulled into range.
    pub fn would_clamp(&self, celsius: f64) -> bool {
        !(self.min_c..=self.max_c).contains(&celsius)
    }

    pub fn clamp(&self, celsius: f64) -> f64 {
        celsius.clamp(self.min_c, self.max_c)
    }
}

/// °C → a NuHeat wire temperature, clamped to what is allowed.
///
/// Clamping rather than erroring is deliberate: a rule driving the floor from
/// an outdoor sensor will eventually ask for something silly, and the useful
/// behaviour is "go to your maximum", the same as every physical thermostat.
/// The caller publishes back what the thermostat then reports, so the UI shows
/// the clamped value rather than the requested one.
pub fn encode_celsius_within(celsius: f64, limits: SetpointLimits) -> i64 {
    (limits.clamp(celsius) * HUNDREDTHS_PER_DEGREE).round() as i64
}

/// Two decimal places, which is all the wire format can carry anyway.
///
/// Without this, `2224 / 100.0` publishes as `22.240000000000002` on some
/// values and the UI renders the noise.
fn round_hundredths(c: f64) -> f64 {
    (c * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor: NuHeat's own documented hold example.
    ///
    /// Their reference sends `"setPointTemp": 3000` as an ordinary comfortable
    /// hold. That is 30.00 °C / 86 °F under this scale and 300 °C under the
    /// "1/10 °C" the prose claims, which is what settles which reading is real.
    #[test]
    fn the_documented_example_decodes_to_a_floor_temperature() {
        assert_eq!(decode_celsius(3000), Some(30.0));
        assert_eq!(c_to_f(30.0), 86.0);
    }

    #[test]
    fn a_room_temperature_round_trips() {
        // 72 °F, the number a person actually sets.
        let c = f_to_c(72.0);
        let wire = encode_celsius_within(c, SetpointLimits::default());
        assert_eq!(wire, 2222);
        let back = decode_celsius(wire).expect("plausible");
        assert!((c_to_f(back) - 72.0).abs() < 0.05, "{back} °C");
    }

    #[test]
    fn decoding_rejects_a_factor_of_ten_mistake() {
        // What a reading looks like if the scale were ever tenths instead:
        // 2100 tenths would be 210 °C, which no floor reaches.
        assert_eq!(decode_celsius(21_000), None);
        assert_eq!(decode_celsius(-9_000), None);
        // ...and the same numbers read correctly are fine.
        assert_eq!(decode_celsius(2100), Some(21.0));
    }

    #[test]
    fn setpoints_clamp_to_what_the_thermostat_accepts() {
        let hw = SetpointLimits::default();
        assert_eq!(encode_celsius_within(100.0, hw), 3000);
        assert_eq!(encode_celsius_within(-40.0, hw), 500);
        assert!(hw.would_clamp(100.0));
        assert!(hw.would_clamp(4.9));
        assert!(!hw.would_clamp(21.0));
    }

    /// The floor-covering limit: a rule asking for 30 °C on a floor configured
    /// to stop at 27 gets 27, not a damaged floor.
    #[test]
    fn a_configured_maximum_narrows_the_hardware_range() {
        let limits = SetpointLimits::new(None, Some(27.0));
        assert_eq!(limits.max_c, 27.0);
        assert_eq!(encode_celsius_within(30.0, limits), 2700);
        assert!(limits.would_clamp(28.0));
    }

    /// Config cannot widen what the thermostat accepts, only narrow it.
    #[test]
    fn a_configured_limit_cannot_exceed_the_hardware() {
        let limits = SetpointLimits::new(Some(-10.0), Some(45.0));
        assert_eq!(limits.min_c, SETPOINT_MIN_C);
        assert_eq!(limits.max_c, SETPOINT_MAX_C);
    }

    /// Limits entered the wrong way round are a typo, and taking them
    /// literally would pin every setpoint to one value.
    #[test]
    fn inverted_limits_fall_back_to_the_hardware_range() {
        let limits = SetpointLimits::new(Some(28.0), Some(18.0));
        assert_eq!(limits, SetpointLimits::default());
    }

    #[test]
    fn published_readings_carry_no_float_noise() {
        // 2224 / 100.0 is not exactly representable; the published value must
        // still be two decimal places.
        let c = decode_celsius(2224).expect("plausible");
        assert_eq!(c, 22.24);
        assert_eq!(format!("{c}"), "22.24");
    }

    #[test]
    fn config_limits_convert_from_the_scale_the_operator_wrote_them_in() {
        assert_eq!(Scale::Celsius.to_celsius(27.0), 27.0);
        assert!((Scale::Fahrenheit.to_celsius(82.0) - 27.78).abs() < 0.01);
    }
}
