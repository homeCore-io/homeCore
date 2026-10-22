//! What a thermostat reports and what it can be told to do.
//!
//! Names follow the shared vocabulary (`hc_types::device_vocabulary`):
//! `temperature`, `setpoint` (both °C), `hvac_mode` and `hvac_action`. The
//! rest are its own.
//!
//! **Two things are writable**: `{"setpoint": 21}` and `{"hvac_mode": "cool"}`.
//! Everything else is reported only and changes through a declared action
//! (`{"action": "hold", "setpoint": 19, "for_secs": 7200}`).
//!
//! **Per thermostat**, because the modes are: an AC or a fan offers `cool` and
//! `off`, a heater `heat` and `off`.
//!
//! Only what `controller::state_payload` emits appears here; the tests walk
//! that function and fail if the two drift apart.

use crate::config::{Kind, MAX_GUARD_SECS, MAX_HYSTERESIS, SETPOINT_RANGE};
use crate::controller::MAX_HOLD_SECS;
use plugin_sdk_rs::device_actions::{with_actions, Action, Param};
use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use plugin_sdk_rs::DevicePublisher;
use serde_json::Value;
use std::collections::HashMap;

fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

fn ro_unit(kind: AttributeKind, label: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, label);
    a.unit = Some(unit.to_string());
    a
}

fn ro_enum(label: &str, options: &[(&str, &str)]) -> AttributeSchema {
    let mut a = ro(AttributeKind::Enum, label);
    a.options = Some(
        options
            .iter()
            .map(|(v, l)| AttributeOption::new(*v).labelled(*l))
            .collect(),
    );
    a
}

fn in_cat(mut a: AttributeSchema, c: AttributeCategory) -> AttributeSchema {
    a.category = Some(c);
    a
}

fn yes_no(a: AttributeSchema, t: (&str, &str), f: (&str, &str)) -> AttributeSchema {
    a.with_states(BoolStates {
        when_true: StateLabel::verbed(t.0, t.1),
        when_false: StateLabel::verbed(f.0, f.1),
    })
}

/// What `reason` says, in words.
pub const REASONS: &[(&str, &str)] = &[
    ("cooling", "Cooling"),
    ("heating", "Heating"),
    ("idle_in_band", "Idle"),
    ("waiting_min_off", "Waiting — short-cycle guard"),
    ("waiting_min_on", "Waiting to stop — short-cycle guard"),
    ("override", "Left alone — switched by hand"),
    ("sensors_stale", "No current reading"),
    ("actuator_not_responding", "Switch not responding"),
    ("max_run_exceeded", "Longest run reached"),
    ("no_switch", "No switch chosen"),
    ("off", "Off"),
];

fn mode_label(m: &str) -> &'static str {
    match m {
        "heat" => "Heat",
        "cool" => "Cool",
        _ => "Off",
    }
}

