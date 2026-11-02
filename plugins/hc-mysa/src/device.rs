//! One Mysa thermostat, as homeCore sees it.
//!
//! Three jobs, all pure so they can be tested without a broker or an account:
//! the device id, the state payload and schema (both read from what Mysa says
//! the hardware can do), and turning a command into Mysa writes.
//!
//! ## The state contract
//!
//! The plugin publishes what the thermostat reports, never what it was asked
//! for. A command becomes a list of [`Write`]s; the caller sends them, waits,
//! reads the thermostat back and publishes that.
//!
//! ## Names
//!
//! The shared names and units (`hc_types::device_vocabulary`), so the app's
//! thermostat card draws it with no plugin code: `temperature`, `humidity`,
//! `heat_setpoint` and `cool_setpoint` in °C, `hvac_mode`, `fan_mode`. Mysa
//! reports neither whether it is heating right now nor whether it is holding,
//! so `hvac_action` and `schedule_mode` are not published rather than
//! guessed.
//!
//! ## Only what the hardware can do
//!
//! Every writable attribute and action comes from `GET /v1/capabilities`: a
//! baseboard heater gets a heat setpoint and no fan; a mini-split gets heat,
//! cool, auto, fan and swing. Every number sent is first moved onto the values
//! Mysa lists ([`units`]), because Mysa refuses anything between them.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, DeviceAction, DeviceSchema,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};

use crate::api::{AutoTarget, Capabilities, ClimateMode, State};
use crate::units;

/// homeCore's device id for a Mysa device id.
///
/// Mysa's id is stable and survives a rename in the Mysa app, which is what
/// rules and history need.
pub fn device_id(mysa_id: &str) -> String {
    let clean: String = mysa_id
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("mysa_{clean}")
}

/// Mysa's mode name ⇄ the shared one.
const MODES: &[(&str, &str, &str)] = &[
    ("off", "off", "Off"),
    ("heat", "heat", "Heat"),
    ("cool", "cool", "Cool"),
    ("auto", "auto", "Auto"),
    ("fanOnly", "fan_only", "Fan only"),
    ("dry", "dry", "Dry"),
    ("emergencyHeat", "emergency_heat", "Emergency heat"),
];

pub fn shared_mode(mysa: &str) -> Option<&'static str> {
    MODES
        .iter()
        .find(|(m, _, _)| *m == mysa)
        .map(|(_, s, _)| *s)
}

pub fn mysa_mode(shared: &str) -> Option<&'static str> {
    MODES
        .iter()
        .find(|(m, s, _)| *s == shared || *m == shared)
        .map(|(m, _, _)| *m)
}

fn mode_label(mysa: &str) -> &'static str {
    MODES
        .iter()
        .find(|(m, _, _)| *m == mysa)
        .map(|(_, _, l)| *l)
        .unwrap_or("Other")
}

/// One call to Mysa.
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    Mode(&'static str),
    Heat(f64),
    Cool(f64),
    Auto(AutoTarget),
    Fan {
        mode: String,
        speed: Option<i64>,
    },
    Lockout(i64),
    Brightness {
        active: Option<i64>,
        idle: Option<i64>,
    },
    Swing {
        axis: &'static str,
        position: i64,
    },
    Hold,
    Resume,
}

/// Why a command was refused before anything was sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    Unrecognised,
    /// Mysa describes nothing for this thermostat yet, and refuses every write.
    Undescribed,
    /// The thermostat has no such control, or nothing it can be set to.
    Unsupported(String),
    /// A value off the thermostat's own list, for one that can't be moved onto it.
    Disallowed(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecognised => write!(f, "nothing in the command this thermostat acts on"),
            Self::Undescribed => write!(
                f,
                "Mysa doesn't describe this thermostat's controls yet, and refuses every change"
            ),
            Self::Unsupported(what) => write!(f, "this thermostat has no {what} control"),
            Self::Disallowed(what) => write!(f, "{what} isn't one this thermostat accepts"),
        }
    }
}

/// The lists one setpoint may take.
fn list(c: &Option<crate::api::Choice<f64>>) -> Vec<f64> {
    c.as_ref()
        .map(|c| c.settable().to_vec())
        .unwrap_or_default()
}

