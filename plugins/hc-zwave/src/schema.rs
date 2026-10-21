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
//! [`Translator::commands`] is the only thing that turns an attribute write
//! into a Z-Wave `setValue`, and whether it can depends on the node (a
//! contact sensor's `open` is read-only, a garage door's is not) — declaring
//! an attribute writable that it cannot route would render a control that
//! silently does nothing. The schema asks it directly.
//!
//! **Shared names are declared as `hc_types::device_vocabulary` defines
//! them** — kind, unit and range — whatever JSON number the value happened to
//! be: `brightness_pct` is a float 0–100 even when it reads `50`.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::translator::Translator;
use crate::types::NodeState;
use plugin_sdk_rs::device_actions::{with_actions, Action, Param, Role};
use std::collections::BTreeMap;

/// The unit for a canonical attribute, where the alias table fixes one.
fn unit_for(name: &str) -> Option<&'static str> {
    match name {
        "battery_pct" | "humidity" | "brightness_pct" | "position" | "tilt_pct"
        | "light_level_pct" => Some("%"),
        "temperature" | "heat_setpoint" | "cool_setpoint" => Some("°C"),
        "power_w" | "power_w_exported" => Some("W"),
        "energy_kwh" | "energy_kwh_exported" => Some("kWh"),
        "voltage" => Some("V"),
        "current_a" => Some("A"),
        "illuminance" => Some("lux"),
        "co2" => Some("ppm"),
        "pressure" => Some("hPa"),
        "absolute_humidity" => Some("g/m³"),
        "color_temp" => Some("K"),
        "reactive_power_kvar" => Some("kVAr"),
        "reactive_energy_kvarh" => Some("kVArh"),
        "apparent_energy_kvah" => Some("kVAh"),
        "uv_index" => Some("index"),
        "lock_timeout_secs" | "lock_auto_relock_secs" => Some("s"),
        _ => None,
    }
}

/// The kind and range a shared name is declared with, where it is not simply
/// what the value looks like.
fn declared_kind(name: &str) -> Option<(AttributeKind, Option<(f64, f64)>)> {
    use AttributeKind::*;
    Some(match name {
        "brightness_pct" | "position" | "tilt_pct" | "battery_pct" | "humidity"
        | "light_level_pct" => (Float, Some((0.0, 100.0))),
        "temperature" | "heat_setpoint" | "cool_setpoint" | "illuminance" | "pressure"
        | "power_w" | "energy_kwh" | "voltage" | "current_a" | "co2" => (Float, None),
        "color_rgb" => (ColorRgb, None),
        "color_temp" => (
            ColorTemp,
            Some((
                f64::from(crate::convert::WARM_WHITE_K),
                f64::from(crate::convert::COLD_WHITE_K),
            )),
        ),
        "hvac_mode" | "hvac_action" => (Enum, None),
        "fan_mode" => (String, None),
        _ => return None,
    })
}

/// `hvac_action`'s values, as the vocabulary lists them.
const HVAC_ACTIONS: &[&str] = &["idle", "heating", "cooling", "fan", "drying", "off"];

