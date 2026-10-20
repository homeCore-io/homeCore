//! Parse Ecowitt form-encoded POST data into HomeCore device updates.
//!
//! The Ecowitt gateway's "custom server" feature sends flat key-value pairs
//! as `application/x-www-form-urlencoded`.  Field names follow Ecowitt/WU
//! conventions (e.g. `tempf`, `humidity`, `windspeedmph`, `dailyrainin`), and
//! the values are imperial whatever the gateway's display says. Each is
//! converted to homeCore's units here (see `units`), under the same name and
//! device id the poll (`parser`) gives it.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use tracing::debug;

use crate::battery;
use crate::parser::{rain_update, DeviceUpdate};
use crate::units::{f_to_c, in_to_mm, inhg_to_hpa, mph_to_ms, round2};

/// Fields that are metadata / not sensor data — don't warn about them.
const KNOWN_META: &[&str] = &[
    "PASSKEY",
    "stationtype",
    "dateutc",
    "freq",
    "model",
    "runtime",
    "heap",
    "interval",
    "ws90_ver",
    "ws90cap_volt",
];

/// Parse form-encoded POST fields into device updates.
pub fn parse_form_data(fields: &HashMap<String, String>, prefix: &str) -> Vec<DeviceUpdate> {
    let mut updates = Vec::new();
    let mut consumed: HashSet<String> = HashSet::new();

    // Outdoor weather station (aggregated)
    if let Some(u) = parse_outdoor(fields, prefix, &mut consumed) {
        updates.push(u);
    }

    // Indoor sensor
    if let Some(u) = parse_indoor(fields, prefix, &mut consumed) {
        updates.push(u);
    }

    // Rain gauges: the tipping bucket and the piezo (WS90/WS85).
    updates.extend(parse_rain(fields, prefix, &mut consumed));

    // Multi-channel temperature/humidity sensors (temp1f..temp8f, humidity1..humidity8)
    parse_numbered_temp(fields, prefix, &mut consumed, &mut updates);

    // Soil moisture sensors (soilmoisture1..soilmoisture16)
    parse_soil_moisture(fields, prefix, &mut consumed, &mut updates);

    // WN34 temperature probes (tf_ch1..tf_ch8)
    parse_temp_probe(fields, prefix, &mut consumed, &mut updates);

    // Soil temp/EC sensors — WH52 3-in-1 (soilec_ch1..soilec_ch16)
    parse_soil_ec(fields, prefix, &mut consumed, &mut updates);

    // Leaf wetness sensors (leafwetness_ch1..leafwetness_ch8)
    parse_leaf_wetness(fields, prefix, &mut consumed, &mut updates);

    // Leak sensors (leak_ch1..leak_ch4)
    parse_leak(fields, prefix, &mut consumed, &mut updates);

    // Water level (WH54 LDS: lds_air_ch1..lds_air_ch4)
    parse_lds(fields, prefix, &mut consumed, &mut updates);

    // PM2.5 sensors (pm25_ch1..pm25_ch4)
    parse_pm25(fields, prefix, &mut consumed, &mut updates);

    // Lightning
    if let Some(u) = parse_lightning(fields, prefix, &mut consumed) {
        updates.push(u);
    }

    // CO2 sensor
    if let Some(u) = parse_co2(fields, prefix, &mut consumed) {
        updates.push(u);
    }

    // Log any unrecognized fields so missing sensors are easy to spot.
    let unknown: Vec<&str> = fields
        .keys()
        .map(|k| k.as_str())
        .filter(|k| !consumed.contains(*k) && !KNOWN_META.contains(k))
        .collect();
    if !unknown.is_empty() {
        debug!(fields = ?unknown, "Unrecognized Ecowitt POST fields");
    }

    updates
}

/// How an upload field's value becomes homeCore's.
type Conv = fn(f64) -> f64;

/// As sent.
fn same(v: f64) -> f64 {
    v
}

// ---------------------------------------------------------------------------
// Outdoor weather station
// ---------------------------------------------------------------------------

