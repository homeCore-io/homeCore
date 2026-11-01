//! Open-Meteo: the weather now, the forecast and the air.
//!
//! Three calls, each on its own timer: `current` (the data changes every
//! 15 minutes, its `interval`), the daily and hourly forecast (every half
//! hour), and air quality (hourly data). No key. Open-Meteo is asked for
//! homeCore's units (°C, hPa, m/s, mm), so nothing here converts; each answer's
//! `*_units` is checked against what was asked, and a mismatch is an error
//! rather than a wrong number.
//!
//! Data: Open-Meteo.com, CC BY 4.0. The credit is on the plugin's page and in
//! its README.

use crate::conditions;
use anyhow::{bail, Context, Result};
use chrono::{FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{json, Map, Value};

const CURRENT: &str = "temperature_2m,apparent_temperature,relative_humidity_2m,pressure_msl,\
wind_speed_10m,wind_gusts_10m,wind_direction_10m,precipitation,cloud_cover,is_day,weather_code";
const DAILY: &str = "weather_code,temperature_2m_max,temperature_2m_min,\
precipitation_probability_max,precipitation_sum,uv_index_max";
const HOURLY: &str =
    "temperature_2m,precipitation_probability,precipitation,weather_code,wind_gusts_10m";

/// The forecast API, for the weather now.
pub fn current_url(base: &str, lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat}&longitude={lon}&current={CURRENT}&wind_speed_unit=ms&timezone=auto"
    )
}

/// The forecast API, for seven days and the next 24 hours.
pub fn forecast_url(base: &str, lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat}&longitude={lon}&daily={DAILY}&hourly={HOURLY}\
         &forecast_days=7&forecast_hours=24&wind_speed_unit=ms&timezone=auto"
    )
}

/// The air-quality API, for the air now.
pub fn air_url(base: &str, lat: f64, lon: f64) -> String {
    format!("{base}?latitude={lat}&longitude={lon}&current=us_aqi,pm2_5,ozone&timezone=auto")
}

/// The units each reading must come back in: homeCore's.
const UNITS: [(&str, &str); 7] = [
    ("temperature_2m", "°C"),
    ("apparent_temperature", "°C"),
    ("pressure_msl", "hPa"),
    ("wind_speed_10m", "m/s"),
    ("wind_gusts_10m", "m/s"),
    ("precipitation", "mm"),
    ("precipitation_sum", "mm"),
];

fn check_units(units: Option<&Value>) -> Result<()> {
    let Some(units) = units else { return Ok(()) };
    for (name, want) in UNITS {
        if let Some(got) = units.get(name).and_then(Value::as_str) {
            if got != want {
                bail!("Open-Meteo sent {name} in {got}, not {want}");
            }
        }
    }
    Ok(())
}

fn offset_of(v: &Value) -> FixedOffset {
    let secs = v["utc_offset_seconds"].as_i64().unwrap_or(0) as i32;
    FixedOffset::east_opt(secs).unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero"))
}

/// "2026-11-01" at the place's midnight, with its offset:
/// "2026-11-01T00:00:00-05:00". A bare date is read by a browser as UTC
/// midnight, the evening before anywhere west of Greenwich.
fn local_midnight(date: &str, offset: FixedOffset) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let t = offset
        .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
        .single()?;
    Some(t.to_rfc3339())
}

/// "2026-11-01T13:00" (Open-Meteo's local time) with the place's offset.
fn local_time(t: &str, offset: FixedOffset) -> Option<String> {
    let n = NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M").ok()?;
    Some(offset.from_local_datetime(&n).single()?.to_rfc3339())
}

/// One decimal, as a reading reads.
fn tenth(v: &Value) -> Value {
    v.as_f64()
        .map(|f| json!((f * 10.0).round() / 10.0))
        .unwrap_or(Value::Null)
}

