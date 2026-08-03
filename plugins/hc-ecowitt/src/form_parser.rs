//! Parse Ecowitt form-encoded POST data into HomeCore device updates.
//!
//! The Ecowitt gateway's "custom server" feature sends flat key-value pairs
//! as `application/x-www-form-urlencoded`.  Field names follow Ecowitt/WU
//! conventions (e.g. `tempf`, `humidity`, `windspeedmph`, `dailyrainin`).

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use tracing::debug;

use crate::battery;
use crate::parser::DeviceUpdate;

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

    // Rain sensor
    if let Some(u) = parse_rain(fields, prefix, &mut consumed) {
        updates.push(u);
    }

    // Multi-channel temperature/humidity sensors (temp1f..temp8f, humidity1..humidity8)
    parse_numbered_temp(fields, prefix, &mut consumed, &mut updates);

    // Soil moisture sensors (soilmoisture1..soilmoisture16)
    parse_soil_moisture(fields, prefix, &mut consumed, &mut updates);

    // Soil temp/EC sensors — WH52 3-in-1 (soilec_ch1..soilec_ch16)
    parse_soil_ec(fields, prefix, &mut consumed, &mut updates);

    // Leaf wetness sensors (leafwetness_ch1..leafwetness_ch8)
    parse_leaf_wetness(fields, prefix, &mut consumed, &mut updates);

    // Leak sensors (leak_ch1..leak_ch4)
    parse_leak(fields, prefix, &mut consumed, &mut updates);

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

// ---------------------------------------------------------------------------
// Outdoor weather station
// ---------------------------------------------------------------------------

