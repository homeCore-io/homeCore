//! What each ISY device kind reports, declared rather than inferred.
//!
//! This plugin published no [`DeviceSchema`], so every client guessed each
//! attribute from its observed value and took the attribute's *meaning* from a
//! lexicon keyed on its name.
//!
//! That guess is wrong on a contact sensor. [`node_to_state`](crate::device)
//! publishes `contact` equal to `open`, so `contact: true` means the door is
//! OPEN — the reverse of the usual convention, where a closed contact circuit
//! means the door is shut. Declaring the pair is what settles it.
//!
//! It is wrong on a water sensor too, in the other direction: `leak` is true
//! when `ST <= 0`, because Insteon leak sensors report ON when *dry*.
//!
//! **Writability is taken from `cmd_to_isy`, not assumed.** That function is
//! the only thing that turns a payload into an ISY command, so an attribute it
//! does not read cannot be written — declaring it writable would render a
//! control that silently does nothing. The tests check both directions.

use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use std::collections::HashMap;

use crate::device::DeviceKind;

fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

fn rw(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::new(kind).labelled(label)
}

fn ranged(mut a: AttributeSchema, min: f64, max: f64, unit: Option<&str>) -> AttributeSchema {
    a.min = Some(min);
    a.max = Some(max);
    a.unit = unit.map(|u| u.to_string());
    a
}

fn enum_of(mut a: AttributeSchema, options: &[&str]) -> AttributeSchema {
    a.options = Some(options.iter().map(|s| s.to_string()).collect());
    a
}

/// A boolean with both of its state names.
fn boolean(a: AttributeSchema, on: (&str, &str), off: (&str, &str)) -> AttributeSchema {
    a.with_states(BoolStates {
        when_true: StateLabel::verbed(on.0, on.1),
        when_false: StateLabel::verbed(off.0, off.1),
    })
}

fn on_off(a: AttributeSchema) -> AttributeSchema {
    boolean(a, ("on", "turns on"), ("off", "turns off"))
}

fn open_closed(a: AttributeSchema) -> AttributeSchema {
    boolean(a, ("open", "opens"), ("closed", "closes"))
}

/// The schema for an ISY device kind.
pub fn schema_for(kind: &DeviceKind) -> DeviceSchema {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    match kind {
        DeviceKind::Light => {
            a.insert("on".into(), on_off(rw(AttributeKind::Bool, "Power")));
            a.insert(
                "brightness".into(),
                ranged(
                    rw(AttributeKind::Integer, "Brightness (raw)"),
                    0.0,
                    255.0,
                    None,
                ),
            );
            a.insert(
                "brightness_pct".into(),
                ranged(
                    rw(AttributeKind::Integer, "Brightness"),
                    0.0,
                    100.0,
                    Some("%"),
                ),
            );
        }

        DeviceKind::Switch | DeviceKind::Scene => {
            a.insert("on".into(), on_off(rw(AttributeKind::Bool, "Power")));
        }

        DeviceKind::ContactSensor => {
            a.insert("open".into(), open_closed(ro(AttributeKind::Bool, "Door")));
        }

        DeviceKind::MotionSensor => {
            let motion = |label| {
                boolean(
                    ro(AttributeKind::Bool, label),
                    ("detecting motion", "detects motion"),
                    ("clear", "stops detecting motion"),
                )
            };
            a.insert("motion".into(), motion("Motion"));
        }

        DeviceKind::WaterSensor => {
            // Inverted at the source: Insteon leak sensors report ON when DRY,
            // so `node_to_state` sets leak = (ST <= 0).
            let wet = |label| {
                boolean(
                    ro(AttributeKind::Bool, label),
                    ("wet", "detects water"),
                    ("dry", "dries out"),
                )
            };
            a.insert("water_detected".into(), wet("Water"));
        }

        DeviceKind::BinarySensor => {
            a.insert("on".into(), on_off(ro(AttributeKind::Bool, "State")));
            // Only present when the node type maps to one; still declared,
            // because a rule may legitimately read it.
            a.insert(
                "device_class".into(),
                ro(AttributeKind::String, "Device class"),
            );
        }

        DeviceKind::Sensor => {
            a.insert("value".into(), ro(AttributeKind::Float, "Value"));
            a.insert("unit".into(), ro(AttributeKind::String, "Unit"));
        }

        DeviceKind::Lock => {
            a.insert(
                "locked".into(),
                boolean(
                    rw(AttributeKind::Bool, "Lock"),
                    ("locked", "locks"),
                    ("unlocked", "unlocks"),
                ),
            );
        }

        DeviceKind::Cover => {
            a.insert(
                "position".into(),
                ranged(
                    rw(AttributeKind::Integer, "Position"),
                    0.0,
                    100.0,
                    Some("%"),
                ),
            );
            a.insert(
                "state".into(),
                enum_of(rw(AttributeKind::Enum, "State"), &["open", "closed"]),
            );
        }

        DeviceKind::Fan => {
            a.insert("on".into(), on_off(rw(AttributeKind::Bool, "Power")));
            a.insert(
                "speed".into(),
                enum_of(
                    rw(AttributeKind::Enum, "Speed"),
                    &["off", "low", "medium", "high"],
                ),
            );
        }

        DeviceKind::Thermostat => {
            a.insert(
                "state".into(),
                enum_of(
                    ro(AttributeKind::Enum, "Operating state"),
                    &["idle", "heating", "cooling"],
                ),
            );
            a.insert(
                "temperature".into(),
                ro(AttributeKind::Float, "Current temperature"),
            );
            a.insert("hvac_mode".into(), ro(AttributeKind::String, "HVAC mode"));
            a.insert(
                "target_temp_heat".into(),
                rw(AttributeKind::Float, "Heat setpoint"),
            );
            a.insert(
                "target_temp_cool".into(),
                rw(AttributeKind::Float, "Cool setpoint"),
            );
            a.insert(
                "fan_mode".into(),
                enum_of(ro(AttributeKind::Enum, "Fan mode"), &["auto", "on"]),
            );
        }
    }

    DeviceSchema {
        attributes: a,
        ..Default::default()
    }
}

