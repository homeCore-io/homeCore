//! Z-Wave's scales ↔ homeCore's shared names and units.
//!
//! Every conversion between what a Z-Wave device reports or takes and what
//! `hc_types::device_vocabulary` says homeCore publishes lives here, as a pure
//! function with tests at both ends of its scale. The translator decides
//! *which* value is which; this decides *how much* it is.

/// The highest level a Multilevel Switch or Window Covering takes. `99` is
/// "fully on/open"; `255` on a write means "restore the previous level".
pub const LEVEL_MAX: u8 = 99;

/// Multilevel Switch `targetValue` that restores the level the dimmer had
/// before it was turned off (zwave-js passes it through to the device).
pub const LEVEL_RESTORE: u8 = 255;

/// Z-Wave level 0–99 → percent 0–100.
///
/// `99` is 100 %, and any level above zero is at least 1 % — a dimmer at its
/// lowest step is on, and a light that is on never reports 0 %. Out-of-range
/// levels (254 "unknown", 255) clamp to 100.
pub fn level_to_pct(level: f64) -> u8 {
    if level <= 0.0 || !level.is_finite() {
        return 0;
    }
    let level = level.min(f64::from(LEVEL_MAX));
    let pct = (level * 100.0 / f64::from(LEVEL_MAX)).round();
    pct.max(1.0) as u8
}

/// Percent 0–100 → Z-Wave level 0–99.
///
/// 100 % is 99 (never 255, which means "restore"); 0 % is 0 (off); anything
/// above 0 % is at least level 1.
pub fn pct_to_level(pct: f64) -> u8 {
    if pct <= 0.0 || !pct.is_finite() {
        return 0;
    }
    let pct = pct.min(100.0);
    let level = (pct * f64::from(LEVEL_MAX) / 100.0).round();
    level.clamp(1.0, f64::from(LEVEL_MAX)) as u8
}

/// A Window Covering slat angle (0–99, where 50 is open and both ends are
/// closed) → `tilt_pct` (100 is fully open).
pub fn slat_to_tilt_pct(level: f64) -> u8 {
    let level = level.clamp(0.0, f64::from(LEVEL_MAX));
    let from_open = (level - 50.0).abs();
    (100.0 - from_open * 2.0).round().clamp(0.0, 100.0) as u8
}

/// `tilt_pct` → slat angle, closing towards 0.
pub fn tilt_pct_to_slat(pct: f64) -> u8 {
    let pct = pct.clamp(0.0, 100.0);
    (pct / 2.0).round() as u8
}

fn round_to(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

/// Whether a Z-Wave unit string is Fahrenheit.
fn is_fahrenheit(unit: Option<&str>) -> bool {
    matches!(unit.map(str::trim), Some("°F" | "F" | "℉" | "degF"))
}

/// A temperature in the unit zwave-js names → °C, to two decimals.
/// No unit, or any Celsius spelling, is already °C.
pub fn to_celsius(v: f64, unit: Option<&str>) -> f64 {
    if is_fahrenheit(unit) {
        round_to((v - 32.0) * 5.0 / 9.0, 2)
    } else {
        v
    }
}

/// °C → the unit the device's setpoint is in. Fahrenheit thermostats take
/// whole degrees; Celsius ones a tenth.
pub fn from_celsius(c: f64, unit: Option<&str>) -> f64 {
    if is_fahrenheit(unit) {
        (c * 9.0 / 5.0 + 32.0).round()
    } else {
        round_to(c, 1)
    }
}

/// What a Multilevel Sensor (CC 49) value measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    Temperature,
    Humidity,
    Illuminance,
    Pressure,
    Power,
    Voltage,
    Current,
    Co2,
    Uv,
}

