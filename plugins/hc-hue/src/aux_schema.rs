//! What a Hue auxiliary device reports, described from what it reported.
//!
//! Lights and groups have declared themselves since the schema existed; the
//! sensors never did. A Hue motion sensor publishes motion, temperature,
//! illuminance and battery and declared none of it, so every client inferred
//! each attribute from its value and demoted the battery by name — the exact
//! guessing `AttributeCategory` and `BoolStates` exist to stop.
//!
//! **Generated from state, not hand-listed.** `compact_motion_facets` merges
//! several Hue resources — motion, temperature, light_level, device_power —
//! onto one homeCore device, so what a given device reports depends on how the
//! bridge modelled it and how this plugin was configured. Describing what was
//! actually published cannot drift by construction, the same approach
//! hc-ecowitt and hc-zwave take.

use std::collections::HashMap;

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use serde_json::{Map, Value};

/// The unit for an attribute, where Hue fixes one.
fn unit_for(name: &str) -> Option<&'static str> {
    match name {
        "battery_pct" | "brightness_pct" => Some("%"),
        "temperature" | "temperature_c" => Some("°C"),
        "temperature_f" => Some("°F"),
        "illuminance" | "illuminance_lux" | "lux" => Some("lux"),
        "color_temp_mirek" => Some("mirek"),
        "button_repeat_interval_ms" => Some("ms"),
        _ => None,
    }
}

/// Both state names for a boolean, in the device's own words.
fn states_for(name: &str) -> BoolStates {
    let pair = |t: (&str, &str), f: (&str, &str)| BoolStates {
        when_true: StateLabel::verbed(t.0, t.1),
        when_false: StateLabel::verbed(f.0, f.1),
    };
    match name {
        "motion" => pair(("motion", "detects motion"), ("clear", "clears")),
        "tampered" => pair(("tampered", "is tampered with"), ("intact", "reads intact")),
        "enabled" => pair(("enabled", "is enabled"), ("disabled", "is disabled")),
        "on" => pair(("on", "turns on"), ("off", "turns off")),
        "entertainment_active" => pair(
            ("streaming", "starts streaming"),
            ("idle", "stops streaming"),
        ),
        // `*_valid` says whether the reading beside it can be trusted.
        n if n.ends_with("_valid") => pair(("valid", "becomes valid"), ("stale", "goes stale")),
        _ => pair(("yes", "becomes true"), ("no", "becomes false")),
    }
}

/// Turn an attribute name into something a person reads.
fn humanise(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, part) in name.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match part {
            "pct" => out.push('%'),
            "ms" => out.push_str("ms"),
            "id" => out.push_str("ID"),
            _ => {
                let mut c = part.chars();
                if let Some(first) = c.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(c.as_str());
                }
            }
        }
    }
    out
}

/// Describe one attribute from its reported value and the state it arrived in.
///
/// Everything is read-only: a sensor's command path takes `enabled` and
/// motion sensitivity through the accessory command, not an attribute write,
/// and declaring otherwise would render controls that do nothing.
/// The unit a `*_unit` sibling names, if this attribute has one in `state`.
///
/// **The sibling is the truth and the table is a guess.** A Hue motion sensor
/// publishes `temperature` in whichever scale the operator configured, says so
/// in `temperature_unit`, and the schema declared `°C` unconditionally — so a
/// sensor reporting 71.33 °F was declared as 71.33 °C. The unit is the only
/// thing telling a client how to read the number, which makes declaring it
/// wrong worse than not declaring it at all.
fn unit_from_sibling(name: &str, state: Option<&Map<String, Value>>) -> Option<String> {
    let raw = state?.get(&format!("{name}_unit"))?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(match raw.to_ascii_uppercase().as_str() {
        // Hue reports the scale as a bare letter; everything else is already
        // the symbol a person reads.
        "C" => "°C".to_string(),
        "F" => "°F".to_string(),
        "K" => "K".to_string(),
        _ => raw.to_string(),
    })
}

