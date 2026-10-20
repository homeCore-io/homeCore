//! Parse Ecowitt `get_livedata_info` JSON response into HomeCore device state maps.
//!
//! Each top-level key in the response (common_list, rain, co2, ch_aisle, etc.)
//! produces one or more (device_id, device_type, state_json) entries — with the
//! same device ids, attribute names and units as the custom-server upload
//! (`form_parser`), so it does not matter to anything downstream which way the
//! plugin hears its gateway.

use crate::battery::{self, BatteryKind};
use crate::id_map::{common_id, rain_id, Dest, Quantity};
use crate::units;
use serde_json::{json, Map, Value};

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

    let mut weather = Map::new();
    let mut indoor = Map::new();
    let mut rain = Map::new();

    // common_list → mostly the outdoor station, with indoor and rain readings
    // sent to their own devices as the upload does.
    if let Some(items) = data.get("common_list").and_then(|v| v.as_array()) {
        parse_common_list(items, &mut weather, &mut indoor, &mut rain);
    }

    // wh25 → indoor temperature and humidity; its barometer reads the air
    // pressure, which belongs with the weather (where the upload puts it).
    if let Some(items) = data.get("wh25").and_then(|v| v.as_array()) {
        for item in items {
            parse_wh25(item, &mut indoor, &mut weather);
        }
    }

    // rain → rain gauge
    if let Some(items) = data.get("rain").and_then(|v| v.as_array()) {
        parse_rain_array(items, &mut rain);
    }

    if !weather.is_empty() {
        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_weather"),
            device_type: "weather_station",
            name: "Weather Station".to_string(),
            state: Value::Object(weather),
        });
    }
    if !indoor.is_empty() {
        updates.push(DeviceUpdate {
            device_id: format!("{prefix}_indoor"),
            device_type: "temperature_sensor",
            name: "Indoor Sensor".to_string(),
            state: Value::Object(indoor),
        });
    }
    if !rain.is_empty() {
        updates.push(rain_update(prefix, "rain", rain));
    }

    // piezoRain → piezo rain gauge (WS90/WS85)
    if let Some(items) = data.get("piezoRain").and_then(|v| v.as_array()) {
        let mut piezo = Map::new();
        parse_rain_array(items, &mut piezo);
        if !piezo.is_empty() {
            updates.push(rain_update(prefix, "piezo_rain", piezo));
        }
    }

    // lightning
    if let Some(items) = data.get("lightning").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(u) = parse_lightning(item, prefix) {
                updates.push(u);
            }
        }
    }

    // co2
    if let Some(items) = data.get("co2").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(u) = parse_co2(item, prefix) {
                updates.push(u);
            }
        }
    }

    // Multi-channel sensors. `ch_aisle` is the WH31 thermo-hygrometer, which
    // the upload sends as `temp{n}f` — `{prefix}_temp_{n}` on both paths.
    // `ch_temp` is the WN34 probe, the upload's `tf_ch{n}`: `{prefix}_tf_{n}`.
    for (key, slug, device_type) in [
        ("ch_aisle", "temp", "temperature_sensor"),
        ("ch_temp", "tf", "temperature_sensor"),
        ("ch_soil", "soil", "soil_sensor"),
        ("ch_leaf", "leaf", "leaf_sensor"),
        ("ch_leak", "leak", "leak_sensor"),
        ("ch_pm25", "pm25", "air_quality_sensor"),
        ("ch_lds", "lds", "level_sensor"),
        ("ch_ec", "ec", "ec_sensor"),
    ] {
        parse_multi_channel(data, key, prefix, slug, device_type, &mut updates);
    }

    updates
}

pub(crate) fn rain_update(prefix: &str, suffix: &str, state: Map<String, Value>) -> DeviceUpdate {
    DeviceUpdate {
        device_id: format!("{prefix}_{suffix}"),
        device_type: "rain_sensor",
        name: if suffix == "piezo_rain" {
            "Piezo Rain Sensor".into()
        } else {
            "Rain Sensor".into()
        },
        state: Value::Object(state),
    }
}