fn parse_outdoor(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    let outdoor_fields: &[(&str, &str)] = &[
        ("tempf", "temperature"),
        ("tempc", "temperature"),
        ("humidity", "humidity"),
        ("dewptf", "dewpoint"),
        ("dewptc", "dewpoint"),
        ("windchillf", "windchill"),
        ("windchillc", "windchill"),
        ("feelslikef", "feels_like"),
        ("heatindexf", "heat_index"),
        ("vpd", "vpd"),
        ("winddir", "wind_direction"),
        ("winddir_avg10m", "wind_direction_avg10m"),
        ("windspeedmph", "wind_speed"),
        ("windspdmph_avg10m", "wind_speed_avg10m"),
        ("windgustmph", "gust_speed"),
        ("maxdailygust", "daily_max_wind"),
        ("solarradiation", "light"),
        ("uv", "uvi"),
        ("baromrelin", "barometric_rel"),
        ("baromabsin", "barometric_abs"),
    ];

    for &(field, attr) in outdoor_fields {
        if insert_f64(&mut state, fields, field, attr) {
            consumed.insert(field.into());
        }
    }

    if let Some(v) = fields.get("dateutc") {
        state.insert("datetime".into(), json!(v));
        consumed.insert("dateutc".into());
    }

    // Temperature unit hint
    if fields.contains_key("tempf") {
        state.insert("temperature_unit".into(), json!("°F"));
    } else if fields.contains_key("tempc") {
        state.insert("temperature_unit".into(), json!("°C"));
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

    for &(field, attr) in &[
        ("tempinf", "temperature"),
        ("tempinc", "temperature"),
        ("humidityin", "humidity"),
    ] {
        if insert_f64(&mut state, fields, field, attr) {
            consumed.insert(field.into());
        }
    }

    if fields.contains_key("tempinf") {
        state.insert("temperature_unit".into(), json!("°F"));
    } else if fields.contains_key("tempinc") {
        state.insert("temperature_unit".into(), json!("°C"));
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
// Rain sensor
// ---------------------------------------------------------------------------

fn parse_rain(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    let rain_fields: &[(&str, &str)] = &[
        ("rainratein", "rain_rate"),
        ("rainratemm", "rain_rate"),
        ("eventrainin", "rain_event"),
        ("eventrainmm", "rain_event"),
        ("hourlyrainin", "rain_hour"),
        ("hourlyrainmm", "rain_hour"),
        ("dailyrainin", "rain_day"),
        ("dailyrainmm", "rain_day"),
        ("weeklyrainin", "rain_week"),
        ("weeklyrainmm", "rain_week"),
        ("monthlyrainin", "rain_month"),
        ("monthlyrainmm", "rain_month"),
        ("yearlyrainin", "rain_year"),
        ("yearlyrainmm", "rain_year"),
        ("totalrainin", "rain_total"),
        ("totalrainmm", "rain_total"),
        // Piezo rain
        ("rrain_piezo", "rain_rate_piezo"),
        ("erain_piezo", "rain_event_piezo"),
        ("hrain_piezo", "rain_hour_piezo"),
        ("drain_piezo", "rain_day_piezo"),
        ("wrain_piezo", "rain_week_piezo"),
        ("mrain_piezo", "rain_month_piezo"),
        ("yrain_piezo", "rain_year_piezo"),
        ("srain_piezo", "rain_state_piezo"),
        ("last24hrain_piezo", "rain_24h_piezo"),
    ];

    for &(field, attr) in rain_fields {
        if insert_f64(&mut state, fields, field, attr) {
            consumed.insert(field.into());
        }
    }

    if state.is_empty() {
        return None;
    }

    // Rain gauge battery (wh40batt = voltage AA-class, wh90batt = voltage
    // supercap if WS90 doubles as the rain source on this station).
    for key in ["wh40batt", "wh90batt"] {
        if battery::insert(&mut state, fields, key) {
            consumed.insert(key.into());
            break;
        }
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_rain"),
        device_type: "rain_sensor",
        name: "Rain Sensor".into(),
        state: Value::Object(state),
    })
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
        let temp_f_key = format!("temp{ch}f");
        let temp_c_key = format!("temp{ch}c");
        let humi_key = format!("humidity{ch}");
        let batt_key = format!("batt{ch}");

        let mut state = Map::new();
        if insert_f64_dyn(&mut state, fields, &temp_f_key, "temperature") {
            consumed.insert(temp_f_key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &temp_c_key, "temperature") {
            consumed.insert(temp_c_key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &humi_key, "humidity") {
            consumed.insert(humi_key.clone());
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key.clone());
        }

        if state.is_empty() {
            continue;
        }

        if fields.contains_key(&temp_f_key) {
            state.insert("temperature_unit".into(), json!("°F"));
        } else if fields.contains_key(&temp_c_key) {
            state.insert("temperature_unit".into(), json!("°C"));
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
// Soil moisture sensors (soilmoisture1..soilmoisture16)
// ---------------------------------------------------------------------------

fn parse_soil_moisture(
    fields: &HashMap<String, String>,
    prefix: &str,
    consumed: &mut HashSet<String>,
    updates: &mut Vec<DeviceUpdate>,
) {
    for ch in 1..=16 {
        let key = format!("soilmoisture{ch}");
        let ad_key = format!("soilad{ch}");
        let batt_key = format!("soilbatt{ch}");

        let mut state = Map::new();
        if insert_f64_dyn(&mut state, fields, &key, "moisture") {
            consumed.insert(key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &ad_key, "moisture_ad") {
            consumed.insert(ad_key.clone());
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key.clone());
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
        let ec_key = format!("soilec_ch{ch}");
        let hum_key = format!("soilec_hum{ch}");
        let temp_key = format!("soilec_temp{ch}");

        let mut state = Map::new();
        if insert_f64_dyn(&mut state, fields, &ec_key, "ec") {
            consumed.insert(ec_key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &hum_key, "moisture") {
            consumed.insert(hum_key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &temp_key, "temperature") {
            consumed.insert(temp_key.clone());
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
        if insert_f64_dyn(&mut state, fields, &key, "wetness") {
            consumed.insert(key.clone());
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key.clone());
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

        let mut state = Map::new();
        if insert_f64_dyn(&mut state, fields, &key, "leak") {
            consumed.insert(key.clone());
        }
        if !state.is_empty() {
            if battery::insert(&mut state, fields, &batt_key) {
                consumed.insert(batt_key.clone());
            }

            updates.push(DeviceUpdate {
                device_id: format!("{prefix}_leak_{ch}"),
                device_type: "leak_sensor",
                name: format!("Leak Detector Ch {ch}"),
                state: Value::Object(state),
            });
        }
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
        let key = format!("pm25_ch{ch}");
        let avg_key = format!("pm25_avg_24h_ch{ch}");
        let batt_key = format!("pm25batt{ch}");

        let mut state = Map::new();
        if insert_f64_dyn(&mut state, fields, &key, "pm25") {
            consumed.insert(key.clone());
        }
        if insert_f64_dyn(&mut state, fields, &avg_key, "pm25_24h") {
            consumed.insert(avg_key.clone());
        }
        if battery::insert(&mut state, fields, &batt_key) {
            consumed.insert(batt_key.clone());
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

    // Distance field is just "lightning" (not "lightning_distance")
    for &(field, attr) in &[("lightning", "distance"), ("lightning_num", "strike_count")] {
        if insert_f64(&mut state, fields, field, attr) {
            consumed.insert(field.into());
        }
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

    let co2_fields: &[(&str, &str)] = &[
        ("co2", "co2"),
        ("co2_24h", "co2_24h"),
        ("co2in", "co2_indoor"),
        ("co2in_24h", "co2_indoor_24h"),
        ("pm25_co2", "pm25"),
        ("pm25_24h_co2", "pm25_24h"),
        ("pm10_co2", "pm10"),
        ("pm10_24h_co2", "pm10_24h"),
        ("tf_co2", "temperature"),
        ("humi_co2", "humidity"),
    ];

    for &(field, attr) in co2_fields {
        if insert_f64(&mut state, fields, field, attr) {
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

/// Insert a field value as f64. Returns true if inserted.
fn insert_f64(
    state: &mut Map<String, Value>,
    fields: &HashMap<String, String>,
    key: &str,
    attr: &str,
) -> bool {
    if let Some(v) = fields.get(key).and_then(|s| s.parse::<f64>().ok()) {
        state.insert(attr.into(), json!(v));
        true
    } else {
        false
    }
}

/// Same as insert_f64 but for dynamically-built key strings.
fn insert_f64_dyn(
    state: &mut Map<String, Value>,
    fields: &HashMap<String, String>,
    key: &str,
    attr: &str,
) -> bool {
    if let Some(v) = fields.get(key).and_then(|s| s.parse::<f64>().ok()) {
        state.insert(attr.into(), json!(v));
        true
    } else {
        false
    }
}
