//! What each YoLink device kind reports, declared rather than guessed.
//!
//! Until this module existed hc-yolink published no [`DeviceSchema`] at all —
//! only `register_device_full`, which carries a name and a device *type*. Every
//! client therefore inferred each attribute from its observed value: a bool got
//! a switch, a number got a slider, and the attribute's *meaning* came from a
//! lexicon keyed on its name.
//!
//! Naming both states of a boolean is the point. A boolean attribute is two
//! events — a door opens and a door closes — and a client given only one name
//! offers one row, leaving the other direction to a Not gate wrapped round the
//! trigger.
//!
//! This plugin also used to publish a `contact` attribute carrying the same
//! value as `open`, which is the reverse of what `contact` conventionally
//! means, so any client keying on the name read the sensor backwards. The
//! alias is gone; `open` says what it says, and [`BoolStates`] names both ends
//! of it.
//!
//! **Only what the translators actually emit appears here.** An attribute
//! declared but never published is a control for something that does not exist,
//! which is the same class of error in the other direction — see
//! `every_declared_attribute_is_published` in the tests.

use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use std::collections::HashMap;

use crate::devices::DeviceKind;
use plugin_sdk_rs::DevicePublisher;

/// A read-only attribute of `kind`, labelled for display.
fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

/// A read-only attribute with a unit.
fn ro_unit(kind: AttributeKind, label: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, label);
    a.unit = Some(unit.to_string());
    a
}

/// A read-only boolean whose two states have names.
fn ro_bool(label: &str, on: (&str, &str), off: (&str, &str)) -> AttributeSchema {
    ro(AttributeKind::Bool, label).with_states(BoolStates {
        when_true: StateLabel::verbed(on.0, on.1),
        when_false: StateLabel::verbed(off.0, off.1),
    })
}

/// The battery percentage every battery-powered YoLink device reports.
///
/// YoLink sends a 0–4 scale; `battery_pct` converts it, so the declared range
/// is the converted one, not the wire one.
fn battery() -> AttributeSchema {
    let mut a = ro_unit(AttributeKind::Integer, "Battery", "%");
    a.min = Some(0.0);
    a.max = Some(100.0);
    a
}

/// The schema for a device kind, or `None` for the kinds this plugin does not
/// bridge at all.
pub fn schema_for(kind: &DeviceKind) -> Option<DeviceSchema> {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    match kind {
        // The relay kinds. `on` is the one writable attribute in the whole
        // plugin's read path — `translate_command` accepts `{"on": bool}`.
        DeviceKind::Outlet
        | DeviceKind::SmartPlug
        | DeviceKind::Switch
        | DeviceKind::MultiOutlet => {
            a.insert(
                "on".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Power")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("on", "turns on"),
                        when_false: StateLabel::verbed("off", "turns off"),
                    }),
            );
            // Only outlets with a power meter publish these, so they are part
            // of the schema but will simply never appear on the others.
            a.insert(
                "power_w".into(),
                ro_unit(AttributeKind::Float, "Power draw", "W"),
            );
            a.insert(
                "energy_kwh".into(),
                ro_unit(AttributeKind::Float, "Energy used", "kWh"),
            );
        }

        DeviceKind::Siren => {
            a.insert(
                "on".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Siren")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("sounding", "starts sounding"),
                        when_false: StateLabel::verbed("silent", "goes silent"),
                    }),
            );
        }

        DeviceKind::DoorSensor => {
            a.insert(
                "open".into(),
                ro_bool("Door", ("open", "opens"), ("closed", "closes")),
            );
            a.insert("battery".into(), battery());
        }

        DeviceKind::MotionSensor => {
            a.insert(
                "motion".into(),
                ro_bool(
                    "Motion",
                    ("detecting motion", "detects motion"),
                    ("clear", "stops detecting motion"),
                ),
            );
            a.insert("battery".into(), battery());
        }

        DeviceKind::LeakSensor => {
            let wet = || ro_bool("Water", ("wet", "detects water"), ("dry", "dries out"));
            a.insert("water_detected".into(), wet());
            a.insert("battery".into(), battery());
        }

        DeviceKind::VibrationSensor => {
            a.insert(
                "vibration".into(),
                ro_bool(
                    "Vibration",
                    ("vibrating", "starts vibrating"),
                    ("still", "stops vibrating"),
                ),
            );
            a.insert("battery".into(), battery());
        }

        DeviceKind::THSensor => {
            a.insert(
                "temperature".into(),
                ro(AttributeKind::Float, "Temperature"),
            );
            // The unit the reading is in, as configured — text, not a number.
            a.insert(
                "temperature_unit".into(),
                ro(AttributeKind::String, "Temperature unit"),
            );
            a.insert(
                "humidity_pct".into(),
                ro_unit(AttributeKind::Float, "Humidity", "%"),
            );
            a.insert("battery".into(), battery());
        }

        DeviceKind::Lock | DeviceKind::LockV2 => {
            a.insert(
                "locked".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Lock")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("locked", "locks"),
                        when_false: StateLabel::verbed("unlocked", "unlocks"),
                    }),
            );
            // The bolt and the door are different things: a lock can be thrown
            // while the door stands open, and that is worth a rule.
            a.insert(
                "door_open".into(),
                ro_bool("Door", ("open", "opens"), ("closed", "closes")),
            );
            a.insert("last_alert".into(), ro(AttributeKind::String, "Last alert"));
            a.insert(
                "auto_lock_secs".into(),
                ro_unit(AttributeKind::Integer, "Auto-lock delay", "s"),
            );
            a.insert(
                "sound_level".into(),
                ro(AttributeKind::Integer, "Sound level"),
            );
            a.insert("battery".into(), battery());
        }

        DeviceKind::Hub | DeviceKind::Unknown(_) => return None,
    }

    Some(DeviceSchema {
        attributes: a,
        actions: Vec::new(),
    })
}

