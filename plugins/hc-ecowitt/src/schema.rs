//! What an Ecowitt device reports, described from the reading itself.
//!
//! This plugin published no [`DeviceSchema`], so clients inferred every
//! attribute from its observed value and took its *meaning* from a lexicon
//! keyed on the attribute name.
//!
//! **The schema is generated from the state, not hand-listed.** Ecowitt's
//! attribute set is not fixed: roughly sixty names appear across the sensor
//! models this plugin parses, and which of them a given gateway emits depends
//! on what hardware is paired with it — a WS90 publishes piezo rain keys a
//! WH65 never will. A hand-written per-type list would declare attributes that
//! do not exist on most devices and miss ones that do, and it would drift the
//! first time a model is added.
//!
//! Describing what was actually reported cannot drift by construction. The
//! knowledge lives in [`describe`] — units, labels, and the names of the two
//! boolean states — and presence comes from the data.
//!
//! **Nothing is writable.** Ecowitt gateways are receivers: the plugin
//! subscribes to commands only for its own management actions, and no device
//! attribute is ever written back.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The unit for an attribute.
///
/// The parsers convert every reading to homeCore's units (`units`), so each
/// name has exactly one: °C, hPa, m/s, mm, km, W/m², whatever the gateway's
/// display is set to. Showing °F or inches is the app's job. A reading with no
/// unit (the UV index, an AQI, a count) declares none.
fn unit_for(name: &str) -> Option<&'static str> {
    let name = base_name(name).0;
    match name {
        "temperature" | "dewpoint" | "windchill" | "feels_like" | "heat_index" | "wbgt" => {
            Some("°C")
        }
        "humidity" | "soil_moisture_pct" | "leaf_wetness_pct" | "battery_pct" => Some("%"),
        "pressure" | "pressure_absolute" => Some("hPa"),
        "vpd" => Some("kPa"),
        "wind_speed" | "wind_speed_avg10m" | "wind_gust" | "wind_gust_daily_max" => Some("m/s"),
        "wind_direction" | "wind_direction_avg10m" => Some("°"),
        "rain_rate_mm_hr" => Some("mm/h"),
        n if n.starts_with("rain_") && n.ends_with("_mm") => Some("mm"),
        "air_gap_mm" | "depth_mm" => Some("mm"),
        "distance_km" => Some("km"),
        "solar_radiation" => Some("W/m²"),
        "ec" => Some("µS/cm"),
        "co2" | "co2_24h" | "co2_indoor" | "co2_indoor_24h" => Some("ppm"),
        n if n.starts_with("pm") && !n.ends_with("_aqi") => Some("µg/m³"),
        "voltage" => Some("V"),
        "customserver.interval" => Some("s"),
        _ => None,
    }
}

/// A name and the channel number on its end, if it has one:
/// `temperature_2` and `temperature2` → (`temperature`, `Some("2")`).
///
/// Every channel sensor is its own device today, so its readings carry the
/// plain name. This keeps a numbered name, should one ever be reported,
/// labelled and united like its base reading rather than mechanically.
fn base_name(name: &str) -> (&str, Option<&str>) {
    let digits = name.len() - name.chars().rev().take_while(char::is_ascii_digit).count();
    if digits == name.len() || digits == 0 {
        return (name, None);
    }
    let base = name[..digits].trim_end_matches('_');
    let n = &name[digits..];
    // `pm25`, `pm10`, `pm1`, `pm4`, `co2`: the digits are the name.
    if label_of(name).is_some() || label_of(base).is_none() {
        return (name, None);
    }
    (base, Some(n))
}

