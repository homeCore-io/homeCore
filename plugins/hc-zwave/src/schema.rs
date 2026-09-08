//! What a Z-Wave node reports, described from the node's own values.
//!
//! This plugin published no [`DeviceSchema`], so every client inferred each
//! attribute from its observed value and took the meaning from a lexicon keyed
//! on the attribute name.
//!
//! **Generated from state, not hand-listed.** A Z-Wave node's attribute set is
//! whatever its command classes expose: the alias table maps forty-five
//! canonical names, and anything unmapped still surfaces under a synthetic
//! `cc{n}_{property}` name so nothing silently drops. Which of those a given
//! node has depends entirely on the hardware, so a static per-type schema would
//! be wrong for almost every node. Describing what was actually reported cannot
//! drift by construction.
//!
//! **Writability comes from the translator, not from a guess.**
//! [`Translator::write_target`] is the only thing that turns an attribute write
//! into a Z-Wave `setValue`, so an attribute it has no reverse entry for cannot
//! be written — and declaring it writable would render a control that silently
//! does nothing. The schema asks it directly.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::translator::Translator;

/// The unit for a canonical attribute, where the alias table fixes one.
fn unit_for(name: &str) -> Option<&'static str> {
    match name {
        "battery" | "humidity" | "brightness" | "position" => Some("%"),
        "temperature" | "target_temp" => Some("°"),
        "power_w" | "power_w_exported" => Some("W"),
        "energy_kwh" | "energy_kwh_exported" => Some("kWh"),
        "voltage" => Some("V"),
        "current_a" => Some("A"),
        "illuminance" => Some("lux"),
        "co2_ppm" => Some("ppm"),
        "pressure" => Some("kPa"),
        "reactive_power_kvar" => Some("kVAr"),
        "reactive_energy_kvarh" => Some("kVArh"),
        "apparent_energy_kvah" => Some("kVAh"),
        "uv_index" => Some("index"),
        "lock_timeout_secs" | "lock_auto_relock_secs" => Some("s"),
        _ => None,
    }
}

/// Both state names for a boolean attribute.
///
/// A boolean attribute is two events, not one: a client given only one name
/// offers one row, and the other direction needs a Not gate wrapped round the
/// trigger. Every name below comes from the alias table.
fn states_for(name: &str) -> BoolStates {
    let pair = |t: (&str, &str), f: (&str, &str)| BoolStates {
        when_true: StateLabel::verbed(t.0, t.1),
        when_false: StateLabel::verbed(f.0, f.1),
    };
    match name {
        "on" => pair(("on", "turns on"), ("off", "turns off")),
        // Named for what it measures, not for a convention: the alias table
        // maps CC 48 "Door/Window" straight through, and TRUE means OPEN.
        "contact_open" => pair(("open", "opens"), ("closed", "closes")),
        "locked" => pair(("locked", "locks"), ("unlocked", "unlocks")),
        "motion" => pair(
            ("detecting motion", "detects motion"),
            ("clear", "stops detecting motion"),
        ),
        "water_detected" => pair(("wet", "detects water"), ("dry", "dries out")),
        "smoke" => pair(("detecting smoke", "detects smoke"), ("clear", "clears")),
        "co" | "co2_alarm" => pair(("alarming", "alarms"), ("clear", "clears")),
        "heat_alarm" => pair(("alarming", "alarms"), ("clear", "clears")),
        "freeze" => pair(("freezing", "starts freezing"), ("clear", "clears")),
        "glass_break" => pair(("triggered", "triggers"), ("clear", "clears")),
        "tamper" => pair(("tampered", "is tampered"), ("clear", "clears")),
        "battery_low" => pair(("low", "goes low"), ("healthy", "recovers")),
        "sensor_active" => pair(("active", "activates"), ("idle", "goes idle")),
        "tilt" => pair(("tilted", "tilts"), ("level", "levels out")),
        other => {
            // Mechanical, and still two rows — an unmapped boolean is far more
            // usable this way than behind a Not gate.
            let word = other.replace('_', " ");
            pair(
                (&word.clone(), &format!("becomes {word}")),
                (&format!("not {word}"), &format!("stops being {word}")),
            )
        }
    }
}

fn humanise(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let mut c = spaced.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => spaced,
    }
}

/// Whether this name is the translator's synthetic form for an unmapped
/// command-class value — `cc113_alarmlevel`, `cc112_e2_3`.
///
/// Deliberately narrow: `cc`, digits, then `_`. Nothing in the alias table
/// starts this way, and a real attribute named `ccx_level` is not one of these.
fn is_synthetic_cc_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("cc") else {
        return false;
    };
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && rest[digits..].starts_with('_')
}

/// Describe one attribute from its reported value and the translator.
pub fn describe(name: &str, value: &Value, translator: &Translator) -> AttributeSchema {
    let kind = match value {
        Value::Bool(_) => AttributeKind::Bool,
        Value::Number(n) if n.is_i64() || n.is_u64() => AttributeKind::Integer,
        Value::Number(_) => AttributeKind::Float,
        Value::String(_) => AttributeKind::String,
        _ => AttributeKind::Json,
    };

    // The translator's reverse map is the authority on whether a write can
    // reach the device at all.
    let writable = translator.write_target(name).is_some();

    let mut a = if writable {
        AttributeSchema::new(kind)
    } else {
        AttributeSchema::read_only(kind)
    };
    a = a.labelled(humanise(name));
    a.unit = unit_for(name).map(|u| u.to_string());
    // A node reports its battery and its name alongside whether it is locked.
    // Saying which of those is the point of the device is the difference
    // between a client leading with the lock and leading with the battery.
    a.category = AttributeCategory::for_name(name).or_else(|| {
        // A `cc{n}_{property}` name is what the translator emits for a value
        // the alias table does **not** map — so it is raw by definition:
        // `cc114_manufacturerid` is identity and `cc112_3` is a configuration
        // parameter nobody named. Surfacing them is right; ranking them beside
        // the reading a node exists for is not. A live switch ranked
        // `[on, cc112_19, cc112_3, cc114_manufacturerid]`.
        is_synthetic_cc_name(name).then_some(AttributeCategory::Diagnostic)
    });
    if matches!(value, Value::Bool(_)) {
        a.states = Some(states_for(name));
    }
    a
}