/// `hvac_mode`'s values when the node has not said which it supports.
const HVAC_MODES_FALLBACK: &[&str] = &["off", "heat", "cool", "auto"];

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
        // TRUE means OPEN — a door sensor, or a garage door.
        "open" => pair(("open", "opens"), ("closed", "closes")),
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
        "tampered" => pair(("tampered", "is tampered"), ("clear", "clears")),
        "battery_low" => pair(("low", "goes low"), ("healthy", "recovers")),
        "sensor_active" => pair(("active", "activates"), ("idle", "goes idle")),
        "tilt" => pair(("tilted", "tilts"), ("level", "levels out")),
        "jammed" => pair(("jammed", "jams"), ("free", "frees")),
        "child_lock" => pair(
            ("buttons locked", "locks its buttons"),
            ("buttons working", "unlocks its buttons"),
        ),
        "overload" | "overcurrent" => pair(("overloaded", "overloads"), ("normal", "recovers")),
        "intrusion" => pair(("intrusion", "detects an intrusion"), ("clear", "clears")),
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
    let known = match name {
        "power_w" => Some("Power"),
        "power_w_exported" => Some("Power sent back"),
        "energy_kwh" => Some("Energy"),
        "energy_kwh_exported" => Some("Energy sent back"),
        "current_a" => Some("Current"),
        "battery_pct" => Some("Battery"),
        "brightness_pct" => Some("Brightness"),
        _ => None,
    };
    if let Some(label) = known {
        return label.to_string();
    }
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
pub fn describe(
    node_id: u32,
    name: &str,
    value: &Value,
    translator: &Translator,
) -> AttributeSchema {
    let declared = declared_kind(name);
    let kind = match (&declared, value) {
        (Some((kind, _)), _) => *kind,
        (_, Value::Bool(_)) => AttributeKind::Bool,
        (_, Value::Number(n)) if n.is_i64() || n.is_u64() => AttributeKind::Integer,
        (_, Value::Number(_)) => AttributeKind::Float,
        (_, Value::String(_)) => AttributeKind::String,
        _ => AttributeKind::Json,
    };

    // The translator is the authority on whether a write can reach the
    // device at all.
    let writable = translator.writable(node_id, name);

    let mut a = if writable {
        AttributeSchema::new(kind)
    } else {
        AttributeSchema::read_only(kind)
    };
    a = a.labelled(humanise(name));
    a.unit = unit_for(name).map(|u| u.to_string());
    if let Some((_, Some((min, max)))) = declared {
        a.min = Some(min);
        a.max = Some(max);
    }
    let options: Option<Vec<std::string::String>> = match name {
        "hvac_action" => Some(HVAC_ACTIONS.iter().map(|s| s.to_string()).collect()),
        "hvac_mode" => translator
            .options(node_id, name)
            .or_else(|| Some(HVAC_MODES_FALLBACK.iter().map(|s| s.to_string()).collect())),
        "fan_mode" => translator.options(node_id, name),
        _ => None,
    };
    a.options = options.map(|o| o.into_iter().map(AttributeOption::new).collect());
    if name == "lock_operation_type" {
        a.options = Some(vec![
            AttributeOption::new("1").labelled("Stays as it is"),
            AttributeOption::new("2").labelled("Locks again by itself"),
        ]);
        a.category = Some(AttributeCategory::Config);
    }
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
    if matches!(
        name,
        "lock_timeout_secs" | "lock_auto_relock_secs" | "child_lock"
    ) {
        a.category = Some(AttributeCategory::Config);
    }
    if matches!(value, Value::Bool(_)) {
        a.states = Some(states_for(name));
    }
    a
}

fn diagnostic(mut a: AttributeSchema) -> AttributeSchema {
    a.category = Some(AttributeCategory::Diagnostic);
    a
}

