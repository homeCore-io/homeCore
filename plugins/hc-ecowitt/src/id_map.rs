//! What each reading is called and what it measures, for both ways a gateway
//! reports: the `get_livedata_info` poll (hex ids, values with their units)
//! and the custom-server upload (`form_parser`, imperial field names).
//!
//! **One name per reading on both paths.** The poll used to call the outdoor
//! temperature `outdoor_temperature` while the upload called it `temperature`,
//! so an automation written against one broke when the plugin was switched to
//! the other. Both paths now take their names from here, and their values
//! through [`Quantity::convert`] into homeCore's units.

use crate::units;
use serde_json::{json, Value};

/// What a reading measures, which says how its value converts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quantity {
    /// → °C.
    Temperature,
    /// A percentage, as sent.
    Percent,
    /// → hPa.
    Pressure,
    /// → kPa (vapour-pressure deficit).
    PressureKpa,
    /// → m/s.
    Wind,
    /// Degrees from north, as sent.
    Direction,
    /// → mm, or mm/h for a rate.
    Rain,
    /// → W/m².
    Solar,
    /// A plain number (UV index, counts, µg/m³, ppm), as sent.
    Plain,
}

impl Quantity {
    /// `v` in `unit` as homeCore's unit for this quantity, or `None` when the
    /// unit is one this cannot convert — skipped rather than mislabelled.
    pub fn convert(self, v: f64, unit: &str) -> Option<f64> {
        match self {
            Quantity::Temperature => units::temperature_c(v, unit),
            Quantity::Pressure => units::pressure_hpa(v, unit),
            Quantity::PressureKpa if unit.trim().is_empty() => Some(units::round2(v)),
            Quantity::PressureKpa => units::pressure_kpa(v, unit),
            Quantity::Wind => units::wind_ms(v, unit),
            Quantity::Rain => units::rain_mm(v, unit),
            Quantity::Solar => units::solar_wm2(v, unit),
            Quantity::Percent | Quantity::Direction | Quantity::Plain => Some(v),
        }
    }

    /// A reading string with its unit (`"29.92 inHg"`, or a bare number whose
    /// unit is `unit_hint`) as the published JSON value; `None` when unreadable.
    pub fn read(self, val: &str, unit_hint: &str) -> Option<Value> {
        let (v, parsed) = units::parse_reading(val)?;
        let unit = if parsed.is_empty() || parsed == "%" {
            unit_hint
        } else {
            parsed
        };
        self.convert(v, unit).map(|x| json!(x))
    }
}

/// Which device a `common_list` reading belongs to. The poll lists the indoor
/// sensor's readings and the rain gauge's beside the outdoor ones on some
/// firmware; the upload never did, so they are sent where the upload sends them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dest {
    Weather,
    Indoor,
    Rain,
}

/// A `common_list` item id (e.g. `"0x02"`) → where it goes, its name, and what
/// it measures.
pub fn common_id(id: &str) -> Option<(Dest, &'static str, Quantity)> {
    use Dest::*;
    use Quantity::*;
    if let Some(attr) = rain_id(id) {
        return Some((Dest::Rain, attr, Quantity::Rain));
    }
    Some(match id {
        "0x01" | "0x1" => (Indoor, "temperature", Temperature),
        "0x02" | "0x2" => (Weather, "temperature", Temperature),
        "0x03" | "0x3" => (Weather, "dewpoint", Temperature),
        "0x04" | "0x4" => (Weather, "windchill", Temperature),
        "0x05" | "0x5" => (Weather, "heat_index", Temperature),
        "0x06" | "0x6" => (Indoor, "humidity", Percent),
        "0x07" | "0x7" => (Weather, "humidity", Percent),
        "0x08" | "0x8" => (Weather, "pressure_absolute", Pressure),
        "0x09" | "0x9" => (Weather, "pressure", Pressure),
        "0x0A" | "0xA" => (Weather, "wind_direction", Direction),
        "0x0B" | "0xB" => (Weather, "wind_speed", Wind),
        "0x0C" | "0xC" => (Weather, "wind_gust", Wind),
        "0x15" => (Weather, "solar_radiation", Solar),
        // 0x16 is the raw UV radiation; the index (0x17) is what anyone
        // reads, and it is the only one the upload sends.
        "0x17" => (Weather, "uvi", Plain),
        "0x19" => (Weather, "wind_gust_daily_max", Wind),
        // Feels-like / apparent temperature (id "3" without 0x in some firmware)
        "3" => (Weather, "feels_like", Temperature),
        // Vapour-pressure deficit (firmware v1.0.4)
        "5" => (Weather, "vpd", PressureKpa),
        // Wet-bulb globe temperature (firmware v1.0.6)
        "0x6D" => (Weather, "wbgt", Temperature),
        _ => return None,
    })
}

/// A rain item id → its name. The same ids appear in `rain`, `piezoRain` and,
/// on some firmware, `common_list`.
pub fn rain_id(id: &str) -> Option<&'static str> {
    Some(match id {
        "0x0D" | "0xD" => "rain_event_mm",
        "0x0E" | "0xE" => "rain_rate_mm_hr",
        "0x0F" | "0xF" => "rain_hourly_mm",
        "0x10" => "rain_daily_mm",
        "0x11" => "rain_weekly_mm",
        "0x12" => "rain_monthly_mm",
        "0x13" => "rain_yearly_mm",
        "0x14" => "rain_total_mm",
        "0x7C" => "rain_24h_mm",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_convert_by_the_unit_they_carry() {
        assert_eq!(
            Quantity::Pressure.read("29.92 inHg", ""),
            Some(json!(1013.21))
        );
        assert_eq!(Quantity::Wind.read("10.0 mph", ""), Some(json!(4.47)));
        assert_eq!(Quantity::Rain.read("0.50 in/Hr", ""), Some(json!(12.7)));
        assert_eq!(Quantity::Temperature.read("68.0", "F"), Some(json!(20.0)));
        assert_eq!(Quantity::Percent.read("45%", ""), Some(json!(45.0)));
        assert_eq!(Quantity::Solar.read("512.3 W/m2", ""), Some(json!(512.3)));
    }

    #[test]
    fn unreadable_values_are_skipped_not_zero() {
        assert_eq!(Quantity::Temperature.read("--", "F"), None);
        assert_eq!(Quantity::Temperature.read("21.0", ""), None, "unit unknown");
        assert_eq!(Quantity::Wind.read("3 bft", ""), None);
    }

    #[test]
    fn the_outdoor_temperature_has_the_uploads_name() {
        assert_eq!(
            common_id("0x02"),
            Some((Dest::Weather, "temperature", Quantity::Temperature))
        );
        assert_eq!(common_id("0x01").unwrap().0, Dest::Indoor);
        assert_eq!(
            common_id("0x10").unwrap(),
            (Dest::Rain, "rain_daily_mm", Quantity::Rain)
        );
        assert_eq!(common_id("0x16"), None);
    }
}