/// How auto is driven: one setpoint, or a heat and cool band.
fn auto_is_single(auto: &ClimateMode) -> bool {
    auto.min_setpoint_delta == Some(0.0) && list(&auto.heat_setpoint) == list(&auto.cool_setpoint)
}

/// What the heat and cool targets may be set to, from whichever modes have them.
struct Targets {
    heat: Vec<f64>,
    cool: Vec<f64>,
}

fn targets(caps: &Capabilities) -> Targets {
    let c = &caps.climate;
    let mut heat = c
        .heat
        .as_ref()
        .map(|m| list(&m.setpoint))
        .unwrap_or_default();
    let mut cool = c
        .cool
        .as_ref()
        .map(|m| list(&m.setpoint))
        .unwrap_or_default();
    if let Some(auto) = &c.auto {
        if heat.is_empty() {
            heat = list(&auto.heat_setpoint);
        }
        if cool.is_empty() {
            cool = list(&auto.cool_setpoint);
        }
    }
    Targets { heat, cool }
}

/// Every fan mode and speed any of the thermostat's modes offer.
fn fan_choices(caps: &Capabilities) -> (Vec<String>, Vec<i64>) {
    let mut modes = BTreeSet::new();
    let mut speeds = BTreeSet::new();
    for (_, m) in caps.climate.modes() {
        if let Some(fan) = &m.fan {
            modes.extend(fan.mode.settable().iter().cloned());
            speeds.extend(fan.speed.settable().iter().copied());
        }
    }
    // Mysa refuses fan "off" outright: it isn't a fan setting on this API.
    modes.remove("off");
    (modes.into_iter().collect(), speeds.into_iter().collect())
}

fn swing_choices(caps: &Capabilities, axis: &str) -> Vec<i64> {
    caps.climate
        .swing
        .as_ref()
        .and_then(|s| match axis {
            "vertical" => s.vertical.as_ref(),
            _ => s.horizontal.as_ref(),
        })
        .map(|a| a.state.settable().to_vec())
        .unwrap_or_default()
}

fn lockout_choices(caps: &Capabilities) -> Vec<i64> {
    caps.display
        .lockout
        .as_ref()
        .map(|c| c.settable().to_vec())
        .unwrap_or_default()
}

fn brightness_settable(caps: &Capabilities) -> bool {
    caps.display
        .brightness
        .as_ref()
        .is_some_and(|b| b.user_controllable)
}

/// The state payload published on `homecore/devices/{id}/state`.
pub fn state_payload(state: &State, caps: &Capabilities, model: Option<&str>) -> Value {
    let mut out = Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            out.insert(k.into(), v);
        }
    };
    put("temperature", json!(state.current_temperature));
    put("humidity", json!(state.humidity));
    let mode = state.mode.as_deref();
    put("hvac_mode", json!(mode.and_then(shared_mode)));

    if let Some(sp) = &state.setpoints {
        let auto = caps.climate.auto.as_ref();
        match (mode, auto) {
            // In auto the targets are auto's own.
            (Some("auto"), Some(a)) if auto_is_single(a) => {
                put("heat_setpoint", json!(sp.auto));
                put("cool_setpoint", json!(sp.auto));
            }
            (Some("auto"), _) => {
                put("heat_setpoint", json!(sp.auto_heat.or(sp.heat)));
                put("cool_setpoint", json!(sp.auto_cool.or(sp.cool)));
            }
            _ => {
                put("heat_setpoint", json!(sp.heat));
                put("cool_setpoint", json!(sp.cool));
            }
        }
    }
    if let Some(fan) = &state.fan {
        put("fan_mode", json!(fan.mode));
        put("fan_speed", json!(fan.speed));
    }
    if let Some(sw) = &state.swing {
        put("swing_vertical", json!(sw.vertical));
        put("swing_horizontal", json!(sw.horizontal));
    }
    put(
        "keypad_lockout",
        json!(state.lockout.map(|l| l.to_string())),
    );
    if let Some(b) = &state.brightness {
        put("display_brightness", json!(b.active_intensity));
        put("display_idle_brightness", json!(b.idle_intensity));
    }
    put("model", json!(model.or(caps.model.as_deref())));
    Value::Object(out)
}