/// Publish the retained schema for a device.
pub async fn publish(
    publisher: &DevicePublisher,
    device_id: &str,
    kind: &DeviceKind,
) -> anyhow::Result<()> {
    let value = serde_json::to_value(schema_for(kind))?;
    publisher
        .register_device_schema_json(device_id, &value)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{cmd_to_isy, node_to_state};
    use crate::isy::IsyNode;
    use serde_json::json;

    const ALL: &[DeviceKind] = &[
        DeviceKind::Light,
        DeviceKind::Switch,
        DeviceKind::ContactSensor,
        DeviceKind::MotionSensor,
        DeviceKind::WaterSensor,
        DeviceKind::BinarySensor,
        DeviceKind::Sensor,
        DeviceKind::Lock,
        DeviceKind::Cover,
        DeviceKind::Fan,
        DeviceKind::Thermostat,
        DeviceKind::Scene,
    ];

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one: a client given only one name
    /// offers one row, and the other direction needs a Not gate.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        for kind in ALL {
            for (name, attr) in &schema_for(kind).attributes {
                if !matches!(attr.kind, AttributeKind::Bool) {
                    continue;
                }
                let s = attr
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{kind:?}.{name} is a bool with no state names"));
                assert!(!s.when_true.label.is_empty(), "{kind:?}.{name}");
                assert_ne!(
                    s.when_true.label, s.when_false.label,
                    "{kind:?}.{name} names both states the same thing"
                );
            }
        }
    }

    /// One name per reading, and the inversion still declared correctly.
    ///
    /// This plugin used to publish `contact` beside `open`, `occupancy` beside
    /// `motion`, and `leak` beside `water_detected` — the same value under two
    /// names each time, which showed every sensor twice and offered a rule
    /// author two identical choices. The aliases are gone.
    ///
    /// The remaining inversion is real and stays declared: a water sensor is
    /// wet when `ST <= 0`, because Insteon leak sensors report ON when dry.
    #[test]
    fn one_name_per_reading_with_the_inversion_declared() {
        let contact = schema_for(&DeviceKind::ContactSensor).attributes;
        assert!(contact.contains_key("open"));
        assert!(!contact.contains_key("contact"));

        let motion = schema_for(&DeviceKind::MotionSensor).attributes;
        assert!(motion.contains_key("motion"));
        assert!(!motion.contains_key("occupancy"));

        let water = schema_for(&DeviceKind::WaterSensor).attributes;
        assert!(!water.contains_key("leak"));
        let w = water["water_detected"].states.as_ref().unwrap();
        assert_eq!(w.get(true).label, "wet");
    }

    /// Nothing is declared writable that `cmd_to_isy` will not act on.
    ///
    /// It is the only path from a payload to an ISY command, so an attribute it
    /// ignores cannot be written, and a control for it would silently do
    /// nothing.
    #[test]
    fn nothing_is_writable_that_cmd_to_isy_ignores() {
        for kind in ALL {
            for (name, attr) in &schema_for(kind).attributes {
                if !attr.writable {
                    continue;
                }
                // A value of the right shape for this attribute's kind.
                let probe = match attr.kind {
                    AttributeKind::Bool => json!({ name.as_str(): true }),
                    AttributeKind::Integer | AttributeKind::Float => {
                        json!({ name.as_str(): 50 })
                    }
                    AttributeKind::Enum => json!({
                        name.as_str(): attr.options.as_ref().unwrap()[0].as_str()
                    }),
                    _ => continue,
                };
                assert!(
                    !cmd_to_isy(&probe, kind).is_empty(),
                    "{kind:?}.{name} is declared writable but cmd_to_isy produces nothing"
                );
            }
        }
    }

    /// Everything a device publishes is declared.
    ///
    /// `node_to_state` is the authority. An undeclared attribute falls back to
    /// client inference, which is exactly what this module exists to replace.
    #[test]
    fn every_published_attribute_is_declared() {
        for kind in ALL {
            // A node rich enough to exercise the OPTIONAL branches too. A
            // default node has no properties, so a thermostat would publish
            // only `state` and the test would pass without ever checking the
            // five attributes that appear when the ISY reports them.
            let mut node = IsyNode::default();
            for prop in ["ST", "CLITEMP", "CLIMD", "CLISPH", "CLISPC", "CLIFAN"] {
                node.properties.insert(
                    prop.into(),
                    crate::isy::IsyProperty {
                        value: 1,
                        formatted: "1".into(),
                        uom: "17".into(),
                        prec: 0,
                    },
                );
            }
            // A node type that maps to a binary-sensor device class, so that
            // optional key is exercised as well.
            node.node_type = "16.8.0.0".into();
            let published = node_to_state(&node, kind);
            let declared = schema_for(kind).attributes;
            for name in published.as_object().unwrap().keys() {
                assert!(
                    declared.contains_key(name),
                    "{kind:?} publishes `{name}` but does not declare it"
                );
            }
        }
    }
}
