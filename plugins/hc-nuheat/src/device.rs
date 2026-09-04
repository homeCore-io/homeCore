//! One NuHeat thermostat, as homeCore sees it.
//!
//! Three jobs, all pure so they can be tested without a broker or an account:
//! the device id, the state payload, and turning an inbound command into an
//! API call.
//!
//! ## The state contract
//!
//! homeCore never writes device state — the plugin does, and it publishes what
//! *actually happened* rather than what was asked for. So a command here
//! produces an [`Intent`], the caller performs it, and the state that follows
//! comes from the thermostat's own next reading. A hold the API rejects leaves
//! the published setpoint where it was, which is the behaviour that lets a UI
//! be trusted.
//!
//! ## Attribute names
//!
//! `current_temperature` and `setpoint` match hc-thermostat, so a rule or a
//! dashboard written against one works against the other. The `thermostat`
//! dashboard widget in `hc_types::dashboard_vocabulary` takes `attribute` and
//! `target` names precisely so it can bind either spelling, but matching the
//! existing plugin costs nothing and means the widget's defaults land.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeSchema, BoolStates, DeviceAction, DeviceSchema,
    ParamKind, ParamSpec, StateLabel,
};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::api::{Mode, Thermostat, MAX_HOLD_HOURS};
use crate::units;

/// homeCore's device id for a thermostat serial.
///
/// The serial is NuHeat's own durable identifier and survives a rename in
/// their app, which is exactly what a device id has to do — rules and history
/// are keyed to it.
pub fn device_id(serial: &str) -> String {
    format!("nuheat_{}", serial.trim().to_lowercase())
}

/// What a command turned out to mean.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Resume the schedule.
    Auto,
    /// Hold `celsius` until `hold_until`, or until the next scheduled event
    /// when that is `None`.
    Hold {
        celsius: f64,
        hold_until: Option<String>,
    },
    /// Hold `celsius` indefinitely.
    PermanentHold { celsius: f64 },
}

/// Why a command could not be understood. Worth distinguishing from "it
/// failed": the operator's fix is different, and so is the log line.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    /// Nothing in the payload this plugin acts on.
    NotUnderstood,
    /// A hold was asked for with no temperature and none is known.
    HoldWithoutTemperature,
    /// A hold longer than NuHeat allows.
    HoldTooLong { hours: i64 },
    /// A mode name that is not one of the three.
    UnknownMode(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUnderstood => write!(
                f,
                "no setpoint or mode in the command; expected one of \
                 {{\"setpoint\": 21.5}}, {{\"mode\": \"auto\"}}, or an action"
            ),
            Self::HoldWithoutTemperature => write!(
                f,
                "a hold needs a temperature, and none is configured or currently known"
            ),
            Self::HoldTooLong { hours } => write!(
                f,
                "NuHeat allows a hold of at most {MAX_HOLD_HOURS} hours; {hours} was asked for"
            ),
            Self::UnknownMode(m) => {
                write!(
                    f,
                    "unknown mode {m:?}; expected auto, hold or permanent_hold"
                )
            }
        }
    }
}

/// Everything about how this plugin was told to behave that affects reading a
/// bare `{"setpoint": …}`.
#[derive(Debug, Clone, Copy)]
pub struct CommandContext {
    /// What a setpoint with no mode means: a temporary hold, or a permanent
    /// one. Operator preference — some people want the schedule to reassert
    /// itself, some do not.
    pub bare_setpoint_is_permanent: bool,
    /// How long a temporary hold lasts when nothing says otherwise. `None`
    /// leaves it to NuHeat, which resumes at the next scheduled event.
    pub default_hold_hours: Option<i64>,
    /// The thermostat's current setpoint in °C, for a hold that names no
    /// temperature of its own.
    pub current_setpoint_c: Option<f64>,
}

