//! Parse Ecowitt `get_livedata_info` JSON response into HomeCore device state maps.
//!
//! Each top-level key in the response (common_list, rain, co2, ch_temp, etc.)
//! produces one or more (device_id, device_type, state_json) entries.

use crate::battery::{self, BatteryKind};
use crate::id_map::{common_id_to_attr, parse_value};
use serde_json::{json, Value};

/// A parsed device update ready to be published to HomeCore.
#[derive(Debug)]
pub struct DeviceUpdate {
    pub device_id: String,
    pub device_type: &'static str,
    pub name: String,
    pub state: Value,
}

/// Parse the full `get_livedata_info` response into device updates.
pub fn parse_livedata(data: &Value, prefix: &str) -> Vec<DeviceUpdate> {
    let mut updates = Vec::new();

    // common_list → aggregated weather station device
    if let Some(items) = data.get("common_list").and_then(|v| v.as_array()) {
        if let Some(update) = parse_common_list(items, prefix) {
            updates.push(update);
        }
    }

    // wh25 → indoor sensor
    if let Some(items) = data.get("wh25").and_then(|v| v.as_array()) {
        for item in items {
            updates.push(parse_wh25(item, prefix));
        }
    }

    // rain → rain sensor
    if let Some(items) = data.get("rain").and_then(|v| v.as_array()) {
        if let Some(u) = parse_rain_array(items, prefix, "rain") {
            updates.push(u);
        }
    }

    // piezoRain → piezo rain sensor
    if let Some(items) = data.get("piezoRain").and_then(|v| v.as_array()) {
        if let Some(u) = parse_rain_array(items, prefix, "piezo_rain") {
            updates.push(u);
        }
    }

    // lightning
    if let Some(items) = data.get("lightning").and_then(|v| v.as_array()) {
        for item in items {
            updates.push(parse_lightning(item, prefix));
        }
    }

    // co2
    if let Some(items) = data.get("co2").and_then(|v| v.as_array()) {
        for item in items {
            updates.push(parse_co2(item, prefix));
        }
    }

    // Multi-channel sensors
    parse_multi_channel(
        data,
        "ch_aisle",
        prefix,
        "aisle",
        "temperature_sensor",
        &mut updates,
    );
    parse_multi_channel(
        data,
        "ch_temp",
        prefix,
        "temp",
        "temperature_sensor",
        &mut updates,
    );
    parse_multi_channel(data, "ch_soil", prefix, "soil", "soil_sensor", &mut updates);
    parse_multi_channel(data, "ch_leaf", prefix, "leaf", "leaf_sensor", &mut updates);
    parse_multi_channel(data, "ch_leak", prefix, "leak", "leak_sensor", &mut updates);
    parse_multi_channel(
        data,
        "ch_pm25",
        prefix,
        "pm25",
        "air_quality_sensor",
        &mut updates,
    );
    parse_multi_channel(data, "ch_lds", prefix, "lds", "level_sensor", &mut updates);
    parse_multi_channel(data, "ch_ec", prefix, "ec", "ec_sensor", &mut updates);

    updates
}

// ---------------------------------------------------------------------------
// common_list — aggregated outdoor weather
// ---------------------------------------------------------------------------

fn parse_common_list(items: &[Value], prefix: &str) -> Option<DeviceUpdate> {
    let mut state = serde_json::Map::new();
    let mut has_battery = false;

    for item in items {
        let id = item["id"].as_str().unwrap_or("");
        let val_str = item["val"].as_str().unwrap_or("0");
        let unit = item.get("unit").and_then(|v| v.as_str()).unwrap_or("");

        if let Some(attr) = common_id_to_attr(id) {
            let (num, parsed_unit) = parse_value(val_str);
            state.insert(attr.to_string(), json!(num));
            if !unit.is_empty() {
                state.insert(format!("{attr}_unit"), json!(unit));
            } else if !parsed_unit.is_empty() {
                state.insert(format!("{attr}_unit"), json!(parsed_unit));
            }
        }

        if let Some(batt) = item.get("battery").and_then(|v| v.as_str()) {
            if !has_battery {
                let (b, _) = parse_value(batt);
                // Outdoor weather block can be reporting from WH65 (binary),
                // WH68 (AA voltage), or WS80/WS85/WS90 (supercap voltage) —
                // the cloud API doesn't tell us which sensor. Skip
                // classification rather than guess.
                battery::classify(&mut state, b, None);
                has_battery = true;
            }
        }
    }

    if state.is_empty() {
        return None;
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_weather"),
        device_type: "weather_station",
        name: "Weather Station".to_string(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// wh25 — indoor temp/humidity/barometric
// ---------------------------------------------------------------------------

fn parse_wh25(item: &Value, prefix: &str) -> DeviceUpdate {
    let mut state = serde_json::Map::new();

    if let Some(t) = item.get("intemp").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(t);
        state.insert("temperature".into(), json!(v));
    }
    if let Some(u) = item.get("unit").and_then(|v| v.as_str()) {
        state.insert("temperature_unit".into(), json!(u));
    }
    if let Some(h) = item.get("inhumi").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(h);
        state.insert("humidity".into(), json!(v));
    }
    if let Some(a) = item.get("abs").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(a);
        state.insert("barometric_abs".into(), json!(v));
    }
    if let Some(r) = item.get("rel").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(r);
        state.insert("barometric_rel".into(), json!(v));
    }

    DeviceUpdate {
        device_id: format!("{prefix}_indoor"),
        device_type: "temperature_sensor",
        name: "Indoor Sensor".to_string(),
        state: Value::Object(state),
    }
}