/// The attribute half of the schema.
fn attributes(kind: Kind) -> HashMap<String, AttributeSchema> {
    use AttributeCategory::{Config, Diagnostic};
    use AttributeKind::{Bool, Enum, Float, Integer, Json, String as Text};
    let running = if kind == Kind::Heater {
        ("heating", "Heating")
    } else {
        ("cooling", "Cooling")
    };
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();
    let mut put = |k: &str, v: AttributeSchema| {
        a.insert(k.to_string(), v);
    };

    // -- what it is doing --------------------------------------------------
    // Bookkeeping (today's counts, when a hold ends, whether the switch
    // confirmed) is diagnostic: readable and usable in rules, but not each
    // change listed as something that happened.
    put("temperature", ro_unit(Float, "Temperature", "°C"));
    put(
        "hvac_action",
        // What the switch is confirmed to be doing, not what it was asked.
        ro_enum("Doing now", &[("idle", "Idle"), running, ("off", "Off")]),
    );
    put("reason", in_cat(ro_enum("Why", REASONS), Diagnostic));
    put("hold_setpoint", ro_unit(Float, "Holding at", "°C"));
    put("hold_until", in_cat(ro(Text, "Holding until"), Diagnostic));
    put(
        "override_until",
        in_cat(ro(Text, "Left alone until"), Diagnostic),
    );
    put(
        "sensors_stale",
        yes_no(
            ro(Bool, "No current reading"),
            ("no current reading", "loses its readings"),
            ("reading", "has a reading again"),
        ),
    );
    put("sensor_readings", in_cat(ro(Json, "Readings"), Diagnostic));
    put(
        "actuator_state",
        yes_no(
            ro(Bool, "Switch"),
            ("on", "turns its switch on"),
            ("off", "turns its switch off"),
        ),
    );
    put(
        "actuator_confirmed",
        in_cat(
            yes_no(
                ro(Bool, "Switch confirmed"),
                ("confirmed", "hears its switch confirm"),
                ("not confirmed", "waits for its switch to confirm"),
            ),
            Diagnostic,
        ),
    );
    put(
        "actuator_online",
        yes_no(
            ro(Bool, "Switch online"),
            ("switch online", "hears its switch again"),
            ("switch offline", "loses its switch"),
        ),
    );
    put(
        "pending_call",
        in_cat(ro_enum("Waiting to start", &[running]), Diagnostic),
    );
    put(
        "lockout_until",
        in_cat(ro(Text, "Short-cycle guard until"), Diagnostic),
    );
    put(
        "actuator_last_change",
        in_cat(ro(Text, "Switch last changed"), Diagnostic),
    );
    put("on_since", in_cat(ro(Text, "On since"), Diagnostic));
    put(
        "last_command_error",
        in_cat(ro(Text, "Last command error"), Diagnostic),
    );
    put(
        "today_from",
        in_cat(ro(Text, "Today counted from"), Diagnostic),
    );
    put(
        "run_secs_today",
        in_cat(ro_unit(Integer, "Ran today", "s"), Diagnostic),
    );
    put(
        "cycles_today",
        in_cat(ro(Integer, "Times on today"), Diagnostic),
    );
    put(
        "longest_run_secs_today",
        in_cat(ro_unit(Integer, "Longest run today", "s"), Diagnostic),
    );
    put(
        "settled",
        in_cat(
            yes_no(
                ro(Bool, "Settled"),
                ("settled", "settles"),
                ("starting", "starts"),
            ),
            Diagnostic,
        ),
    );

    // -- what it is set to -------------------------------------------------
    let mut setpoint = AttributeSchema::new(Float).labelled("Target");
    setpoint.unit = Some("°C".into());
    setpoint.min = Some(*SETPOINT_RANGE.start());
    setpoint.max = Some(*SETPOINT_RANGE.end());
    setpoint.step = Some(0.5);
    put("setpoint", setpoint);
    let mut mode = AttributeSchema::new(Enum).labelled("Mode");
    mode.options = Some(
        kind.modes()
            .iter()
            .map(|m| AttributeOption::new(*m).labelled(mode_label(m)))
            .collect(),
    );
    put("hvac_mode", mode);
    put(
        "kind",
        in_cat(
            ro_enum(
                "Runs",
                &[("ac", "An AC"), ("heater", "A heater"), ("fan", "A fan")],
            ),
            Config,
        ),
    );
    // A width in °C, not a temperature: no `°C` unit, or an app showing °F
    // would add 32 to it.
    put(
        "hysteresis",
        in_cat(ro(Float, "Swing (°C difference)"), Config),
    );
    put(
        "aggregation",
        in_cat(
            ro_enum(
                "Combine sensors by",
                &[
                    ("average", "Average"),
                    ("min", "Coldest"),
                    ("max", "Warmest"),
                ],
            ),
            Config,
        ),
    );
    put("sensor_ids", in_cat(ro(Json, "Sensors"), Config));
    put(
        "sensor_attribute",
        in_cat(ro(Text, "Sensor attribute"), Diagnostic),
    );
    put("actuator_device_id", in_cat(ro(Text, "Switch"), Config));
    for (k, l) in [
        ("min_on_secs", "Once on, stays on at least"),
        ("min_off_secs", "Once off, stays off at least"),
        ("max_run_secs", "Longest run"),
        ("sensor_timeout_secs", "A sensor goes quiet after"),
        ("override_secs", "Switched by hand, left alone for"),
    ] {
        put(k, in_cat(ro_unit(Integer, l, "s"), Config));
    }
    put(
        "on_stale",
        in_cat(
            ro_enum(
                "With no reading",
                &[("off", "Turn it off"), ("hold", "Leave it")],
            ),
            Config,
        ),
    );
    a
}