fn parse_outdoor(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    let outdoor_fields: &[(&str, &str, Conv)] = &[
        ("tempf", "temperature", f_to_c),
        ("tempc", "temperature", round2),
        ("humidity", "humidity", same),
        ("dewptf", "dewpoint", f_to_c),
        ("dewptc", "dewpoint", round2),
        ("windchillf", "windchill", f_to_c),
        ("windchillc", "windchill", round2),
        ("feelslikef", "feels_like", f_to_c),
        ("feelslikec", "feels_like", round2),
        ("heatindexf", "heat_index", f_to_c),
        ("heatindexc", "heat_index", round2),
        // kPa, as the gateway sends it.
        ("vpd", "vpd", same),
        ("winddir", "wind_direction", same),
        ("winddir_avg10m", "wind_direction_avg10m", same),
        ("windspeedmph", "wind_speed", mph_to_ms),
        ("windspdmph_avg10m", "wind_speed_avg10m", mph_to_ms),
        ("windgustmph", "wind_gust", mph_to_ms),
        ("maxdailygust", "wind_gust_daily_max", mph_to_ms),
        // W/m², which is what it is — not lux.
        ("solarradiation", "solar_radiation", same),
        ("uv", "uvi", same),
        ("baromrelin", "pressure", inhg_to_hpa),
        ("baromabsin", "pressure_absolute", inhg_to_hpa),
    ];

    for &(field, attr, conv) in outdoor_fields {
        if put(&mut state, fields, field, attr, conv) {
            consumed.insert(field.into());
        }
    }

    if let Some(v) = fields.get("dateutc") {
        state.insert("datetime".into(), json!(v));
        consumed.insert("dateutc".into());
    }

    if state.is_empty() {
        return None;
    }

    // Outdoor battery (wh65batt, wh68batt, wh80batt, wh90batt, ws90batt).
    // Each has different semantics — battery::insert classifies per key.
    for key in ["wh65batt", "wh68batt", "wh80batt", "wh90batt", "ws90batt"] {
        if battery::insert(&mut state, fields, key) {
            consumed.insert(key.into());
            break;
        }
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_weather"),
        device_type: "weather_station",
        name: "Weather Station".into(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// Indoor sensor
// ---------------------------------------------------------------------------

fn parse_indoor(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    for &(field, attr, conv) in &[
        ("tempinf", "temperature", f_to_c as Conv),
        ("tempinc", "temperature", round2),
        ("humidityin", "humidity", same),
    ] {
        if put(&mut state, fields, field, attr, conv) {
            consumed.insert(field.into());
        }
    }

    if state.is_empty() {
        return None;
    }

    // Indoor sensor battery (wh25batt = binary, wh26batt = binary).
    for key in ["wh25batt", "wh26batt"] {
        if battery::insert(&mut state, fields, key) {
            consumed.insert(key.into());
            break;
        }
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_indoor"),
        device_type: "temperature_sensor",
        name: "Indoor Sensor".into(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// Rain gauges
// ---------------------------------------------------------------------------

/// The tipping-bucket gauge (`{prefix}_rain`) and the piezo one
/// (`{prefix}_piezo_rain`), as two devices with the same names — as the poll
/// reports them. A station can have both, and each is its own instrument.
fn parse_rain(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Vec<DeviceUpdate> {
    let mut out = Vec::new();

    let bucket: &[(&str, &str, Conv)] = &[
        ("rainratein", "rain_rate_mm_hr", in_to_mm),
        ("rainratemm", "rain_rate_mm_hr", round2),
        ("eventrainin", "rain_event_mm", in_to_mm),
        ("eventrainmm", "rain_event_mm", round2),
        ("hourlyrainin", "rain_hourly_mm", in_to_mm),
        ("hourlyrainmm", "rain_hourly_mm", round2),
        ("last24hrainin", "rain_24h_mm", in_to_mm),
        ("last24hrainmm", "rain_24h_mm", round2),
        ("dailyrainin", "rain_daily_mm", in_to_mm),
        ("dailyrainmm", "rain_daily_mm", round2),
        ("weeklyrainin", "rain_weekly_mm", in_to_mm),
        ("weeklyrainmm", "rain_weekly_mm", round2),
        ("monthlyrainin", "rain_monthly_mm", in_to_mm),
        ("monthlyrainmm", "rain_monthly_mm", round2),
        ("yearlyrainin", "rain_yearly_mm", in_to_mm),
        ("yearlyrainmm", "rain_yearly_mm", round2),
        ("totalrainin", "rain_total_mm", in_to_mm),
        ("totalrainmm", "rain_total_mm", round2),
    ];
    let piezo: &[(&str, &str, Conv)] = &[
        ("rrain_piezo", "rain_rate_mm_hr", in_to_mm),
        ("erain_piezo", "rain_event_mm", in_to_mm),
        ("hrain_piezo", "rain_hourly_mm", in_to_mm),
        ("last24hrain_piezo", "rain_24h_mm", in_to_mm),
        ("drain_piezo", "rain_daily_mm", in_to_mm),
        ("wrain_piezo", "rain_weekly_mm", in_to_mm),
        ("mrain_piezo", "rain_monthly_mm", in_to_mm),
        ("yrain_piezo", "rain_yearly_mm", in_to_mm),
    ];

    for (table, suffix, batteries) in [
        (bucket, "rain", &["wh40batt"][..]),
        (piezo, "piezo_rain", &["wh90batt"][..]),
    ] {
        let mut state = Map::new();
        for &(field, attr, conv) in table {
            if put(&mut state, fields, field, attr, conv) {
                consumed.insert(field.into());
            }
        }
        if suffix == "piezo_rain" {
            // Raining now, as the piezo sensor feels it: 1 or 0.
            if let Some(v) = fields
                .get("srain_piezo")
                .and_then(|s| s.parse::<f64>().ok())
            {
                state.insert("raining".into(), json!(v >= 1.0));
                consumed.insert("srain_piezo".into());
            }
        }
        if state.is_empty() {
            continue;
        }
        for key in batteries {
            if battery::insert(&mut state, fields, key) {
                consumed.insert((*key).into());
                break;
            }
        }
        out.push(rain_update(prefix, suffix, state));
    }
    out
}

// ---------------------------------------------------------------------------
// Numbered temperature/humidity channels (temp1f..temp8f, humidity1..humidity8)
// ---------------------------------------------------------------------------

fn parse_numbered_temp(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=8 {
        let mut state = Map::new();
        for (key, attr, conv) in [
            (format!("temp{ch}f"), "temperature", f_to_c as Conv),
            (format!("temp{ch}c"), "temperature", round2),
            (format!("humidity{ch}"), "humidity", same),
        ] {
            if put(&mut state, fields, &key, attr, conv) {
                consumed.insert(key);
            }
        }
        let batt_key = format!("batt{ch}");
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        if state.is_empty() {
            continue;
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_temp_{ch}"),
            device_type: "temperature_sensor",
            name: format!("Temp/Humidity Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// WN34 temperature probes (tf_ch1..tf_ch8)
// ---------------------------------------------------------------------------

fn parse_temp_probe(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=8 {
        let key = format!("tf_ch{ch}");
        let batt_key = format!("tf_batt{ch}");

        let mut state = Map::new();
        if put(&mut state, fields, &key, "temperature", f_to_c) {
            consumed.insert(key);
        }
        if state.is_empty() {
            continue;
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_tf_{ch}"),
            device_type: "temperature_sensor",
            name: format!("Temperature Probe Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Soil moisture sensors (soilmoisture1..soilmoisture16)
// ---------------------------------------------------------------------------

fn parse_soil_moisture(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=16 {
        let mut state = Map::new();
        // The moisture as a percentage, and the sensor's raw reading behind it.
        for (key, attr) in [
            (format!("soilmoisture{ch}"), "soil_moisture_pct"),
            (format!("soilad{ch}"), "soil_moisture_raw"),
        ] {
            if put(&mut state, fields, &key, attr, same) {
                consumed.insert(key);
            }
        }
        let batt_key = format!("soilbatt{ch}");
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        if state.is_empty() {
            continue;
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_soil_{ch}"),
            device_type: "soil_sensor",
            name: format!("Soil Moisture Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Soil EC sensors — WH52 3-in-1 (soilec_ch1..soilec_ch16)
// ---------------------------------------------------------------------------

fn parse_soil_ec(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=16 {
        let mut state = Map::new();
        // The upload is imperial throughout, so the soil temperature is °F.
        for (key, attr, conv) in [
            (format!("soilec_ch{ch}"), "ec", same as Conv),
            (format!("soilec_hum{ch}"), "soil_moisture_pct", same),
            (format!("soilec_temp{ch}"), "temperature", f_to_c),
        ] {
            if put(&mut state, fields, &key, attr, conv) {
                consumed.insert(key);
            }
        }

        if state.is_empty() {
            continue;
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_ec_{ch}"),
            device_type: "ec_sensor",
            name: format!("Soil EC Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Leaf wetness sensors (leafwetness_ch1..leafwetness_ch8)
// ---------------------------------------------------------------------------

fn parse_leaf_wetness(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=8 {
        let key = format!("leafwetness_ch{ch}");
        let batt_key = format!("leaf_batt{ch}");

        let mut state = Map::new();
        if put(&mut state, fields, &key, "leaf_wetness_pct", same) {
            consumed.insert(key);
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        if state.is_empty() {
            continue;
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_leaf_{ch}"),
            device_type: "leaf_sensor",
            name: format!("Leaf Wetness Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Leak sensors (leak_ch1..leak_ch4)
// ---------------------------------------------------------------------------

fn parse_leak(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=4 {
        let key = format!("leak_ch{ch}");
        let batt_key = format!("leakbatt{ch}");

        // 0 dry, 1 wet.
        let Some(v) = fields.get(&key).and_then(|s| s.parse::<f64>().ok()) else {
            continue;
        };
        consumed.insert(key);
        let mut state = Map::new();
        state.insert("water_detected".into(), json!(v >= 1.0));
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_leak_{ch}"),
            device_type: "leak_sensor",
            name: format!("Leak Detector Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Water level — WH54 LDS (lds_air_ch1..lds_air_ch4)
// ---------------------------------------------------------------------------

fn parse_lds(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=4 {
        let mut state = Map::new();
        // Millimetres: the distance to the surface, and the depth below it.
        for (key, attr) in [
            (format!("lds_air_ch{ch}"), "air_gap_mm"),
            (format!("lds_depth_ch{ch}"), "depth_mm"),
        ] {
            if put(&mut state, fields, &key, attr, round2) {
                consumed.insert(key);
            }
        }
        if state.is_empty() {
            continue;
        }
        let batt_key = format!("ldsbatt{ch}");
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_lds_{ch}"),
            device_type: "level_sensor",
            name: format!("Level Sensor Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// PM2.5 sensors (pm25_ch1..pm25_ch4)
// ---------------------------------------------------------------------------

fn parse_pm25(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=4 {
        let mut state = Map::new();
        for (key, attr) in [
            (format!("pm25_ch{ch}"), "pm25"),
            (format!("pm25_avg_24h_ch{ch}"), "pm25_24h"),
        ] {
            if put(&mut state, fields, &key, attr, same) {
                consumed.insert(key);
            }
        }
        let batt_key = format!("pm25batt{ch}");
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key);
        }

        if state.is_empty() {
            continue;
        }

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_pm25_{ch}"),
            device_type: "air_quality_sensor",
            name: format!("PM2.5 Sensor Ch {ch}"),
            state: Value::Object(state),
        });
    }
}

// ---------------------------------------------------------------------------
// Lightning
// ---------------------------------------------------------------------------

fn parse_lightning(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    // The distance field is just "lightning", in km.
    if put(&mut state, fields, "lightning", "distance_km", round2) {
        consumed.insert("lightning".into());
    }
    if let Some(n) = fields
        .get("lightning_num")
        .and_then(|s| s.parse::<f64>().ok())
    {
        state.insert("strike_count".into(), json!(n as u64));
        consumed.insert("lightning_num".into());
    }
    if let Some(v) = fields.get("lightning_time") {
        state.insert("last_strike".into(), json!(v));
        consumed.insert("lightning_time".into());
    }
    if battery::insert(&mut state, fields, "wh57batt") {
        consumed.insert("wh57batt".into());
    }

    if state.is_empty() {
        return None;
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_lightning"),
        device_type: "lightning_sensor",
        name: "Lightning Sensor".into(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// CO2 / air quality (WH45, WH46D)
// ---------------------------------------------------------------------------

fn parse_co2(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    let co2_fields: &[(&str, &str, Conv)] = &[
        ("co2", "co2", same),
        ("co2_24h", "co2_24h", same),
        ("co2in", "co2_indoor", same),
        ("co2in_24h", "co2_indoor_24h", same),
        ("pm25_co2", "pm25", same),
        ("pm25_24h_co2", "pm25_24h", same),
        ("pm10_co2", "pm10", same),
        ("pm10_24h_co2", "pm10_24h", same),
        ("pm1_co2", "pm1", same),
        ("pm4_co2", "pm4", same),
        ("tf_co2", "temperature", f_to_c),
        ("humi_co2", "humidity", same),
    ];

    for &(field, attr, conv) in co2_fields {
        if put(&mut state, fields, field, attr, conv) {
            consumed.insert(field.into());
        }
    }

    // co2_batt is a 0..=6 level, NOT a percentage. Classify via battery::insert.
    if battery::insert(&mut state, fields, "co2_batt") {
        consumed.insert("co2_batt".into());
    }

    if state.is_empty() {
        return None;
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_co2"),
        device_type: "air_quality_sensor",
        name: "CO2 / Air Quality Sensor".into(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `fields[key]`, converted, as `state[attr]`. Returns true if inserted; a
/// value that does not parse (`--` from a lost sensor) is skipped, never 0.
fn put(
    state: &mut Map<String, Value>,
    fields: &HashMap<String, String>,
    key: &str,
    attr: &str,
    conv: Conv,
) -> bool {
    match fields.get(key).and_then(|s| s.trim().parse::<f64>().ok()) {
        Some(v) if v.is_finite() => {
            state.insert(attr.into(), json!(conv(v)));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(pairs: &[(&str, &str)]) -> Vec<DeviceUpdate> {
        let f: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parse_form_data(&f, "eco")
    }

    fn find<'a>(u: &'a [DeviceUpdate], id: &str) -> &'a Map<String, Value> {
        u.iter()
            .find(|d| d.device_id == id)
            .unwrap_or_else(|| panic!("no {id}"))
            .state
            .as_object()
            .unwrap()
    }

    #[test]
    fn the_outdoor_station_in_homecores_units() {
        let u = upload(&[
            ("tempf", "68.0"),
            ("humidity", "45"),
            ("dewptf", "50.0"),
            ("windspeedmph", "10.0"),
            ("windgustmph", "20.0"),
            ("maxdailygust", "0"),
            ("winddir", "213"),
            ("solarradiation", "512.30"),
            ("uv", "3"),
            ("baromrelin", "29.92"),
            ("baromabsin", "29.50"),
            ("wh65batt", "0"),
        ]);
        let w = find(&u, "eco_weather");
        assert_eq!(w["temperature"], json!(20.0));
        assert_eq!(w["humidity"], json!(45.0));
        assert_eq!(w["dewpoint"], json!(10.0));
        assert_eq!(w["wind_speed"], json!(4.47));
        assert_eq!(w["wind_gust"], json!(8.94));
        assert_eq!(w["wind_gust_daily_max"], json!(0.0));
        assert_eq!(w["wind_direction"], json!(213.0));
        assert_eq!(w["solar_radiation"], json!(512.3));
        assert_eq!(w["uvi"], json!(3.0));
        assert_eq!(w["pressure"], json!(1013.21));
        assert_eq!(w["pressure_absolute"], json!(998.98));
        assert_eq!(w["battery_low"], json!(false));
        for old in [
            "light",
            "barometric_rel",
            "gust_speed",
            "temperature_unit",
            "battery",
        ] {
            assert!(!w.contains_key(old), "{old} is still published");
        }
    }

    #[test]
    fn a_lost_reading_is_skipped_not_zero() {
        let u = upload(&[("tempf", "--"), ("humidity", "45")]);
        let w = find(&u, "eco_weather");
        assert!(!w.contains_key("temperature"));
        assert_eq!(w["humidity"], json!(45.0));
    }

    #[test]
    fn the_two_rain_gauges_are_two_devices_with_one_set_of_names() {
        let u = upload(&[
            ("rainratein", "0.50"),
            ("dailyrainin", "1.00"),
            ("drain_piezo", "0.10"),
            ("srain_piezo", "1"),
            ("wh40batt", "1.5"),
        ]);
        let r = find(&u, "eco_rain");
        assert_eq!(r["rain_rate_mm_hr"], json!(12.7));
        assert_eq!(r["rain_daily_mm"], json!(25.4));
        assert_eq!(r["voltage"], json!(1.5));
        let p = find(&u, "eco_piezo_rain");
        assert_eq!(p["rain_daily_mm"], json!(2.54));
        assert_eq!(p["raining"], json!(true));
        assert!(!p.contains_key("rain_day_piezo"));
    }

    #[test]
    fn channels_have_their_own_names_for_their_own_readings() {
        let u = upload(&[
            ("temp2f", "32.0"),
            ("humidity2", "50"),
            ("batt2", "1"),
            ("tf_ch1", "50.0"),
            ("tf_batt1", "1.1"),
            ("soilmoisture1", "37"),
            ("soilad1", "250"),
            ("soilbatt1", "1.5"),
            ("leafwetness_ch1", "12"),
            ("leak_ch1", "1"),
            ("leakbatt1", "5"),
            ("lightning", "8.0"),
            ("lightning_num", "3"),
            ("wh57batt", "4"),
            ("tf_co2", "70.0"),
            ("co2", "612"),
            ("co2_batt", "6"),
        ]);
        let wh31 = find(&u, "eco_temp_2");
        assert_eq!(wh31["temperature"], json!(0.0));
        assert_eq!(wh31["humidity"], json!(50.0));
        assert_eq!(wh31["battery_low"], json!(true));
        assert!(
            !wh31.contains_key("battery_pct"),
            "a 0/1 flag is not a percentage"
        );

        let probe = find(&u, "eco_tf_1");
        assert_eq!(probe["temperature"], json!(10.0));
        assert_eq!(probe["voltage"], json!(1.1));

        let soil = find(&u, "eco_soil_1");
        assert_eq!(soil["soil_moisture_pct"], json!(37.0));
        assert_eq!(soil["soil_moisture_raw"], json!(250.0));
        assert!(!soil.contains_key("moisture"));

        assert_eq!(find(&u, "eco_leaf_1")["leaf_wetness_pct"], json!(12.0));

        let leak = find(&u, "eco_leak_1");
        assert_eq!(leak["water_detected"], json!(true));
        assert_eq!(leak["battery_pct"], json!(100.0));
        assert!(!leak.contains_key("leak"));

        let l = find(&u, "eco_lightning");
        assert_eq!(l["distance_km"], json!(8.0));
        assert_eq!(l["strike_count"], json!(3));
        assert_eq!(l["battery_pct"], json!(80.0));

        let co2 = find(&u, "eco_co2");
        assert_eq!(co2["temperature"], json!(21.11));
        assert_eq!(co2["battery_pct"], json!(100.0));
    }

    /// The poll and the upload describe the same gateway the same way.
    #[test]
    fn both_paths_agree() {
        let pushed = upload(&[
            ("tempf", "68.0"),
            ("humidity", "45"),
            ("windspeedmph", "10.0"),
            ("baromrelin", "29.92"),
            ("solarradiation", "512.30"),
            ("tempinf", "72.5"),
            ("humidityin", "40"),
            ("dailyrainin", "1.00"),
            ("temp2f", "32.0"),
            ("humidity2", "50"),
            ("soilmoisture1", "37"),
            ("leafwetness_ch1", "12"),
            ("leak_ch1", "0"),
        ]);
        let polled = crate::parser::parse_livedata(
            &json!({
                "common_list": [
                    { "id": "0x02", "val": "68.0", "unit": "F" },
                    { "id": "0x07", "val": "45%" },
                    { "id": "0x0B", "val": "10.0 mph" },
                    { "id": "0x15", "val": "512.30 W/m2" }
                ],
                "wh25": [{ "intemp": "72.5", "unit": "F", "inhumi": "40%", "rel": "29.92 inHg" }],
                "rain": [{ "id": "0x10", "val": "1.00 in" }],
                "ch_aisle": [{ "channel": "2", "temp": "32.0", "unit": "F", "humidity": "50%" }],
                "ch_soil": [{ "channel": "1", "humidity": "37%" }],
                "ch_leaf": [{ "channel": "1", "humidity": "12%" }],
                "ch_leak": [{ "channel": "1", "status": "Normal" }]
            }),
            "eco",
        );
        for id in [
            "eco_weather",
            "eco_indoor",
            "eco_rain",
            "eco_temp_2",
            "eco_soil_1",
            "eco_leaf_1",
            "eco_leak_1",
        ] {
            assert_eq!(find(&pushed, id), find(&polled, id), "{id}");
        }
    }
}
