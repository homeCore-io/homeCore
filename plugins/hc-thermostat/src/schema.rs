//! What a virtual thermostat reports and what it can be told to do.
//!
//! This plugin published no [`DeviceSchema`] before, so every client inferred
//! each attribute from its observed value — a bool became a switch, a number
//! became a slider, and the attribute's meaning came from a lexicon keyed on
//! its name.
//!
//! **Nothing here is writable.** `on_device_command` dispatches entirely on a
//! `command` string — `{"command": "set_setpoint", "value": 21.5}` — and
//! ignores attribute-style writes completely. Declaring `setpoint` writable
//! would render a slider whose every drag is silently dropped, so the writable
//! surface is declared as [`DeviceAction`]s instead and every attribute is
//! read-only.
//!
//! Only what `Runtime::state_payload` actually emits appears here; the tests
//! walk that function and fail if the two drift apart.

use plugin_sdk_rs::device_actions::{with_actions, Action, Param};
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use serde_json::Value;
use std::collections::HashMap;

/// A read-only attribute, labelled for display.
///
/// Every attribute on a thermostat is read-only — see the module note.
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
    a.options = Some(options.iter().map(|s| s.to_string()).collect());
    a
}

/// The attribute half of the schema.
fn attributes() -> HashMap<String, AttributeSchema> {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    // -- what it is doing right now ------------------------------------------
    a.insert(
        "current_temperature".into(),
        ro(AttributeKind::Float, "Current temperature"),
    );
    a.insert(
        "call_for".into(),
        // "stale" is a real value: it is what the aggregate reads when the
        // sensors have stopped reporting, and a rule wants to catch that.
        ro_enum("Calling for", &["idle", "heat", "cool", "stale"]),
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
        ro(AttributeKind::String, "Pending call"),
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
    // Reported, not writable: these change through `set_*` commands.
    a.insert("setpoint".into(), ro(AttributeKind::Float, "Setpoint"));
    a.insert("hysteresis".into(), ro(AttributeKind::Float, "Hysteresis"));
    a.insert("mode".into(), ro_enum("Mode", &["heat", "cool", "off"]));
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
            .sentence("set {device} to {value}")
            .param(Param::float("value").label("Target temperature").required()),
        Action::new("set_mode")
            .label("Set the mode")
            .category("Thermostat")
            .sentence("set {device} to {value}")
            .param(
                Param::enum_("value")
                    .options(["heat", "cool", "off"])
                    .label("Mode")
                    .required(),
            ),
        Action::new("set_hysteresis")
            .label("Set the hysteresis band")
            .category("Tuning")
            .param(Param::float("value").label("Hysteresis").required()),
        Action::new("set_aggregation")
            .label("Set how multiple sensors combine")
            .category("Tuning")
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

    /// Nothing is writable, because `on_device_command` dispatches only on a
    /// `command` string. A writable declaration here renders a control whose
    /// every use is silently dropped.
    #[test]
    fn no_attribute_is_ever_writable() {
        for (name, attr) in &schema().attributes {
            assert!(!attr.writable, "{name} claims to be writable");
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