/// Read a command payload as an [`Intent`].
///
/// Accepts both shapes homeCore commands arrive in: attribute writes
/// (`{"setpoint": 21.5}`, `{"mode": "auto"}`) because the schema declares
/// those attributes writable, and action calls (`{"action": "hold_temperature",
/// "temperature": 21.5, "hours": 3}`) because the schema declares those
/// actions. A plugin that declares both and honours only one produces controls
/// that look live and do nothing.
pub fn interpret(
    payload: &Value,
    ctx: CommandContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Intent, Rejection> {
    // ── Action style ────────────────────────────────────────────────────
    if let Some(action) = payload.get("action").and_then(Value::as_str) {
        return match action {
            "resume_schedule" => Ok(Intent::Auto),
            "hold_temperature" => {
                let celsius = read_temperature(payload, &["temperature", "setpoint"])
                    .or(ctx.current_setpoint_c)
                    .ok_or(Rejection::HoldWithoutTemperature)?;
                let hours = payload
                    .get("hours")
                    .and_then(Value::as_i64)
                    .or(ctx.default_hold_hours);
                let hold_until = hold_deadline(hours, now)?;
                Ok(Intent::Hold {
                    celsius,
                    hold_until,
                })
            }
            "set_permanent_hold" => {
                let celsius = read_temperature(payload, &["temperature", "setpoint"])
                    .or(ctx.current_setpoint_c)
                    .ok_or(Rejection::HoldWithoutTemperature)?;
                Ok(Intent::PermanentHold { celsius })
            }
            other => Err(Rejection::UnknownMode(other.to_string())),
        };
    }

    // ── Attribute style ─────────────────────────────────────────────────
    let requested_mode = payload
        .get("mode")
        .and_then(Value::as_str)
        .map(|m| Mode::parse(m).ok_or_else(|| Rejection::UnknownMode(m.to_string())))
        .transpose()?;

    let celsius = read_temperature(payload, &["setpoint", "target_temperature", "temperature"]);

    match (requested_mode, celsius) {
        (Some(Mode::Auto), _) => Ok(Intent::Auto),

        (Some(Mode::Hold), c) => {
            let celsius = c
                .or(ctx.current_setpoint_c)
                .ok_or(Rejection::HoldWithoutTemperature)?;
            let hold_until = hold_deadline(
                payload
                    .get("hours")
                    .and_then(Value::as_i64)
                    .or(ctx.default_hold_hours),
                now,
            )?;
            // An explicit `hold_until` wins over an hour count: a rule that
            // computes a wake-up time knows something the default does not.
            let hold_until = payload
                .get("hold_until")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(hold_until);
            Ok(Intent::Hold {
                celsius,
                hold_until,
            })
        }

        (Some(Mode::PermanentHold), c) => {
            let celsius = c
                .or(ctx.current_setpoint_c)
                .ok_or(Rejection::HoldWithoutTemperature)?;
            Ok(Intent::PermanentHold { celsius })
        }

        // A bare setpoint. Which hold it becomes is the operator's setting —
        // see `bare_setpoint_is_permanent`.
        (None, Some(celsius)) => {
            if ctx.bare_setpoint_is_permanent {
                Ok(Intent::PermanentHold { celsius })
            } else {
                Ok(Intent::Hold {
                    celsius,
                    hold_until: hold_deadline(ctx.default_hold_hours, now)?,
                })
            }
        }

        (None, None) => Err(Rejection::NotUnderstood),
    }
}

/// An hour count → the RFC-3339 instant NuHeat wants, rejecting one it will not
/// accept.
///
/// Refusing locally rather than letting the API refuse is deliberate: their
/// rejection is a bare 400, and a rule that quietly stops working every time it
/// asks for a 24-hour hold is a bad afternoon.
fn hold_deadline(
    hours: Option<i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>, Rejection> {
    let Some(hours) = hours else {
        return Ok(None);
    };
    if hours <= 0 || hours > MAX_HOLD_HOURS {
        return Err(Rejection::HoldTooLong { hours });
    }
    Ok(Some(
        (now + chrono::Duration::hours(hours))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    ))
}

/// First of `keys` that holds a number.
fn read_temperature(payload: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_f64))
}

