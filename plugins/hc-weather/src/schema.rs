//! What a place's weather device reports. Nothing is writable: it is a
//! reading of the sky, not something to command.
//!
//! The names are homeCore's (`device_vocabulary`) where it has one, and
//! Ecowitt's where the app already reads it, so the app's Weather card shows
//! this device and a weather station the same way.

use crate::conditions::CONDITIONS;
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use std::collections::HashMap;

fn reading(kind: AttributeKind, name: &str, unit: Option<&str>) -> AttributeSchema {
    AttributeSchema {
        kind,
        display_name: Some(name.to_string()),
        unit: unit.map(str::to_string),
        ..Default::default()
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
    put("cloud_cover_pct", reading(Float, "Cloud cover", Some("%")));
    put("uvi", reading(Float, "UV index", None));
    put("today_high", reading(Float, "Today's high", Some("°C")));
    put("today_low", reading(Float, "Today's low", Some("°C")));
    put(
        "rain_chance_pct",
        reading(Float, "Chance of rain today", Some("%")),
    );
    put("aqi", reading(Integer, "Air quality (US AQI)", None));
    put("pm25_ugm3", reading(Float, "PM2.5", Some("µg/m³")));
    put("ozone_ugm3", reading(Float, "Ozone", Some("µg/m³")));
    put(
        "is_day",
        AttributeSchema {
            kind: Bool,
            display_name: Some("Daylight".into()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("day", "turns to day"),
                when_false: StateLabel::verbed("night", "turns to night"),
            }),
            ..Default::default()
        },
    );
    put(
        "condition",
        AttributeSchema {
            kind: Enum,
            display_name: Some("Sky".into()),
            options: Some(
                CONDITIONS
                    .iter()
                    .map(|(v, l, i)| AttributeOption::new(*v).labelled(*l).icon(*i))
                    .collect(),
            ),
            ..Default::default()
        },
    );
    put("forecast", reading(Json, "Seven days", None));
    put("forecast_hourly", reading(Json, "The next 24 hours", None));
    put("alert_count", reading(Integer, "Weather warnings", None));
    put(
        "alert_level",
        AttributeSchema {
            kind: Integer,
            display_name: Some("Warning level".into()),
            min: Some(0.0),
            max: Some(3.0),
            options: Some(vec![
                AttributeOption::new("0").labelled("None"),
                AttributeOption::new("1").labelled("Advisory"),
                AttributeOption::new("2").labelled("Watch"),
                AttributeOption::new("3").labelled("Warning"),
            ]),
            ..Default::default()
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