/// A Multilevel Sensor reading in the scale its metadata names → the name and
/// value homeCore publishes.
///
/// `None` when the unit is one this does not know: publishing it under a
/// shared name would claim a unit it is not in, so the caller falls back to
/// the raw synthetic name instead. A missing unit is taken to be the standard
/// one — zwave-js always sends metadata, and the old behaviour assumed it.
pub fn sensor_reading(sensor: Sensor, v: f64, unit: Option<&str>) -> Option<(&'static str, f64)> {
    let u = unit.map(|u| u.trim().to_ascii_lowercase());
    let u = u.as_deref();
    Some(match (sensor, u) {
        (Sensor::Temperature, None | Some("°c" | "c" | "℃")) => ("temperature", v),
        (Sensor::Temperature, Some("°f" | "f" | "℉")) => {
            ("temperature", to_celsius(v, Some("°F")))
        }
        (Sensor::Humidity, None | Some("%")) => ("humidity", v),
        // Absolute humidity is another measure, not relative humidity.
        (Sensor::Humidity, Some("g/m³" | "g/m3")) => ("absolute_humidity", v),
        (Sensor::Illuminance, None | Some("lux" | "lx")) => ("illuminance", v),
        // A percentage of the sensor's range is not lux.
        (Sensor::Illuminance, Some("%")) => ("light_level_pct", v),
        (Sensor::Pressure, None | Some("kpa")) => ("pressure", round_to(v * 10.0, 2)),
        (Sensor::Pressure, Some("inhg")) => ("pressure", round_to(v * 33.863_886, 2)),
        (Sensor::Pressure, Some("hpa" | "mbar")) => ("pressure", v),
        (Sensor::Power, None | Some("w")) => ("power_w", v),
        (Sensor::Power, Some("btu/h")) => ("power_w", round_to(v * 0.293_071_07, 2)),
        (Sensor::Voltage, None | Some("v")) => ("voltage", v),
        (Sensor::Voltage, Some("mv")) => ("voltage", round_to(v / 1000.0, 3)),
        (Sensor::Current, None | Some("a")) => ("current_a", v),
        (Sensor::Current, Some("ma")) => ("current_a", round_to(v / 1000.0, 3)),
        (Sensor::Co2, None | Some("ppm")) => ("co2", v),
        (Sensor::Uv, _) => ("uv_index", v),
        _ => return None,
    })
}

/// The white points a warm-white / cold-white pair is taken to span. Z-Wave's
/// Color Switch has no kelvin — only how bright each white channel is — so
/// colour temperature is the mix between these two.
pub const WARM_WHITE_K: u32 = 2700;
pub const COLD_WHITE_K: u32 = 6500;

/// Warm/cold white channel levels (0–255) → kelvin. `None` when both are off
/// (the bulb is showing colour, so it has no white temperature).
pub fn whites_to_kelvin(warm: f64, cold: f64) -> Option<u32> {
    let (warm, cold) = (warm.max(0.0), cold.max(0.0));
    if warm + cold <= 0.0 {
        return None;
    }
    let t = cold / (warm + cold);
    let span = f64::from(COLD_WHITE_K - WARM_WHITE_K);
    Some((f64::from(WARM_WHITE_K) + t * span).round() as u32)
}

/// Kelvin → `(warm, cold)` channel levels, the stronger channel at full.
pub fn kelvin_to_whites(k: f64) -> (u8, u8) {
    let span = f64::from(COLD_WHITE_K - WARM_WHITE_K);
    let t = ((k - f64::from(WARM_WHITE_K)) / span).clamp(0.0, 1.0);
    let (warm, cold) = (1.0 - t, t);
    let scale = 255.0 / warm.max(cold);
    ((warm * scale).round() as u8, (cold * scale).round() as u8)
}

/// A fade in milliseconds → a zwave-js `transitionDuration` string.
///
/// Z-Wave durations are whole seconds (and whole minutes past 127 s), so the
/// fade rounds to the nearest second: `"2s"`, `"1m30s"`.
pub fn transition_duration(ms: f64) -> String {
    let secs = (ms.max(0.0) / 1000.0).round() as u64;
    let (m, s) = (secs / 60, secs % 60);
    match (m, s) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s}s"),
    }
}

// ---------------------------------------------------------------------------
// Thermostat tables
// ---------------------------------------------------------------------------

/// Thermostat Mode (CC 64) number ↔ `hvac_mode`.
///
/// The shared names (`off`, `heat`, `cool`, `auto`, `fan_only`, `dry`) where
/// the mode is one of those; the rest keep their own lowercase name so a write
/// round-trips to the exact mode the device reported.
pub const HVAC_MODES: &[(u64, &str)] = &[
    (0, "off"),
    (1, "heat"),
    (2, "cool"),
    (3, "auto"),
    (4, "aux_heat"),
    (5, "resume"),
    (6, "fan_only"),
    (7, "furnace"),
    (8, "dry"),
    (9, "moist"),
    (10, "auto_changeover"),
    (11, "energy_heat"),
    (12, "energy_cool"),
    (13, "away"),
    (15, "full_power"),
    (31, "manufacturer_specific"),
];