/// The state payload published on `homecore/devices/{id}/state`.
///
/// `decoded` carries the outcome of the range check in [`units`]: a reading
/// that fails it is omitted rather than published wrong, and the caller raises
/// a notice about it.
pub fn state_payload(t: &Thermostat) -> (Value, ScaleCheck) {
    let mut check = ScaleCheck::Ok;

    let mut decode = |wire: Option<i64>| -> Option<f64> {
        let wire = wire?;
        match units::decode_celsius(wire) {
            Some(c) => Some(c),
            None => {
                check = ScaleCheck::Implausible(wire);
                None
            }
        }
    };

    let current = decode(t.current_temperature);
    let setpoint = decode(t.set_point_temperature);
    let mode = t.mode.and_then(Mode::from_wire);

    let payload = json!({
        "current_temperature": current,
        "setpoint": setpoint,
        // Both scales published, because a floor thermostat is one of the few
        // devices people quote in Fahrenheit to each other even where the rest
        // of the house is metric. Derived, never a second source of truth.
        "current_temperature_f": current.map(units::c_to_f).map(round_tenth),
        "setpoint_f": setpoint.map(units::c_to_f).map(round_tenth),
        "mode": mode.map(Mode::as_str),
        "heating": t.is_heating,
        "online": t.online,
        "hold_until": t.hold_until,
        "error_state": t.error_state,
        "serial_number": t.serial_number,
        "last_update": chrono::Utc::now().to_rfc3339(),
    });
    (payload, check)
}

fn round_tenth(f: f64) -> f64 {
    (f * 10.0).round() / 10.0
}

/// Whether a reading survived the unit-scale range check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScaleCheck {
    Ok,
    /// The wire value that decoded to something no floor reaches.
    Implausible(i64),
}