/// The full schema for a node: what it reported, plus what every device can
/// say (`last_command_error`) and, on a lock, its lock events and its
/// `set_code_enabled` action — with the code slots named by who they are for.
///
/// `None` for a node that has reported nothing yet (still interviewing) and
/// is not a lock.
pub fn node_schema(
    node: &NodeState,
    state: &Value,
    translator: &Translator,
    names: &BTreeMap<u32, String>,
) -> Option<Value> {
    let empty = Map::new();
    let obj = state.as_object().unwrap_or(&empty);
    let lock = translator.info(node.node_id).lock;
    if obj.is_empty() && !lock {
        return None;
    }
    let mut schema = schema_for_state(node.node_id, obj, translator);
    let a = &mut schema.attributes;
    a.insert(
        "last_command_error".into(),
        diagnostic(
            AttributeSchema::read_only(AttributeKind::String).labelled("Last command error"),
        ),
    );
    if !lock {
        return serde_json::to_value(&schema).ok();
    }
    let slot_options: Vec<AttributeOption> = names
        .iter()
        .map(|(slot, who)| {
            AttributeOption::new(slot.to_string()).labelled(format!("Code {slot} · {who}"))
        })
        .collect();
    let mut jammed = AttributeSchema::read_only(AttributeKind::Bool).labelled("Jammed");
    jammed.states = Some(states_for("jammed"));
    a.entry("jammed".into()).or_insert(jammed);
    let mut action = AttributeSchema::read_only(AttributeKind::Enum).labelled("Last lock event");
    action.options = Some(
        crate::lock::ACTIONS
            .iter()
            .map(|(v, l)| AttributeOption::new(*v).labelled(*l))
            .collect(),
    );
    a.insert("last_lock_action".into(), action);
    let mut method = AttributeSchema::read_only(AttributeKind::Enum).labelled("How");
    method.options = Some(
        crate::lock::METHODS
            .iter()
            .map(|(v, l)| AttributeOption::new(*v).labelled(*l))
            .collect(),
    );
    a.insert("last_lock_method".into(), method);
    let mut slot = AttributeSchema::read_only(AttributeKind::Integer).labelled("Code used");
    if !slot_options.is_empty() {
        slot.options = Some(slot_options);
    }
    a.insert("last_lock_slot".into(), slot);
    a.insert(
        "last_lock_event".into(),
        diagnostic(
            AttributeSchema::read_only(AttributeKind::Json).labelled("Last lock event, in full"),
        ),
    );
    let slot_param = Param::int("slot")
        .label("Code")
        .required()
        .range(1.0, 250.0);
    let slot_param = if names.is_empty() {
        slot_param
    } else {
        slot_param.labelled_options(
            names
                .iter()
                .map(|(slot, who)| (slot.to_string(), format!("{slot} · {who}"))),
        )
    };
    let actions = vec![Action::new("set_code_enabled")
        .label("Turn a code on or off")
        .description(
            "Turn one of the lock's codes on or off. Off keeps it on the lock, so it can be \
             turned back on — the cleaner's code only on Tuesdays. Codes themselves are set on \
             the lock's Codes tab, never in an automation.",
        )
        .category("Codes")
        .sentence("turn code {slot} on {device} {enabled}")
        .requires_role(Role::Admin)
        .param(slot_param)
        .param(Param::bool_("enabled").label("On").required().default(true))];
    Some(with_actions(&schema, actions))
}