fn target_param(name: &str) -> Param {
    Param::float(name)
        .label("Target")
        // °C, said as the unit so an app shows it in the person's own.
        .unit("°C")
        .range(*SETPOINT_RANGE.start(), *SETPOINT_RANGE.end())
        .step(0.5)
        .required()
}

/// The commands `Controller::on_command` actually takes, and nothing else.
fn actions(kind: Kind) -> Vec<Action> {
    vec![
        Action::new("set_setpoint")
            .label("Set the target")
            .category("Thermostat")
            .writes("setpoint")
            .sentence("set {device} to {value}")
            .param(target_param("value")),
        Action::new("set_mode")
            .label("Set the mode")
            .category("Thermostat")
            .writes("hvac_mode")
            .sentence("set {device} to {value}")
            .param(
                Param::enum_("value")
                    .labelled_options(kind.modes().iter().map(|m| (*m, mode_label(m))))
                    .label("Mode")
                    .required(),
            ),
        Action::new("hold")
            .label("Hold a different target for a while")
            .description("Then it goes back to its usual target by itself.")
            .category("Thermostat")
            .sentence("hold {device} at {setpoint} for {for_secs}")
            .param(target_param("setpoint"))
            .param(
                Param::duration("for_secs")
                    .label("For")
                    .unit("s")
                    .range(60.0, MAX_HOLD_SECS as f64)
                    .default(7200)
                    .required(),
            ),
        Action::new("resume")
            .label("Back to the usual target")
            .description("Ends a hold, or a hand-flip override, now.")
            .category("Thermostat")
            .sentence("put {device} back to its usual target"),
        Action::new("start_now")
            .label("Start now anyway")
            .description("Skips the short-cycle guard once.")
            .category("Thermostat")
            .sentence("start {device} now, past its short-cycle guard"),
        Action::new("resync")
            .label("Send the switch what it should be")
            .description("Sends on or off again now, even if it seems so already.")
            .category("Thermostat")
            .sentence("send {device}'s switch what it should be"),
        Action::new("set_hysteresis")
            .label("Set the swing")
            .category("Tuning")
            .writes("hysteresis")
            .sentence("set {device}'s swing to {value} °C")
            .param(
                Param::float("value")
                    .label("Swing (°C difference)")
                    .range(0.0, MAX_HYSTERESIS)
                    .step(0.1)
                    .required(),
            ),
        Action::new("set_aggregation")
            .label("Set how several sensors combine")
            .category("Tuning")
            .writes("aggregation")
            .sentence("make {device} combine its sensors by {value}")
            .param(
                Param::enum_("value")
                    .labelled_options([
                        ("average", "Average"),
                        ("min", "Coldest"),
                        ("max", "Warmest"),
                    ])
                    .label("Combine by")
                    .required(),
            ),
        Action::new("set_short_cycle")
            .label("Set the short-cycle guards")
            .category("Tuning")
            .sentence("set {device}'s short-cycle guards")
            .param(
                Param::duration("min_on_secs")
                    .label("Once on, stays on at least")
                    .unit("s")
                    .range(0.0, MAX_GUARD_SECS as f64),
            )
            .param(
                Param::duration("min_off_secs")
                    .label("Once off, stays off at least")
                    .unit("s")
                    .range(0.0, MAX_GUARD_SECS as f64),
            ),
        Action::new("set_sensors")
            .label("Choose the sensors")
            .category("Setup")
            .writes("sensor_ids")
            .sentence("make {device} read {sensor_ids}")
            .param(
                Param::string("sensor_ids")
                    .label("Sensor device ids, comma-separated")
                    .required(),
            ),
        Action::new("set_actuator")
            .label("Choose the switch")
            .description(
                "A plug or switch — a device with an on/off that isn't a lock or a \
                 thermostat. The old one is turned off first if it was running.",
            )
            .category("Setup")
            .writes("actuator_device_id")
            .sentence("make {device} switch {device_id}")
            .param(
                Param::device_ref("device_id")
                    .label("Plug or switch")
                    .required(),
            ),
        Action::new("recalculate")
            .label("Re-evaluate now")
            .category("Thermostat")
            .sentence("re-evaluate {device}"),
    ]
}