fn condition_of(code: &Value) -> Value {
    code.as_i64()
        .and_then(conditions::from_wmo)
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// The weather now, as the device's attributes.
pub fn parse_current(v: &Value) -> Result<Map<String, Value>> {
    check_units(v.get("current_units"))?;
    let c = v.get("current").context("no current block")?;
    let mut out = Map::new();
    out.insert("temperature".into(), tenth(&c["temperature_2m"]));
    out.insert("feels_like".into(), tenth(&c["apparent_temperature"]));
    out.insert("humidity".into(), tenth(&c["relative_humidity_2m"]));
    out.insert("pressure".into(), tenth(&c["pressure_msl"]));
    out.insert("wind_speed".into(), tenth(&c["wind_speed_10m"]));
    out.insert("wind_gust".into(), tenth(&c["wind_gusts_10m"]));
    out.insert(
        "wind_direction".into(),
        c["wind_direction_10m"]
            .as_f64()
            .map(|d| json!(d.round() as i64))
            .unwrap_or(Value::Null),
    );
    out.insert("precipitation_mm".into(), tenth(&c["precipitation"]));
    out.insert("cloud_cover_pct".into(), tenth(&c["cloud_cover"]));
    // 0 or 1 on the wire.
    out.insert(
        "is_day".into(),
        c["is_day"]
            .as_i64()
            .map(|d| json!(d == 1))
            .unwrap_or(Value::Null),
    );
    out.insert("condition".into(), condition_of(&c["weather_code"]));
    Ok(out)
}

/// The forecast: today's figures as attributes, the seven days as
/// `forecast` (the app's Weather card draws it), and the next 24 hours as
/// `forecast_hourly` (for automations).
pub fn parse_forecast(v: &Value) -> Result<Map<String, Value>> {
    check_units(v.get("daily_units"))?;
    check_units(v.get("hourly_units"))?;
    let offset = offset_of(v);
    let d = v.get("daily").context("no daily block")?;
    let at = |name: &str, i: usize| d[name].get(i).cloned().unwrap_or(Value::Null);
    let days: Vec<Value> = d["time"]
        .as_array()
        .context("no days")?
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            Some(json!({
                "time": local_midnight(t.as_str()?, offset)?,
                "high": tenth(&at("temperature_2m_max", i)),
                "low": tenth(&at("temperature_2m_min", i)),
                "condition": condition_of(&at("weather_code", i)),
                "precipitation_probability": at("precipitation_probability_max", i),
                "precipitation_mm": tenth(&at("precipitation_sum", i)),
            }))
        })
        .collect();

    let h = v.get("hourly").context("no hourly block")?;
    let hat = |name: &str, i: usize| h[name].get(i).cloned().unwrap_or(Value::Null);
    let hours: Vec<Value> = h["time"]
        .as_array()
        .context("no hours")?
        .iter()
        .enumerate()
        .take(24)
        .filter_map(|(i, t)| {
            Some(json!({
                "time": local_time(t.as_str()?, offset)?,
                "temperature": tenth(&hat("temperature_2m", i)),
                "condition": condition_of(&hat("weather_code", i)),
                "precipitation_probability": hat("precipitation_probability", i),
                "precipitation_mm": tenth(&hat("precipitation", i)),
                "wind_gust": tenth(&hat("wind_gusts_10m", i)),
            }))
        })
        .collect();

    let mut out = Map::new();
    out.insert("today_high".into(), tenth(&at("temperature_2m_max", 0)));
    out.insert("today_low".into(), tenth(&at("temperature_2m_min", 0)));
    out.insert(
        "rain_chance_pct".into(),
        at("precipitation_probability_max", 0),
    );
    out.insert("uvi".into(), tenth(&at("uv_index_max", 0)));
    out.insert("forecast".into(), Value::Array(days));
    out.insert("forecast_hourly".into(), Value::Array(hours));
    Ok(out)
}

/// The air now: US AQI, PM2.5 and ozone (µg/m³).
pub fn parse_air(v: &Value) -> Result<Map<String, Value>> {
    let c = v.get("current").context("no current block")?;
    let mut out = Map::new();
    out.insert(
        "aqi".into(),
        c["us_aqi"]
            .as_f64()
            .map(|a| json!(a.round() as i64))
            .unwrap_or(Value::Null),
    );
    out.insert("pm25_ugm3".into(), tenth(&c["pm2_5"]));
    out.insert("ozone_ugm3".into(), tenth(&c["ozone"]));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "current" => include_str!("fixtures/current.json"),
            "forecast" => include_str!("fixtures/forecast.json"),
            "air" => include_str!("fixtures/air.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn the_weather_now_in_homecore_names_and_units() {
        let now = parse_current(&fixture("current")).unwrap();
        for name in [
            "temperature",
            "feels_like",
            "humidity",
            "pressure",
            "wind_speed",
            "wind_gust",
            "wind_direction",
            "precipitation_mm",
            "cloud_cover_pct",
            "is_day",
            "condition",
        ] {
            assert!(!now[name].is_null(), "{name} is missing");
        }
        assert_eq!(now["temperature"], json!(15.8));
        assert!(now["is_day"].is_boolean());
        assert!(now["wind_direction"].is_i64());
    }

    #[test]
    fn seven_days_each_at_local_midnight_today_first() {
        let f = parse_forecast(&fixture("forecast")).unwrap();
        let days = f["forecast"].as_array().unwrap();
        assert_eq!(days.len(), 7);
        assert_eq!(days[0]["time"], json!("2026-11-01T00:00:00-05:00"));
        assert_eq!(days[0]["high"], json!(17.1));
        assert_eq!(days[0]["low"], json!(8.2));
        assert_eq!(days[0]["condition"], json!("cloudy"));
        assert_eq!(days[2]["condition"], json!("fog"));
        assert_eq!(days[5]["precipitation_mm"], json!(16.5));
        assert_eq!(days[5]["precipitation_probability"], json!(48));
        assert_eq!(f["today_high"], json!(17.1));
        assert_eq!(f["uvi"], json!(3.6));
    }

    #[test]
    fn the_next_24_hours_with_their_offset() {
        let f = parse_forecast(&fixture("forecast")).unwrap();
        let hours = f["forecast_hourly"].as_array().unwrap();
        assert_eq!(hours.len(), 24);
        assert_eq!(hours[0]["time"], json!("2026-11-01T12:00:00-05:00"));
        assert!(hours[0]["wind_gust"].is_number());
    }

    #[test]
    fn the_air() {
        let a = parse_air(&fixture("air")).unwrap();
        assert_eq!(a["aqi"], json!(87));
        assert_eq!(a["pm25_ugm3"], json!(6.1));
    }

    #[test]
    fn a_unit_it_did_not_ask_for_is_an_error() {
        let mut v = fixture("current");
        v["current_units"]["temperature_2m"] = json!("°F");
        let err = parse_current(&v).unwrap_err().to_string();
        assert!(err.contains("°F"), "{err}");
    }

    #[test]
    fn midnight_west_and_east_of_greenwich() {
        let west = FixedOffset::west_opt(5 * 3600).unwrap();
        let east = FixedOffset::east_opt(3600).unwrap();
        assert_eq!(
            local_midnight("2026-11-01", west).unwrap(),
            "2026-11-01T00:00:00-05:00"
        );
        assert_eq!(
            local_midnight("2026-11-01", east).unwrap(),
            "2026-11-01T00:00:00+01:00"
        );
    }
}