/// Thermostat Operating State (CC 66) number → `hvac_action`, which is one of
/// `idle`, `heating`, `cooling`, `fan`, `drying`, `off`. "Pending" states are
/// not yet running, so they are idle.
pub fn hvac_action(state: u64) -> &'static str {
    match state {
        1 | 7 | 8 | 10 | 11 => "heating",
        2 | 9 => "cooling",
        3 | 6 => "fan",
        _ => "idle",
    }
}

/// Thermostat Fan Mode (CC 68) number ↔ `fan_mode`.
pub const FAN_MODES: &[(u64, &str)] = &[
    (0, "auto_low"),
    (1, "low"),
    (2, "auto_high"),
    (3, "high"),
    (4, "auto_medium"),
    (5, "medium"),
    (6, "circulation"),
    (7, "humidity_circulation"),
    (8, "left_right"),
    (9, "up_down"),
    (10, "quiet"),
    (11, "external_circulation"),
];

pub fn name_of(table: &[(u64, &'static str)], n: u64) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
}

pub fn number_of(table: &[(u64, &str)], name: &str) -> Option<u64> {
    table.iter().find(|(_, v)| *v == name).map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_to_pct_at_each_end_and_between() {
        assert_eq!(level_to_pct(0.0), 0);
        // The lowest step is on, so never 0 %.
        assert_eq!(level_to_pct(1.0), 1);
        assert_eq!(level_to_pct(50.0), 51);
        assert_eq!(level_to_pct(98.0), 99);
        assert_eq!(level_to_pct(99.0), 100);
        // 254 "unknown" / 255 never read above 100.
        assert_eq!(level_to_pct(255.0), 100);
        assert_eq!(level_to_pct(-3.0), 0);
    }

    #[test]
    fn pct_to_level_at_each_end_and_between() {
        assert_eq!(pct_to_level(0.0), 0);
        assert_eq!(pct_to_level(0.4), 1, "above 0 % is on");
        assert_eq!(pct_to_level(1.0), 1);
        assert_eq!(pct_to_level(50.0), 50);
        assert_eq!(pct_to_level(100.0), 99, "100 % is 99, never 255");
        assert_eq!(pct_to_level(150.0), 99);
        assert_eq!(pct_to_level(-1.0), 0);
    }

    #[test]
    fn levels_round_trip() {
        for level in 0..=99u8 {
            assert_eq!(
                pct_to_level(f64::from(level_to_pct(f64::from(level)))),
                level
            );
        }
    }

    #[test]
    fn slats_open_in_the_middle() {
        assert_eq!(slat_to_tilt_pct(50.0), 100);
        assert_eq!(slat_to_tilt_pct(0.0), 0);
        assert_eq!(slat_to_tilt_pct(99.0), 2);
        assert_eq!(slat_to_tilt_pct(25.0), 50);
        assert_eq!(tilt_pct_to_slat(100.0), 50);
        assert_eq!(tilt_pct_to_slat(0.0), 0);
        assert_eq!(tilt_pct_to_slat(50.0), 25);
    }

    #[test]
    fn temperatures_in_fahrenheit_become_celsius() {
        assert_eq!(to_celsius(32.0, Some("°F")), 0.0);
        assert_eq!(to_celsius(212.0, Some("°F")), 100.0);
        assert_eq!(to_celsius(72.0, Some("°F")), 22.22);
        assert_eq!(to_celsius(-40.0, Some("°F")), -40.0);
        assert_eq!(to_celsius(21.5, Some("°C")), 21.5);
        assert_eq!(to_celsius(21.5, None), 21.5);
    }

    #[test]
    fn setpoints_go_back_in_the_device_s_unit() {
        assert_eq!(from_celsius(22.22, Some("°F")), 72.0);
        assert_eq!(from_celsius(0.0, Some("°F")), 32.0);
        assert_eq!(from_celsius(21.56, Some("°C")), 21.6);
        assert_eq!(from_celsius(21.0, None), 21.0);
        // A published °F setpoint written back unchanged lands where it was.
        for f in 50..=90 {
            let c = to_celsius(f64::from(f), Some("°F"));
            assert_eq!(from_celsius(c, Some("°F")), f64::from(f));
        }
    }

    #[test]
    fn every_sensor_scale_converts_to_the_standard_unit() {
        use Sensor::*;
        let r = |s, v, u| sensor_reading(s, v, u);
        assert_eq!(
            r(Temperature, 21.5, Some("°C")),
            Some(("temperature", 21.5))
        );
        assert_eq!(
            r(Temperature, 68.0, Some("°F")),
            Some(("temperature", 20.0))
        );
        assert_eq!(r(Temperature, 21.5, None), Some(("temperature", 21.5)));
        assert_eq!(r(Humidity, 45.0, Some("%")), Some(("humidity", 45.0)));
        assert_eq!(
            r(Humidity, 9.0, Some("g/m³")),
            Some(("absolute_humidity", 9.0))
        );
        assert_eq!(
            r(Illuminance, 300.0, Some("Lux")),
            Some(("illuminance", 300.0))
        );
        assert_eq!(
            r(Illuminance, 40.0, Some("%")),
            Some(("light_level_pct", 40.0))
        );
        assert_eq!(
            r(Pressure, 101.325, Some("kPa")),
            Some(("pressure", 1013.25))
        );
        assert_eq!(
            r(Pressure, 29.92, Some("inHg")),
            Some(("pressure", 1013.21))
        );
        assert_eq!(r(Power, 100.0, Some("W")), Some(("power_w", 100.0)));
        assert_eq!(r(Power, 1000.0, Some("BTU/h")), Some(("power_w", 293.07)));
        assert_eq!(r(Voltage, 120.0, Some("V")), Some(("voltage", 120.0)));
        assert_eq!(r(Voltage, 3300.0, Some("mV")), Some(("voltage", 3.3)));
        assert_eq!(r(Current, 1.5, Some("A")), Some(("current_a", 1.5)));
        assert_eq!(r(Current, 250.0, Some("mA")), Some(("current_a", 0.25)));
        assert_eq!(r(Co2, 600.0, Some("ppm")), Some(("co2", 600.0)));
        assert_eq!(r(Uv, 3.0, Some("UV index")), Some(("uv_index", 3.0)));
        // An unknown scale is never published under a shared name.
        assert_eq!(r(Temperature, 300.0, Some("K")), None);
        assert_eq!(r(Pressure, 1.0, Some("psi")), None);
    }

    #[test]
    fn white_channels_mix_to_kelvin_and_back() {
        assert_eq!(whites_to_kelvin(255.0, 0.0), Some(2700));
        assert_eq!(whites_to_kelvin(0.0, 255.0), Some(6500));
        assert_eq!(whites_to_kelvin(255.0, 255.0), Some(4600));
        assert_eq!(whites_to_kelvin(0.0, 0.0), None);
        assert_eq!(kelvin_to_whites(2700.0), (255, 0));
        assert_eq!(kelvin_to_whites(2000.0), (255, 0));
        assert_eq!(kelvin_to_whites(6500.0), (0, 255));
        assert_eq!(kelvin_to_whites(4600.0), (255, 255));
        let (w, c) = kelvin_to_whites(3650.0);
        assert_eq!(whites_to_kelvin(w.into(), c.into()), Some(3650));
    }

    #[test]
    fn fades_are_whole_seconds() {
        assert_eq!(transition_duration(0.0), "0s");
        assert_eq!(transition_duration(400.0), "0s");
        assert_eq!(transition_duration(1500.0), "2s");
        assert_eq!(transition_duration(59_000.0), "59s");
        assert_eq!(transition_duration(60_000.0), "1m");
        assert_eq!(transition_duration(90_000.0), "1m30s");
    }

    #[test]
    fn thermostat_tables() {
        assert_eq!(name_of(HVAC_MODES, 6), Some("fan_only"));
        assert_eq!(name_of(HVAC_MODES, 8), Some("dry"));
        assert_eq!(number_of(HVAC_MODES, "auto"), Some(3));
        assert_eq!(name_of(HVAC_MODES, 14), None);
        for (n, name) in HVAC_MODES {
            assert_eq!(number_of(HVAC_MODES, name), Some(*n));
        }
        assert_eq!(hvac_action(0), "idle");
        assert_eq!(hvac_action(1), "heating");
        assert_eq!(hvac_action(2), "cooling");
        assert_eq!(hvac_action(3), "fan");
        assert_eq!(hvac_action(4), "idle");
        assert_eq!(hvac_action(9), "cooling");
        assert_eq!(hvac_action(11), "heating");
        assert_eq!(name_of(FAN_MODES, 1), Some("low"));
        assert_eq!(number_of(FAN_MODES, "circulation"), Some(6));
    }
}
