//! What a place's weather device reports. Nothing is writable: it is a
//! reading of the sky, not something to command.
//!
//! The names are homeCore's (`device_vocabulary`) where it has one, and
//! Ecowitt's where the app already reads it, so the app's Weather card shows
//! this device and a weather station the same way.

use crate::conditions::CONDITIONS;
use crate::openmeteo::AQI_CATEGORIES;
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use std::collections::HashMap;

/// A read-only reading.
///
/// Built from [`AttributeSchema::read_only`], never `..Default::default()`:
/// the SDK's `Default` mirrors the wire default, which is `writable: true`,
/// so a literal that forgets `writable: false` publishes a control that does
/// nothing.
fn reading(kind: AttributeKind, name: &str, unit: Option<&str>) -> AttributeSchema {
    AttributeSchema {
        unit: unit.map(str::to_string),
        ..AttributeSchema::read_only(kind).labelled(name)
    }
}

pub fn device_schema() -> DeviceSchema {
    use AttributeKind::*;
    let mut a = HashMap::new();
    let mut put = |k: &str, s: AttributeSchema| {
        a.insert(k.to_string(), s);
    };

    put("temperature", reading(Float, "Temperature", Some("°C")));
    put("feels_like", reading(Float, "Feels like", Some("°C")));
    put("humidity", reading(Float, "Humidity", Some("%")));
    put("pressure", reading(Float, "Pressure", Some("hPa")));
    put("wind_speed", reading(Float, "Wind", Some("m/s")));
    put("wind_gust", reading(Float, "Gusts", Some("m/s")));
    put("wind_direction", reading(Integer, "Wind from", Some("°")));
    put(
        "precipitation_mm",
        reading(Float, "Rain, last 15 min", Some("mm")),
    );
    put("dewpoint", reading(Float, "Dew point", Some("°C")));
    put(
        "rain_next_hour_mm",
        reading(Float, "Rain, next hour", Some("mm")),
    );
    put(
        "rain_starts_in_min",
        reading(Integer, "Rain starts in", Some("min")),
    );
    put(
        "rain_soon",
        AttributeSchema {
            display_name: Some("Rain within 30 min".into()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("rain soon", "expects rain soon"),
                when_false: StateLabel::verbed("dry", "expects no rain soon"),
            }),
            ..AttributeSchema::read_only(Bool)
        },
    );
    put("cloud_cover_pct", reading(Float, "Cloud cover", Some("%")));
    put("uvi", reading(Float, "UV index", None));
    put("today_high", reading(Float, "Today's high", Some("°C")));
    put("today_low", reading(Float, "Today's low", Some("°C")));
    put("tonight_low", reading(Float, "Tonight's low", Some("°C")));
    put(
        "freeze_tonight",
        AttributeSchema {
            display_name: Some("Freezing tonight".into()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("freezing", "expects a freeze"),
                when_false: StateLabel::verbed("above freezing", "expects no freeze"),
            }),
            ..AttributeSchema::read_only(Bool)
        },
    );
    put(
        "tomorrow_high",
        reading(Float, "Tomorrow's high", Some("°C")),
    );
    put("tomorrow_low", reading(Float, "Tomorrow's low", Some("°C")));
    put(
        "rain_chance_pct",
        reading(Float, "Chance of rain today", Some("%")),
    );
    put("snow_today_mm", reading(Float, "Snow today", Some("mm")));
    put("aqi", reading(Integer, "Air quality (US AQI)", None));
    put(
        "aqi_category",
        AttributeSchema {
            display_name: Some("Air quality".into()),
            options: Some(
                AQI_CATEGORIES
                    .iter()
                    .map(|(v, l)| AttributeOption::new(*v).labelled(*l))
                    .collect(),
            ),
            ..AttributeSchema::read_only(Enum)
        },
    );
    put("pm25_ugm3", reading(Float, "PM2.5", Some("µg/m³")));
    put("ozone_ugm3", reading(Float, "Ozone", Some("µg/m³")));
    put(
        "is_day",
        AttributeSchema {
            display_name: Some("Daylight".into()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("day", "turns to day"),
                when_false: StateLabel::verbed("night", "turns to night"),
            }),
            ..AttributeSchema::read_only(Bool)
        },
    );
    put(
        "condition",
        AttributeSchema {
            display_name: Some("Sky".into()),
            options: Some(
                CONDITIONS
                    .iter()
                    .map(|(v, l, i)| AttributeOption::new(*v).labelled(*l).icon(*i))
                    .collect(),
            ),
            ..AttributeSchema::read_only(Enum)
        },
    );
    put("forecast", reading(Json, "Seven days", None));
    put("forecast_hourly", reading(Json, "The next 24 hours", None));
    put("alert_count", reading(Integer, "Weather warnings", None));
    put(
        "alert_level",
        AttributeSchema {
            display_name: Some("Warning level".into()),
            min: Some(0.0),
            max: Some(3.0),
            options: Some(vec![
                AttributeOption::new("0").labelled("None"),
                AttributeOption::new("1").labelled("Advisory"),
                AttributeOption::new("2").labelled("Watch"),
                AttributeOption::new("3").labelled("Warning"),
            ]),
            ..AttributeSchema::read_only(Integer)
        },
    );
    put(
        "alert_headline",
        reading(String, "Most serious warning", None),
    );

    DeviceSchema {
        attributes: a,
        primary: [
            "temperature",
            "condition",
            "humidity",
            "wind_speed",
            "alert_level",
        ]
        .map(str::to_string)
        .to_vec(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A weather device is a reading of the sky. A writable attribute would
    /// render a control that does nothing, and `AttributeSchema::default()`
    /// is writable, so this is pinned rather than trusted.
    #[test]
    fn nothing_is_writable() {
        let s = device_schema();
        assert!(!s.attributes.is_empty());
        for (name, attr) in &s.attributes {
            assert!(!attr.writable, "{name} claims to be writable");
        }
        // And on the wire, where the absent-field default is also `true`.
        let wire = serde_json::to_value(&s).unwrap();
        for (name, attr) in wire["attributes"].as_object().unwrap() {
            assert_eq!(attr["writable"], serde_json::json!(false), "{name}");
        }
    }

    #[test]
    fn every_reading_has_a_display_name() {
        for (name, attr) in &device_schema().attributes {
            let label = attr.display_name.as_deref().unwrap_or("");
            assert!(!label.is_empty(), "{name} has no display name");
        }
    }
}