fn float_target(label: &str, allowed: &[f64]) -> AttributeSchema {
    let (min, max, step) = units::range_of(allowed).unwrap_or((5.0, 30.0, 0.5));
    AttributeSchema {
        kind: AttributeKind::Float,
        writable: true,
        display_name: Some(label.into()),
        unit: Some("°C".into()),
        min: Some(min),
        max: Some(max),
        step: Some(step),
        ..Default::default()
    }
}

fn int_choice(label: &str, allowed: &[i64]) -> AttributeSchema {
    AttributeSchema {
        kind: AttributeKind::Integer,
        writable: true,
        display_name: Some(label.into()),
        min: allowed.iter().min().map(|v| *v as f64),
        max: allowed.iter().max().map(|v| *v as f64),
        step: Some(1.0),
        ..Default::default()
    }
}

/// What this thermostat is, for clients that render controls from the schema.
pub fn device_schema(caps: &Capabilities) -> DeviceSchema {
    let mut attributes = HashMap::new();
    let mut primary = vec!["temperature".to_string()];

    attributes.insert(
        "temperature".into(),
        AttributeSchema {
            display_name: Some("Temperature".into()),
            unit: Some("°C".into()),
            ..AttributeSchema::read_only(AttributeKind::Float)
        },
    );
    if caps.sensors.humidity || !caps.published {
        attributes.insert(
            "humidity".into(),
            AttributeSchema {
                display_name: Some("Humidity".into()),
                unit: Some("%".into()),
                min: Some(0.0),
                max: Some(100.0),
                ..AttributeSchema::read_only(AttributeKind::Float)
            },
        );
    }

    let t = targets(caps);
    if !t.heat.is_empty() {
        attributes.insert("heat_setpoint".into(), float_target("Heat to", &t.heat));
        primary.push("heat_setpoint".into());
    }
    if !t.cool.is_empty() {
        attributes.insert("cool_setpoint".into(), float_target("Cool to", &t.cool));
        primary.push("cool_setpoint".into());
    }

    let modes: Vec<AttributeOption> = caps
        .modes
        .iter()
        .filter_map(|m| shared_mode(m).map(|s| AttributeOption::new(s).labelled(mode_label(m))))
        .collect();
    if !modes.is_empty() {
        attributes.insert(
            "hvac_mode".into(),
            AttributeSchema {
                kind: AttributeKind::Enum,
                // One mode can't be changed to another.
                writable: modes.len() > 1,
                display_name: Some("Mode".into()),
                options: Some(modes),
                ..Default::default()
            },
        );
        primary.push("hvac_mode".into());
    }

    let (fan_modes, fan_speeds) = fan_choices(caps);
    if !fan_modes.is_empty() {
        attributes.insert(
            "fan_mode".into(),
            AttributeSchema {
                kind: AttributeKind::Enum,
                writable: true,
                display_name: Some("Fan".into()),
                options: Some(
                    fan_modes
                        .iter()
                        .map(|m| {
                            let label = match m.as_str() {
                                "auto" => "Auto",
                                "sleep" => "Sleep",
                                "manual" => "Set speed",
                                other => other,
                            };
                            AttributeOption::new(m.clone()).labelled(label)
                        })
                        .collect(),
                ),
                ..Default::default()
            },
        );
    }
    if !fan_speeds.is_empty() {
        attributes.insert("fan_speed".into(), int_choice("Fan speed", &fan_speeds));
    }
    for (axis, attr, label) in [
        ("vertical", "swing_vertical", "Swing, up and down"),
        ("horizontal", "swing_horizontal", "Swing, side to side"),
    ] {
        let choices = swing_choices(caps, axis);
        if !choices.is_empty() {
            attributes.insert(attr.into(), int_choice(label, &choices));
        }
    }

    let lockout = lockout_choices(caps);
    if !lockout.is_empty() {
        attributes.insert(
            "keypad_lockout".into(),
            AttributeSchema {
                kind: AttributeKind::Enum,
                writable: true,
                display_name: Some("Keypad".into()),
                category: Some(AttributeCategory::Config),
                options: Some(
                    lockout
                        .iter()
                        .map(|l| {
                            let label = if *l == 0 {
                                "Unlocked".to_string()
                            } else {
                                format!("Locked, level {l}")
                            };
                            AttributeOption::new(l.to_string()).labelled(label)
                        })
                        .collect(),
                ),
                ..Default::default()
            },
        );
    }
    if brightness_settable(caps) {
        for (attr, label) in [
            ("display_brightness", "Display brightness"),
            ("display_idle_brightness", "Display brightness, idle"),
        ] {
            attributes.insert(
                attr.into(),
                AttributeSchema {
                    kind: AttributeKind::Integer,
                    writable: true,
                    display_name: Some(label.into()),
                    unit: Some("%".into()),
                    min: Some(0.0),
                    max: Some(100.0),
                    step: Some(5.0),
                    category: Some(AttributeCategory::Config),
                    ..Default::default()
                },
            );
        }
    }
    attributes.insert(
        "model".into(),
        AttributeSchema {
            display_name: Some("Model".into()),
            category: Some(AttributeCategory::Diagnostic),
            ..AttributeSchema::read_only(AttributeKind::String)
        },
    );

    let mut actions = Vec::new();
    if caps.features.scheduling {
        // `writes` is left empty on both: Mysa doesn't report whether a
        // thermostat is holding, so there is no attribute to say it changed.
        actions.push(DeviceAction {
            id: "hold".into(),
            label: "Hold until the next change".into(),
            description: Some(
                "Keep what it's doing now until its schedule's next event, then follow the \
                 schedule again."
                    .into(),
            ),
            category: Some("Schedule".into()),
            icon: Some("lock".into()),
            params: vec![],
            writes: None,
            sentence: Some("hold {device} until its schedule's next change".into()),
            confirm: None,
            requires_role: Default::default(),
        });
        actions.push(DeviceAction {
            id: "resume_schedule".into(),
            label: "Follow the schedule".into(),
            description: Some("Cancel any hold and follow its schedule.".into()),
            category: Some("Schedule".into()),
            icon: Some("schedule".into()),
            params: vec![],
            writes: None,
            sentence: Some("have {device} follow its schedule".into()),
            confirm: None,
            requires_role: Default::default(),
        });
    }

    DeviceSchema {
        attributes,
        primary,
        actions,
    }
}