/// `item[key]` read as `q`, in the unit it carries or else `unit_hint`, into
/// `state[attr]`. Nothing is inserted for a value that cannot be read.
fn put(
    state: &mut Map<String, Value>,
    item: &Value,
    key: &str,
    attr: &str,
    q: Quantity,
    hint: &str,
) {
    if let Some(v) = item
        .get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| q.read(s, hint))
    {
        state.insert(attr.into(), v);
    }
}

fn unit_of(item: &Value) -> &str {
    item.get("unit").and_then(|v| v.as_str()).unwrap_or("")
}

// ---------------------------------------------------------------------------
// common_list — aggregated outdoor weather
// ---------------------------------------------------------------------------

fn parse_common_list(
    items: &[Value],
    weather: &mut Map<String, Value>,
    indoor: &mut Map<String, Value>,
    rain: &mut Map<String, Value>,
) {
    let mut has_battery = false;

    for item in items {
        let id = item["id"].as_str().unwrap_or("");
        if let Some((dest, attr, q)) = common_id(id) {
            let target = match dest {
                Dest::Weather => &mut *weather,
                Dest::Indoor => &mut *indoor,
                Dest::Rain => &mut *rain,
            };
            put(target, item, "val", attr, q, unit_of(item));
        }

        if let Some(batt) = item.get("battery").and_then(|v| v.as_str()) {
            if !has_battery {
                if let Some((b, _)) = units::parse_reading(batt) {
                    // Outdoor weather block can be reporting from WH65 (binary),
                    // WH68 (AA voltage), or WS80/WS85/WS90 (supercap voltage) —
                    // the API doesn't tell us which sensor. Skip
                    // classification rather than guess.
                    battery::classify(weather, b, None);
                    has_battery = true;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// wh25 — indoor temp/humidity/barometric
// ---------------------------------------------------------------------------

fn parse_wh25(item: &Value, indoor: &mut Map<String, Value>, weather: &mut Map<String, Value>) {
    let unit = unit_of(item);
    put(
        indoor,
        item,
        "intemp",
        "temperature",
        Quantity::Temperature,
        unit,
    );
    put(indoor, item, "inhumi", "humidity", Quantity::Percent, "");
    put(
        weather,
        item,
        "abs",
        "pressure_absolute",
        Quantity::Pressure,
        "",
    );
    put(weather, item, "rel", "pressure", Quantity::Pressure, "");
}

// ---------------------------------------------------------------------------
// rain / piezoRain
// ---------------------------------------------------------------------------

fn parse_rain_array(items: &[Value], state: &mut Map<String, Value>) {
    for item in items {
        let id = item["id"].as_str().unwrap_or("");

        if id == "srain_piezo" {
            if let Some((v, _)) = item["val"].as_str().and_then(units::parse_reading) {
                state.insert("raining".into(), json!(v >= 1.0));
            }
        } else if let Some(attr) = rain_id(id) {
            put(state, item, "val", attr, Quantity::Rain, unit_of(item));
        }

        if let Some(batt) = item.get("battery").and_then(|v| v.as_str()) {
            if let Some((b, _)) = units::parse_reading(batt) {
                // Rain block can be WH40 (AA voltage) or rolled up from WS90
                // (supercap voltage) or WH65 (binary) — same ambiguity as
                // common_list. Skip classification.
                battery::classify(state, b, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// lightning
// ---------------------------------------------------------------------------

fn parse_lightning(item: &Value, prefix: &str) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    if let Some((v, unit)) = item
        .get("distance")
        .and_then(|v| v.as_str())
        .and_then(units::parse_reading)
    {
        let unit = if unit.is_empty() { unit_of(item) } else { unit };
        if let Some(km) = units::distance_km(v, unit) {
            state.insert("distance_km".into(), json!(km));
        }
    }
    if let Some((v, _)) = item
        .get("count")
        .and_then(|v| v.as_str())
        .and_then(units::parse_reading)
    {
        state.insert("strike_count".into(), json!(v as u64));
    }
    if let Some(ts) = item.get("timestamp").and_then(|v| v.as_str()) {
        state.insert("last_strike".into(), json!(ts));
    }
    if let Some((v, _)) = item
        .get("battery")
        .and_then(|v| v.as_str())
        .and_then(units::parse_reading)
    {
        battery::classify(&mut state, v, Some(BatteryKind::Level { max: 5 }));
    }

    (!state.is_empty()).then(|| DeviceUpdate {
        device_id: format!("{prefix}_lightning"),
        device_type: "lightning_sensor",
        name: "Lightning Sensor".to_string(),
        state: Value::Object(state),
    })
}

// ---------------------------------------------------------------------------
// co2 — air quality
// ---------------------------------------------------------------------------

fn parse_co2(item: &Value, prefix: &str) -> Option<DeviceUpdate> {
    let mut state = Map::new();

    put(
        &mut state,
        item,
        "temp",
        "temperature",
        Quantity::Temperature,
        unit_of(item),
    );
    put(
        &mut state,
        item,
        "humidity",
        "humidity",
        Quantity::Percent,
        "",
    );
    for (key, attr) in [
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
        put(&mut state, item, key, attr, Quantity::Plain, "");
    }
    // WH45/CO2 battery is a 0..=6 level, NOT a percentage.
    if let Some((v, _)) = item
        .get("battery")
        .and_then(|v| v.as_str())
        .and_then(units::parse_reading)
    {
        battery::classify(&mut state, v, Some(BatteryKind::Level { max: 6 }));
    }

    (!state.is_empty()).then(|| DeviceUpdate {
        device_id: format!("{prefix}_co2"),
        device_type: "air_quality_sensor",
        name: "CO2 / Air Quality Sensor".to_string(),
        state: Value::Object(state),
    })
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
        let unit = unit_of(item);
        let mut state = Map::new();

        put(
            &mut state,
            item,
            "temp",
            "temperature",
            Quantity::Temperature,
            unit,
        );
        // The `humidity` field is the reading's percentage, which on a soil or
        // leaf sensor is not air humidity.
        let pct_name = match slug {
            "soil" | "ec" => "soil_moisture_pct",
            "leaf" => "leaf_wetness_pct",
            _ => "humidity",
        };
        put(
            &mut state,
            item,
            "humidity",
            pct_name,
            Quantity::Percent,
            "",
        );
        put(&mut state, item, "voltage", "voltage", Quantity::Plain, "");
        put(&mut state, item, "PM25", "pm25", Quantity::Plain, "");
        put(
            &mut state,
            item,
            "PM25_RealAQI",
            "pm25_aqi",
            Quantity::Plain,
            "",
        );
        put(
            &mut state,
            item,
            "PM25_24HAQI",
            "pm25_24h_aqi",
            Quantity::Plain,
            "",
        );
        put(&mut state, item, "ec", "ec", Quantity::Plain, "");

        // Water level (WH54 LDS): distance from the sensor to the surface, and
        // the depth below it.
        for (field, attr) in [("air", "air_gap_mm"), ("depth", "depth_mm")] {
            if let Some((v, u)) = item
                .get(field)
                .and_then(|v| v.as_str())
                .and_then(units::parse_reading)
            {
                let u = if u.is_empty() { unit } else { u };
                if let Some(mm) = units::length_mm(v, u) {
                    state.insert(attr.into(), json!(mm));
                }
            }
        }

        // Leak detector: "Normal" or "Leaking" (or 0/1 on some firmware).
        if slug == "leak" {
            if let Some(s) = item.get("status").and_then(|v| v.as_str()) {
                if let Some(wet) = leak_status(s) {
                    state.insert("water_detected".into(), json!(wet));
                }
            }
        }

        if let Some((v, _)) = item
            .get("battery")
            .and_then(|v| v.as_str())
            .and_then(units::parse_reading)
        {
            battery::classify(&mut state, v, multi_channel_battery_kind(slug));
        }

        if state.is_empty() {
            continue;
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

/// A leak detector's status as wet or dry; `None` for one this cannot read.
fn leak_status(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "normal" | "0" | "dry" | "ok" => Some(false),
        "leaking" | "leak" | "1" | "alarm" | "wet" => Some(true),
        _ => None,
    }
}

pub(crate) fn slug_to_label(slug: &str) -> &str {
    match slug {
        "temp" => "Temp/Humidity",
        "tf" => "Temperature Probe",
        "soil" => "Soil Moisture",
        "leaf" => "Leaf Wetness",
        "leak" => "Leak Detector",
        "pm25" => "PM2.5 Sensor",
        "lds" => "Level Sensor",
        "ec" => "Soil EC",
        _ => slug,
    }
}

/// Map a multi-channel sensor slug to the battery kind it reports.
///
/// Each channel block has a generic `battery` field and the surrounding key
/// (e.g. `ch_soil`) tells us which sensor model is reporting. See
/// `battery::BatteryKind` docs for the per-family rules.
fn multi_channel_battery_kind(slug: &str) -> Option<BatteryKind> {
    match slug {
        // WH31 multi-channel T/H — binary 0/1
        "temp" => Some(BatteryKind::Binary),
        // WN34 soil temp / WH51 soil moisture / WN35 leaf wetness / LDS / EC
        // — all single-AA powered, voltage with ~1.2V threshold
        "tf" | "soil" | "leaf" | "lds" | "ec" => Some(BatteryKind::Voltage { low_threshold: 1.2 }),
        // PM2.5 channels 1-4 + leak detectors — discrete level 0..=5
        "pm25" | "leak" => Some(BatteryKind::Level { max: 5 }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(u: &'a [DeviceUpdate], id: &str) -> &'a Map<String, Value> {
        u.iter()
            .find(|d| d.device_id == id)
            .unwrap_or_else(|| panic!("no {id}"))
            .state
            .as_object()
            .unwrap()
    }

    /// A gateway whose display is imperial, as the poll reports it.
    fn imperial() -> Value {
        json!({
            "common_list": [
                { "id": "0x02", "val": "68.0", "unit": "F" },
                { "id": "0x07", "val": "45%" },
                { "id": "0x03", "val": "50.0", "unit": "F" },
                { "id": "0x0A", "val": "213" },
                { "id": "0x0B", "val": "10.0 mph" },
                { "id": "0x0C", "val": "--" },
                { "id": "0x15", "val": "512.30 W/m2" },
                { "id": "0x17", "val": "3" },
                { "id": "0x19", "val": "20.0 mph" },
                { "id": "0x16", "val": "1234.5" }
            ],
            "wh25": [{ "intemp": "72.5", "unit": "F", "inhumi": "40%",
                       "abs": "29.50 inHg", "rel": "29.92 inHg" }],
            "rain": [
                { "id": "0x0E", "val": "0.50 in/Hr" },
                { "id": "0x10", "val": "1.00 in", "battery": "1.4" }
            ],
            "piezoRain": [
                { "id": "srain_piezo", "val": "1" },
                { "id": "0x10", "val": "0.10 in" }
            ],
            "lightning": [{ "distance": "5 mi", "count": "3", "timestamp": "1700000000", "battery": "4" }],
            "ch_aisle": [{ "channel": "2", "name": "", "battery": "0", "temp": "32.0", "unit": "F", "humidity": "50%" }],
            "ch_temp": [{ "channel": "1", "name": "Pond", "battery": "1.1", "temp": "50.0", "unit": "F" }],
            "ch_soil": [{ "channel": "1", "name": "", "battery": "1.5", "humidity": "37%" }],
            "ch_leaf": [{ "channel": "1", "name": "", "battery": "1.5", "humidity": "12%" }],
            "ch_leak": [{ "channel": "1", "name": "", "battery": "5", "status": "Leaking" }],
            "co2": [{ "temp": "70.0", "unit": "F", "humidity": "40%", "CO2": "612", "battery": "6" }]
        })
    }

    #[test]
    fn readings_get_the_shared_names_in_homecores_units() {
        let u = parse_livedata(&imperial(), "eco");
        let w = find(&u, "eco_weather");
        assert_eq!(w["temperature"], json!(20.0));
        assert_eq!(w["humidity"], json!(45.0));
        assert_eq!(w["dewpoint"], json!(10.0));
        assert_eq!(w["wind_direction"], json!(213.0));
        assert_eq!(w["wind_speed"], json!(4.47));
        assert_eq!(w["wind_gust_daily_max"], json!(8.94));
        assert_eq!(w["solar_radiation"], json!(512.3));
        assert_eq!(w["uvi"], json!(3.0));
        assert_eq!(w["pressure"], json!(1013.21));
        assert_eq!(w["pressure_absolute"], json!(998.98));
        for old in [
            "outdoor_temperature",
            "outdoor_humidity",
            "light",
            "barometric_rel",
            "uv",
            "temperature_unit",
            "wind_speed_unit",
        ] {
            assert!(!w.contains_key(old), "{old} is still published");
        }
    }

    /// `--` is a lost sensor, not a calm day.
    #[test]
    fn an_unreadable_value_is_skipped() {
        let u = parse_livedata(&imperial(), "eco");
        assert!(!find(&u, "eco_weather").contains_key("wind_gust"));
    }

    #[test]
    fn indoor_rain_and_lightning() {
        let u = parse_livedata(&imperial(), "eco");
        let i = find(&u, "eco_indoor");
        assert_eq!(i["temperature"], json!(22.5));
        assert_eq!(i["humidity"], json!(40.0));
        assert!(
            !i.contains_key("pressure"),
            "pressure goes with the weather"
        );

        let r = find(&u, "eco_rain");
        assert_eq!(r["rain_rate_mm_hr"], json!(12.7));
        assert_eq!(r["rain_daily_mm"], json!(25.4));

        let p = find(&u, "eco_piezo_rain");
        assert_eq!(p["raining"], json!(true));
        assert_eq!(p["rain_daily_mm"], json!(2.54));

        let l = find(&u, "eco_lightning");
        assert_eq!(l["distance_km"], json!(8.05));
        assert_eq!(l["strike_count"], json!(3));
        assert_eq!(l["battery_pct"], json!(80.0));
    }

    /// The same sensor gets the same device id as the upload gives it.
    #[test]
    fn channels_match_the_upload() {
        let u = parse_livedata(&imperial(), "eco");
        let wh31 = find(&u, "eco_temp_2");
        assert_eq!(wh31["temperature"], json!(0.0));
        assert_eq!(wh31["humidity"], json!(50.0));
        assert_eq!(wh31["battery_low"], json!(false));
        assert!(
            !wh31.contains_key("battery_pct"),
            "a 0/1 flag is not a percentage"
        );

        let probe = find(&u, "eco_tf_1");
        assert_eq!(probe["temperature"], json!(10.0));
        assert_eq!(probe["voltage"], json!(1.1));
        assert_eq!(probe["battery_low"], json!(true));

        let soil = find(&u, "eco_soil_1");
        assert_eq!(soil["soil_moisture_pct"], json!(37.0));
        assert!(!soil.contains_key("humidity"));

        assert_eq!(find(&u, "eco_leaf_1")["leaf_wetness_pct"], json!(12.0));
        assert_eq!(find(&u, "eco_leak_1")["water_detected"], json!(true));

        let co2 = find(&u, "eco_co2");
        assert_eq!(co2["temperature"], json!(21.11));
        assert_eq!(co2["co2"], json!(612.0));
        assert_eq!(co2["battery_pct"], json!(100.0));
    }

    #[test]
    fn a_metric_gateway_needs_no_conversion() {
        let data = json!({
            "common_list": [
                { "id": "0x02", "val": "21.4", "unit": "C" },
                { "id": "0x0B", "val": "3.2 m/s" },
                { "id": "0x0B", "val": "3.2 m/s" }
            ],
            "wh25": [{ "intemp": "23.9", "unit": "C", "inhumi": "45%", "rel": "1011.9 hPa" }],
            "rain": [{ "id": "0x10", "val": "4.2 mm" }]
        });
        let u = parse_livedata(&data, "eco");
        let w = find(&u, "eco_weather");
        assert_eq!(w["temperature"], json!(21.4));
        assert_eq!(w["wind_speed"], json!(3.2));
        assert_eq!(w["pressure"], json!(1011.9));
        assert_eq!(find(&u, "eco_rain")["rain_daily_mm"], json!(4.2));
    }
}
