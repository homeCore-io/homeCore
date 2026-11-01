//! What the sky is doing, in ten words.
//!
//! Open-Meteo reports the WMO "present weather" code (0 to 99, about thirty
//! in use). People and automations want fewer and plainer: `condition` is one
//! of [`CONDITIONS`], the same values on the device and in each forecast day,
//! so the app finds a day's label and icon in the attribute's options.
//!
//! Day and night are `is_day`, not separate values: "clear" at night is still
//! clear, and an automation asking for clear skies shouldn't need two words.

/// Each condition: its value, what to call it, and the icon a client shows
/// (a semantic name, as the app's icon set spells it).
pub const CONDITIONS: [(&str, &str, &str); 10] = [
    ("clear", "Clear", "sun"),
    ("partly_cloudy", "Partly cloudy", "cloud-sun"),
    ("cloudy", "Cloudy", "cloud"),
    ("fog", "Fog", "cloud-fog"),
    ("drizzle", "Drizzle", "cloud-drizzle"),
    ("rain", "Rain", "cloud-rain"),
    ("freezing_rain", "Freezing rain", "cloud-hail"),
    ("snow", "Snow", "cloud-snow"),
    ("showers", "Showers", "cloud-rain-wind"),
    ("thunderstorm", "Thunderstorm", "cloud-lightning"),
];

/// A WMO weather code as a condition; `None` for a code WMO doesn't use for
/// present weather, so a new one is never shown as something it isn't.
pub fn from_wmo(code: i64) -> Option<&'static str> {
    Some(match code {
        0 => "clear",
        1 | 2 => "partly_cloudy",
        3 => "cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 | 66 | 67 => "freezing_rain",
        61 | 63 | 65 => "rain",
        71 | 73 | 75 | 77 | 85 | 86 => "snow",
        80..=82 => "showers",
        95 | 96 | 99 => "thunderstorm",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_open_meteo_documents_has_a_condition() {
        let documented = [
            0, 1, 2, 3, 45, 48, 51, 53, 55, 56, 57, 61, 63, 65, 66, 67, 71, 73, 75, 77, 80, 81, 82,
            85, 86, 95, 96, 99,
        ];
        for code in documented {
            let c = from_wmo(code).unwrap_or_else(|| panic!("code {code} has no condition"));
            assert!(
                CONDITIONS.iter().any(|(v, _, _)| *v == c),
                "{c} isn't declared"
            );
        }
    }

    #[test]
    fn an_unknown_code_is_not_guessed() {
        assert_eq!(from_wmo(4), None);
        assert_eq!(from_wmo(-1), None);
    }

    #[test]
    fn the_obvious_ones() {
        assert_eq!(from_wmo(0), Some("clear"));
        assert_eq!(from_wmo(3), Some("cloudy"));
        assert_eq!(from_wmo(63), Some("rain"));
        assert_eq!(from_wmo(95), Some("thunderstorm"));
    }
}
