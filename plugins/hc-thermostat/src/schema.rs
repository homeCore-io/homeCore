//! What a virtual thermostat reports and what it can be told to do.
//!
//! This plugin published no [`DeviceSchema`] before, so every client inferred
//! each attribute from its observed value — a bool became a switch, a number
//! became a slider, and the attribute's meaning came from a lexicon keyed on
//! its name.
//!
//! Names follow the shared vocabulary (`hc_types::device_vocabulary`):
//! `temperature`, `setpoint` (both °C), `hvac_mode` and `hvac_action`.
//!
//! **Two things are writable**: `{"setpoint": 21}` and `{"hvac_mode": "heat"}`
//! are handled as attribute writes by `on_device_command`. Everything else is
//! reported only and changes through a declared action
//! (`{"action": "set_hysteresis", "value": 1.0}`).
//!
//! Only what `Runtime::state_payload` actually emits appears here; the tests
//! walk that function and fail if the two drift apart.

use plugin_sdk_rs::device_actions::{with_actions, Action, Param};
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use serde_json::Value;
use std::collections::HashMap;

/// A read-only attribute, labelled for display.
///
/// Most attributes on a thermostat are read-only — see the module note.
fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

fn ro_unit(kind: AttributeKind, label: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, label);
    a.unit = Some(unit.to_string());
    a
}

fn ro_enum(label: &str, options: &[&str]) -> AttributeSchema {
    let mut a = ro(AttributeKind::Enum, label);
    a.options = Some(options.iter().copied().map(AttributeOption::from).collect());
    a
}

/// The `hvac_mode`s a virtual thermostat can be in.
pub const MODES: &[&str] = &["heat", "cool", "off"];

/// The attribute half of the schema.
fn attributes() -> HashMap<String, AttributeSchema> {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    // -- what it is doing right now ------------------------------------------
    a.insert(
        "temperature".into(),
        ro_unit(AttributeKind::Float, "Temperature", "°C"),
    );
    a.insert(
        "hvac_action".into(),
        // What the actuator is doing for it now, not what it would like to.
        ro_enum("Doing now", &["idle", "heating", "cooling", "off"]),
    );
    a.insert(
        "sensors_stale".into(),
        // Its sensors have stopped reporting, so it holds the actuator as it
        // is. A rule wants to catch that.
        ro(AttributeKind::Bool, "Sensors stale").with_states(BoolStates {
            when_true: StateLabel::verbed("sensors stale", "loses its sensors"),
            when_false: StateLabel::verbed("sensors reporting", "hears its sensors again"),
        }),
    );
    a.insert(
        "settled".into(),
        // A boolean is two events. Without both names a client offers "settled"
        // and pushes "still waiting" behind a Not gate.
        ro(AttributeKind::Bool, "Settled").with_states(BoolStates {
            when_true: StateLabel::verbed("settled", "settles"),
            when_false: StateLabel::verbed("waiting for sensors", "starts waiting for sensors"),
        }),
    );
    a.insert(
        "actuator_state".into(),
        ro(AttributeKind::Bool, "Actuator").with_states(BoolStates {
            when_true: StateLabel::verbed("on", "turns on"),
            when_false: StateLabel::verbed("off", "turns off"),
        }),
    );
    a.insert(
        "pending_call".into(),
        // What it will do once the short-cycle lockout ends, in
        // `hvac_action`'s words; absent when nothing waits.
        ro_enum("Waiting to start", &["idle", "heating", "cooling"]),
    );
    a.insert(
        "actuator_last_change".into(),
        ro(AttributeKind::String, "Actuator last changed"),
    );
    a.insert(
        "lockout_until".into(),
        ro(AttributeKind::String, "Short-cycle lockout until"),
    );
    a.insert(
        "actuator_last_error".into(),
        ro(AttributeKind::Json, "Last actuator error"),
    );
    a.insert(
        "last_update".into(),
        ro(AttributeKind::String, "Last update"),
    );

    // -- how it is configured -------------------------------------------------
    // The setpoint and the mode take attribute writes; the rest change through
    // `set_*` actions.
    let mut setpoint = AttributeSchema::new(AttributeKind::Float).labelled("Setpoint");
    setpoint.unit = Some("°C".into());
    a.insert("setpoint".into(), setpoint);
    let mut mode = AttributeSchema::new(AttributeKind::Enum).labelled("Mode");
    mode.options = Some(MODES.iter().copied().map(AttributeOption::from).collect());
    a.insert("hvac_mode".into(), mode);
    // A width in degrees Celsius, not a temperature: no `°C` unit, or an app
    // showing °F would add 32 to it.
    a.insert(
        "hysteresis".into(),
        ro(AttributeKind::Float, "Deadband (°C difference)"),
    );
    a.insert(
        "aggregation".into(),
        ro_enum("Sensor aggregation", &["average", "min", "max"]),
    );
    a.insert("sensor_ids".into(), ro(AttributeKind::Json, "Sensors"));
    a.insert(
        "sensor_attribute".into(),
        ro(AttributeKind::String, "Sensor attribute"),
    );
    a.insert(
        "actuator_device_id".into(),
        ro(AttributeKind::String, "Actuator device"),
    );
    a.insert(
        "min_on_secs".into(),
        ro_unit(AttributeKind::Integer, "Minimum on time", "s"),
    );
    a.insert(
        "min_off_secs".into(),
        ro_unit(AttributeKind::Integer, "Minimum off time", "s"),
    );

    a
}

