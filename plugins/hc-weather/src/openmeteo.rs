//! Open-Meteo: the weather now, the forecast and the air.
//!
//! Three calls, each on its own timer: `current` with the next two hours in
//! 15-minute steps (the data changes every 15 minutes, its `interval`), the
//! daily and hourly forecast (every half hour), and air quality (hourly
//! data). No key. Open-Meteo is asked for
//! homeCore's units (°C, hPa, m/s, mm), so nothing here converts; each answer's
//! `*_units` is checked against what was asked, and a mismatch is an error
//! rather than a wrong number.
//!
//! Data: Open-Meteo.com, CC BY 4.0. The credit is on the plugin's page and in
//! its README.

use crate::conditions;
use anyhow::{bail, Context, Result};
use chrono::{Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde_json::{json, Map, Value};

const CURRENT: &str = "temperature_2m,apparent_temperature,relative_humidity_2m,dew_point_2m,\
pressure_msl,wind_speed_10m,wind_gusts_10m,wind_direction_10m,precipitation,cloud_cover,is_day,\
weather_code";
const DAILY: &str = "weather_code,temperature_2m_max,temperature_2m_min,\
precipitation_probability_max,precipitation_sum,snowfall_sum,wind_gusts_10m_max,uv_index_max";
const HOURLY: &str =
    "temperature_2m,precipitation_probability,precipitation,weather_code,wind_gusts_10m";

/// The forecast API, for the weather now and the rain in the next two
/// hours.
pub fn current_url(base: &str, lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat}&longitude={lon}&current={CURRENT}\
         &minutely_15=precipitation&forecast_minutely_15=9&wind_speed_unit=ms&timezone=auto"
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
const UNITS: [(&str, &str); 10] = [
    ("temperature_2m", "°C"),
    ("apparent_temperature", "°C"),
    ("dew_point_2m", "°C"),
    ("snowfall_sum", "cm"),
    ("wind_gusts_10m_max", "m/s"),
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

/// Where the answer's times are: the place's zone (`timezone=auto` names
/// it), so each day gets its own offset across a clock change; or, if the
/// zone isn't one chrono-tz knows, the fixed offset Open-Meteo gave.
#[derive(Clone, Copy)]
enum Zone {
    Named(Tz),
    Fixed(FixedOffset),
}

fn zone_of(v: &Value) -> Zone {
    if let Some(tz) = v["timezone"].as_str().and_then(|n| n.parse::<Tz>().ok()) {
        return Zone::Named(tz);
    }
    let secs = v["utc_offset_seconds"].as_i64().unwrap_or(0) as i32;
    Zone::Fixed(
        FixedOffset::east_opt(secs).unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero")),
    )
}

impl Zone {
    /// A wall-clock time there, with its offset. In the hour a clock skips,
    /// the time just after the change; in the hour it repeats, the first.
    fn at(self, n: NaiveDateTime) -> Option<String> {
        match self {
            Zone::Fixed(o) => Some(o.from_local_datetime(&n).single()?.to_rfc3339()),
            Zone::Named(tz) => {
                let t = tz
                    .from_local_datetime(&n)
                    .earliest()
                    .or_else(|| tz.from_local_datetime(&(n + Duration::hours(1))).earliest())?;
                Some(t.fixed_offset().to_rfc3339())
            }
        }
    }
}

/// "2026-11-01" at the place's midnight, with that day's offset:
/// "2026-11-01T00:00:00-05:00". A bare date is read by a browser as UTC
/// midnight, the evening before anywhere west of Greenwich.
fn local_midnight(date: &str, zone: Zone) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    zone.at(d.and_hms_opt(0, 0, 0)?)
}

/// Open-Meteo's local times: "2026-11-01T13:00".
fn naive(t: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M").ok()
}

/// "2026-11-01T13:00" with the place's offset at that time.
fn local_time(t: &str, zone: Zone) -> Option<String> {
    zone.at(naive(t)?)
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

/// Rain heavier than this in a 15-minute step counts (0.4 mm an hour, light
/// rain); less is a damp trace that shouldn't start an automation.
const WET_MM: f64 = 0.1;

/// The next two hours in 15-minute steps: how much falls in the next hour,
/// and how many minutes until it starts (none in the next two hours: null).
///
/// Open-Meteo's step at time T is what falls in the 15 minutes before T, and
/// the first step is the current one, so step 1 covers now to 15 minutes
/// from now.
fn rain_ahead(m: Option<&Value>, out: &mut Map<String, Value>) {
    let steps: Vec<f64> = m
        .and_then(|m| m["precipitation"].as_array())
        .map(|a| a.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect())
        .unwrap_or_default();
    if steps.len() < 5 {
        for k in ["rain_next_hour_mm", "rain_starts_in_min", "rain_soon"] {
            out.insert(k.into(), Value::Null);
        }
        return;
    }
    let next_hour: f64 = steps[1..5].iter().sum();
    let starts = steps
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, mm)| **mm >= WET_MM)
        .map(|(i, _)| (i as i64 - 1) * 15);
    out.insert(
        "rain_next_hour_mm".into(),
        json!((next_hour * 10.0).round() / 10.0),
    );
    out.insert(
        "rain_starts_in_min".into(),
        starts.map(Value::from).unwrap_or(Value::Null),
    );
    out.insert("rain_soon".into(), json!(starts.is_some_and(|s| s <= 30)));
}