/// [`describe`], with the rest of the published state available so an
/// attribute's own `*_unit` sibling can say what it is measured in.
pub fn describe_in(
    name: &str,
    value: &Value,
    state: Option<&Map<String, Value>>,
) -> AttributeSchema {
    let kind = match value {
        Value::Bool(_) => AttributeKind::Bool,
        Value::Number(n) if n.is_i64() || n.is_u64() => AttributeKind::Integer,
        Value::Number(_) => AttributeKind::Float,
        Value::String(_) => AttributeKind::String,
        _ => AttributeKind::Json,
    };

    let mut a = AttributeSchema::read_only(kind).labelled(humanise(name));
    a.unit = unit_from_sibling(name, state).or_else(|| unit_for(name).map(str::to_string));
    // Battery, firmware and the bridge/resource ids are not what a motion
    // sensor is for. One lexicon, in the crate that defines the field.
    a.category = AttributeCategory::for_name(name).or_else(|| {
        // Hue-specific housekeeping the shared list cannot know about: the
        // `_valid` flag beside a reading, the v1 API's own id, the room a
        // scene belongs to, and the bounds of a range rather than a value
        // within it.
        (name.ends_with("_valid")
            || name.ends_with("_min")
            || name.ends_with("_max")
            || name.starts_with("group_")
            || name == "id_v1"
            || name == "raw"
            || name == "resource_type")
            .then_some(AttributeCategory::Diagnostic)
    });
    if matches!(value, Value::Bool(_)) {
        a.states = Some(states_for(name));
    }
    a
}

/// Fill in whatever a device publishes that its hand-written schema does not
/// declare, described from the value.
///
/// **A hand-written schema covers the controls and stops.** hc-hue's lights
/// declared `on`, `brightness_pct`, `color_temp` and `color_xy` while
/// publishing fifteen more attributes — the bridge and resource ids, the
/// capability flags, the colour-temperature bounds — and its scenes declared
/// `active` while publishing eight. 47 of the 82 devices in the reference
/// house with an undeclared attribute were Hue's, and an undeclared attribute
/// is a value a person can read and nothing can label, rank or hide.
///
/// The hand-written entries win: they carry writability and ranges that
/// cannot be inferred from a value. Everything else is described the way an
/// auxiliary device's is, so this stays correct as Hue adds fields.
pub fn with_published(mut schema: DeviceSchema, state: &Value) -> DeviceSchema {
    let Some(obj) = state.as_object() else {
        return schema;
    };
    for (name, value) in obj {
        schema
            .attributes
            .entry(name.clone())
            .or_insert_with(|| describe_in(name, value, Some(obj)));
    }
    schema
}