/// The commands `on_device_command` actually accepts, and nothing else.
fn actions() -> Vec<Action> {
    vec![
        Action::new("set_setpoint")
            .label("Set the target temperature")
            .category("Thermostat")
            // Stands in for the reading, so a screen starts the field at it.
            .writes("setpoint")
            .sentence("set {device} to {value}")
            .param(
                // °C, said as the unit so an app shows it in the person's own.
                Param::float("value")
                    .label("Target temperature")
                    .unit("°C")
                    .required(),
            ),
        Action::new("set_mode")
            .label("Set the mode")
            .category("Thermostat")
            .writes("hvac_mode")
            .sentence("set {device} to {value}")
            .param(
                Param::enum_("value")
                    .options(MODES.iter().copied())
                    .label("Mode")
                    .required(),
            ),
        Action::new("set_hysteresis")
            .label("Set the hysteresis band")
            .category("Tuning")
            .writes("hysteresis")
            .param(Param::float("value").label("Hysteresis").required()),
        Action::new("set_aggregation")
            .label("Set how multiple sensors combine")
            .category("Tuning")
            .writes("aggregation")
            .param(
                Param::enum_("value")
                    .options(["average", "min", "max"])
                    .label("Aggregation")
                    .required(),
            ),
        Action::new("set_short_cycle")
            .label("Set the short-cycle guards")
            .category("Tuning")
            .param(Param::int("min_on_secs").label("Minimum on time"))
            .param(Param::int("min_off_secs").label("Minimum off time")),
        Action::new("set_sensors")
            .label("Choose the sensors")
            .category("Setup")
            .writes("sensor_ids")
            .param(Param::string("sensor_ids").label("Sensor device ids, comma-separated"))
            .param(Param::string("attribute").label("Sensor attribute (°C)")),
        Action::new("set_actuator")
            .label("Choose what it switches")
            .category("Setup")
            .writes("actuator_device_id")
            // `on_cmd` / `off_cmd` (JSON payloads) are taken too, but have
            // no control to declare them with; they belong in the config file.
            .param(Param::device_ref("device_id").label("Actuator device")),
        Action::new("recalculate")
            .label("Re-evaluate now")
            .category("Thermostat"),
    ]
}

/// The schema every thermostat device publishes. All thermostats are the same
/// shape, so there is one.
pub fn device_schema_json() -> Value {
    with_actions(
        &DeviceSchema {
            attributes: attributes(),
            ..Default::default()
        },
        actions(),
    )
}

/// Publish the retained schema for a thermostat device.
///
/// Retained on purpose: a client connecting long after this plugin last ran
/// still learns what the device's attributes mean.
pub async fn publish(publisher: &DevicePublisher, device_id: &str) -> anyhow::Result<()> {
    publisher
        .register_device_schema_json(device_id, &device_schema_json())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> DeviceSchema {
        serde_json::from_value(device_schema_json()).expect("schema round-trips")
    }

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one: a client given only one name
    /// offers one row, and the other direction needs a Not gate wrapped round
    /// the trigger.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        for (name, attr) in &schema().attributes {
            if !matches!(attr.kind, AttributeKind::Bool) {
                continue;
            }
            let s = attr
                .states
                .as_ref()
                .unwrap_or_else(|| panic!("{name} is a bool with no state names"));
            assert!(!s.when_true.label.is_empty(), "{name}");
            assert_ne!(s.when_true.label, s.when_false.label, "{name}");
        }
    }

    /// Only what `on_device_command` takes as an attribute write is writable.
    /// A writable declaration for anything else renders a control whose every
    /// use is silently dropped.
    #[test]
    fn only_the_setpoint_and_mode_are_writable() {
        for (name, attr) in &schema().attributes {
            let expected = matches!(name.as_str(), "setpoint" | "hvac_mode");
            assert_eq!(attr.writable, expected, "{name}");
        }
    }

    /// Temperatures are declared in °C, as the vocabulary has them.
    #[test]
    fn temperatures_are_celsius() {
        let a = schema().attributes;
        for name in ["temperature", "setpoint"] {
            assert_eq!(a[name].unit.as_deref(), Some("°C"), "{name}");
        }
    }

    /// The schema must not drift from what the device actually publishes.
    ///
    /// `Runtime::state_payload` is the authority; this asserts the declared set
    /// matches it exactly in both directions. An attribute declared but never
    /// published is a control for something that does not exist; one published
    /// but not declared is the inference this schema exists to replace.
    #[test]
    fn the_declared_attributes_are_exactly_what_is_published() {
        let published = crate::bridge::sample_state_payload();
        let published: std::collections::HashSet<&str> = published
            .as_object()
            .expect("state payload is an object")
            .keys()
            .map(|s| s.as_str())
            .collect();
        let declared = schema().attributes;
        let declared: std::collections::HashSet<&str> =
            declared.keys().map(|s| s.as_str()).collect();

        let missing: Vec<_> = published.difference(&declared).collect();
        let extra: Vec<_> = declared.difference(&published).collect();
        assert!(
            missing.is_empty(),
            "published but not declared: {missing:?}"
        );
        assert!(extra.is_empty(), "declared but never published: {extra:?}");
    }

    /// Every declared action is one `on_device_command` actually handles.
    #[test]
    fn every_action_is_a_command_the_bridge_accepts() {
        const HANDLED: &[&str] = &[
            "set_setpoint",
            "set_mode",
            "set_hysteresis",
            "set_aggregation",
            "set_short_cycle",
            "set_sensors",
            "set_actuator",
            "recalculate",
        ];
        for a in &schema().actions {
            assert!(
                HANDLED.contains(&a.id.as_str()),
                "{} is declared but the bridge does not handle it",
                a.id
            );
        }
    }
}