// ---------------------------------------------------------------------------
// rain / piezoRain
// ---------------------------------------------------------------------------

fn parse_rain_array(items: &[Value], prefix: &str, suffix: &str) -> Option<DeviceUpdate> {
    let mut state = serde_json::Map::new();

    for item in items {
        let id = item["id"].as_str().unwrap_or("");
        let val_str = item["val"].as_str().unwrap_or("0");

        let attr = match id {
            "0x0D" | "0xD" => "rain_event",
            "0x0E" | "0xE" => "rain_rate",
            "0x10" => "rain_day",
            "0x11" => "rain_week",
            "0x12" => "rain_month",
            "0x13" => "rain_year",
            "0x14" => "rain_total",
            _ => continue,
        };

        let (v, unit) = parse_value(val_str);
        state.insert(attr.into(), json!(v));
        if !unit.is_empty() {
            state.insert(format!("{attr}_unit"), json!(unit));
        }

        if let Some(batt) = item.get("battery").and_then(|v| v.as_str()) {
            let (b, _) = parse_value(batt);
            // Rain block can be WH40 (AA voltage) or rolled up from WS90
            // (supercap voltage) or WH65 (binary) — same ambiguity as
            // common_list. Skip classification.
            battery::classify(&mut state, b, None);
        }
    }

    if state.is_empty() {
        return None;
    }

    Some(DeviceUpdate {
        device_id: format!("{prefix}_{suffix}"),
        device_type: "rain_sensor",
        name: if suffix == "piezo_rain" {
            "Piezo Rain Sensor".into()
        } else {
            "Rain Sensor".into()
        },
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// lightning
// ---------------------------------------------------------------------------

fn parse_lightning(item: &Value, prefix: &str) -> DeviceUpdate {
    let mut state = serde_json::Map::new();

    if let Some(d) = item.get("distance").and_then(|v| v.as_str()) {
        let (v, unit) = parse_value(d);
        state.insert("distance".into(), json!(v));
        if !unit.is_empty() {
            state.insert("distance_unit".into(), json!(unit));
        }
    }
    if let Some(c) = item.get("count").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(c);
        state.insert("strike_count".into(), json!(v as u64));
    }
    if let Some(ts) = item.get("timestamp").and_then(|v| v.as_str()) {
        state.insert("last_strike".into(), json!(ts));
    }
    if let Some(b) = item.get("battery").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(b);
        battery::classify(&mut state, v, Some(BatteryKind::Level { max: 5 }));
    }

    DeviceUpdate {
        device_id: format!("{prefix}_lightning"),
        device_type: "lightning_sensor",
        name: "Lightning Sensor".to_string(),
        state: Value::Object(state),
    }
}

// ---------------------------------------------------------------------------
// co2 — air quality
// ---------------------------------------------------------------------------

fn parse_co2(item: &Value, prefix: &str) -> DeviceUpdate {
    let mut state = serde_json::Map::new();

    for (key, attr) in &[
        ("temp", "temperature"),
        ("humidity", "humidity"),
        ("PM25", "pm25"),
        ("PM25_RealAQI", "pm25_aqi"),
        ("PM25_24HAQI", "pm25_24h_aqi"),
        ("PM10", "pm10"),
        ("PM10_RealAQI", "pm10_aqi"),
        ("PM10_24HAQI", "pm10_24h_aqi"),
        ("PM1", "pm1"),
        ("PM1_RealAQI", "pm1_aqi"),
        ("PM4", "pm4"),
        ("CO2", "co2"),
        ("CO2_24H", "co2_24h"),
    ] {
        if let Some(v) = item.get(*key).and_then(|v| v.as_str()) {
            let (num, _) = parse_value(v);
            state.insert(attr.to_string(), json!(num));
        }
    }
    // WH45/CO2 battery is a 0..=6 level, NOT a percentage.
    if let Some(b) = item.get("battery").and_then(|v| v.as_str()) {
        let (v, _) = parse_value(b);
        battery::classify(&mut state, v, Some(BatteryKind::Level { max: 6 }));
    }
    if let Some(u) = item.get("unit").and_then(|v| v.as_str()) {
        state.insert("temperature_unit".into(), json!(u));
    }

    DeviceUpdate {
        device_id: format!("{prefix}_co2"),
        device_type: "air_quality_sensor",
        name: "CO2 / Air Quality Sensor".to_string(),
        state: Value::Object(state),
    }
}

// ---------------------------------------------------------------------------
// Multi-channel sensors (ch_aisle, ch_temp, ch_soil, ch_leaf, ch_leak, etc.)
// ---------------------------------------------------------------------------

fn parse_multi_channel(
    data: &Value,
    key: &str,
    prefix: &str,
    slug: &str,
    device_type: &'static str,
    updates: &mut Vec<DeviceUpdate>,
) {
    let Some(items) = data.get(key).and_then(|v| v.as_array()) else {
        return;
    };

    for item in items {
        let channel = item["channel"].as_str().unwrap_or("0");
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mut state = serde_json::Map::new();

        // Extract all known fields
        for (field, attr) in &[
            ("temp", "temperature"),
            ("humidity", "humidity"),
            ("voltage", "voltage"),
            ("air", "air"),
            ("depth", "depth"),
            ("status", "status"),
            ("PM25", "pm25"),
            ("PM25_RealAQI", "pm25_aqi"),
            ("PM25_24HAQI", "pm25_24h_aqi"),
        ] {
            if let Some(v) = item.get(*field).and_then(|v| v.as_str()) {
                if *field == "status" {
                    // Leak sensor: keep as string
                    state.insert(attr.to_string(), json!(v));
                } else {
                    let (num, _) = parse_value(v);
                    state.insert(attr.to_string(), json!(num));
                }
            }
        }

        if let Some(u) = item.get("unit").and_then(|v| v.as_str()) {
            state.insert("temperature_unit".into(), json!(u));
        }
        if let Some(b) = item.get("battery").and_then(|v| v.as_str()) {
            let (v, _) = parse_value(b);
            battery::classify(&mut state, v, multi_channel_battery_kind(slug));
        }

        let display_name = if name.is_empty() {
            format!("{} Ch {}", slug_to_label(slug), channel)
        } else {
            name.to_string()
        };

        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_{slug}_{channel}"),
            device_type,
            name: display_name,
            state: Value::Object(state),
        });
    }
}

