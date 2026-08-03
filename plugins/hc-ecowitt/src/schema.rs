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
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use serde_json::{Map, Value};
use std::collections::HashMap;

/// The unit for an attribute, where it is unambiguous.
///
/// Deliberately sparse. Temperature, rain and distance are reported in units
/// the gateway chooses and publishes *as their own attributes*
/// (`temperature_unit`, `distance_unit`), so hard-coding one here would
/// contradict the device half the time.
fn unit_for(name: &str) -> Option<&'static str> {
    match name {
        "humidity" => Some("%"),
        "co2" | "co2_24h" | "co2_indoor" | "co2_indoor_24h" => Some("ppm"),
        n if n.starts_with("pm") && !n.ends_with("_aqi") => Some("µg/m³"),
        "light" => Some("lux"),
        "uvi" => Some("index"),
        "voltage" => Some("V"),
        "battery" => Some("%"),
        "wind_direction" | "wind_direction_avg10m" => Some("°"),
        _ => None,
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
        other => {
            let word = other.replace('_', " ");
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

/// `wind_speed_avg10m` → `Wind speed avg10m`.
fn humanise(name: &str) -> String {
    let spaced = name.replace('_', " ");
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

    let mut a = AttributeSchema::read_only(kind).labelled(humanise(name));
    a.unit = unit_for(name).map(|u| u.to_string());
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
            "rain_rate_piezo": 0.0,
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
            "temperature_unit": "°F",
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
        // The gateway reports its own temperature unit, so we must not assert one.
        assert_eq!(s.attributes["temperature"].unit, None);
        assert!(matches!(s.attributes["model"].kind, AttributeKind::String));
        assert_eq!(s.attributes["pm25"].unit.as_deref(), Some("µg/m³"));
    }

    #[test]
    fn labels_read_as_words() {
        let s = schema(json!({ "wind_speed_avg10m": 3.2 }));
        assert_eq!(
            s.attributes["wind_speed_avg10m"].display_name.as_deref(),
            Some("Wind speed avg10m")
        );
    }
}