/// The label for every attribute this plugin reports, by its exact name.
fn label_of(name: &str) -> Option<&'static str> {
    Some(match name {
        // The weather station.
        "temperature" => "Temperature",
        "dewpoint" => "Dew point",
        "windchill" => "Wind chill",
        "feels_like" => "Feels like",
        "heat_index" => "Heat index",
        "wbgt" => "Wet bulb globe temperature",
        "humidity" => "Humidity",
        "pressure" => "Pressure",
        "pressure_absolute" => "Absolute pressure",
        "vpd" => "Vapor pressure deficit",
        "wind_direction" => "Wind direction",
        "wind_direction_avg10m" => "Wind direction, 10 min average",
        "wind_speed" => "Wind speed",
        "wind_speed_avg10m" => "Wind speed, 10 min average",
        "wind_gust" => "Wind gust",
        "wind_gust_daily_max" => "Max gust today",
        "solar_radiation" => "Solar radiation",
        "uvi" => "UV index",
        "datetime" => "Report time",
        // Rain.
        "raining" => "Raining",
        "rain_rate_mm_hr" => "Rain rate",
        "rain_event_mm" => "Rain this event",
        "rain_hourly_mm" => "Rain this hour",
        "rain_24h_mm" => "Rain, last 24 hours",
        "rain_daily_mm" => "Rain today",
        "rain_weekly_mm" => "Rain this week",
        "rain_monthly_mm" => "Rain this month",
        "rain_yearly_mm" => "Rain this year",
        "rain_total_mm" => "Total rain",
        // Soil, leaf, leak and water level.
        "soil_moisture_pct" => "Soil moisture",
        "soil_moisture_raw" => "Soil moisture (raw)",
        "ec" => "Soil conductivity",
        "leaf_wetness_pct" => "Leaf wetness",
        "water_detected" => "Leak",
        "air_gap_mm" => "Air gap",
        "depth_mm" => "Water depth",
        // Air quality.
        "pm1" => "PM1.0",
        "pm1_aqi" => "PM1.0 AQI",
        "pm25" => "PM2.5",
        "pm25_24h" => "PM2.5, 24 hour average",
        "pm25_aqi" => "PM2.5 AQI",
        "pm25_24h_aqi" => "PM2.5 AQI, 24 hour",
        "pm4" => "PM4.0",
        "pm10" => "PM10",
        "pm10_24h" => "PM10, 24 hour average",
        "pm10_aqi" => "PM10 AQI",
        "pm10_24h_aqi" => "PM10 AQI, 24 hour",
        "co2" => "CO2",
        "co2_24h" => "CO2, 24 hour average",
        "co2_indoor" => "Indoor CO2",
        "co2_indoor_24h" => "Indoor CO2, 24 hour average",
        // Lightning.
        "distance_km" => "Lightning distance",
        "strike_count" => "Lightning strikes",
        "last_strike" => "Last strike",
        // The sensor's battery.
        "battery_pct" => "Battery",
        "battery_low" => "Battery low",
        "battery_kind" => "Battery type",
        "battery_raw" => "Battery (raw)",
        "voltage" => "Battery voltage",
        // The gateway itself.
        "ip" => "IP address",
        "mac" => "MAC address",
        "model" => "Model",
        "firmware" => "Firmware",
        "platform" => "Platform",
        "update_available" => "Firmware update",
        "gateway_time" => "Gateway time",
        "timezone" => "Time zone",
        "ntp_server" => "Time server",
        "units.temperature" => "Temperature unit",
        "units.pressure" => "Pressure unit",
        "units.wind" => "Wind unit",
        "units.rain" => "Rain unit",
        "units.light" => "Light unit",
        "network.ssid" => "Wi-Fi network",
        "network.ip" => "Wi-Fi IP address",
        "network.netmask" => "Wi-Fi netmask",
        "network.gateway" => "Wi-Fi gateway",
        "network.dns" => "Wi-Fi DNS server",
        "customserver.dialect" => "Upload settings API",
        "customserver.protocol" => "Upload protocol",
        "customserver.enable" => "Upload enabled",
        "customserver.server" => "Upload server",
        "customserver.path" => "Upload path",
        "customserver.port" => "Upload port",
        "customserver.interval" => "Upload interval",
        _ => return None,
    })
}

/// The display name for an attribute: its own label, a channel number after
/// its base reading's (`temperature_2` → `Temperature 2`), or, for a name this
/// plugin has never reported, the name in words.
fn label_for(name: &str) -> String {
    if let Some(l) = label_of(name) {
        return l.to_string();
    }
    match base_name(name) {
        (base, Some(n)) => format!("{} {n}", label_of(base).unwrap_or(base)),
        _ => humanize(name),
    }
}