/// The schema for a node that reported [`state`].
pub fn schema_for_state(
    node_id: u32,
    state: &Map<String, Value>,
    translator: &Translator,
) -> DeviceSchema {
    let mut attributes: HashMap<String, AttributeSchema> = HashMap::new();
    for (name, value) in state {
        attributes.insert(name.clone(), describe(node_id, name, value, translator));
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
#[cfg(test)]
pub fn schema_json(node_id: u32, state: &Value, translator: &Translator) -> Option<Value> {
    let obj = state.as_object()?;
    if obj.is_empty() {
        return None;
    }
    serde_json::to_value(schema_for_state(node_id, obj, translator)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeState;
    use serde_json::json;

    /// A lock reports whether it is locked; it also reports battery, and its
    /// own name. Only the first is the point of the device.
    #[test]
    fn housekeeping_is_declared_and_the_reading_is_left_primary() {
        let s = schema(json!({
            "locked": true,
            "battery_pct": 80,
            "name": "Front Door",
        }));
        assert_eq!(s.attributes["locked"].category, None);
        for name in ["battery_pct", "name"] {
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

    const NODE: u32 = 7;

    /// A dimmer with a lock, a barrier and colour — enough that every
    /// writable shared name has somewhere to go.
    fn translator() -> Translator {
        let t = Translator::new();
        let node = NodeState::from_value(&json!({
            "nodeId": NODE,
            "values": [
                { "commandClass": 38, "endpoint": 0, "property": "currentValue", "value": 10 },
                { "commandClass": 98, "endpoint": 0, "property": "currentMode", "value": 0 },
                { "commandClass": 51, "endpoint": 0, "property": "currentColor",
                  "value": { "warmWhite": 0, "coldWhite": 0, "red": 0, "green": 0, "blue": 0 } },
                { "commandClass": 64, "endpoint": 0, "property": "mode", "value": 1,
                  "metadata": { "states": { "0": "Off", "1": "Heat" } } }
            ]
        }))
        .unwrap();
        t.learn_node(&node);
        t
    }

    fn schema(v: Value) -> DeviceSchema {
        schema_for_state(NODE, v.as_object().unwrap(), &translator())
    }

    /// Every boolean names both of its states, mapped or not.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let s = schema(json!({
            "on": true,
            "open": false,
            "motion": true,
            "water_detected": false,
            "smoke": false,
            "tampered": false,
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
        assert_eq!(
            s.attributes["tampered"]
                .states
                .as_ref()
                .unwrap()
                .get(true)
                .label,
            "tampered"
        );
    }

    /// `open` is TRUE when the door is OPEN.
    #[test]
    fn open_means_open() {
        let s = schema(json!({ "open": true }));
        let st = s.attributes["open"].states.as_ref().unwrap();
        assert_eq!(st.get(true).label, "open");
        assert_eq!(st.get(false).label, "closed");
    }

    /// Writability is exactly what the translator can route on this node.
    #[test]
    fn writability_matches_the_translator() {
        let t = translator();
        let s = schema(json!({
            "on": true,
            "brightness_pct": 50,
            "motion": true,
            "battery_pct": 90,
            "temperature": 21.5,
            "open": false,
        }));
        for (name, attr) in &s.attributes {
            assert_eq!(attr.writable, t.writable(NODE, name), "{name}");
        }
        assert!(s.attributes["on"].writable);
        assert!(s.attributes["brightness_pct"].writable);
        assert!(!s.attributes["motion"].writable);
        assert!(!s.attributes["battery_pct"].writable);
        // A contact reading, not a garage door: nothing to write.
        assert!(!s.attributes["open"].writable);
    }

    /// Shared names carry the vocabulary's kind, unit and range, whatever
    /// JSON number the reading happened to be.
    #[test]
    fn shared_names_are_declared_in_their_units() {
        let s = schema(json!({
            "brightness_pct": 50,
            "battery_pct": 90,
            "temperature": 21,
            "heat_setpoint": 20,
            "pressure": 1013,
            "illuminance": 300,
            "color_rgb": { "r": 1, "g": 2, "b": 3 },
            "color_temp": 2700,
            "hvac_mode": "heat",
            "hvac_action": "idle",
        }));
        let a = |n: &str| &s.attributes[n];
        assert_eq!(a("brightness_pct").kind, AttributeKind::Float);
        assert_eq!(a("brightness_pct").unit.as_deref(), Some("%"));
        assert_eq!(
            (a("brightness_pct").min, a("brightness_pct").max),
            (Some(0.0), Some(100.0))
        );
        assert_eq!(a("battery_pct").unit.as_deref(), Some("%"));
        assert_eq!(a("temperature").kind, AttributeKind::Float);
        assert_eq!(a("temperature").unit.as_deref(), Some("°C"));
        assert_eq!(a("heat_setpoint").unit.as_deref(), Some("°C"));
        assert!(a("heat_setpoint").writable);
        assert_eq!(a("pressure").unit.as_deref(), Some("hPa"));
        assert_eq!(a("illuminance").unit.as_deref(), Some("lux"));
        assert_eq!(a("color_rgb").kind, AttributeKind::ColorRgb);
        assert!(a("color_rgb").writable);
        assert_eq!(a("color_temp").kind, AttributeKind::ColorTemp);
        assert_eq!(a("color_temp").unit.as_deref(), Some("K"));
        assert_eq!(a("hvac_mode").kind, AttributeKind::Enum);
        let modes: Vec<&str> = a("hvac_mode")
            .options
            .as_ref()
            .unwrap()
            .iter()
            .map(|o| o.value.as_str())
            .collect();
        assert_eq!(modes, vec!["off", "heat"]);
        assert!(!a("hvac_action").writable);
        assert_eq!(a("hvac_action").options.as_ref().unwrap().len(), 6);
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
        assert!(schema_json(1, &json!({}), &t).is_none());
        assert!(schema_json(1, &Value::Null, &t).is_none());
    }
}