/// What a thermostat is doing, for reading a command against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Now<'a> {
    pub state: Option<&'a State>,
}

impl Now<'_> {
    fn mode(&self) -> Option<&str> {
        self.state?.mode.as_deref()
    }
    fn sp(&self) -> Option<&crate::api::Setpoints> {
        self.state?.setpoints.as_ref()
    }
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn int(v: &Value) -> Option<i64> {
    number(v).map(|f| f.round() as i64)
}

/// Read a command as Mysa writes.
///
/// Accepts attribute writes (`{"heat_setpoint": 21.5}`, `{"hvac_mode": "cool"}`,
/// several at once) and the schema's actions (`{"action": "hold"}`).
pub fn interpret(payload: &Value, caps: &Capabilities, now: Now) -> Result<Vec<Write>, Rejection> {
    if !caps.published {
        return Err(Rejection::Undescribed);
    }
    if let Some(action) = payload.get("action").and_then(Value::as_str) {
        if !caps.features.scheduling {
            return Err(Rejection::Unsupported("schedule".into()));
        }
        return match action {
            "hold" => Ok(vec![Write::Hold]),
            "resume_schedule" => Ok(vec![Write::Resume]),
            other => Err(Rejection::Unsupported(other.to_string())),
        };
    }

    let mut writes = Vec::new();
    let t = targets(caps);

    let asked_mode = match payload.get("hvac_mode").and_then(Value::as_str) {
        Some(m) => {
            let mysa = mysa_mode(m)
                .filter(|mm| caps.modes.iter().any(|x| x == mm))
                .ok_or_else(|| Rejection::Disallowed(format!("mode {m:?}")))?;
            Some(mysa)
        }
        None => None,
    };
    let heat = payload
        .get("heat_setpoint")
        .or_else(|| {
            payload
                .get("setpoint")
                .filter(|_| now.mode() != Some("cool"))
        })
        .and_then(number);
    let cool = payload
        .get("cool_setpoint")
        .or_else(|| {
            payload
                .get("setpoint")
                .filter(|_| now.mode() == Some("cool"))
        })
        .and_then(number);

    let in_auto = asked_mode
        .map(|m| m == "auto")
        .unwrap_or(now.mode() == Some("auto"));
    if (heat.is_some() || cool.is_some()) && in_auto {
        let auto = caps
            .climate
            .auto
            .as_ref()
            .ok_or_else(|| Rejection::Unsupported("auto".into()))?;
        writes.push(Write::Auto(auto_target(auto, heat, cool, now)?));
    } else {
        if let Some(m) = asked_mode {
            // A setpoint write also switches mode; send the mode on its own
            // only when no setpoint says it already.
            let implied = (m == "heat" && heat.is_some()) || (m == "cool" && cool.is_some());
            if !implied {
                writes.push(Write::Mode(m));
            }
        }
        if let Some(c) = heat {
            if t.heat.is_empty() {
                return Err(Rejection::Unsupported("heat".into()));
            }
            let current = now.sp().and_then(|s| s.heat);
            writes.push(Write::Heat(
                units::toward(c, current, &t.heat).expect("list is not empty"),
            ));
        }
        if let Some(c) = cool {
            if t.cool.is_empty() {
                return Err(Rejection::Unsupported("cool".into()));
            }
            let current = now.sp().and_then(|s| s.cool);
            writes.push(Write::Cool(
                units::toward(c, current, &t.cool).expect("list is not empty"),
            ));
        }
    }

    let (fan_modes, fan_speeds) = fan_choices(caps);
    let fan_speed = payload.get("fan_speed").and_then(int);
    if let Some(mode) = payload.get("fan_mode").and_then(Value::as_str) {
        if !fan_modes.iter().any(|m| m == mode) {
            return Err(Rejection::Disallowed(format!("fan {mode:?}")));
        }
        let speed = if mode == "manual" {
            let wanted = fan_speed
                .or_else(|| now.state.and_then(|s| s.fan.as_ref()).and_then(|f| f.speed))
                .or_else(|| fan_speeds.first().copied());
            wanted.and_then(|w| units::nearest_int(w, &fan_speeds))
        } else {
            None
        };
        writes.push(Write::Fan {
            mode: mode.to_string(),
            speed,
        });
    } else if let Some(w) = fan_speed {
        let speed = units::nearest_int(w, &fan_speeds)
            .ok_or_else(|| Rejection::Unsupported("fan speed".into()))?;
        writes.push(Write::Fan {
            mode: "manual".into(),
            speed: Some(speed),
        });
    }

    for (attr, axis) in [
        ("swing_vertical", "vertical"),
        ("swing_horizontal", "horizontal"),
    ] {
        if let Some(w) = payload.get(attr).and_then(int) {
            let position = units::nearest_int(w, &swing_choices(caps, axis))
                .ok_or_else(|| Rejection::Unsupported(format!("{axis} swing")))?;
            writes.push(Write::Swing { axis, position });
        }
    }

    if let Some(v) = payload.get("keypad_lockout") {
        let level = int(v).ok_or_else(|| Rejection::Disallowed(format!("keypad {v}")))?;
        let choices = lockout_choices(caps);
        if choices.is_empty() {
            return Err(Rejection::Unsupported("keypad lock".into()));
        }
        // Never snapped: a lockout level means something exact.
        if !choices.contains(&level) {
            return Err(Rejection::Disallowed(format!("keypad level {level}")));
        }
        writes.push(Write::Lockout(level));
    }

    let active = payload.get("display_brightness").and_then(int);
    let idle = payload.get("display_idle_brightness").and_then(int);
    if active.is_some() || idle.is_some() {
        if !brightness_settable(caps) {
            return Err(Rejection::Unsupported("display brightness".into()));
        }
        writes.push(Write::Brightness {
            active: active.map(|v| v.clamp(0, 100)),
            idle: idle.map(|v| v.clamp(0, 100)),
        });
    }

    if writes.is_empty() {
        return Err(Rejection::Unrecognised);
    }
    Ok(writes)
}