/// The schema for a node that reported [`state`].
pub fn schema_for_state(state: &Map<String, Value>, translator: &Translator) -> DeviceSchema {
    let mut attributes: HashMap<String, AttributeSchema> = HashMap::new();
    for (name, value) in state {
        attributes.insert(name.clone(), describe(name, value, translator));
    }
    DeviceSchema {
        attributes,
        ..Default::default()
    }
}

/// Build the retained schema payload for a node's state, or `None` when there
/// is nothing to say.
///
/// An empty schema is a claim ("this device has no attributes"); silence is
/// not, and a freshly-included node whose interview has not finished reports
/// nothing at all.
pub fn schema_json(state: &Value, translator: &Translator) -> Option<Value> {
    let obj = state.as_object()?;
    if obj.is_empty() {
        return None;
    }
    serde_json::to_value(schema_for_state(obj, translator)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A lock reports whether it is locked; it also reports battery, and its
    /// own name. Only the first is the point of the device.
    #[test]
    fn housekeeping_is_declared_and_the_reading_is_left_primary() {
        let s = schema(json!({
            "locked": true,
            "battery": 80,
            "name": "Front Door",
        }));
        assert_eq!(s.attributes["locked"].category, None);
        for name in ["battery", "name"] {
            assert_eq!(
                s.attributes[name].category,
                Some(AttributeCategory::Diagnostic),
                "{name}"
            );
        }
    }

    /// A live switch ranked `[on, cc112_19, cc112_3, cc114_manufacturerid]`.
    /// The `cc` names are values the alias table did not map — raw by
    /// definition, and never what a node is for.
    #[test]
    fn unmapped_command_class_values_are_not_the_reading() {
        let s = schema(json!({
            "on": true,
            "cc112_19": 0,
            "cc114_manufacturerid": 271,
            "cc113_e2_alarmlevel": 0,
        }));
        assert_eq!(s.attributes["on"].category, None);
        for name in ["cc112_19", "cc114_manufacturerid", "cc113_e2_alarmlevel"] {
            assert_eq!(
                s.attributes[name].category,
                Some(AttributeCategory::Diagnostic),
                "{name}"
            );
        }
    }

    /// Narrow on purpose: an ordinary name that merely starts with "cc" is a
    /// reading like any other.
    #[test]
    fn a_name_that_only_looks_synthetic_is_left_alone() {
        assert!(is_synthetic_cc_name("cc112_3"));
        assert!(!is_synthetic_cc_name("ccx_level"));
        assert!(!is_synthetic_cc_name("cc_level"));
        assert!(!is_synthetic_cc_name("current_a"));
    }

    fn schema(v: Value) -> DeviceSchema {
        let t = Translator::new();
        schema_for_state(v.as_object().unwrap(), &t)
    }

    /// Every boolean names both of its states, mapped or not.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let s = schema(json!({
            "on": true,
            "contact_open": false,
            "motion": true,
            "water_detected": false,
            "smoke": false,
            "tamper": false,
            "battery_low": false,
            "cc113_some_future_flag": true,
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
        // An unmapped boolean still gets two usable rows.
        let f = s.attributes["cc113_some_future_flag"]
            .states
            .as_ref()
            .unwrap();
        assert!(f.get(true).transition().starts_with("becomes"));
        assert!(f.get(false).transition().starts_with("stops being"));
    }

    /// `contact_open` is TRUE when the door is OPEN.
    ///
    /// The alias table maps CC 48 "Door/Window" straight through, so this is
    /// the opposite of what a client lexicon keyed on the word "contact"
    /// assumes — a closed contact circuit conventionally means shut.
    #[test]
    fn contact_open_is_declared_the_way_the_alias_table_maps_it() {
        let s = schema(json!({ "contact_open": true }));
        let st = s.attributes["contact_open"].states.as_ref().unwrap();
        assert_eq!(st.get(true).label, "open");
        assert_eq!(st.get(false).label, "closed");
    }

    /// Writability is exactly what the translator can act on.
    ///
    /// Declaring an attribute writable that `write_target` does not map renders
    /// a control whose every use is silently dropped.
    #[test]
    fn writability_matches_the_translators_reverse_map() {
        let t = Translator::new();
        let s = schema(json!({
            "on": true,
            "brightness": 50,
            "motion": true,
            "battery": 90,
            "temperature": 21.5,
        }));
        for (name, attr) in &s.attributes {
            assert_eq!(
                attr.writable,
                t.write_target(name).is_some(),
                "{name} writability disagrees with the translator"
            );
        }
        // And the reverse map really does distinguish them: a sensor reading
        // is not writable, while a switch is.
        assert!(!s.attributes["motion"].writable);
        assert!(!s.attributes["battery"].writable);
    }

    /// The declared set is exactly what the node reported.
    #[test]
    fn the_schema_declares_exactly_what_was_reported() {
        let reported = json!({ "on": true, "power_w": 12.5 });
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

    /// A node mid-interview reports nothing; publishing an empty schema would
    /// claim it has no attributes.
    #[test]
    fn an_empty_state_publishes_no_schema() {
        let t = Translator::new();
        assert!(schema_json(&json!({}), &t).is_none());
        assert!(schema_json(&Value::Null, &t).is_none());
    }
}
