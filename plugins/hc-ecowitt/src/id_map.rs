//! Maps Ecowitt common_list hex IDs to human-readable attribute names.

/// Convert a common_list item ID (e.g. "0x02") to an attribute name.
pub fn common_id_to_attr(id: &str) -> Option<&'static str> {
    match id {
        "0x01" | "0x1" => Some("indoor_temperature"),
        "0x02" | "0x2" => Some("outdoor_temperature"),
        "0x03" | "0x3" => Some("dewpoint"),
        "0x04" | "0x4" => Some("windchill"),
        "0x05" | "0x5" => Some("heat_index"),
        "0x06" | "0x6" => Some("indoor_humidity"),
        "0x07" | "0x7" => Some("outdoor_humidity"),
        "0x08" | "0x8" => Some("barometric_abs"),
        "0x09" | "0x9" => Some("barometric_rel"),
        "0x0A" | "0xA" => Some("wind_direction"),
        "0x0B" | "0xB" => Some("wind_speed"),
        "0x0C" | "0xC" => Some("gust_speed"),
        "0x0D" | "0xD" => Some("rain_event"),
        "0x0E" | "0xE" => Some("rain_rate"),
        "0x0F" | "0xF" => Some("rain_gain"),
        "0x10" => Some("rain_day"),
        "0x11" => Some("rain_week"),
        "0x12" => Some("rain_month"),
        "0x13" => Some("rain_year"),
        "0x14" => Some("rain_total"),
        "0x15" => Some("light"),
        "0x16" => Some("uv"),
        "0x17" => Some("uvi"),
        "0x18" => Some("datetime"),
        "0x19" => Some("daily_max_wind"),
        // Feel-like / apparent temperature (id "3" without 0x prefix in some firmware)
        "3" => Some("feels_like"),
        // Vapor pressure deficit (added in firmware v1.0.4)
        "5" => Some("vpd"),
        // Wet-bulb globe temperature (added in firmware v1.0.6)
        "0x6D" => Some("wbgt"),
        _ => None,
    }
}

/// Parse a value string, stripping common unit suffixes.
/// Returns (numeric_value, unit_string).
pub fn parse_value(val: &str) -> (f64, &str) {
    let val = val.trim();

    // Try stripping known suffixes
    for (suffix, unit) in &[
        (" mph", "mph"),
        (" km/h", "km/h"),
        (" m/s", "m/s"),
        (" in", "in"),
        (" mm", "mm"),
        (" in/Hr", "in/hr"),
        (" mm/Hr", "mm/hr"),
        (" w/m2", "w/m2"),
        (" W/m2", "W/m2"),
        (" inHg", "inHg"),
        (" hpa", "hpa"),
        (" hPa", "hPa"),
        ("%", "%"),
    ] {
        if let Some(num) = val.strip_suffix(suffix) {
            if let Ok(v) = num.trim().parse::<f64>() {
                return (v, unit);
            }
        }
    }

    // Try direct parse (no suffix)
    if let Ok(v) = val.parse::<f64>() {
        return (v, "");
    }

    (0.0, "")
}