/// Publish the retained schema for a device, if its kind declares one.
///
/// Retained on purpose: a client that connects long after this plugin last ran
/// still learns what the device's attributes mean. A kind that declares nothing
/// publishes nothing rather than an empty schema, because an empty schema is a
/// claim ("this device has no attributes") and silence is not.
pub async fn publish(
    publisher: &DevicePublisher,
    hc_id: &str,
    kind: &DeviceKind,
) -> anyhow::Result<()> {
    let Some(schema) = schema_for(kind) else {
        return Ok(());
    };
    let value = serde_json::to_value(&schema)?;
    publisher.register_device_schema_json(hc_id, &value).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TemperatureUnit;
    use serde_json::json;

    fn all_kinds() -> Vec<DeviceKind> {
        vec![
            DeviceKind::Outlet,
            DeviceKind::SmartPlug,
            DeviceKind::Switch,
            DeviceKind::MultiOutlet,
            DeviceKind::DoorSensor,
            DeviceKind::MotionSensor,
            DeviceKind::LeakSensor,
            DeviceKind::THSensor,
            DeviceKind::VibrationSensor,
            DeviceKind::Lock,
            DeviceKind::LockV2,
            DeviceKind::Siren,
        ]
    }

    #[test]
    fn every_bridged_kind_has_a_schema() {
        for kind in all_kinds() {
            assert!(
                schema_for(&kind).is_some(),
                "{kind:?} is bridged but declares nothing"
            );
        }
        assert!(schema_for(&DeviceKind::Hub).is_none());
        assert!(schema_for(&DeviceKind::Unknown("x".into())).is_none());
    }

    /// Every boolean must name both of its states.
    ///
    /// A boolean attribute is two events: without the pair, a client offers one
    /// row and the other direction needs a Not gate wrapped round the trigger.
    /// Half-declaring is worse than not declaring, because the client's own
    /// fallback is skipped for an attribute that then has no second name.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        for kind in all_kinds() {
            let schema = schema_for(&kind).unwrap();
            for (name, attr) in &schema.attributes {
                if !matches!(attr.kind, AttributeKind::Bool) {
                    continue;
                }
                let states = attr
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{kind:?}.{name} is a bool with no state names"));
                assert!(!states.when_true.label.is_empty(), "{kind:?}.{name}");
                assert!(!states.when_false.label.is_empty(), "{kind:?}.{name}");
                assert_ne!(
                    states.when_true.label, states.when_false.label,
                    "{kind:?}.{name} names both states the same thing"
                );
            }
        }
    }

    /// A door sensor reports `open` and nothing that duplicates it.
    ///
    /// The `contact` alias carried the same value under a name that
    /// conventionally means the opposite, so every client keying on the name
    /// read the sensor backwards — and it put two identical choices in front
    /// of anyone writing a rule.
    #[test]
    fn a_door_reports_open_and_no_alias_for_it() {
        let schema = schema_for(&DeviceKind::DoorSensor).unwrap();
        assert!(schema.attributes.contains_key("open"));
        assert!(!schema.attributes.contains_key("contact"));

        let open = schema.attributes["open"].states.as_ref().unwrap();
        assert_eq!(open.get(true).label, "open");
        assert_eq!(open.get(false).label, "closed");

        // And the translator agrees, which is the actual authority.
        let state = DeviceKind::DoorSensor
            .translate_state(&json!({ "state": "open" }), &TemperatureUnit::C)
            .unwrap();
        assert_eq!(state["open"], json!(true));
        assert!(state.get("contact").is_none());
    }

    /// A declared attribute the device never publishes is a control for
    /// something that does not exist. Walk the real translators and check that
    /// every name we declare can actually appear.
    #[test]
    fn every_declared_attribute_is_published() {
        // Reports rich enough to exercise every optional branch of each
        // translator, so anything genuinely reachable shows up.
        let cases: Vec<(DeviceKind, serde_json::Value)> = vec![
            (
                DeviceKind::Switch,
                json!({ "state": "open", "power": 12.5, "electricity": 3.25 }),
            ),
            (DeviceKind::Siren, json!({ "state": "open" })),
            (
                DeviceKind::DoorSensor,
                json!({ "state": "open", "battery": 4 }),
            ),
            (
                DeviceKind::MotionSensor,
                json!({ "alarm": true, "battery": 3 }),
            ),
            (
                DeviceKind::LeakSensor,
                json!({ "alarm": true, "battery": 2 }),
            ),
            (
                DeviceKind::VibrationSensor,
                json!({ "alarm": true, "battery": 1 }),
            ),
            (
                DeviceKind::THSensor,
                json!({ "temperature": 21.5, "humidity": 44.0, "tempUnit": "℃", "battery": 4 }),
            ),
            (
                DeviceKind::LockV2,
                json!({
                    "state": { "lock": "locked", "door": "open" },
                    "battery": 4,
                    "alert": { "type": "DoorOpenAlarm" },
                    "attributes": { "autoLock": 30, "soundLevel": 2 }
                }),
            ),
        ];

        for (kind, report) in cases {
            let declared = schema_for(&kind).unwrap();
            let published = kind
                .translate_state(&report, &TemperatureUnit::C)
                .unwrap_or_else(|| panic!("{kind:?} translated nothing"));
            let published = published.as_object().unwrap();

            for name in declared.attributes.keys() {
                assert!(
                    published.contains_key(name),
                    "{kind:?} declares `{name}` but a full report does not publish it; \
                     published: {:?}",
                    published.keys().collect::<Vec<_>>()
                );
            }
        }
    }

    /// Only `on` and `locked` are writable, because those are the only two
    /// things `translate_command` accepts. A writable declaration the command
    /// path rejects renders a control that fails on use.
    #[test]
    fn nothing_is_writable_that_the_command_path_refuses() {
        for kind in all_kinds() {
            let schema = schema_for(&kind).unwrap();
            for (name, attr) in &schema.attributes {
                if attr.writable {
                    assert!(
                        name == "on" || name == "locked",
                        "{kind:?}.{name} claims to be writable"
                    );
                    // And the command path really does take it.
                    let cmd = json!({ name.as_str(): true });
                    assert!(
                        kind.translate_command(&cmd).is_ok(),
                        "{kind:?} declares `{name}` writable but rejects the command"
                    );
                }
            }
        }
    }
}