/// The schema for an auxiliary device that published [`state`].
pub fn schema_for_state(state: &Map<String, Value>) -> DeviceSchema {
    let mut attributes: HashMap<String, AttributeSchema> = HashMap::new();
    for (name, value) in state {
        attributes.insert(name.clone(), describe_in(name, value, Some(state)));
    }
    DeviceSchema {
        attributes,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(v: Value) -> DeviceSchema {
        schema_for_state(v.as_object().unwrap())
    }

    /// A motion sensor is for motion. Its battery, its temperature validity
    /// flag and the bridge id it came from are not what anyone opened it for.
    #[test]
    fn a_motion_sensor_declares_motion_primary_and_the_rest_housekeeping() {
        let s = schema(json!({
            "motion": true,
            "temperature": 21.5,
            "battery_pct": 90,
            "motion_valid": true,
            "bridge_id": "abc123",
        }));
        assert_eq!(s.attributes["motion"].category, None);
        assert_eq!(s.attributes["temperature"].category, None);
        for name in ["battery_pct", "motion_valid", "bridge_id"] {
            assert_eq!(
                s.attributes[name].category,
                Some(AttributeCategory::Diagnostic),
                "{name}"
            );
        }
    }

    /// **A hand-written schema covers the controls and stops.** A Hue light
    /// declared four attributes and published nineteen; 47 of the 82 devices
    /// in the reference house with an undeclared attribute were Hue's.
    #[test]
    fn what_a_device_publishes_fills_in_what_the_schema_forgot() {
        let mut declared = DeviceSchema::default();
        declared.attributes.insert(
            "brightness_pct".into(),
            AttributeSchema {
                writable: true,
                min: Some(1.0),
                max: Some(100.0),
                ..AttributeSchema::new(AttributeKind::Integer)
            },
        );

        let filled = with_published(
            declared,
            &json!({
                "brightness_pct": 40,
                "bridge_id": "abc",
                "supports_dimming": true,
                "color_temp_min": 2000,
            }),
        );

        // The hand-written entry wins: writability and range cannot be
        // inferred from a value.
        let bri = &filled.attributes["brightness_pct"];
        assert!(bri.writable);
        assert_eq!(bri.min, Some(1.0));

        // The rest arrive described, and demoted.
        for name in ["bridge_id", "supports_dimming", "color_temp_min"] {
            let a = filled
                .attributes
                .get(name)
                .unwrap_or_else(|| panic!("{name} was not filled in"));
            assert!(!a.writable, "{name} claims to be writable");
            assert_eq!(
                a.category,
                Some(AttributeCategory::Diagnostic),
                "{name} ranks as a reading"
            );
        }
    }

    /// A boolean is two events, not one — and "motion, but Not" is a logic
    /// gate standing in for a word the device already has.
    #[test]
    fn motion_names_both_of_its_states() {
        let s = schema(json!({ "motion": false }));
        let states = s.attributes["motion"].states.as_ref().expect("states");
        assert_eq!(states.when_true.label, "motion");
        assert_eq!(states.when_true.verb.as_deref(), Some("detects motion"));
        assert_eq!(states.when_false.label, "clear");
    }

    /// **The sibling is the truth and the table is a guess.** A Hue motion
    /// sensor publishes `temperature` in whichever scale the operator
    /// configured and says so in `temperature_unit`; the schema declared `°C`
    /// unconditionally, so a sensor reporting 71.33 °F was declared as
    /// 71.33 °C. The unit is the only thing telling a client how to read the
    /// number.
    #[test]
    fn the_unit_sibling_outranks_the_table() {
        let f = schema(json!({ "temperature": 71.33, "temperature_unit": "F" }));
        assert_eq!(f.attributes["temperature"].unit.as_deref(), Some("°F"));

        let c = schema(json!({ "temperature": 21.85, "temperature_unit": "C" }));
        assert_eq!(c.attributes["temperature"].unit.as_deref(), Some("°C"));

        // No sibling, so the table still answers.
        let bare = schema(json!({ "temperature": 21.85 }));
        assert_eq!(bare.attributes["temperature"].unit.as_deref(), Some("°C"));
    }

    #[test]
    fn readings_carry_the_units_hue_fixes() {
        let s = schema(json!({ "battery_pct": 80, "illuminance": 120, "temperature": 20.0 }));
        assert_eq!(s.attributes["battery_pct"].unit.as_deref(), Some("%"));
        assert_eq!(s.attributes["illuminance"].unit.as_deref(), Some("lux"));
        assert_eq!(s.attributes["temperature"].unit.as_deref(), Some("°C"));
    }

    /// Nothing here is writable: a sensor's settings go through the accessory
    /// command path, not an attribute write.
    #[test]
    fn nothing_a_sensor_reports_claims_to_be_writable() {
        let s = schema(json!({ "motion": true, "enabled": true, "battery_pct": 50 }));
        for (name, a) in &s.attributes {
            assert!(!a.writable, "{name} claims to be writable");
        }
    }
}