/// A thermostat's schema: its modes are its kind's.
#[cfg(test)]
pub fn device_schema(kind: Kind) -> DeviceSchema {
    serde_json::from_value(device_schema_json(kind)).expect("schema round-trips")
}

pub fn device_schema_json(kind: Kind) -> Value {
    with_actions(
        &DeviceSchema {
            attributes: attributes(kind),
            ..Default::default()
        },
        actions(kind),
    )
}

/// Publish the retained schema for a thermostat device.
pub async fn publish(
    publisher: &DevicePublisher,
    device_id: &str,
    kind: Kind,
) -> anyhow::Result<()> {
    publisher
        .register_device_schema_json(device_id, &device_schema_json(kind))
        .await
}

/// Every command the controller takes by `action`.
#[cfg(test)]
pub const HANDLED: &[&str] = &[
    "set_setpoint",
    "set_mode",
    "hold",
    "resume",
    "start_now",
    "resync",
    "set_hysteresis",
    "set_aggregation",
    "set_short_cycle",
    "set_sensors",
    "set_actuator",
    "recalculate",
];

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 3] = [Kind::Ac, Kind::Heater, Kind::Fan];

    #[test]
    fn the_schema_is_conformant() {
        for k in KINDS {
            plugin_sdk_rs::conformance::check_device_schema(&device_schema(k)).assert_ok();
        }
    }

    /// Only what the controller takes as an attribute write is writable.
    #[test]
    fn only_the_setpoint_and_mode_are_writable() {
        for (name, attr) in &device_schema(Kind::Ac).attributes {
            let expected = matches!(name.as_str(), "setpoint" | "hvac_mode");
            assert_eq!(attr.writable, expected, "{name}");
        }
    }

    /// H8: an AC offers Cool and Off; a heater Heat and Off.
    #[test]
    fn the_modes_are_the_kinds() {
        let modes = |k: Kind| -> Vec<String> {
            device_schema(k).attributes["hvac_mode"]
                .options
                .clone()
                .unwrap()
                .into_iter()
                .map(|o| o.value)
                .collect()
        };
        assert_eq!(modes(Kind::Ac), ["cool", "off"]);
        assert_eq!(modes(Kind::Fan), ["cool", "off"]);
        assert_eq!(modes(Kind::Heater), ["heat", "off"]);
    }

    #[test]
    fn temperatures_are_celsius_and_the_swing_is_not_one() {
        let a = device_schema(Kind::Ac).attributes;
        for name in ["temperature", "setpoint", "hold_setpoint"] {
            assert_eq!(a[name].unit.as_deref(), Some("°C"), "{name}");
        }
        assert_eq!(a["hysteresis"].unit, None);
    }

    /// The schema must not drift from what the device publishes.
    #[test]
    fn the_declared_attributes_are_exactly_what_is_published() {
        let published = crate::controller::sample_state_payload();
        let published: std::collections::HashSet<&str> = published
            .as_object()
            .expect("state payload is an object")
            .keys()
            .map(|s| s.as_str())
            .collect();
        let declared = device_schema(Kind::Ac).attributes;
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

    /// Every declared action is one the controller handles, and back.
    #[test]
    fn every_action_is_a_command_the_controller_takes() {
        let declared: Vec<String> = device_schema(Kind::Ac)
            .actions
            .into_iter()
            .map(|a| a.id)
            .collect();
        for a in &declared {
            assert!(HANDLED.contains(&a.as_str()), "{a} declared, not handled");
        }
        for h in HANDLED {
            assert!(declared.iter().any(|a| a == h), "{h} handled, not declared");
        }
    }

    /// Every `reason` the controller can give is named.
    #[test]
    fn every_reason_has_words() {
        let src = include_str!("controller.rs");
        for (code, _) in REASONS {
            assert!(src.contains(&format!("\"{code}\"")), "{code}");
        }
    }
}