fn slug_to_label(slug: &str) -> &str {
    match slug {
        "aisle" => "Temp/Humidity",
        "temp" => "Temperature",
        "soil" => "Soil Moisture",
        "leaf" => "Leaf Wetness",
        "leak" => "Leak Detector",
        "pm25" => "PM2.5 Sensor",
        "lds" => "Level Sensor",
        "ec" => "EC Sensor",
        _ => slug,
    }
}

/// Map a multi-channel sensor slug to the battery kind it reports.
///
/// Cloud-API path: each channel block has a generic `battery` field and
/// the surrounding key (e.g. `ch_soil`) tells us which sensor model is
/// reporting. See `battery::BatteryKind` docs for the per-family rules.
fn multi_channel_battery_kind(slug: &str) -> Option<BatteryKind> {
    match slug {
        // WH31 multi-channel T/H — binary 0/1
        "aisle" => Some(BatteryKind::Binary),
        // WN34 soil temp / WH51 soil moisture / WN35 leaf wetness / LDS / EC
        // — all single-AA powered, voltage with ~1.2V threshold
        "temp" | "soil" | "leaf" | "lds" | "ec" => {
            Some(BatteryKind::Voltage { low_threshold: 1.2 })
        }
        // PM2.5 channels 1-4 + leak detectors — discrete level 0..=5
        "pm25" | "leak" => Some(BatteryKind::Level { max: 5 }),
        _ => None,
    }
}