/// What this device is, for clients that render controls from the schema.
///
/// `setpoint` and `mode` are writable and [`interpret`] honours both, which is
/// the promise `writable` makes. The rest are read-only: they are the
/// thermostat reporting, not settings.
pub fn device_schema(limits: units::SetpointLimits) -> DeviceSchema {
    let mut attributes = HashMap::new();

    attributes.insert(
        "current_temperature".into(),
        AttributeSchema {
            display_name: Some("Floor temperature".into()),
            unit: Some("°C".into()),
            ..AttributeSchema::read_only(AttributeKind::Float)
        },
    );

    attributes.insert(
        "setpoint".into(),
        AttributeSchema {
            kind: AttributeKind::Float,
            writable: true,
            display_name: Some("Target".into()),
            unit: Some("°C".into()),
            min: Some(limits.min_c),
            max: Some(limits.max_c),
            step: Some(0.5),
            ..Default::default()
        },
    );

    attributes.insert(
        "mode".into(),
        AttributeSchema {
            kind: AttributeKind::Enum,
            writable: true,
            display_name: Some("Mode".into()),
            options: Some(vec!["auto".into(), "hold".into(), "permanent_hold".into()]),
            ..Default::default()
        },
    );

    attributes.insert(
        "heating".into(),
        AttributeSchema {
            display_name: Some("Heating".into()),
            // A boolean is two events. Without both named, "stops heating"
            // becomes "heating, but Not" in a rule sentence.
            states: Some(BoolStates {
                when_true: StateLabel::verbed("heating", "starts heating"),
                when_false: StateLabel::verbed("idle", "stops heating"),
            }),
            ..AttributeSchema::read_only(AttributeKind::Bool)
        },
    );

    attributes.insert(
        "hold_until".into(),
        AttributeSchema {
            display_name: Some("Hold ends".into()),
            ..AttributeSchema::read_only(AttributeKind::String)
        },
    );

    // Diagnostics: real, worth having, and not what someone opened the card
    // for. `category` is what keeps them from crowding out the temperature.
    attributes.insert(
        "online".into(),
        AttributeSchema {
            display_name: Some("Online".into()),
            category: Some(AttributeCategory::Diagnostic),
            states: Some(BoolStates {
                when_true: StateLabel::new("online"),
                when_false: StateLabel::new("offline"),
            }),
            ..AttributeSchema::read_only(AttributeKind::Bool)
        },
    );
    attributes.insert(
        "error_state".into(),
        AttributeSchema {
            display_name: Some("Fault".into()),
            category: Some(AttributeCategory::Diagnostic),
            ..AttributeSchema::read_only(AttributeKind::String)
        },
    );
    attributes.insert(
        "serial_number".into(),
        AttributeSchema {
            display_name: Some("Serial number".into()),
            category: Some(AttributeCategory::Diagnostic),
            ..AttributeSchema::read_only(AttributeKind::String)
        },
    );

    DeviceSchema {
        attributes,
        actions: vec![
            DeviceAction {
                id: "resume_schedule".into(),
                label: "Resume the schedule".into(),
                description: Some(
                    "Cancel any hold and let the thermostat's own schedule take over.".into(),
                ),
                category: Some("Mode".into()),
                icon: Some("schedule".into()),
                params: vec![],
                writes: Some("mode".into()),
                sentence: Some("resume the schedule on {device}".into()),
                confirm: None,
                requires_role: Default::default(),
            },
            DeviceAction {
                id: "hold_temperature".into(),
                label: "Hold a temperature".into(),
                description: Some(
                    "Hold a target temperature for a while, then go back to the schedule.".into(),
                ),
                category: Some("Mode".into()),
                icon: Some("thermostat".into()),
                params: vec![
                    ParamSpec {
                        name: "temperature".into(),
                        kind: ParamKind::Float,
                        label: Some("Temperature".into()),
                        required: true,
                        default: None,
                        unit: Some("°C".into()),
                        min: Some(limits.min_c),
                        max: Some(limits.max_c),
                        step: Some(0.5),
                        options: None,
                        options_from: None,
                    },
                    ParamSpec {
                        name: "hours".into(),
                        kind: ParamKind::Int,
                        label: Some("For".into()),
                        required: false,
                        default: None,
                        unit: Some("hours".into()),
                        min: Some(1.0),
                        // NuHeat's own cap, so the control cannot offer a hold
                        // the API will reject.
                        max: Some(MAX_HOLD_HOURS as f64),
                        step: Some(1.0),
                        options: None,
                        options_from: None,
                    },
                ],
                writes: Some("setpoint".into()),
                sentence: Some("hold {device} at {temperature} for {hours} hours".into()),
                confirm: None,
                requires_role: Default::default(),
            },
            DeviceAction {
                id: "set_permanent_hold".into(),
                label: "Hold indefinitely".into(),
                description: Some(
                    "Hold a target temperature until something changes it. The schedule stays off."
                        .into(),
                ),
                category: Some("Mode".into()),
                icon: Some("lock".into()),
                params: vec![ParamSpec {
                    name: "temperature".into(),
                    kind: ParamKind::Float,
                    label: Some("Temperature".into()),
                    required: true,
                    default: None,
                    unit: Some("°C".into()),
                    min: Some(limits.min_c),
                    max: Some(limits.max_c),
                    step: Some(0.5),
                    options: None,
                    options_from: None,
                }],
                writes: Some("setpoint".into()),
                sentence: Some("hold {device} at {temperature} indefinitely".into()),
                confirm: None,
                requires_role: Default::default(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CommandContext {
        CommandContext {
            bare_setpoint_is_permanent: false,
            default_hold_hours: None,
            current_setpoint_c: Some(21.0),
        }
    }

    fn at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-04T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn device_ids_are_stable_and_lowercased() {
        assert_eq!(device_id("ABC123"), "nuheat_abc123");
        assert_eq!(device_id(" abc123 "), "nuheat_abc123");
    }

    #[test]
    fn a_bare_setpoint_becomes_a_temporary_hold_by_default() {
        let intent = interpret(&json!({"setpoint": 22.5}), ctx(), at()).expect("understood");
        assert_eq!(
            intent,
            Intent::Hold {
                celsius: 22.5,
                hold_until: None
            }
        );
    }

    #[test]
    fn a_bare_setpoint_can_be_configured_to_hold_permanently() {
        let mut c = ctx();
        c.bare_setpoint_is_permanent = true;
        let intent = interpret(&json!({"setpoint": 22.5}), c, at()).expect("understood");
        assert_eq!(intent, Intent::PermanentHold { celsius: 22.5 });
    }

    #[test]
    fn the_widget_and_rule_spellings_of_a_setpoint_all_work() {
        for key in ["setpoint", "target_temperature", "temperature"] {
            let payload = json!({ key: 20.0 });
            assert!(
                matches!(
                    interpret(&payload, ctx(), at()),
                    Ok(Intent::Hold { celsius, .. }) if celsius == 20.0
                ),
                "{key} was not understood"
            );
        }
    }

    #[test]
    fn resuming_the_schedule_works_as_a_mode_or_an_action() {
        assert_eq!(
            interpret(&json!({"mode": "auto"}), ctx(), at()),
            Ok(Intent::Auto)
        );
        assert_eq!(
            interpret(&json!({"action": "resume_schedule"}), ctx(), at()),
            Ok(Intent::Auto)
        );
    }

    #[test]
    fn a_hold_with_hours_gets_a_deadline_the_api_accepts() {
        let intent = interpret(
            &json!({"action": "hold_temperature", "temperature": 24.0, "hours": 3}),
            ctx(),
            at(),
        )
        .expect("understood");
        assert_eq!(
            intent,
            Intent::Hold {
                celsius: 24.0,
                hold_until: Some("2026-09-04T15:00:00Z".into())
            }
        );
    }

    /// NuHeat caps a hold at 23 hours. Catching it here turns an opaque 400
    /// into a message naming the limit.
    #[test]
    fn a_hold_longer_than_nuheat_allows_is_refused_before_it_is_sent() {
        let err = interpret(
            &json!({"action": "hold_temperature", "temperature": 24.0, "hours": 24}),
            ctx(),
            at(),
        )
        .expect_err("too long");
        assert_eq!(err, Rejection::HoldTooLong { hours: 24 });
        assert!(err.to_string().contains("23"), "{err}");
    }

    #[test]
    fn an_explicit_deadline_beats_the_configured_default() {
        let mut c = ctx();
        c.default_hold_hours = Some(2);
        let intent = interpret(
            &json!({"mode": "hold", "setpoint": 23.0, "hold_until": "2026-09-05T06:00:00Z"}),
            c,
            at(),
        )
        .expect("understood");
        assert_eq!(
            intent,
            Intent::Hold {
                celsius: 23.0,
                hold_until: Some("2026-09-05T06:00:00Z".into())
            }
        );
    }

    #[test]
    fn a_hold_with_no_temperature_falls_back_to_the_current_one() {
        let intent = interpret(&json!({"mode": "hold"}), ctx(), at()).expect("understood");
        assert_eq!(
            intent,
            Intent::Hold {
                celsius: 21.0,
                hold_until: None
            }
        );
    }

    #[test]
    fn a_hold_with_no_temperature_and_nothing_known_says_why() {
        let mut c = ctx();
        c.current_setpoint_c = None;
        assert_eq!(
            interpret(&json!({"mode": "hold"}), c, at()),
            Err(Rejection::HoldWithoutTemperature)
        );
    }

    #[test]
    fn an_empty_or_unknown_command_is_refused_with_a_usable_message() {
        let err = interpret(&json!({}), ctx(), at()).expect_err("nothing to do");
        assert_eq!(err, Rejection::NotUnderstood);
        assert!(err.to_string().contains("setpoint"), "{err}");

        assert_eq!(
            interpret(&json!({"mode": "eco"}), ctx(), at()),
            Err(Rejection::UnknownMode("eco".into()))
        );
    }

    #[test]
    fn the_state_payload_carries_both_scales_and_the_mode_name() {
        let t = Thermostat {
            serial_number: "12345678".into(),
            current_temperature: Some(2224),
            set_point_temperature: Some(2500),
            online: true,
            is_heating: true,
            mode: Some(2),
            ..Default::default()
        };
        let (state, check) = state_payload(&t);
        assert_eq!(check, ScaleCheck::Ok);
        assert_eq!(state["current_temperature"], json!(22.24));
        assert_eq!(state["current_temperature_f"], json!(72.0));
        assert_eq!(state["setpoint"], json!(25.0));
        assert_eq!(state["mode"], json!("hold"));
        assert_eq!(state["heating"], json!(true));
    }

    /// The scale tripwire, end to end: an implausible reading is withheld
    /// rather than published, and the caller is told.
    #[test]
    fn an_implausible_reading_is_withheld_rather_than_published() {
        let t = Thermostat {
            serial_number: "1".into(),
            current_temperature: Some(210_000),
            ..Default::default()
        };
        let (state, check) = state_payload(&t);
        assert_eq!(check, ScaleCheck::Implausible(210_000));
        assert_eq!(state["current_temperature"], Value::Null);
    }

    /// A thermostat that has never reported publishes nulls, not zeroes. Zero
    /// is a temperature; absent is not.
    #[test]
    fn a_thermostat_with_no_reading_publishes_null_not_zero() {
        let t = Thermostat {
            serial_number: "1".into(),
            ..Default::default()
        };
        let (state, _) = state_payload(&t);
        assert_eq!(state["current_temperature"], Value::Null);
        assert_eq!(state["setpoint"], Value::Null);
        assert_eq!(state["mode"], Value::Null);
    }

    /// The schema's promise: anything marked writable must be something
    /// `interpret` actually acts on, or clients render dead controls.
    #[test]
    fn every_writable_attribute_is_one_that_commands_accept() {
        let schema = device_schema(units::SetpointLimits::default());
        let writable: Vec<&String> = schema
            .attributes
            .iter()
            .filter(|(_, a)| a.writable)
            .map(|(name, _)| name)
            .collect();
        assert_eq!(writable.len(), 2, "{writable:?}");

        for name in writable {
            let payload = match name.as_str() {
                "setpoint" => json!({ "setpoint": 21.0 }),
                "mode" => json!({ "mode": "auto" }),
                other => panic!("{other} is writable but untested here"),
            };
            assert!(
                interpret(&payload, ctx(), at()).is_ok(),
                "{name} is declared writable but commands reject it"
            );
        }
    }

    /// The same promise for actions: a declared action a command handler does
    /// not know is a button that does nothing.
    #[test]
    fn every_declared_action_is_one_that_commands_accept() {
        for action in device_schema(units::SetpointLimits::default()).actions {
            let mut payload = json!({ "action": action.id });
            // Supply required params so the test exercises dispatch rather
            // than validation.
            for p in &action.params {
                if p.required {
                    payload[&p.name] = json!(21.0);
                }
            }
            assert!(
                interpret(&payload, ctx(), at()).is_ok(),
                "action {:?} is declared but not handled",
                action.id
            );
        }
    }

    /// Actions that supersede an attribute have to say so, or a client offers
    /// both a slider and a button for the same thing.
    #[test]
    fn actions_declare_the_attribute_they_supersede() {
        for action in device_schema(units::SetpointLimits::default()).actions {
            assert!(
                action.writes.is_some(),
                "action {:?} claims no attribute",
                action.id
            );
        }
    }
}