/// The weather now, as the device's attributes.
pub fn parse_current(v: &Value) -> Result<Map<String, Value>> {
    check_units(v.get("current_units"))?;
    check_units(v.get("minutely_15_units"))?;
    let c = v.get("current").context("no current block")?;
    let mut out = Map::new();
    out.insert("temperature".into(), tenth(&c["temperature_2m"]));
    out.insert("feels_like".into(), tenth(&c["apparent_temperature"]));
    out.insert("humidity".into(), tenth(&c["relative_humidity_2m"]));
    out.insert("dewpoint".into(), tenth(&c["dew_point_2m"]));
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
    rain_ahead(v.get("minutely_15"), &mut out);
    Ok(out)
}

/// Snow in mm: Open-Meteo gives centimetres of snow, and homeCore's length
/// is the millimetre (the app shows inches or centimetres).
fn snow_mm(v: &Value) -> Value {
    v.as_f64()
        .map(|cm| json!((cm * 100.0).round() / 10.0))
        .unwrap_or(Value::Null)
}

/// The coldest of the hours from the first one (now) to 9 in the morning:
/// tonight's low, or this morning's when it's still before nine.
fn coldest_until_morning(times: &[Value], temps: &[Value]) -> Option<f64> {
    let first = naive(times.first()?.as_str()?)?;
    let nine = NaiveTime::from_hms_opt(9, 0, 0)?;
    let end = if first.time() < nine {
        first.date().and_time(nine)
    } else {
        (first.date() + Duration::days(1)).and_time(nine)
    };
    times
        .iter()
        .zip(temps)
        .filter(|(t, _)| t.as_str().and_then(naive).is_some_and(|t| t <= end))
        .filter_map(|(_, c)| c.as_f64())
        .reduce(f64::min)
}

