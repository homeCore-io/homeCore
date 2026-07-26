//! Schemas for the devices core owns.
//!
//! Timers, switches, counters and the rest are real devices on the bus — they
//! appear in the device list, rules trigger on them, dashboards render them —
//! but they are created directly in the state store rather than registered over
//! MQTT by a plugin, and nothing ever gave them a [`DeviceSchema`].
//!
//! So every client inferred them. A timer's `state` became a free text box
//! wanting `"finished"` *with the quotes*, and `repeat` became a bare boolean
//! whose other direction needed a Not gate — on the one device type whose whole
//! purpose is to fire a rule when it finishes.
//!
//! **Writability is the command surface, not the attribute list.** Glue devices
//! take `{"command": "start"}`-shaped payloads, so an attribute-style write of
//! `remaining_secs` does nothing. Only the attributes a command path genuinely
//! honours are declared writable; the rest are reported.

use hc_types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use std::collections::HashMap;

fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

fn ro_unit(kind: AttributeKind, label: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, label);
    a.unit = Some(unit.to_string());
    a
}

fn enum_of(mut a: AttributeSchema, options: &[&str]) -> AttributeSchema {
    a.options = Some(options.iter().map(|s| s.to_string()).collect());
    a
}

/// A boolean with both of its state names. Every boolean here has them: a
/// boolean attribute is two events, and a client given one name pushes the
/// other direction into a Not gate.
fn boolean(a: AttributeSchema, on: (&str, &str), off: (&str, &str)) -> AttributeSchema {
    a.with_states(BoolStates {
        when_true: StateLabel::verbed(on.0, on.1),
        when_false: StateLabel::verbed(off.0, off.1),
    })
}

fn on_off(a: AttributeSchema) -> AttributeSchema {
    boolean(a, ("on", "turns on"), ("off", "turns off"))
}

/// The schema for a glue device type, or `None` for a type we do not model.
///
/// The key is the `device_type` recorded on the device — the same string the
/// migration in [`super`] derives from the id prefix.
pub fn schema_for(device_type: &str) -> Option<DeviceSchema> {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    match device_type {
        "timer" => {
            a.insert(
                "state".into(),
                // "finished" is the value rules actually wait for, and it was
                // being typed by hand as a JSON string literal.
                enum_of(
                    ro(AttributeKind::Enum, "State"),
                    &["idle", "running", "paused", "finished", "cancelled"],
                ),
            );
            a.insert(
                "remaining_secs".into(),
                ro_unit(AttributeKind::Integer, "Remaining", "s"),
            );
            a.insert(
                "duration_secs".into(),
                ro_unit(AttributeKind::Integer, "Duration", "s"),
            );
            a.insert(
                "repeat".into(),
                boolean(
                    ro(AttributeKind::Bool, "Repeat"),
                    ("repeating", "starts repeating"),
                    ("one-shot", "stops repeating"),
                ),
            );
        }

        "switch" => {
            // The one glue attribute that really is written as an attribute.
            a.insert("on".into(), on_off(AttributeSchema::new(AttributeKind::Bool).labelled("State")));
        }

        "counter" => {
            a.insert("count".into(), ro(AttributeKind::Integer, "Count"));
        }

        "number" => {
            a.insert("value".into(), ro(AttributeKind::Float, "Value"));
        }

        "select" => {
            a.insert("selected".into(), ro(AttributeKind::String, "Selected"));
        }

        "text" => {
            a.insert("value".into(), ro(AttributeKind::String, "Value"));
        }

        "datetime" => {
            a.insert("value".into(), ro(AttributeKind::String, "Value"));
        }

        "button" => {
            a.insert(
                "last_pressed".into(),
                ro(AttributeKind::String, "Last pressed"),
            );
        }

        "group" => {
            a.insert(
                "on".into(),
                boolean(
                    ro(AttributeKind::Bool, "Any member on"),
                    ("on", "turns on"),
                    ("off", "turns off"),
                ),
            );
            a.insert(
                "active_count".into(),
                ro(AttributeKind::Integer, "Members on"),
            );
            a.insert(
                "member_count".into(),
                ro(AttributeKind::Integer, "Members"),
            );
        }

        "threshold" => {
            a.insert(
                "above".into(),
                boolean(
                    ro(AttributeKind::Bool, "Above threshold"),
                    ("above", "rises above"),
                    ("below", "drops below"),
                ),
            );
            a.insert(
                "source_value".into(),
                ro(AttributeKind::Float, "Source value"),
            );
        }

        "schedule" => {
            a.insert(
                "active".into(),
                boolean(
                    ro(AttributeKind::Bool, "Active"),
                    ("active", "becomes active"),
                    ("inactive", "becomes inactive"),
                ),
            );
        }

        _ => return None,
    }

    Some(DeviceSchema {
        attributes: a,
        ..Default::default()
    })
}