/// The two state names for a boolean attribute.
///
/// A boolean attribute is two events, not one: a client given only one name
/// offers one row, and the other direction needs a Not gate wrapped round the
/// trigger. The fallback is mechanical but still two rows.
fn states_for(name: &str) -> BoolStates {
    match name {
        "battery_low" => BoolStates {
            when_true: StateLabel::verbed("low", "goes low"),
            when_false: StateLabel::verbed("healthy", "recovers"),
        },
        "update_available" => BoolStates {
            when_true: StateLabel::verbed("out of date", "has an update"),
            when_false: StateLabel::verbed("up to date", "becomes up to date"),
        },
        "raining" => BoolStates {
            when_true: StateLabel::verbed("raining", "starts raining"),
            when_false: StateLabel::verbed("dry", "stops raining"),
        },
        "water_detected" => BoolStates {
            when_true: StateLabel::verbed("wet", "detects water"),
            when_false: StateLabel::verbed("dry", "dries out"),
        },
        "customserver.enable" => BoolStates {
            when_true: StateLabel::verbed("uploading", "starts uploading"),
            when_false: StateLabel::verbed("not uploading", "stops uploading"),
        },
        other => {
            let word = other.replace(['_', '.'], " ");
            BoolStates {
                when_true: StateLabel::verbed(word.clone(), format!("becomes {word}")),
                when_false: StateLabel::verbed(
                    format!("not {word}"),
                    format!("stops being {word}"),
                ),
            }
        }
    }
}

/// `some_new_reading` → `Some new reading`.
fn humanize(name: &str) -> String {
    let spaced = name.replace(['_', '.'], " ");
    let mut c = spaced.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => spaced,
    }
}

/// Describe one attribute from its reported value.
///
/// Everything is read-only — see the module note.
pub fn describe(name: &str, value: &Value) -> AttributeSchema {
    let kind = match value {
        Value::Bool(_) => AttributeKind::Bool,
        Value::Number(n) if n.is_i64() || n.is_u64() => AttributeKind::Integer,
        Value::Number(_) => AttributeKind::Float,
        Value::String(_) => AttributeKind::String,
        // An array or object reading has no dedicated control; say so rather
        // than pretending it is a number.
        _ => AttributeKind::Json,
    };

    let mut a = AttributeSchema::read_only(kind).labelled(label_for(name));
    a.unit = unit_for(name).map(|u| u.to_string());
    // A weather station reports temperature and, beside it, the battery in the
    // sensor reporting it. Only one of those is what anyone opened the page
    // for. The gateway's ip, mac, model and firmware are the same story.
    a.category = AttributeCategory::for_name(name)
        .or_else(|| {
            // Gateway settings rather than readings: `units.*` is display
            // preference, `customserver.*` is where it uploads to.
            (name.starts_with("units.") || name.starts_with("customserver."))
                .then_some(AttributeCategory::Config)
        })
        .or_else(|| {
            // On an Ecowitt sensor the voltage is always its battery's, and
            // the other two say how the battery was read.
            matches!(name, "voltage" | "battery_raw" | "battery_kind")
                .then_some(AttributeCategory::Diagnostic)
        });
    if matches!(value, Value::Bool(_)) {
        a.states = Some(states_for(name));
    }
    a
}

/// The schema for a device that reported [`state`].
pub fn schema_for_state(state: &Map<String, Value>) -> DeviceSchema {
    let mut attributes: HashMap<String, AttributeSchema> = HashMap::new();
    for (name, value) in state {
        attributes.insert(name.clone(), describe(name, value));
    }
    DeviceSchema {
        attributes,
        ..Default::default()
    }
}