/// The forecast: today's and tomorrow's figures and tonight's low as
/// attributes, the seven days as `forecast` (the app's Weather card draws
/// it), and the next 24 hours as `forecast_hourly` (for automations).
pub fn parse_forecast(v: &Value) -> Result<Map<String, Value>> {
    check_units(v.get("daily_units"))?;
    check_units(v.get("hourly_units"))?;
    let zone = zone_of(v);
    let d = v.get("daily").context("no daily block")?;
    let at = |name: &str, i: usize| d[name].get(i).cloned().unwrap_or(Value::Null);
    let days: Vec<Value> = d["time"]
        .as_array()
        .context("no days")?
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            Some(json!({
                "time": local_midnight(t.as_str()?, zone)?,
                "high": tenth(&at("temperature_2m_max", i)),
                "low": tenth(&at("temperature_2m_min", i)),
                "condition": condition_of(&at("weather_code", i)),
                "precipitation_probability": at("precipitation_probability_max", i),
                "precipitation_mm": tenth(&at("precipitation_sum", i)),
                "snowfall_mm": snow_mm(&at("snowfall_sum", i)),
                "wind_gust": tenth(&at("wind_gusts_10m_max", i)),
            }))
        })
        .collect();

    let h = v.get("hourly").context("no hourly block")?;
    let hat = |name: &str, i: usize| h[name].get(i).cloned().unwrap_or(Value::Null);
    let times = h["time"].as_array().context("no hours")?;
    let hours: Vec<Value> = times
        .iter()
        .enumerate()
        .take(24)
        .filter_map(|(i, t)| {
            Some(json!({
                "time": local_time(t.as_str()?, zone)?,
                "temperature": tenth(&hat("temperature_2m", i)),
                "condition": condition_of(&hat("weather_code", i)),
                "precipitation_probability": hat("precipitation_probability", i),
                "precipitation_mm": tenth(&hat("precipitation", i)),
                "wind_gust": tenth(&hat("wind_gusts_10m", i)),
            }))
        })
        .collect();
    let tonight = h["temperature_2m"]
        .as_array()
        .and_then(|temps| coldest_until_morning(times, temps));

    let mut out = Map::new();
    out.insert("today_high".into(), tenth(&at("temperature_2m_max", 0)));
    out.insert("today_low".into(), tenth(&at("temperature_2m_min", 0)));
    out.insert("tomorrow_high".into(), tenth(&at("temperature_2m_max", 1)));
    out.insert("tomorrow_low".into(), tenth(&at("temperature_2m_min", 1)));
    out.insert(
        "tonight_low".into(),
        tonight.map(|t| tenth(&json!(t))).unwrap_or(Value::Null),
    );
    out.insert(
        "freeze_tonight".into(),
        tonight.map(|t| json!(t <= 0.0)).unwrap_or(Value::Null),
    );
    out.insert(
        "rain_chance_pct".into(),
        at("precipitation_probability_max", 0),
    );
    out.insert("snow_today_mm".into(), snow_mm(&at("snowfall_sum", 0)));
    out.insert("uvi".into(), tenth(&at("uv_index_max", 0)));
    out.insert("forecast".into(), Value::Array(days));
    out.insert("forecast_hourly".into(), Value::Array(hours));
    Ok(out)
}

/// The US EPA's names for an AQI, as `aqi_category`'s values.
pub const AQI_CATEGORIES: [(&str, &str); 6] = [
    ("good", "Good"),
    ("moderate", "Moderate"),
    ("unhealthy_sensitive", "Unhealthy for sensitive groups"),
    ("unhealthy", "Unhealthy"),
    ("very_unhealthy", "Very unhealthy"),
    ("hazardous", "Hazardous"),
];

fn aqi_category(aqi: i64) -> &'static str {
    let i = match aqi {
        ..=50 => 0,
        51..=100 => 1,
        101..=150 => 2,
        151..=200 => 3,
        201..=300 => 4,
        _ => 5,
    };
    AQI_CATEGORIES[i].0
}

