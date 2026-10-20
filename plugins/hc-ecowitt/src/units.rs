//! Ecowitt's units, turned into homeCore's.
//!
//! A gateway sends whatever its display is set to — °F or °C, inHg or hPa,
//! mph or m/s, inches or millimetres — and the upload protocol is imperial
//! whatever the display says. homeCore's devices speak one set (see
//! `hc_types::device_vocabulary`): **°C, hPa, m/s, mm, km, W/m²**. Every
//! reading is converted here, at the edge, and the app shows °F or inches
//! from the person's own setting.
//!
//! A value that does not parse — Ecowitt sends `--` for a sensor it has lost —
//! or a unit this module does not know is **skipped**, never published as 0:
//! a 0 °C reading from a dead probe is a frost alert nobody asked for.

/// Round to two decimals, which is finer than any Ecowitt sensor resolves.
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// °F → °C.
pub fn f_to_c(f: f64) -> f64 {
    round2((f - 32.0) * 5.0 / 9.0)
}

/// inHg → hPa.
pub fn inhg_to_hpa(inhg: f64) -> f64 {
    round2(inhg * 33.863_886)
}

/// mph → m/s.
pub fn mph_to_ms(mph: f64) -> f64 {
    round2(mph * 0.447_04)
}

/// Inches → millimetres (rain, and rain rate per hour).
pub fn in_to_mm(inches: f64) -> f64 {
    round2(inches * 25.4)
}

/// Miles → kilometres.
pub fn mi_to_km(mi: f64) -> f64 {
    round2(mi * 1.609_344)
}

/// Lux → W/m², by the sunlight factor Ecowitt itself uses (126.7 lux per
/// W/m²). Only meaningful for daylight, which is all a weather station sees.
pub fn lux_to_wm2(lux: f64) -> f64 {
    round2(lux / 126.7)
}

/// A reading string — `"29.92 inHg"`, `"45%"`, `"21.4"`, `"--"` — split into
/// its number and whatever unit follows it. `None` when there is no number.
pub fn parse_reading(s: &str) -> Option<(f64, &str)> {
    let s = s.trim();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || ((c == '-' || c == '+') && i == 0)))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let v: f64 = s[..end].parse().ok()?;
    v.is_finite().then_some((v, s[end..].trim()))
}

/// A temperature in `unit` (`"F"`, `"°F"`, `"℉"`, `"C"`, …) as °C. `None` for
/// a unit that is missing or unknown — guessing would be a 20-degree error.
pub fn temperature_c(v: f64, unit: &str) -> Option<f64> {
    match unit
        .trim()
        .trim_start_matches('°')
        .to_ascii_lowercase()
        .as_str()
    {
        "f" | "℉" => Some(f_to_c(v)),
        "c" | "℃" => Some(round2(v)),
        _ => None,
    }
}

/// A pressure in `unit` as hPa.
pub fn pressure_hpa(v: f64, unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "hpa" | "mbar" | "mb" => Some(round2(v)),
        "inhg" => Some(inhg_to_hpa(v)),
        "mmhg" => Some(round2(v * 1.333_224)),
        "kpa" => Some(round2(v * 10.0)),
        _ => None,
    }
}

/// A pressure in `unit` as kPa (vapour-pressure deficit).
pub fn pressure_kpa(v: f64, unit: &str) -> Option<f64> {
    pressure_hpa(v, unit).map(|h| round2(h / 10.0))
}

/// A wind speed in `unit` as m/s. Beaufort is a scale, not a speed: skipped.
pub fn wind_ms(v: f64, unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "m/s" => Some(round2(v)),
        "mph" => Some(mph_to_ms(v)),
        "km/h" | "kmh" | "kph" => Some(round2(v / 3.6)),
        "knot" | "knots" | "kn" | "kt" => Some(round2(v * 0.514_444)),
        "ft/s" | "fps" => Some(round2(v * 0.3048)),
        _ => None,
    }
}

/// A rain amount — or a rate, per hour — in `unit` as mm.
pub fn rain_mm(v: f64, unit: &str) -> Option<f64> {
    let u = unit.trim().to_ascii_lowercase();
    match u
        .strip_suffix("/hr")
        .or_else(|| u.strip_suffix("/h"))
        .unwrap_or(&u)
    {
        "mm" => Some(round2(v)),
        "in" => Some(in_to_mm(v)),
        _ => None,
    }
}

/// A distance in `unit` as km.
pub fn distance_km(v: f64, unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "km" => Some(round2(v)),
        "mi" | "mile" | "miles" => Some(mi_to_km(v)),
        _ => None,
    }
}