/// The schema for a mode device (`core.mode`).
///
/// Modes are the other core-owned device family: a solar mode reports when it
/// flips and what drove it.
pub fn mode_schema() -> DeviceSchema {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();
    a.insert(
        "on".into(),
        boolean(
            AttributeSchema::new(AttributeKind::Bool).labelled("Active"),
            ("active", "becomes active"),
            ("inactive", "becomes inactive"),
        ),
    );
    a.insert(
        "kind".into(),
        enum_of(ro(AttributeKind::Enum, "Kind"), &["solar", "boolean"]),
    );
    for (key, label) in [
        ("effective_on", "Turns on at"),
        ("effective_off", "Turns off at"),
        ("sunrise_today", "Sunrise"),
        ("sunset_today", "Sunset"),
    ] {
        a.insert(key.into(), ro(AttributeKind::String, label));
    }
    for (key, label) in [
        ("on_offset_minutes", "On offset"),
        ("off_offset_minutes", "Off offset"),
    ] {
        a.insert(key.into(), ro_unit(AttributeKind::Integer, label, "min"));
    }
    DeviceSchema {
        attributes: a,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: &[&str] = &[
        "timer", "switch", "counter", "number", "select", "text", "datetime",
        "button", "group", "threshold", "schedule",
    ];

    #[test]
    fn every_glue_type_declares_something() {
        for t in TYPES {
            assert!(schema_for(t).is_some(), "{t} declares nothing");
        }
        assert!(schema_for("not_a_glue_type").is_none());
    }

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one. Without the pair a client
    /// offers one row and the other direction needs a Not gate — and on a
    /// timer, "stops repeating" is a perfectly ordinary thing to want.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let schemas: Vec<(&str, DeviceSchema)> = TYPES
            .iter()
            .map(|t| (*t, schema_for(t).unwrap()))
            .chain(std::iter::once(("mode", mode_schema())))
            .collect();

        for (name, schema) in schemas {
            for (attr, spec) in &schema.attributes {
                if !matches!(spec.kind, AttributeKind::Bool) {
                    continue;
                }
                let s = spec
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{name}.{attr} is a bool with no state names"));
                assert_ne!(s.when_true.label, s.when_false.label, "{name}.{attr}");
            }
        }
    }

    /// A timer's `state` is the value rules wait for, and it must be offered
    /// rather than typed — including `finished`, which is what a delay-style
    /// rule triggers on.
    #[test]
    fn a_timer_offers_its_states() {
        let s = schema_for("timer").unwrap();
        let opts = s.attributes["state"].options.clone().unwrap();
        assert!(opts.contains(&"finished".to_string()), "{opts:?}");
        assert!(opts.contains(&"running".to_string()));
        assert!(matches!(s.attributes["state"].kind, AttributeKind::Enum));
    }

    /// Glue devices take `{"command": ...}` payloads. A `remaining_secs` that
    /// claims to be writable renders a control whose every use is dropped.
    #[test]
    fn only_the_genuinely_writable_attributes_claim_to_be() {
        for t in TYPES {
            let schema = schema_for(t).unwrap();
            for (attr, spec) in &schema.attributes {
                if spec.writable {
                    assert_eq!(
                        (*t, attr.as_str()),
                        ("switch", "on"),
                        "{t}.{attr} claims to be writable"
                    );
                }
            }
        }
    }
}