/// The air now: US AQI and its category, PM2.5 and ozone (µg/m³).
pub fn parse_air(v: &Value) -> Result<Map<String, Value>> {
    let c = v.get("current").context("no current block")?;
    let aqi = c["us_aqi"].as_f64().map(|a| a.round() as i64);
    let mut out = Map::new();
    out.insert("aqi".into(), aqi.map(Value::from).unwrap_or(Value::Null));
    out.insert(
        "aqi_category".into(),
        aqi.map(|a| json!(aqi_category(a))).unwrap_or(Value::Null),
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
        // The clocks went back at 2 am on the 1st: its midnight was still
        // summer time, and the next day's is winter time.
        assert_eq!(days[0]["time"], json!("2026-11-01T00:00:00-04:00"));
        assert_eq!(days[1]["time"], json!("2026-11-02T00:00:00-05:00"));
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
    fn rain_in_the_next_hour_and_when_it_starts() {
        // Steps: dry, dry, dry, 0.3, 0.6, 0.2 mm, ...: it starts in the
        // third quarter-hour from now.
        let now = parse_current(&fixture("current")).unwrap();
        assert_eq!(now["rain_next_hour_mm"], json!(0.9));
        assert_eq!(now["rain_starts_in_min"], json!(30));
        assert_eq!(now["rain_soon"], json!(true));
        assert_eq!(now["dewpoint"], json!(9.4));
    }

    #[test]
    fn dry_for_two_hours_is_no_start_time() {
        let mut v = fixture("current");
        v["minutely_15"]["precipitation"] = json!([0.4, 0.05, 0, 0, 0, 0, 0, 0, 0]);
        let now = parse_current(&v).unwrap();
        // Rain in the last quarter-hour, and a trace in the next, is not
        // rain to come.
        assert_eq!(now["rain_next_hour_mm"], json!(0.1));
        assert!(now["rain_starts_in_min"].is_null());
        assert_eq!(now["rain_soon"], json!(false));
    }

    #[test]
    fn no_quarter_hours_is_unknown_not_dry() {
        let mut v = fixture("current");
        v.as_object_mut().unwrap().remove("minutely_15");
        let now = parse_current(&v).unwrap();
        assert!(now["rain_soon"].is_null());
        assert!(now["rain_next_hour_mm"].is_null());
    }

    #[test]
    fn tonight_tomorrow_and_snow() {
        let f = parse_forecast(&fixture("forecast")).unwrap();
        // The hours from noon to 9 tomorrow morning; the coldest is 7 am.
        assert_eq!(f["tonight_low"], json!(7.1));
        assert_eq!(f["freeze_tonight"], json!(false));
        let days = f["forecast"].as_array().unwrap();
        assert_eq!(f["tomorrow_high"], days[1]["high"]);
        assert_eq!(f["tomorrow_low"], days[1]["low"]);
        assert_eq!(days[5]["snowfall_mm"], json!(14.0));
        assert_eq!(days[5]["wind_gust"], json!(12.3));
        assert_eq!(f["snow_today_mm"], json!(0.0));
    }

    #[test]
    fn a_freeze_tonight() {
        let mut v = fixture("forecast");
        v["hourly"]["temperature_2m"][14] = json!(-0.4);
        let f = parse_forecast(&v).unwrap();
        assert_eq!(f["tonight_low"], json!(-0.4));
        assert_eq!(f["freeze_tonight"], json!(true));
    }

    #[test]
    fn before_nine_tonight_is_this_morning() {
        let hours: Vec<Value> = (5..12)
            .map(|h| json!(format!("2026-11-02T{h:02}:00")))
            .collect();
        let temps: Vec<Value> = [3.0, 2.0, 1.5, 2.5, 4.0, -9.0, -9.0]
            .map(|t| json!(t))
            .to_vec();
        // 5 to 9 am counts; 10 and 11 don't.
        assert_eq!(coldest_until_morning(&hours, &temps), Some(1.5));
    }

    #[test]
    fn the_air_has_a_category() {
        let a = parse_air(&fixture("air")).unwrap();
        assert_eq!(a["aqi_category"], json!("moderate"));
        assert_eq!(aqi_category(0), "good");
        assert_eq!(aqi_category(50), "good");
        assert_eq!(aqi_category(101), "unhealthy_sensitive");
        assert_eq!(aqi_category(450), "hazardous");
    }

    #[test]
    fn each_day_has_its_own_offset_across_a_clock_change() {
        // New York goes to summer time on 8 March 2026.
        let v = json!({
            "timezone": "America/New_York",
            "utc_offset_seconds": -18000,
            "daily": { "time": ["2026-03-07", "2026-03-08", "2026-03-09"] },
            "hourly": { "time": ["2026-03-08T01:00", "2026-03-08T03:00"] }
        });
        let f = parse_forecast(&v).unwrap();
        let days = f["forecast"].as_array().unwrap();
        assert_eq!(days[0]["time"], json!("2026-03-07T00:00:00-05:00"));
        assert_eq!(days[2]["time"], json!("2026-03-09T00:00:00-04:00"));
        let hours = f["forecast_hourly"].as_array().unwrap();
        assert_eq!(hours[0]["time"], json!("2026-03-08T01:00:00-05:00"));
        assert_eq!(hours[1]["time"], json!("2026-03-08T03:00:00-04:00"));
    }

    #[test]
    fn an_hour_the_clock_skips_is_the_one_after() {
        let ny = Zone::Named("America/New_York".parse().unwrap());
        let skipped = naive("2026-03-08T02:30").unwrap();
        assert_eq!(ny.at(skipped).unwrap(), "2026-03-08T03:30:00-04:00");
    }

    #[test]
    fn an_unknown_zone_falls_back_to_the_offset_given() {
        let v = json!({ "timezone": "Nowhere/Atlantis", "utc_offset_seconds": 3600 });
        let z = zone_of(&v);
        assert_eq!(
            local_midnight("2026-11-01", z).unwrap(),
            "2026-11-01T00:00:00+01:00"
        );
    }

    #[test]
    fn midnight_west_and_east_of_greenwich() {
        let west = Zone::Fixed(FixedOffset::west_opt(5 * 3600).unwrap());
        let east = Zone::Fixed(FixedOffset::east_opt(3600).unwrap());
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