/// A short distance (a water level) in `unit` as mm.
pub fn length_mm(v: f64, unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "mm" => Some(round2(v)),
        "cm" => Some(round2(v * 10.0)),
        "m" => Some(round2(v * 1000.0)),
        "in" => Some(in_to_mm(v)),
        "ft" => Some(round2(v * 304.8)),
        _ => None,
    }
}

/// Solar radiation in `unit` as W/m². The gateway can show it as light
/// (lux, klux, foot-candles) instead, which converts by the daylight factor.
pub fn solar_wm2(v: f64, unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "w/m2" | "w/m²" => Some(round2(v)),
        "lux" => Some(lux_to_wm2(v)),
        "klux" => Some(lux_to_wm2(v * 1000.0)),
        "fc" => Some(lux_to_wm2(v * 10.763_91)),
        "kfc" => Some(lux_to_wm2(v * 10_763.91)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fahrenheit_to_celsius() {
        assert_eq!(f_to_c(32.0), 0.0);
        assert_eq!(f_to_c(212.0), 100.0);
        assert_eq!(f_to_c(-40.0), -40.0);
        assert_eq!(f_to_c(0.0), -17.78);
        assert_eq!(f_to_c(72.5), 22.5);
        assert_eq!(f_to_c(1.0), -17.22);
    }

    #[test]
    fn inhg_to_hectopascals() {
        assert_eq!(inhg_to_hpa(0.0), 0.0);
        assert_eq!(inhg_to_hpa(1.0), 33.86);
        assert_eq!(inhg_to_hpa(29.92), 1013.21);
        assert_eq!(pressure_hpa(1013.2, "hPa"), Some(1013.2));
        assert_eq!(pressure_hpa(760.0, "mmHg"), Some(1013.25));
        assert_eq!(pressure_hpa(1.0, "furlongs"), None);
    }

    #[test]
    fn mph_to_metres_per_second() {
        assert_eq!(mph_to_ms(0.0), 0.0);
        assert_eq!(mph_to_ms(1.0), 0.45);
        assert_eq!(mph_to_ms(10.0), 4.47);
        assert_eq!(mph_to_ms(100.0), 44.7);
        assert_eq!(wind_ms(36.0, "km/h"), Some(10.0));
        assert_eq!(wind_ms(10.0, "knots"), Some(5.14));
        assert_eq!(wind_ms(3.0, "m/s"), Some(3.0));
        assert_eq!(wind_ms(4.0, "bft"), None, "Beaufort is not a speed");
    }

    #[test]
    fn inches_to_millimetres() {
        assert_eq!(in_to_mm(0.0), 0.0);
        assert_eq!(in_to_mm(0.01), 0.25);
        assert_eq!(in_to_mm(1.0), 25.4);
        assert_eq!(in_to_mm(12.34), 313.44);
        assert_eq!(rain_mm(0.5, "in/Hr"), Some(12.7));
        assert_eq!(rain_mm(3.2, "mm/Hr"), Some(3.2));
        assert_eq!(rain_mm(3.2, "mm"), Some(3.2));
    }

    #[test]
    fn other_scales() {
        assert_eq!(mi_to_km(1.0), 1.61);
        assert_eq!(distance_km(12.0, "km"), Some(12.0));
        assert_eq!(solar_wm2(126.7, "lux"), Some(1.0));
        assert_eq!(solar_wm2(12.67, "Klux"), Some(100.0));
        assert_eq!(solar_wm2(512.3, "W/m2"), Some(512.3));
        assert_eq!(length_mm(2.0, "in"), Some(50.8));
        assert_eq!(pressure_kpa(0.1, "inHg"), Some(0.34));
    }

    #[test]
    fn temperatures_need_a_known_unit() {
        assert_eq!(temperature_c(68.0, "F"), Some(20.0));
        assert_eq!(temperature_c(68.0, "°F"), Some(20.0));
        assert_eq!(temperature_c(68.0, "℉"), Some(20.0));
        assert_eq!(temperature_c(20.0, "C"), Some(20.0));
        assert_eq!(temperature_c(20.0, "℃"), Some(20.0));
        assert_eq!(temperature_c(20.0, ""), None);
    }

    #[test]
    fn readings_split_into_number_and_unit() {
        assert_eq!(parse_reading("29.92 inHg"), Some((29.92, "inHg")));
        assert_eq!(parse_reading("45%"), Some((45.0, "%")));
        assert_eq!(parse_reading(" 21.4 "), Some((21.4, "")));
        assert_eq!(parse_reading("-3.5"), Some((-3.5, "")));
        assert_eq!(parse_reading("0.00 in/Hr"), Some((0.0, "in/Hr")));
        assert_eq!(parse_reading("--"), None);
        assert_eq!(parse_reading("---.-"), None);
        assert_eq!(parse_reading(""), None);
    }
}