/// Auto's targets for a new heat or cool value, keeping the band at least
/// `minSetpointDelta` wide by moving the other end when it has to.
fn auto_target(
    auto: &ClimateMode,
    heat: Option<f64>,
    cool: Option<f64>,
    now: Now,
) -> Result<AutoTarget, Rejection> {
    let heat_list = list(&auto.heat_setpoint);
    let cool_list = list(&auto.cool_setpoint);
    if heat_list.is_empty() || cool_list.is_empty() {
        return Err(Rejection::Unsupported("auto setpoints".into()));
    }
    let sp = now.sp();
    if auto_is_single(auto) {
        let wanted = heat.or(cool).expect("a value was asked for");
        let current = sp.and_then(|s| s.auto);
        return Ok(AutoTarget::Single(
            units::toward(wanted, current, &heat_list).expect("list is not empty"),
        ));
    }
    let delta = auto.min_setpoint_delta.unwrap_or(0.0);
    let cur_heat = sp.and_then(|s| s.auto_heat.or(s.heat));
    let cur_cool = sp.and_then(|s| s.auto_cool.or(s.cool));
    let mut h = match heat {
        Some(v) => units::toward(v, cur_heat, &heat_list),
        None => cur_heat.and_then(|v| units::nearest(v, &heat_list)),
    }
    .or_else(|| heat_list.first().copied())
    .expect("list is not empty");
    let mut c = match cool {
        Some(v) => units::toward(v, cur_cool, &cool_list),
        None => cur_cool.and_then(|v| units::nearest(v, &cool_list)),
    }
    .or_else(|| cool_list.last().copied())
    .expect("list is not empty");
    if c - h < delta - 1e-9 {
        // Move the end that wasn't asked about.
        if heat.is_some() && cool.is_none() {
            c = units::nearest(h + delta, &cool_list).expect("list is not empty");
            if c - h < delta - 1e-9 {
                c = cool_list
                    .iter()
                    .copied()
                    .filter(|v| *v - h >= delta - 1e-9)
                    .fold(f64::NAN, f64::min);
            }
        } else {
            h = heat_list
                .iter()
                .copied()
                .filter(|v| c - *v >= delta - 1e-9)
                .fold(f64::NAN, f64::max);
        }
        if h.is_nan() || c.is_nan() {
            return Err(Rejection::Disallowed(
                "a band that narrow, at that end of the range".into(),
            ));
        }
    }
    Ok(AutoTarget::Band { heat: h, cool: c })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fixtures;

    fn bb() -> Capabilities {
        serde_json::from_str(&fixtures::baseboard_capabilities()).unwrap()
    }
    fn ac() -> Capabilities {
        serde_json::from_str(&fixtures::ac_capabilities()).unwrap()
    }
    fn bb_state() -> State {
        serde_json::from_str(fixtures::BASEBOARD_STATE).unwrap()
    }
    fn ac_state() -> State {
        serde_json::from_str(fixtures::AC_STATE).unwrap()
    }

    #[test]
    fn device_ids_are_stable_and_safe() {
        assert_eq!(device_id("BB-1"), "mysa_bb_1");
        assert_eq!(device_id(" a:b/c "), "mysa_a_b_c");
    }

    #[test]
    fn modes_map_to_the_shared_names_and_back() {
        assert_eq!(shared_mode("fanOnly"), Some("fan_only"));
        assert_eq!(shared_mode("emergencyHeat"), Some("emergency_heat"));
        assert_eq!(mysa_mode("fan_only"), Some("fanOnly"));
        assert_eq!(mysa_mode("heat"), Some("heat"));
        assert_eq!(mysa_mode("bogus"), None);
    }

    #[test]
    fn a_baseboard_publishes_its_reading_and_heat_target_in_the_shared_names() {
        let s = state_payload(&bb_state(), &bb(), Some("BB-V2-0"));
        assert_eq!(s["temperature"], 20.4);
        assert_eq!(s["humidity"], 38.0);
        assert_eq!(s["hvac_mode"], "heat");
        assert_eq!(s["heat_setpoint"], 21.5);
        assert!(s.get("cool_setpoint").is_none());
        assert!(s.get("fan_mode").is_none(), "absent stays absent");
        // Not reported by Mysa, so not published.
        assert!(s.get("hvac_action").is_none());
        assert!(s.get("schedule_mode").is_none());
        assert_eq!(s["keypad_lockout"], "0");
        assert_eq!(s["display_idle_brightness"], 40);
    }

    #[test]
    fn in_auto_the_targets_are_autos_band() {
        let mut st = ac_state();
        st.mode = Some("auto".into());
        let s = state_payload(&st, &ac(), None);
        assert_eq!(s["hvac_mode"], "auto");
        assert_eq!(s["heat_setpoint"], 20.0);
        assert_eq!(s["cool_setpoint"], 24.0);
        assert_eq!(s["fan_mode"], "manual");
        assert_eq!(s["swing_vertical"], 1);
    }

    #[test]
    fn the_schema_offers_only_what_the_hardware_can_do() {
        let s = device_schema(&bb());
        let heat = &s.attributes["heat_setpoint"];
        assert_eq!(
            (heat.min, heat.max, heat.step),
            (Some(5.0), Some(30.0), Some(0.5))
        );
        assert_eq!(heat.unit.as_deref(), Some("°C"));
        assert!(!s.attributes.contains_key("cool_setpoint"));
        assert!(!s.attributes.contains_key("fan_mode"));
        assert!(!s.attributes.contains_key("swing_vertical"));
        assert!(s.attributes.contains_key("keypad_lockout"));
        assert!(s.attributes.contains_key("display_brightness"));
        let modes = s.attributes["hvac_mode"].options.as_ref().unwrap();
        assert_eq!(
            modes.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["off", "heat"]
        );
        assert!(s.attributes["hvac_mode"].writable);
        assert_eq!(
            s.actions.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["hold", "resume_schedule"]
        );

        let s = device_schema(&ac());
        assert_eq!(s.attributes["cool_setpoint"].step, Some(1.0));
        let fan = s.attributes["fan_mode"].options.as_ref().unwrap();
        assert_eq!(
            fan.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["auto", "manual"]
        );
        assert_eq!(s.attributes["fan_speed"].max, Some(3.0));
        assert!(s.attributes.contains_key("swing_vertical"));
        assert!(!s.attributes.contains_key("swing_horizontal"));
        assert!(
            !s.attributes.contains_key("keypad_lockout"),
            "not controllable"
        );
        assert!(!s.attributes.contains_key("display_brightness"));
    }

    #[test]
    fn a_setpoint_from_the_card_lands_on_the_thermostats_list() {
        let caps = bb();
        let st = bb_state();
        // The target is 21.5 °C (71 °F). + asks for 71 °F again, 21.67 °C,
        // whose nearest step is where it already is: it moves up a step.
        let w = interpret(
            &json!({ "heat_setpoint": 21.6667 }),
            &caps,
            Now { state: Some(&st) },
        )
        .unwrap();
        assert_eq!(w, vec![Write::Heat(22.0)]);
        // With nothing known, the nearest step.
        let w = interpret(&json!({ "heat_setpoint": 21.6667 }), &caps, Now::default()).unwrap();
        assert_eq!(w, vec![Write::Heat(21.5)]);
        // 70 °F asked from 21.5 moves down, never back to where it was.
        let w = interpret(
            &json!({ "heat_setpoint": 21.1111 }),
            &caps,
            Now { state: Some(&st) },
        )
        .unwrap();
        assert_eq!(w, vec![Write::Heat(21.0)]);
        // A rule's out-of-range number is held at the end of the list.
        let w = interpret(&json!({ "heat_setpoint": 45 }), &caps, Now::default()).unwrap();
        assert_eq!(w, vec![Write::Heat(30.0)]);
    }

    #[test]
    fn a_mode_change_is_checked_against_the_list_and_not_sent_twice() {
        let caps = ac();
        assert_eq!(
            interpret(&json!({ "hvac_mode": "fan_only" }), &caps, Now::default()).unwrap(),
            vec![Write::Mode("fanOnly")]
        );
        // A cool setpoint switches to cool anyway.
        assert_eq!(
            interpret(
                &json!({ "hvac_mode": "cool", "cool_setpoint": 22 }),
                &caps,
                Now::default()
            )
            .unwrap(),
            vec![Write::Cool(22.0)]
        );
        assert!(matches!(
            interpret(&json!({ "hvac_mode": "cool" }), &bb(), Now::default()),
            Err(Rejection::Disallowed(_))
        ));
    }

    #[test]
    fn in_auto_a_target_moves_its_end_and_keeps_the_band_wide_enough() {
        let caps = ac();
        let mut st = ac_state();
        st.mode = Some("auto".into());
        // Heat 20..24 cool; raise heat to 23: cool must stay 2 above.
        let w = interpret(
            &json!({ "heat_setpoint": 23 }),
            &caps,
            Now { state: Some(&st) },
        )
        .unwrap();
        assert_eq!(
            w,
            vec![Write::Auto(AutoTarget::Band {
                heat: 23.0,
                cool: 25.0
            })]
        );
        let w = interpret(
            &json!({ "cool_setpoint": 25 }),
            &caps,
            Now { state: Some(&st) },
        )
        .unwrap();
        assert_eq!(
            w,
            vec![Write::Auto(AutoTarget::Band {
                heat: 20.0,
                cool: 25.0
            })]
        );
    }

    #[test]
    fn fan_swing_lockout_and_brightness_only_where_offered() {
        let st = ac_state();
        assert_eq!(
            interpret(
                &json!({ "fan_mode": "auto" }),
                &ac(),
                Now { state: Some(&st) }
            )
            .unwrap(),
            vec![Write::Fan {
                mode: "auto".into(),
                speed: None
            }]
        );
        assert_eq!(
            interpret(&json!({ "fan_speed": 5 }), &ac(), Now::default()).unwrap(),
            vec![Write::Fan {
                mode: "manual".into(),
                speed: Some(3)
            }]
        );
        assert!(interpret(&json!({ "fan_mode": "off" }), &ac(), Now::default()).is_err());
        assert_eq!(
            interpret(&json!({ "swing_vertical": 2 }), &ac(), Now::default()).unwrap(),
            vec![Write::Swing {
                axis: "vertical",
                position: 2
            }]
        );
        assert_eq!(
            interpret(&json!({ "keypad_lockout": "1" }), &bb(), Now::default()).unwrap(),
            vec![Write::Lockout(1)]
        );
        assert!(interpret(&json!({ "keypad_lockout": "2" }), &bb(), Now::default()).is_err());
        assert!(interpret(&json!({ "keypad_lockout": "1" }), &ac(), Now::default()).is_err());
        assert_eq!(
            interpret(
                &json!({ "display_idle_brightness": 140 }),
                &bb(),
                Now::default()
            )
            .unwrap(),
            vec![Write::Brightness {
                active: None,
                idle: Some(100)
            }]
        );
        assert!(interpret(&json!({ "fan_mode": "auto" }), &bb(), Now::default()).is_err());
    }

    #[test]
    fn hold_and_resume_are_actions() {
        assert_eq!(
            interpret(&json!({ "action": "hold" }), &bb(), Now::default()).unwrap(),
            vec![Write::Hold]
        );
        assert_eq!(
            interpret(
                &json!({ "action": "resume_schedule" }),
                &bb(),
                Now::default()
            )
            .unwrap(),
            vec![Write::Resume]
        );
    }

    #[test]
    fn an_undescribed_thermostat_takes_no_commands() {
        let caps = Capabilities::default();
        assert_eq!(
            interpret(&json!({ "heat_setpoint": 21 }), &caps, Now::default()),
            Err(Rejection::Undescribed)
        );
        assert_eq!(
            interpret(&json!({ "nonsense": 1 }), &bb(), Now::default()),
            Err(Rejection::Unrecognised)
        );
    }
}