/// Publish the retained schema for a device, derived from what it just
/// reported.
///
/// Republished as the reported set grows — a gateway that gains a sensor gains
/// attributes, and the retained schema should say so.
pub async fn publish(
    publisher: &DevicePublisher,
    device_id: &str,
    state: &Value,
) -> anyhow::Result<()> {
    let Some(obj) = state.as_object() else {
        return Ok(());
    };
    if obj.is_empty() {
        return Ok(());
    }
    let value = serde_json::to_value(schema_for_state(obj))?;
    publisher
        .register_device_schema_json(device_id, &value)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(v: Value) -> DeviceSchema {
        schema_for_state(v.as_object().unwrap())
    }

    /// The gateway describes itself too. It publishes ip, mac, model,
    /// firmware and timezone and never had a schema, so those arrived as
    /// attributes no client could label or categorise.
    #[test]
    fn a_gateway_describes_its_own_identity() {
        let s = schema(json!({
            "ip": "192.168.1.50",
            "model": "GW1100B",
            "firmware": "Version: GW1100B_V2.4.5",
            "update_available": false,
        }));
        assert!(matches!(s.attributes["ip"].kind, AttributeKind::String));
        assert!(matches!(s.attributes["model"].kind, AttributeKind::String));
        assert!(!s.attributes["model"].writable, "a gateway is read-only");
        let flag = &s.attributes["update_available"];
        assert!(matches!(flag.kind, AttributeKind::Bool));
        assert!(flag.states.is_some(), "a boolean names both of its states");
    }

    /// A weather station reports temperature and, beside it, the battery in
    /// the sensor reporting it. The gateway's settings are a third thing again.
    #[test]
    fn readings_batteries_and_settings_are_told_apart() {
        let s = schema(json!({
            "temperature": 21.5,
            "battery_pct": 90,
            "voltage": 1.4,
            "model": "GW1100B",
            "units.temperature": "1",
            "customserver.host": "example.test",
        }));
        assert_eq!(s.attributes["temperature"].category, None);
        for name in ["battery_pct", "voltage", "model"] {
            assert_eq!(
                s.attributes[name].category,
                Some(AttributeCategory::Diagnostic),
                "{name}"
            );
        }
        for name in ["units.temperature", "customserver.host"] {
            assert_eq!(
                s.attributes[name].category,
                Some(AttributeCategory::Config),
                "{name}"
            );
        }
    }

    /// Every boolean names both of its states — including one nobody has named.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let s = schema(json!({
            "battery_low": true,
            "update_available": false,
            "some_future_flag": true,
            "temperature": 21.5,
        }));
        for (name, attr) in &s.attributes {
            if !matches!(attr.kind, AttributeKind::Bool) {
                continue;
            }
            let st = attr
                .states
                .as_ref()
                .unwrap_or_else(|| panic!("{name} is a bool with no state names"));
            assert!(!st.when_true.label.is_empty(), "{name}");
            assert_ne!(st.when_true.label, st.when_false.label, "{name}");
        }
        // The mechanical fallback still yields two usable rows.
        let f = s.attributes["some_future_flag"].states.as_ref().unwrap();
        assert_eq!(f.get(true).transition(), "becomes some future flag");
        assert_eq!(f.get(false).transition(), "stops being some future flag");
    }

    /// The declared set is exactly what was reported — no more, no less.
    ///
    /// This is the property the generated approach buys: a hand-written list
    /// would declare piezo rain keys on a gateway that has no WS90.
    #[test]
    fn the_schema_declares_exactly_what_was_reported() {
        let reported = json!({
            "temperature": 21.5,
            "humidity": 44,
            "rain_rate_mm_hr": 0.0,
        });
        let s = schema(reported.clone());
        let declared: std::collections::HashSet<&str> =
            s.attributes.keys().map(|k| k.as_str()).collect();
        let expected: std::collections::HashSet<&str> = reported
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(declared, expected);
    }

    /// Ecowitt gateways are receivers; a writable attribute would render a
    /// control that does nothing at all.
    #[test]
    fn no_attribute_is_ever_writable() {
        let s = schema(json!({
            "temperature": 21.5, "battery_low": false, "model": "GW2000",
        }));
        for (name, attr) in &s.attributes {
            assert!(!attr.writable, "{name} claims to be writable");
        }
    }

    #[test]
    fn kinds_and_units_come_from_the_reading() {
        let s = schema(json!({
            "humidity": 44,
            "temperature": 21.5,
            "model": "GW2000",
            "pm25": 8.1,
        }));
        assert!(matches!(
            s.attributes["humidity"].kind,
            AttributeKind::Integer
        ));
        assert_eq!(s.attributes["humidity"].unit.as_deref(), Some("%"));
        assert!(matches!(
            s.attributes["temperature"].kind,
            AttributeKind::Float
        ));
        // Converted at the edge, so the unit is always the same one.
        assert_eq!(s.attributes["temperature"].unit.as_deref(), Some("°C"));
        assert!(matches!(s.attributes["model"].kind, AttributeKind::String));
        assert_eq!(s.attributes["pm25"].unit.as_deref(), Some("µg/m³"));
    }

    /// Every reading the parsers publish is declared in homeCore's unit — the
    /// shared names (`temperature`, `humidity`, `pressure`, `wind_speed`,
    /// `battery_pct`, `voltage`) in the shared table's.
    #[test]
    fn readings_declare_homecores_units() {
        for (name, unit) in [
            ("temperature", "°C"),
            ("dewpoint", "°C"),
            ("humidity", "%"),
            ("pressure", "hPa"),
            ("pressure_absolute", "hPa"),
            ("wind_speed", "m/s"),
            ("wind_gust", "m/s"),
            ("rain_daily_mm", "mm"),
            ("rain_rate_mm_hr", "mm/h"),
            ("solar_radiation", "W/m²"),
            ("soil_moisture_pct", "%"),
            ("leaf_wetness_pct", "%"),
            ("battery_pct", "%"),
            ("voltage", "V"),
            ("distance_km", "km"),
        ] {
            assert_eq!(unit_for(name), Some(unit), "{name}");
        }
    }

    #[test]
    fn labels_read_as_words() {
        let s = schema(json!({
            "wind_speed_avg10m": 3.2,
            "uvi": 3,
            "solar_radiation": 512.3,
            "rain_rate_mm_hr": 0.0,
            "pm25": 8.1,
            "some_future_reading": 1,
        }));
        let label = |n: &str| s.attributes[n].display_name.clone().unwrap();
        assert_eq!(label("wind_speed_avg10m"), "Wind speed, 10 min average");
        assert_eq!(label("uvi"), "UV index");
        assert_eq!(label("solar_radiation"), "Solar radiation");
        assert_eq!(label("rain_rate_mm_hr"), "Rain rate");
        assert_eq!(label("pm25"), "PM2.5");
        // A name this plugin has never reported is still words.
        assert_eq!(label("some_future_reading"), "Some future reading");
    }

    /// The UV index is a scale, not a quantity: it declares no unit.
    #[test]
    fn unitless_readings_declare_no_unit() {
        for name in [
            "uvi",
            "pm25_aqi",
            "pm25_24h_aqi",
            "strike_count",
            "soil_moisture_raw",
            "battery_raw",
        ] {
            assert_eq!(unit_for(name), None, "{name}");
        }
    }

    /// A numbered reading is labelled, and united, like its base reading.
    #[test]
    fn a_numbered_reading_is_named_for_its_channel() {
        assert_eq!(label_for("temperature_2"), "Temperature 2");
        assert_eq!(label_for("temperature2"), "Temperature 2");
        assert_eq!(unit_for("temperature_2"), Some("°C"));
        assert_eq!(label_for("humidity3"), "Humidity 3");
        assert_eq!(unit_for("humidity3"), Some("%"));
        // The digits in these are the name, not a channel.
        assert_eq!(label_for("pm10"), "PM10");
        assert_eq!(label_for("co2"), "CO2");
        assert_eq!(unit_for("co2"), Some("ppm"));
    }

    /// The gateway's attributes, as `gateway_device` publishes them.
    const GATEWAY_ATTRS: &[&str] = &[
        "ip",
        "mac",
        "gateway_time",
        "timezone",
        "ntp_server",
        "model",
        "firmware",
        "platform",
        "update_available",
        "units.temperature",
        "units.pressure",
        "units.wind",
        "units.rain",
        "units.light",
        "network.ssid",
        "network.ip",
        "network.netmask",
        "network.gateway",
        "network.dns",
        "customserver.dialect",
        "customserver.protocol",
        "customserver.enable",
        "customserver.server",
        "customserver.path",
        "customserver.port",
        "customserver.interval",
    ];

    /// An upload carrying every field the form parser reads, on every scale
    /// a battery can be reported in.
    fn every_upload_field() -> HashMap<String, String> {
        let mut f: Vec<(String, &str)> = [
            "tempf",
            "humidity",
            "dewptf",
            "windchillf",
            "feelslikef",
            "heatindexf",
            "vpd",
            "winddir",
            "winddir_avg10m",
            "windspeedmph",
            "windspdmph_avg10m",
            "windgustmph",
            "maxdailygust",
            "solarradiation",
            "uv",
            "baromrelin",
            "baromabsin",
            "tempinf",
            "humidityin",
            "rainratein",
            "eventrainin",
            "hourlyrainin",
            "last24hrainin",
            "dailyrainin",
            "weeklyrainin",
            "monthlyrainin",
            "yearlyrainin",
            "totalrainin",
            "rrain_piezo",
            "erain_piezo",
            "hrain_piezo",
            "last24hrain_piezo",
            "drain_piezo",
            "wrain_piezo",
            "mrain_piezo",
            "yrain_piezo",
            "srain_piezo",
            "temp1f",
            "humidity1",
            "tf_ch1",
            "soilmoisture1",
            "soilad1",
            "soilec_ch1",
            "soilec_hum1",
            "soilec_temp1",
            "leafwetness_ch1",
            "leak_ch1",
            "lds_air_ch1",
            "lds_depth_ch1",
            "pm25_ch1",
            "pm25_avg_24h_ch1",
            "lightning",
            "lightning_num",
            "co2",
            "co2_24h",
            "co2in",
            "co2in_24h",
            "pm25_co2",
            "pm25_24h_co2",
            "pm10_co2",
            "pm10_24h_co2",
            "pm1_co2",
            "pm4_co2",
            "tf_co2",
            "humi_co2",
            // Batteries: binary, voltage, level.
            "wh65batt",
            "wh80batt",
            "wh25batt",
            "wh40batt",
            "wh90batt",
            "batt1",
            "tf_batt1",
            "soilbatt1",
            "leaf_batt1",
            "leakbatt1",
            "ldsbatt1",
            "pm25batt1",
            "wh57batt",
            "co2_batt",
        ]
        .iter()
        .map(|k| (k.to_string(), "1"))
        .collect();
        f.push(("dateutc".into(), "2026-11-14 10:00:00"));
        f.push(("lightning_time".into(), "1700000000"));
        f.into_iter().map(|(k, v)| (k, v.to_string())).collect()
    }

    /// A poll carrying every section and field the poll parser reads.
    fn every_poll_field() -> Value {
        let common: Vec<Value> = [
            "0x01", "0x02", "0x03", "0x04", "0x05", "0x06", "0x07", "0x08", "0x09", "0x0A", "0x0B",
            "0x0C", "0x15", "0x17", "0x19", "3", "5", "0x6D", "0x0D", "0x0E", "0x7C",
        ]
        .iter()
        .map(|id| json!({ "id": id, "val": "1", "unit": "C", "battery": "1" }))
        .collect();
        let rain: Vec<Value> = [
            "0x0D",
            "0x0E",
            "0x0F",
            "0x10",
            "0x11",
            "0x12",
            "0x13",
            "0x14",
            "0x7C",
            "srain_piezo",
        ]
        .iter()
        .map(|id| json!({ "id": id, "val": "1 mm", "battery": "1" }))
        .collect();
        let ch = |extra: Value| {
            let mut v = json!({ "channel": "1", "name": "", "battery": "1", "unit": "C",
                                "temp": "1", "humidity": "1%", "voltage": "1.4" });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!([v])
        };
        json!({
            "common_list": common,
            "wh25": [{ "intemp": "1", "unit": "C", "inhumi": "1%", "abs": "1000 hPa", "rel": "1000 hPa" }],
            "rain": rain,
            "piezoRain": rain,
            "lightning": [{ "distance": "1 km", "count": "1", "timestamp": "1700000000", "battery": "1" }],
            "co2": [{ "temp": "1", "unit": "C", "humidity": "1", "PM25": "1", "PM25_RealAQI": "1",
                      "PM25_24HAQI": "1", "PM10": "1", "PM10_RealAQI": "1", "PM10_24HAQI": "1",
                      "PM1": "1", "PM1_RealAQI": "1", "PM4": "1", "CO2": "1", "CO2_24H": "1",
                      "battery": "1" }],
            "ch_aisle": ch(json!({})),
            "ch_temp": ch(json!({})),
            "ch_soil": ch(json!({})),
            "ch_leaf": ch(json!({})),
            "ch_leak": ch(json!({ "status": "Normal" })),
            "ch_pm25": ch(json!({ "PM25": "1", "PM25_RealAQI": "1", "PM25_24HAQI": "1" })),
            "ch_lds": ch(json!({ "air": "1 mm", "depth": "1 mm" })),
            "ch_ec": ch(json!({ "ec": "1" })),
        })
    }

    /// Every attribute either path reports, and every one the gateway does.
    fn every_emitted_attribute() -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        let updates = crate::form_parser::parse_form_data(&every_upload_field(), "eco")
            .into_iter()
            .chain(crate::parser::parse_livedata(&every_poll_field(), "eco"));
        for u in updates {
            names.extend(u.state.as_object().unwrap().keys().cloned());
        }
        names.extend(GATEWAY_ATTRS.iter().map(|s| s.to_string()));
        // Every battery scale, including the one no taxonomy entry names.
        let mut b = Map::new();
        crate::battery::classify(&mut b, 1.0, None);
        names.extend(b.keys().cloned());
        names
    }

    /// The app found readings labelled from their key ("Uvi"). Every
    /// attribute this plugin emits has a label of its own, not the mechanical
    /// fallback.
    #[test]
    fn every_emitted_attribute_has_a_display_name() {
        let names = every_emitted_attribute();
        // The fixtures really do reach the readings that were unlabelled.
        for must in [
            "uvi",
            "solar_radiation",
            "rain_rate_mm_hr",
            "battery_raw",
            "ec",
        ] {
            assert!(names.contains(must), "fixture never emits {must}");
        }
        let unlabelled: Vec<&String> = names.iter().filter(|n| label_of(n).is_none()).collect();
        assert!(unlabelled.is_empty(), "no display name for {unlabelled:?}");
        for n in &names {
            let label = label_for(n);
            assert!(!label.contains(['_', '—', '–']), "{n} → {label:?}");
        }
    }

    /// Every emitted reading's unit is the one its converted value is in,
    /// and every temperature is °C.
    #[test]
    fn every_emitted_number_declares_its_unit() {
        let unitless = [
            "uvi",
            "strike_count",
            "soil_moisture_raw",
            "battery_raw",
            "pm25_aqi",
            "pm25_24h_aqi",
            "pm10_aqi",
            "pm10_24h_aqi",
            "pm1_aqi",
            "customserver.port",
        ];
        for n in every_emitted_attribute() {
            let numeric = !matches!(
                n.as_str(),
                "datetime"
                    | "last_strike"
                    | "battery_kind"
                    | "raining"
                    | "water_detected"
                    | "battery_low"
                    | "update_available"
                    | "customserver.enable"
            ) && !GATEWAY_ATTRS.contains(&n.as_str());
            if numeric && !unitless.contains(&n.as_str()) {
                assert!(unit_for(&n).is_some(), "{n} declares no unit");
            }
            if unitless.contains(&n.as_str()) {
                assert_eq!(unit_for(&n), None, "{n}");
            }
        }
        let poll = crate::parser::parse_livedata(&every_poll_field(), "eco");
        let upload = crate::form_parser::parse_form_data(&every_upload_field(), "eco");
        for u in poll.iter().chain(upload.iter()) {
            plugin_sdk_rs::conformance::check_state_temperatures(
                &schema(u.state.clone()),
                &u.state,
            )
            .assert_ok();
        }
    }
}
