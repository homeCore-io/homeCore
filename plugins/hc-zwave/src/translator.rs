//! ValueID ↔ HomeCore attribute translation.
//!
//! Maps `{commandClass}/{endpoint}/{property}[/{propertyKey}]` to the shared
//! names in `hc_types::device_vocabulary`, converting each value from the
//! device's scale to homeCore's (see [`crate::convert`]) — and turns a command
//! written in those names back into `node.set_value` calls.
//!
//! **Some of it depends on the node.** A Multilevel Switch level is a light's
//! `brightness_pct` on a dimmer and `position` on a motorised blind; a sensor's
//! reading is in whatever scale its metadata names; `{"on": true}` goes to a
//! Binary Switch on a relay and to the level on a dimmer. The translator keeps
//! a [`NodeInfo`] per node, learned from the node's values and metadata, for
//! exactly those decisions.
//!
//! **Endpoints.** zwave-js hides a multi-channel device's root values when
//! the endpoints carry them, so an outlet whose only switch is on endpoint 1
//! used to publish `cc37_e1_currentvalue` and nothing writable. Now:
//!
//! - one endpoint with the device's values and none on the root: that
//!   endpoint *is* the device — its values publish under the plain names and
//!   writes go to it;
//! - several: each endpoint's values publish with an `ep{n}_` prefix
//!   (`ep1_on`, `ep2_on`, `ep2_power_w`) — the app shows one switch per
//!   channel — and a write to `ep{n}_on` goes to endpoint `n`;
//! - anything else on another endpoint keeps its raw `cc…_e{n}_…` name.
//!
//! **Not everything a node reports is state.** Identity (Manufacturer
//! Specific, Version, Z-Wave Plus Info), wake-up settings, security, a
//! node's configuration parameters, a lock's code slots, a v1 alarm's raw
//! numbers and a switch's transition duration all have better homes — the
//! device's registration, the Health and Settings tabs, the lock's Codes and
//! History — and were noise in every automation picker. [`is_unpublished`]
//! says which.

use crate::convert::{self, Sensor};
use crate::types::NodeState;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Transform pipeline
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum Transform {
    /// Pass value through unchanged.
    Identity,
    /// Non-zero number/bool → true, zero → false. A Notification CC state
    /// (0 = idle), a Protection state.
    NonzeroBool,
    /// Door Lock CC 98 `currentMode`: 255 is secured (`true`); 254 is
    /// "unknown" — published as null, so homeCore says it does not know
    /// instead of claiming locked; every other mode (0 unsecured, and the
    /// timed / inside / outside unsecured variants 1, 16, 17, 32, 33) is
    /// unlocked.
    LockMode,
    /// Door Lock `doorStatus`: `"open"` / `"closed"`.
    DoorStatus,
    /// CC 113 Access Control "Door state" → `open`: 22 is open, and so are
    /// 5632 / 5633 (22 with a parameter: open in its regular or tilt
    /// position, which zwave-js encodes as `22 << 8 | parameter`); 23 is
    /// closed, 0 idle.
    AccessControlDoorState,
    /// A Notification CC variable that is one particular state: `true` when
    /// the value is that state, `false` when it is anything else (idle).
    StateIs(u64),
    /// Thermostat Mode CC 64 number → `hvac_mode`.
    HvacMode,
    /// Thermostat Operating State CC 66 number → `hvac_action`.
    HvacAction,
    /// Thermostat Fan Mode CC 68 number → `fan_mode`.
    FanMode,
    /// Multilevel Switch CC 38 level 0–99 → `brightness_pct` + `on`, or
    /// `position` when the node is a cover.
    Level,
    /// Thermostat Setpoint CC 67, in the unit its metadata names → °C.
    Setpoint,
    /// Multilevel Sensor CC 49, in the scale its metadata names.
    Sensor(Sensor),
    /// Color Switch CC 51 component object → `color_rgb` / `color_temp`.
    Color,
    /// Barrier Operator CC 102 `currentState` → `open` + `barrier_state`.
    Barrier,
}

/// What a value's translation needs to know about its node.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx<'a> {
    /// The value's unit, from its metadata.
    pub unit: Option<&'a str>,
    /// The node is a Multilevel Switch that drives a cover.
    pub cover: bool,
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A float that is a whole number goes out as an integer, so `22.0` reads
/// `22` like it did before conversion.
fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        Value::from(v as i64)
    } else {
        json!(v)
    }
}

/// The DoorLockMode that means "secured".
pub const LOCK_SECURED: u64 = 255;
/// The DoorLockMode that means "unknown".
pub const LOCK_UNKNOWN: u64 = 254;

/// Whether a Door Lock mode is locked: `Some(true)` only for 255, `None` for
/// 254 (unknown), `Some(false)` for every other mode.
pub fn lock_mode_locked(mode: u64) -> Option<bool> {
    match mode {
        LOCK_SECURED => Some(true),
        LOCK_UNKNOWN => None,
        _ => Some(false),
    }
}

/// CC 113 Access Control door state → open.
pub fn door_state_open(v: u64) -> bool {
    v == 22 || v >> 8 == 22
}

impl Transform {
    /// Apply forward transform: raw ZWave value → `(attribute, value)` pairs.
    /// `None` when the value cannot be published under a shared name (an
    /// unknown sensor scale) — the caller publishes it raw instead. A pair
    /// whose value is `null` says "unknown now" and is published as such.
    pub fn apply(&self, attr: &str, v: &Value, ctx: Ctx) -> Option<Vec<(String, Value)>> {
        let one = |a: &str, v: Value| Some(vec![(a.to_string(), v)]);
        match self {
            Transform::Identity => one(attr, v.clone()),
            Transform::NonzeroBool => {
                let nonzero = num(v).is_some_and(|f| f != 0.0);
                one(attr, Value::Bool(nonzero))
            }
            Transform::LockMode => {
                let mode = num(v)? as u64;
                one(
                    attr,
                    lock_mode_locked(mode).map_or(Value::Null, Value::Bool),
                )
            }
            Transform::DoorStatus => match v {
                Value::String(s) => one(attr, Value::Bool(s.eq_ignore_ascii_case("open"))),
                other => one(attr, Value::Bool(num(other)? == 0.0)),
            },
            Transform::AccessControlDoorState => {
                one(attr, Value::Bool(door_state_open(num(v)? as u64)))
            }
            Transform::StateIs(state) => one(attr, Value::Bool(num(v)? as u64 == *state)),
            Transform::HvacMode => {
                let n = v.as_u64()?;
                let name = convert::name_of(convert::HVAC_MODES, n)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("mode_{n}"));
                one(attr, Value::String(name))
            }
            Transform::HvacAction => one(attr, json!(convert::hvac_action(v.as_u64()?))),
            Transform::FanMode => {
                let n = v.as_u64()?;
                let name = convert::name_of(convert::FAN_MODES, n)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("mode_{n}"));
                one(attr, Value::String(name))
            }
            Transform::Level => {
                let pct = convert::level_to_pct(num(v)?);
                if ctx.cover {
                    one("position", json!(pct))
                } else {
                    Some(vec![
                        ("brightness_pct".into(), json!(pct)),
                        ("on".into(), Value::Bool(pct > 0)),
                    ])
                }
            }
            Transform::Setpoint => one(attr, number(convert::to_celsius(num(v)?, ctx.unit))),
            Transform::Sensor(sensor) => {
                let (name, value) = convert::sensor_reading(*sensor, num(v)?, ctx.unit)?;
                one(name, number(value))
            }
            Transform::Color => {
                let obj = v.as_object()?;
                let c = |k: &str| obj.get(k).and_then(num);
                let mut out = Vec::new();
                if let (Some(r), Some(g), Some(b)) = (c("red"), c("green"), c("blue")) {
                    out.push((
                        "color_rgb".into(),
                        json!({ "r": r as u8, "g": g as u8, "b": b as u8 }),
                    ));
                }
                if let (Some(w), Some(cw)) = (c("warmWhite"), c("coldWhite")) {
                    if let Some(k) = convert::whites_to_kelvin(w, cw) {
                        out.push(("color_temp".into(), json!(k)));
                    }
                }
                Some(out)
            }
            Transform::Barrier => {
                let n = v.as_u64()?;
                let state = match n {
                    0 => "closed",
                    252 => "closing",
                    253 => "stopped",
                    254 => "opening",
                    _ => "open",
                };
                Some(vec![
                    ("open".into(), Value::Bool(n != 0)),
                    ("barrier_state".into(), json!(state)),
                ])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Alias table entry
// ---------------------------------------------------------------------------

struct AliasEntry {
    /// Alias key: "{cc}/{endpoint}/{property}" or "{cc}/{endpoint}/{property}/{propertyKey}"
    key: &'static str,
    attribute: &'static str,
    transform: Transform,
}

const fn alias(key: &'static str, attribute: &'static str, transform: Transform) -> AliasEntry {
    AliasEntry {
        key,
        attribute,
        transform,
    }
}

use Transform::{
    AccessControlDoorState, Barrier, Color, DoorStatus, FanMode, HvacAction, HvacMode, Identity,
    Level, LockMode, NonzeroBool, Setpoint, StateIs,
};

/// What each read value publishes as. Writes are decided in
/// [`Translator::commands`], which knows the node.
static ALIAS_TABLE: &[AliasEntry] = &[
    // Binary Sensor (CC 48). `property` is the BinarySensorType enum name
    // exactly as zwave-js emits it; newValue is true = triggered/open.
    alias("48/0/Motion", "motion", Identity),
    alias("48/0/Door/Window", "open", Identity),
    alias("48/0/Water", "water_detected", Identity),
    alias("48/0/Smoke", "smoke", Identity),
    alias("48/0/CO", "co", Identity),
    alias("48/0/CO2", "co2_alarm", Identity),
    alias("48/0/Heat", "heat_alarm", Identity),
    alias("48/0/Freeze", "freeze", Identity),
    alias("48/0/Tamper", "tampered", Identity),
    alias("48/0/Tilt", "tilt", Identity),
    alias("48/0/Glass Break", "glass_break", Identity),
    // CC 48 v1 devices report all sensor types as "Any".
    alias("48/0/Any", "sensor_active", Identity),
    // Binary Switch (CC 37).
    alias("37/0/currentValue", "on", Identity),
    // Multilevel Switch (CC 38): a dimmer's level, or a cover's position.
    alias("38/0/currentValue", "brightness_pct", Level),
    // Multilevel Sensor (CC 49), each in the scale its metadata names.
    alias(
        "49/0/Air temperature",
        "temperature",
        Transform::Sensor(Sensor::Temperature),
    ),
    alias(
        "49/0/Humidity",
        "humidity",
        Transform::Sensor(Sensor::Humidity),
    ),
    alias(
        "49/0/Illuminance",
        "illuminance",
        Transform::Sensor(Sensor::Illuminance),
    ),
    // zwave-js's older label for the same sensor type.
    alias(
        "49/0/Luminance",
        "illuminance",
        Transform::Sensor(Sensor::Illuminance),
    ),
    alias(
        "49/0/Ultraviolet",
        "uv_index",
        Transform::Sensor(Sensor::Uv),
    ),
    alias("49/0/CO2 level", "co2", Transform::Sensor(Sensor::Co2)),
    alias(
        "49/0/Atmospheric pressure",
        "pressure",
        Transform::Sensor(Sensor::Pressure),
    ),
    alias(
        "49/0/Barometric pressure",
        "pressure",
        Transform::Sensor(Sensor::Pressure),
    ),
    alias("49/0/Power", "power_w", Transform::Sensor(Sensor::Power)),
    alias(
        "49/0/Voltage",
        "voltage",
        Transform::Sensor(Sensor::Voltage),
    ),
    alias(
        "49/0/Current",
        "current_a",
        Transform::Sensor(Sensor::Current),
    ),
    // Meter (CC 50) — propertyKey-encoded, so the unit is fixed by the key:
    //
    //     (meterType << 16) | (scale << 8) | rateType
    //
    // Electric (meterType=1), rateType=Consumed (1): scale 0 kWh → 65537,
    // 1 kVAh → 65793, 2 W → 66049, 3 pulses → 66305, 4 V → 66561, 5 A → 66817,
    // 6 power factor → 67073, 7 kVar → 67329, 8 kVarh → 67585.
    // rateType=Produced (2) gives the parallel exported keys. rateType 0
    // ("unspecified", what Meter v1/v2 devices report) is consumption:
    // 65536 kWh, 66048 W, 66560 V, 66816 A.
    //
    // History note: the original 65537/65538/65539/65540 aliases mapped each
    // propertyKey to a different unit (W/kWh/V/A) — that was wrong; verified
    // against a real Meter v3 device (node 47, 2026-04-24).
    alias("50/0/value/65537", "energy_kwh", Identity),
    alias("50/0/value/65536", "energy_kwh", Identity),
    alias("50/0/value/65793", "apparent_energy_kvah", Identity),
    alias("50/0/value/65792", "apparent_energy_kvah", Identity),
    alias("50/0/value/66049", "power_w", Identity),
    alias("50/0/value/66048", "power_w", Identity),
    alias("50/0/value/66305", "pulse_count", Identity),
    alias("50/0/value/66561", "voltage", Identity),
    alias("50/0/value/66560", "voltage", Identity),
    alias("50/0/value/66817", "current_a", Identity),
    alias("50/0/value/66816", "current_a", Identity),
    alias("50/0/value/67073", "power_factor", Identity),
    alias("50/0/value/67072", "power_factor", Identity),
    alias("50/0/value/67329", "reactive_power_kvar", Identity),
    alias("50/0/value/67585", "reactive_energy_kvarh", Identity),
    alias("50/0/value/65538", "energy_kwh_exported", Identity),
    alias("50/0/value/66050", "power_w_exported", Identity),
    // Battery (CC 128).
    alias("128/0/level", "battery_pct", Identity),
    alias("128/0/isLow", "battery_low", Identity),
    // Door Lock (CC 98). currentMode is what the hardware confirmed;
    // targetMode is where commands go. `boltStatus` said the same as
    // `locked`, so it is not published.
    alias("98/0/currentMode", "locked", LockMode),
    alias("98/0/doorStatus", "open", DoorStatus),
    // 1=Constant, 2=Timed (locks again by itself after the timeout).
    alias("98/0/operationType", "lock_operation_type", Identity),
    alias(
        "98/0/lockTimeoutConfiguration",
        "lock_timeout_secs",
        Identity,
    ),
    alias("98/0/autoRelockTime", "lock_auto_relock_secs", Identity),
    // Barrier Operator (CC 102) — a garage door opener, not a blind.
    alias("102/0/currentState", "open", Barrier),
    alias("102/0/position", "position", Identity),
    // Color Switch (CC 51).
    alias("51/0/currentColor", "color_rgb", Color),
    // Thermostat Setpoint (CC 67), keyed by setpoint type.
    alias("67/0/setpoint/1", "heat_setpoint", Setpoint),
    alias("67/0/setpoint/2", "cool_setpoint", Setpoint),
    // Thermostat Mode (CC 64), Operating State (CC 66), Fan Mode (CC 68).
    alias("64/0/mode", "hvac_mode", HvacMode),
    alias("66/0/state", "hvac_action", HvacAction),
    alias("68/0/mode", "fan_mode", FanMode),
    // Protection (CC 117): the buttons on the device itself do nothing.
    alias("117/0/local", "child_lock", NonzeroBool),
    // Notification (CC 113): property is the notification type's name and
    // propertyKey the variable's, exactly as zwave-js names them (its
    // `Notifications` table). The value is the state's number; 0 is idle.
    //
    // Access Control: "Door state" 22 = open (5632/5633 with a parameter),
    // 23 = closed. Newer zwave-js also creates "Door state (simple)" (22/23
    // only) beside it.
    alias(
        "113/0/Access Control/Door state",
        "open",
        AccessControlDoorState,
    ),
    alias(
        "113/0/Access Control/Door state (simple)",
        "open",
        AccessControlDoorState,
    ),
    // "Lock state" has one state: 11, lock jammed.
    alias("113/0/Access Control/Lock state", "jammed", StateIs(11)),
    alias(
        "113/0/Home Security/Motion sensor status",
        "motion",
        NonzeroBool,
    ),
    alias(
        "113/0/Home Security/Sensor status",
        "intrusion",
        NonzeroBool,
    ),
    // The cover's tamper switch. ("Tampering, invalid code" is an event, not
    // a state — it arrives as a notification and goes in a lock's history.)
    alias("113/0/Home Security/Cover status", "tampered", NonzeroBool),
    alias("113/0/Smoke Alarm/Sensor status", "smoke", NonzeroBool),
    alias("113/0/CO Alarm/Sensor status", "co", NonzeroBool),
    alias("113/0/CO2 Alarm/Sensor status", "co2_alarm", NonzeroBool),
    alias(
        "113/0/Water Alarm/Sensor status",
        "water_detected",
        NonzeroBool,
    ),
    alias(
        "113/0/Heat Alarm/Heat sensor status",
        "heat_alarm",
        NonzeroBool,
    ),
    alias(
        "113/0/Power Management/Over-load status",
        "overload",
        StateIs(8),
    ),
    alias(
        "113/0/Power Management/Over-current status",
        "overcurrent",
        StateIs(6),
    ),
];

// ---------------------------------------------------------------------------
// Window Covering (CC 106)
// ---------------------------------------------------------------------------

/// Window Covering parameters (the propertyKey of `currentValue` /
/// `targetValue`) that carry a position, 0 = closed and 99 = open.
const WC_POSITION_PARAMS: &[u64] = &[1, 3, 5, 7, 9, 13, 15, 17, 19, 21];
/// Slat-angle parameters (vertical, horizontal): 50 = open, 0 and 99 closed.
const WC_TILT_PARAMS: &[u64] = &[11, 23];

// ---------------------------------------------------------------------------
// What is never published
// ---------------------------------------------------------------------------

/// A value that must never leave the plugin: a lock's door codes (User Code,
/// CC 99). zwave-js reports every slot's code in the clear; published, each
/// would sit in homeCore's state, its history and on the bus for anyone who
/// can read a device.
pub fn is_secret(cc: u32, property: &str) -> bool {
    crate::types::is_code_value(cc, property)
}

/// A write target (`targetValue`, `targetMode`, …). A target is what was asked
/// for, not what the device did, so it is never published as state — the
/// attribute comes from the matching current value.
pub fn is_target(cc: u32, property: &str) -> bool {
    matches!(
        (cc, property),
        (37 | 38 | 106, "targetValue")
            | (98, "targetMode")
            | (51, "targetColor")
            | (102, "targetState")
    )
}

/// Command classes none of whose values are state:
///
/// - 32 Basic — a duplicate of the switch it is mapped to;
/// - 89 Association Group Info, 133 Association, 142 Multi Channel
///   Association, 96 Multi Channel, 108 Supervision, 115 Powerlevel,
///   122 Firmware Update — plumbing;
/// - 94 Z-Wave Plus Info, 114 Manufacturer Specific, 134 Version — identity,
///   on the device's registration and its Health tab;
/// - 132 Wake Up — the wake-up interval and controller node, on the Health tab;
/// - 152 Security, 159 Security 2 — on the Health tab;
/// - 112 Configuration — the Settings tab, through `get_config_params`;
/// - 99 User Code — every slot, code or not: the Codes tab, through
///   `list_user_codes`;
/// - 135 Indicator — the device's own LED, not what it is doing.
pub const NOT_STATE_CCS: &[u32] = &[
    32, 89, 94, 96, 99, 108, 112, 114, 115, 122, 132, 133, 134, 135, 142, 152, 159,
];

/// Whether a value is left out of what is published.
pub fn is_unpublished(cc: u32, property: &str, property_key: Option<&str>) -> bool {
    if is_secret(cc, property) || is_target(cc, property) || NOT_STATE_CCS.contains(&cc) {
        return true;
    }
    match cc {
        // A transition's remaining time, and a dimmer's "come back as it
        // was" switch, change by themselves or not at all.
        37 | 38 => matches!(
            property,
            "duration" | "restorePrevious" | "Up" | "Down" | "On" | "Off"
        ),
        // A battery's level and whether it is low are the readings; its
        // chemistry and charging flags are not.
        128 => !matches!(property, "level" | "isLow"),
        // A lock: whether it is locked, whether the door is open, and how it
        // locks again by itself. The bolt and latch said the same as
        // `locked`; the handles and twist assist are installation settings;
        // `lockTimeout` counts down by itself.
        98 => !matches!(
            property,
            "currentMode"
                | "doorStatus"
                | "operationType"
                | "lockTimeoutConfiguration"
                | "autoRelockTime"
        ),
        // A meter's reset is a button, not a reading.
        50 => property == "reset",
        // Notification v1: a lock's raw alarm numbers, decoded into its
        // lock events (`last_lock_*`) where they are known.
        113 => {
            matches!(property, "alarmType" | "alarmLevel")
                || property_key == Some("Keypad state")
                || property_key == Some("Door handle state")
        }
        // Protection: only the device's own buttons are a setting people use.
        117 => property != "local",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Per-node knowledge
// ---------------------------------------------------------------------------

/// The command classes whose values make an endpoint "the device": switches,
/// sensors, meters, colour, covers.
const DEVICE_CCS: &[u32] = &[37, 38, 49, 50, 51, 106];

/// The prefix a multi-channel endpoint's attributes carry.
pub fn ep_prefix(endpoint: u32) -> String {
    format!("ep{endpoint}_")
}

/// `ep2_on` → `(2, "on")`.
pub fn strip_ep_prefix(attr: &str) -> Option<(u32, &str)> {
    let rest = attr.strip_prefix("ep")?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let n: u32 = rest[..digits].parse().ok()?;
    let base = rest[digits..].strip_prefix('_')?;
    (!base.is_empty()).then_some((n, base))
}

/// How an endpoint's values are published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpRole {
    /// Under the plain names: the root, or the one endpoint that stands for
    /// the device.
    Root,
    /// With an `ep{n}_` prefix: one of several channels.
    Channel(u32),
    /// Under its raw `cc…_e{n}_…` name.
    Raw,
}

/// What the translator knows about one node: the parts of its values and
/// metadata that change what an attribute means or where a write goes.
#[derive(Clone, Debug, Default)]
pub struct NodeInfo {
    /// A Multilevel Switch that drives a cover, by its device class.
    pub cover: bool,
    /// Has a Binary Switch (CC 37) on the endpoint that stands for the device.
    pub binary_switch: bool,
    /// Has a Multilevel Switch (CC 38) there.
    pub multilevel_switch: bool,
    /// Has a Barrier Operator (CC 102).
    pub barrier: bool,
    /// Has a Door Lock (CC 98).
    pub lock: bool,
    /// Has Door Lock v4's `autoRelockTime`.
    pub auto_relock: bool,
    /// Has Protection (CC 117) local control; the states it supports.
    pub protection: Option<Vec<u64>>,
    /// The Window Covering parameter that carries the position, if any.
    pub wc_position: Option<u64>,
    /// The Window Covering slat-angle parameter, if any.
    pub wc_tilt: Option<u64>,
    /// The Color Switch components the node has (`red`, `warmWhite`, …).
    pub color_components: Vec<String>,
    /// Unit per alias key, from each value's metadata.
    pub units: HashMap<String, String>,
    /// Supported numeric states per alias key (Thermostat Mode / Fan Mode),
    /// from each value's metadata.
    pub states: HashMap<String, Vec<u64>>,
    /// The endpoint whose values stand for the device: 0, or the only
    /// endpoint with any when zwave-js hides the root's.
    pub root_ep: u32,
    /// Endpoints published as channels, with what switches each has
    /// (`binary`, `multilevel`).
    pub channels: std::collections::BTreeMap<u32, (bool, bool)>,
}

/// Whether a node's device class says it is a motorised cover.
pub fn is_cover_class(generic: &str, specific: &str) -> bool {
    generic == "Multilevel Switch"
        && (specific.contains("Motor Control")
            || specific.contains("Class B")
            || specific.contains("Class C"))
}

impl NodeInfo {
    pub fn from_node(node: &NodeState) -> Self {
        let class = node.device_class.as_ref();
        let generic = class.and_then(|c| c.generic_label()).unwrap_or_default();
        let specific = class.and_then(|c| c.specific_label()).unwrap_or_default();
        let mut info = NodeInfo {
            cover: is_cover_class(generic, specific),
            ..Default::default()
        };

        // Which endpoint is the device, and which are channels.
        let mut root_has = false;
        let mut eps: BTreeSet<u32> = BTreeSet::new();
        for v in &node.values {
            if DEVICE_CCS.contains(&v.command_class) {
                if v.endpoint == 0 {
                    root_has = true;
                } else {
                    eps.insert(v.endpoint);
                }
            }
        }
        if !root_has && eps.len() == 1 {
            info.root_ep = *eps.iter().next().unwrap_or(&0);
        } else if eps.len() >= 2 || (root_has && !eps.is_empty()) {
            for ep in &eps {
                info.channels.insert(*ep, (false, false));
            }
        }

        for v in &node.values {
            let pk = v.property_key.as_ref().and_then(property_key_str);
            let role = info.role(v.endpoint);
            match (v.command_class, v.property.as_str(), role) {
                (37, "currentValue", EpRole::Root) => info.binary_switch = true,
                (38, "currentValue", EpRole::Root) => info.multilevel_switch = true,
                (37, "currentValue", EpRole::Channel(n)) => {
                    if let Some(c) = info.channels.get_mut(&n) {
                        c.0 = true;
                    }
                }
                (38, "currentValue", EpRole::Channel(n)) => {
                    if let Some(c) = info.channels.get_mut(&n) {
                        c.1 = true;
                    }
                }
                (102, _, _) => info.barrier = true,
                (98, "autoRelockTime", _) => {
                    info.lock = true;
                    info.auto_relock = true;
                }
                (98, _, _) => info.lock = true,
                (117, "local", EpRole::Root) => {
                    let states = v
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("states"))
                        .and_then(Value::as_object)
                        .map(|s| {
                            let mut n: Vec<u64> = s.keys().filter_map(|k| k.parse().ok()).collect();
                            n.sort_unstable();
                            n
                        })
                        .unwrap_or_else(|| vec![0, 2]);
                    info.protection = Some(states);
                }
                (106, "currentValue", EpRole::Root) => {
                    if let Some(p) = pk.as_deref().and_then(|p| p.parse::<u64>().ok()) {
                        if WC_POSITION_PARAMS.contains(&p) && info.wc_position.is_none() {
                            info.wc_position = Some(p);
                        }
                        if WC_TILT_PARAMS.contains(&p) && info.wc_tilt.is_none() {
                            info.wc_tilt = Some(p);
                        }
                    }
                }
                (51, "currentColor", EpRole::Root) if pk.is_none() => {
                    if let Some(obj) = v.value.as_ref().and_then(Value::as_object) {
                        info.color_components = obj.keys().cloned().collect();
                    }
                }
                _ => {}
            }
            if let Some(meta) = &v.metadata {
                let ep = if role == EpRole::Root { 0 } else { v.endpoint };
                let key = Translator::alias_key(v.command_class, ep, &v.property, pk.as_deref());
                info.learn_metadata(key, meta);
            }
        }
        info
    }

    /// How values from `endpoint` are published.
    pub fn role(&self, endpoint: u32) -> EpRole {
        if endpoint == 0 || endpoint == self.root_ep {
            EpRole::Root
        } else if self.channels.contains_key(&endpoint) {
            EpRole::Channel(endpoint)
        } else {
            EpRole::Raw
        }
    }

    /// What a channel endpoint can be written as, as though it were a device.
    fn for_channel(&self, endpoint: u32) -> Option<NodeInfo> {
        let (binary, multilevel) = *self.channels.get(&endpoint)?;
        Some(NodeInfo {
            binary_switch: binary,
            multilevel_switch: multilevel,
            root_ep: endpoint,
            units: self.units.clone(),
            ..Default::default()
        })
    }

    fn learn_metadata(&mut self, key: String, meta: &Value) {
        if let Some(unit) = meta.get("unit").and_then(Value::as_str) {
            self.units.insert(key.clone(), unit.to_string());
        }
        if let Some(states) = meta.get("states").and_then(Value::as_object) {
            let mut nums: Vec<u64> = states.keys().filter_map(|k| k.parse().ok()).collect();
            nums.sort_unstable();
            self.states.insert(key, nums);
        }
    }

    fn unit(&self, key: &str) -> Option<&str> {
        self.units.get(key).map(String::as_str)
    }

    fn has_color(&self, c: &str) -> bool {
        self.color_components.iter().any(|k| k == c)
    }

    /// The node routes `{"on": …}` to its level rather than a relay.
    fn dims(&self) -> bool {
        self.multilevel_switch && !self.cover && !self.binary_switch
    }

    /// Whether a write to this attribute reaches the device.
    pub fn writable(&self, attr: &str) -> bool {
        if let Some((n, base)) = strip_ep_prefix(attr) {
            return matches!(base, "on" | "brightness_pct")
                && self.for_channel(n).is_some_and(|c| c.writable(base));
        }
        match attr {
            "on" => self.binary_switch || (self.multilevel_switch && !self.cover),
            "brightness_pct" => self.multilevel_switch && !self.cover,
            "position" => self.wc_position.is_some() || (self.multilevel_switch && self.cover),
            "tilt_pct" => self.wc_tilt.is_some(),
            "open" => self.barrier,
            "locked" | "lock_operation_type" | "lock_timeout_secs" => self.lock,
            "lock_auto_relock_secs" => self.lock && self.auto_relock,
            "child_lock" => self.protection.is_some(),
            "color_rgb" => self.has_color("red"),
            "color_temp" => self.has_color("warmWhite") && self.has_color("coldWhite"),
            "heat_setpoint" | "cool_setpoint" | "hvac_mode" | "fan_mode" => true,
            _ => false,
        }
    }

    /// The `hvac_mode` / `fan_mode` names this node supports, from metadata.
    pub fn options(&self, attr: &str) -> Option<Vec<String>> {
        let (key, table) = match attr {
            "hvac_mode" => ("64/0/mode", convert::HVAC_MODES),
            "fan_mode" => ("68/0/mode", convert::FAN_MODES),
            _ => return None,
        };
        let states = self.states.get(key)?;
        Some(
            states
                .iter()
                .map(|n| {
                    convert::name_of(table, *n)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("mode_{n}"))
                })
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// One `node.set_value` to send.
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    pub command_class: u32,
    pub endpoint: u32,
    pub property: String,
    /// The value's `propertyKey`, where it has one (a setpoint's type).
    pub property_key: Option<Value>,
    pub value: Value,
    /// zwave-js set-value options (`transitionDuration`).
    pub options: Option<Value>,
    /// The attribute it came from, for saying which one failed.
    pub attribute: String,
}

fn write(cc: u32, property: &str, key: Option<Value>, value: Value) -> Write {
    Write {
        command_class: cc,
        endpoint: 0,
        property: property.to_string(),
        property_key: key,
        value,
        options: None,
        attribute: String::new(),
    }
}

/// A mode's number from its name (or its number, sent as-is).
fn mode_number(table: &[(u64, &str)], v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => convert::number_of(table, s)
            .or_else(|| s.strip_prefix("mode_").unwrap_or(s).parse().ok()),
        _ => None,
    }
}

/// Build the writes for one command, given what the node is.
///
/// Returns the writes and the attributes that could not be written (no
/// target on this node, or a value that does not fit). A channel's
/// attributes (`ep2_on`) go to its endpoint; the rest to the endpoint that
/// stands for the device.
pub fn commands_for(info: &NodeInfo, cmd: &Map<String, Value>) -> (Vec<Write>, Vec<String>) {
    let mut writes = Vec::new();
    let mut rejected = Vec::new();
    // Channels first: split the command by endpoint.
    let mut root = Map::new();
    let mut by_channel: std::collections::BTreeMap<u32, Map<String, Value>> = Default::default();
    for (attr, v) in cmd {
        match strip_ep_prefix(attr) {
            Some((n, base)) if info.channels.contains_key(&n) => {
                by_channel
                    .entry(n)
                    .or_default()
                    .insert(base.to_string(), v.clone());
            }
            _ => {
                root.insert(attr.clone(), v.clone());
            }
        }
    }
    // A fade sent with a channel's level applies to it too.
    if let Some(fade) = cmd.get("transition_ms") {
        for c in by_channel.values_mut() {
            c.insert("transition_ms".into(), fade.clone());
        }
    }
    for (n, c) in by_channel {
        let Some(ch) = info.for_channel(n) else {
            continue;
        };
        let (w, r) = commands_for_root(&ch, &c);
        writes.extend(w.into_iter().map(|mut w| {
            w.endpoint = n;
            w.attribute = format!("{}{}", ep_prefix(n), w.attribute);
            w
        }));
        rejected.extend(r.into_iter().map(|a| format!("{}{a}", ep_prefix(n))));
    }
    let (w, r) = commands_for_root(info, &root);
    writes.extend(w.into_iter().map(|mut w| {
        w.endpoint = info.root_ep;
        w
    }));
    rejected.extend(r);
    (writes, rejected)
}

fn commands_for_root(info: &NodeInfo, cmd: &Map<String, Value>) -> (Vec<Write>, Vec<String>) {
    let mut writes = Vec::new();
    let mut rejected = Vec::new();
    let fade = cmd
        .get("transition_ms")
        .and_then(num)
        .map(|ms| json!({ "transitionDuration": convert::transition_duration(ms) }));
    let with_fade = |mut w: Write| {
        w.options = fade.clone();
        w
    };
    let level = |l: u8| with_fade(write(38, "targetValue", None, json!(l)));
    let brightness = cmd.get("brightness_pct").and_then(num);
    let turning_off = cmd.get("on") == Some(&Value::Bool(false));

    for (attr, v) in cmd {
        if attr.starts_with('_') || attr == "correlation_id" || attr == "transition_ms" {
            continue;
        }
        let w = match attr.as_str() {
            "on" => match v.as_bool() {
                // A level sent alongside says how on.
                Some(true) if info.dims() && brightness.is_some() => continue,
                // No level: 255 asks the dimmer for the one it had before.
                Some(true) if info.dims() => Some(level(convert::LEVEL_RESTORE)),
                Some(false) if info.dims() => Some(level(0)),
                Some(on) if info.binary_switch || !info.multilevel_switch => {
                    Some(write(37, "targetValue", None, Value::Bool(on)))
                }
                _ => None,
            },
            "brightness_pct" if info.writable(attr) => {
                if turning_off {
                    continue;
                }
                num(v).map(|p| level(convert::pct_to_level(p)))
            }
            "position" => match (info.wc_position, num(v)) {
                (Some(p), Some(pct)) => Some(with_fade(write(
                    106,
                    "targetValue",
                    Some(json!(p)),
                    json!(convert::pct_to_level(pct)),
                ))),
                (None, Some(pct)) if info.multilevel_switch && info.cover => {
                    Some(level(convert::pct_to_level(pct)))
                }
                _ => None,
            },
            "tilt_pct" => match (info.wc_tilt, num(v)) {
                (Some(p), Some(pct)) => Some(with_fade(write(
                    106,
                    "targetValue",
                    Some(json!(p)),
                    json!(convert::tilt_pct_to_slat(pct)),
                ))),
                _ => None,
            },
            "open" if info.barrier => v
                .as_bool()
                .map(|o| write(102, "targetState", None, json!(if o { 255 } else { 0 }))),
            "locked" => v.as_bool().map(|l| {
                write(
                    98,
                    "targetMode",
                    None,
                    json!(if l { LOCK_SECURED } else { 0 }),
                )
            }),
            "lock_operation_type" => num(v)
                .filter(|n| *n == 1.0 || *n == 2.0)
                .map(|n| write(98, "operationType", None, json!(n as u64))),
            "lock_timeout_secs" => num(v)
                .filter(|n| *n >= 0.0)
                .map(|n| write(98, "lockTimeoutConfiguration", None, json!(n as u64))),
            "lock_auto_relock_secs" if info.auto_relock => num(v)
                .filter(|n| *n >= 0.0)
                .map(|n| write(98, "autoRelockTime", None, json!(n as u64))),
            "child_lock" => match (&info.protection, v.as_bool()) {
                (Some(states), Some(on)) => {
                    // "No operation possible" where the device has it, else
                    // "by sequence"; off is unprotected.
                    let state = if !on {
                        0
                    } else if states.contains(&2) {
                        2
                    } else {
                        1
                    };
                    Some(write(117, "local", None, json!(state)))
                }
                _ => None,
            },
            "heat_setpoint" | "cool_setpoint" => {
                let kind = if attr == "heat_setpoint" { 1 } else { 2 };
                let unit = info.unit(&format!("67/0/setpoint/{kind}"));
                num(v).map(|c| {
                    write(
                        67,
                        "setpoint",
                        Some(json!(kind)),
                        number(convert::from_celsius(c, unit)),
                    )
                })
            }
            // `mode` is the old spelling; harmless to keep accepting.
            "hvac_mode" | "mode" => {
                mode_number(convert::HVAC_MODES, v).map(|n| write(64, "mode", None, json!(n)))
            }
            "fan_mode" => {
                mode_number(convert::FAN_MODES, v).map(|n| write(68, "mode", None, json!(n)))
            }
            "color_rgb" => v.as_object().and_then(|o| {
                let c = |k: &str| o.get(k).and_then(num).map(|n| n.clamp(0.0, 255.0) as u8);
                let (r, g, b) = (c("r")?, c("g")?, c("b")?);
                let mut color = json!({ "red": r, "green": g, "blue": b });
                // Colour replaces white on an RGBW bulb.
                for white in ["warmWhite", "coldWhite"] {
                    if info.has_color(white) {
                        color[white] = json!(0);
                    }
                }
                Some(with_fade(write(51, "targetColor", None, color)))
            }),
            "color_temp" if info.writable(attr) => num(v).map(|k| {
                let (warm, cold) = convert::kelvin_to_whites(k);
                let mut color = json!({ "warmWhite": warm, "coldWhite": cold });
                for c in ["red", "green", "blue"] {
                    if info.has_color(c) {
                        color[c] = json!(0);
                    }
                }
                with_fade(write(51, "targetColor", None, color))
            }),
            _ => None,
        };
        match w {
            Some(mut w) => {
                w.attribute = attr.clone();
                writes.push(w)
            }
            None => rejected.push(attr.clone()),
        }
    }
    (writes, rejected)
}

// ---------------------------------------------------------------------------
// Translator
// ---------------------------------------------------------------------------

pub struct Translator {
    /// Forward map: alias_key → (attribute, transform)
    forward: HashMap<String, (&'static str, Transform)>,
    /// What each node is, learned from its values and metadata.
    nodes: Mutex<HashMap<u32, NodeInfo>>,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    pub fn new() -> Self {
        let mut forward = HashMap::new();
        for entry in ALIAS_TABLE {
            forward
                .entry(entry.key.to_string())
                .or_insert((entry.attribute, entry.transform));
        }
        Self {
            forward,
            nodes: Mutex::new(HashMap::new()),
        }
    }

    fn with_node<R>(&self, node_id: u32, f: impl FnOnce(&NodeInfo) -> R) -> R {
        let nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        match nodes.get(&node_id) {
            Some(info) => f(info),
            None => f(&NodeInfo::default()),
        }
    }

    /// Learn (or relearn) what a node is from its full state.
    pub fn learn_node(&self, node: &NodeState) {
        let info = NodeInfo::from_node(node);
        self.nodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.node_id, info);
    }

    /// Forget a node that has left the network.
    pub fn forget_node(&self, node_id: u32) {
        self.nodes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&node_id);
    }

    /// A copy of what is known about a node.
    pub fn info(&self, node_id: u32) -> NodeInfo {
        self.with_node(node_id, Clone::clone)
    }

    /// A `metadata updated` event: a value's unit or states changed.
    pub fn learn_metadata(
        &self,
        node_id: u32,
        cc: u32,
        endpoint: u32,
        property: &str,
        property_key: Option<&str>,
        meta: &Value,
    ) {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        let info = nodes.entry(node_id).or_default();
        let ep = if info.role(endpoint) == EpRole::Root {
            0
        } else {
            endpoint
        };
        let key = Self::alias_key(cc, ep, property, property_key);
        info.learn_metadata(key, meta);
    }

    /// Whether a value is left out of what is published: a secret, a write
    /// target whose attribute comes from the device's current value, or one
    /// of the values that are not state (see [`is_unpublished`]).
    pub fn is_unpublished(&self, cc: u32, property: &str, property_key: Option<&str>) -> bool {
        is_unpublished(cc, property, property_key)
    }

    /// Build the alias key from a ValueID's components.
    pub fn alias_key(cc: u32, endpoint: u32, property: &str, property_key: Option<&str>) -> String {
        match property_key {
            Some(pk) => format!("{cc}/{endpoint}/{property}/{pk}"),
            None => format!("{cc}/{endpoint}/{property}"),
        }
    }

    /// Translate a raw ZWave value to the `(attribute, value)` pairs it
    /// publishes as. `None` if this ValueID has no shared name (not in the
    /// alias table, or in a scale this does not know) — the caller publishes
    /// it under its synthetic name.
    pub fn translate(
        &self,
        node_id: u32,
        cc: u32,
        endpoint: u32,
        property: &str,
        property_key: Option<&str>,
        raw_value: &Value,
    ) -> Option<Vec<(String, Value)>> {
        self.with_node(node_id, |info| {
            translate_with(self, info, cc, endpoint, property, property_key, raw_value)
        })
    }

    /// The writes for a command to this node, and the attributes it could not
    /// write.
    pub fn commands(&self, node_id: u32, cmd: &Map<String, Value>) -> (Vec<Write>, Vec<String>) {
        self.with_node(node_id, |info| commands_for(info, cmd))
    }

    /// Whether a write to `attr` on this node reaches the device.
    pub fn writable(&self, node_id: u32, attr: &str) -> bool {
        self.with_node(node_id, |info| info.writable(attr))
    }

    /// The options a mode attribute takes on this node.
    pub fn options(&self, node_id: u32, attr: &str) -> Option<Vec<String>> {
        self.with_node(node_id, |info| info.options(attr))
    }

    /// The attribute name a value publishes under when nothing maps it.
    pub fn synthetic_name(
        &self,
        node_id: u32,
        cc: u32,
        endpoint: u32,
        property: &str,
        property_key: Option<&str>,
    ) -> String {
        self.with_node(node_id, |info| match info.role(endpoint) {
            EpRole::Channel(n) => format!(
                "{}{}",
                ep_prefix(n),
                synthetic_attr_name(cc, 0, property, property_key)
            ),
            _ => synthetic_attr_name(cc, endpoint, property, property_key),
        })
    }
}

fn translate_with(
    t: &Translator,
    info: &NodeInfo,
    cc: u32,
    endpoint: u32,
    property: &str,
    property_key: Option<&str>,
    raw: &Value,
) -> Option<Vec<(String, Value)>> {
    let role = info.role(endpoint);
    let prefix = match role {
        EpRole::Root => String::new(),
        EpRole::Channel(n) => ep_prefix(n),
        EpRole::Raw => return None,
    };
    let named = |pairs: Vec<(String, Value)>| {
        pairs
            .into_iter()
            .map(|(k, v)| (format!("{prefix}{k}"), v))
            .collect::<Vec<_>>()
    };
    // Window Covering: which parameter it is says what the value means.
    if cc == 106 && property == "currentValue" {
        if role != EpRole::Root {
            return None;
        }
        let p: u64 = property_key?.parse().ok()?;
        let level = num(raw)?;
        return if WC_TILT_PARAMS.contains(&p) {
            Some(vec![(
                "tilt_pct".into(),
                json!(convert::slat_to_tilt_pct(level)),
            )])
        } else if WC_POSITION_PARAMS.contains(&p) {
            Some(vec![(
                "position".into(),
                json!(convert::level_to_pct(level)),
            )])
        } else {
            None
        };
    }
    let key = Translator::alias_key(cc, 0, property, property_key);
    let (attr, transform) = t.forward.get(&key)?;
    let unit_key = match role {
        EpRole::Root => key.clone(),
        _ => Translator::alias_key(cc, endpoint, property, property_key),
    };
    let ctx = Ctx {
        unit: info.unit(&unit_key),
        cover: info.cover,
    };
    transform.apply(attr, raw, ctx).map(named)
}

/// Deterministic attribute name for a value with no alias entry. Used so
/// every z-wave value the device exposes is at least visible — without
/// this, unmapped command classes silently drop. Naming convention:
///
///   `cc{cc}_e{endpoint}_{property}[_pk{key}]`
///
/// Endpoint is omitted when 0 (the default root endpoint) so common
/// values stay short. Property is normalized to snake_case ASCII; any
/// other characters become `_`.
pub fn synthetic_attr_name(
    cc: u32,
    endpoint: u32,
    property: &str,
    property_key: Option<&str>,
) -> String {
    let prop_safe = sanitize_segment(property);
    let mut name = if endpoint == 0 {
        format!("cc{cc}_{prop_safe}")
    } else {
        format!("cc{cc}_e{endpoint}_{prop_safe}")
    };
    if let Some(pk) = property_key {
        let pk_safe = sanitize_segment(pk);
        name.push_str("_pk");
        name.push_str(&pk_safe);
    }
    name
}

fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Stringify a `propertyKey` Value for use in alias keys.
/// Returns `None` if the key is null/absent (omit from key).
pub fn property_key_str(pk: &Value) -> Option<String> {
    match pk {
        Value::Null => None,
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Translate with no node knowledge (a plain dimmer, standard units).
    fn tr(cc: u32, prop: &str, pk: Option<&str>, v: Value) -> Option<Vec<(String, Value)>> {
        tr_with(&NodeInfo::default(), cc, prop, pk, v)
    }

    fn tr_with(
        info: &NodeInfo,
        cc: u32,
        prop: &str,
        pk: Option<&str>,
        v: Value,
    ) -> Option<Vec<(String, Value)>> {
        translate_with(&Translator::new(), info, cc, 0, prop, pk, &v)
    }

    /// The single attribute a value publishes as.
    fn one(cc: u32, prop: &str, pk: Option<&str>, v: Value) -> (String, Value) {
        let mut out = tr(cc, prop, pk, v).expect("aliased");
        assert_eq!(out.len(), 1, "{out:?}");
        out.remove(0)
    }

    fn cmd(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn dimmer() -> NodeInfo {
        NodeInfo {
            multilevel_switch: true,
            ..Default::default()
        }
    }

    fn with_unit(key: &str, unit: &str) -> NodeInfo {
        let mut info = NodeInfo::default();
        info.units.insert(key.into(), unit.into());
        info
    }

    fn w(cc: u32, prop: &str, key: Option<Value>, value: Value, attr: &str) -> Write {
        let mut w = write(cc, prop, key, value);
        w.attribute = attr.into();
        w
    }

    fn node(j: Value) -> NodeState {
        NodeState::from_value(&j).expect("fixture parses")
    }

    #[test]
    fn a_lock_s_codes_are_never_published() {
        let t = Translator::new();
        assert!(t.is_unpublished(99, "userCode", Some("1")));
        assert!(t.is_unpublished(99, "adminCode", None));
        // Every User Code value — the slots' status included — is the Codes
        // tab's, not state.
        assert!(t.is_unpublished(99, "userIdStatus", Some("1")));
    }

    #[test]
    fn a_target_is_not_published_as_the_device_s_state() {
        let t = Translator::new();
        for (cc, prop) in [
            (37, "targetValue"),
            (38, "targetValue"),
            (98, "targetMode"),
            (51, "targetColor"),
            (102, "targetState"),
            (106, "targetValue"),
        ] {
            assert!(t.is_unpublished(cc, prop, None), "{cc}/{prop}");
            assert!(tr(cc, prop, None, Value::Bool(true)).is_none());
        }
        // A value that is both read and written stays published.
        assert!(!t.is_unpublished(64, "mode", None));
    }

    /// NOISE: identity, settings, codes, raw alarm numbers and a switch's
    /// transition are not state.
    #[test]
    fn what_is_not_state_is_not_published() {
        for (cc, prop, pk) in [
            (114, "manufacturerId", None),
            (134, "firmwareVersions", None),
            (94, "zwavePlusVersion", None),
            (132, "controllerNodeId", None),
            (132, "wakeUpInterval", None),
            (152, "securityClasses", None),
            (159, "securityClasses", None),
            (112, "3", None),
            (32, "currentValue", None),
            (37, "duration", None),
            (38, "duration", None),
            (38, "restorePrevious", None),
            (128, "chargingStatus", None),
            (128, "temperature", None),
            (98, "boltStatus", None),
            (98, "latchStatus", None),
            (98, "lockTimeout", None),
            (113, "alarmType", None),
            (113, "alarmLevel", None),
            (113, "Access Control", Some("Keypad state")),
            (50, "reset", None),
            (117, "rf", None),
        ] {
            assert!(is_unpublished(cc, prop, pk), "{cc}/{prop}/{pk:?}");
        }
        for (cc, prop, pk) in [
            (37, "currentValue", None),
            (128, "level", None),
            (128, "isLow", None),
            (98, "currentMode", None),
            (98, "doorStatus", None),
            (113, "Access Control", Some("Door state")),
            (117, "local", None),
            (50, "value", Some("66049")),
        ] {
            assert!(!is_unpublished(cc, prop, pk), "{cc}/{prop}/{pk:?}");
        }
    }

    #[test]
    fn a_dimmer_publishes_percent_and_on() {
        let out = tr(38, "currentValue", None, json!(99)).unwrap();
        assert_eq!(
            out,
            vec![
                ("brightness_pct".into(), json!(100)),
                ("on".into(), json!(true))
            ]
        );
        let out = tr(38, "currentValue", None, json!(1)).unwrap();
        assert_eq!(out[0], ("brightness_pct".into(), json!(1)));
        let out = tr(38, "currentValue", None, json!(0)).unwrap();
        assert_eq!(
            out,
            vec![
                ("brightness_pct".into(), json!(0)),
                ("on".into(), json!(false))
            ]
        );
    }

    #[test]
    fn a_multilevel_cover_publishes_position() {
        let info = NodeInfo {
            multilevel_switch: true,
            cover: true,
            ..Default::default()
        };
        let out = tr_with(&info, 38, "currentValue", None, json!(99)).unwrap();
        assert_eq!(out, vec![("position".into(), json!(100))]);
        let (w, _) = commands_for(&info, &cmd(json!({ "position": 50 })));
        assert_eq!(w[0].command_class, 38);
        assert_eq!(w[0].value, json!(50));
        assert!(!info.writable("brightness_pct"));
        assert!(!info.writable("on"));
    }

    #[test]
    fn a_window_covering_publishes_position_and_tilt() {
        assert_eq!(
            one(106, "currentValue", Some("13"), json!(99)),
            ("position".into(), json!(100))
        );
        assert_eq!(
            one(106, "currentValue", Some("23"), json!(50)),
            ("tilt_pct".into(), json!(100))
        );
        // A parameter with no position says nothing under a shared name.
        assert!(tr(106, "currentValue", Some("12"), json!(1)).is_none());
        let info = NodeInfo {
            wc_position: Some(13),
            wc_tilt: Some(23),
            ..Default::default()
        };
        let (w, _) = commands_for(
            &info,
            &cmd(json!({ "position": 100, "tilt_pct": 100, "transition_ms": 3000 })),
        );
        let pos = w
            .iter()
            .find(|w| w.property_key == Some(json!(13)))
            .unwrap();
        assert_eq!((pos.command_class, pos.value.clone()), (106, json!(99)));
        assert_eq!(pos.options, Some(json!({ "transitionDuration": "3s" })));
        let tilt = w
            .iter()
            .find(|w| w.property_key == Some(json!(23)))
            .unwrap();
        assert_eq!(tilt.value, json!(50));
    }

    #[test]
    fn a_barrier_is_a_garage_door_not_a_blind() {
        let out = tr(102, "currentState", None, json!(255)).unwrap();
        assert_eq!(
            out,
            vec![
                ("open".into(), json!(true)),
                ("barrier_state".into(), json!("open"))
            ]
        );
        let out = tr(102, "currentState", None, json!(252)).unwrap();
        assert_eq!(out[1], ("barrier_state".into(), json!("closing")));
        let out = tr(102, "currentState", None, json!(0)).unwrap();
        assert_eq!(out[0], ("open".into(), json!(false)));
        // CC 102 currentValue is not a position.
        assert!(tr(102, "currentValue", None, json!(50)).is_none());
        let info = NodeInfo {
            barrier: true,
            ..Default::default()
        };
        let (wr, _) = commands_for(&info, &cmd(json!({ "open": false })));
        assert_eq!(wr, vec![w(102, "targetState", None, json!(0), "open")]);
        // A contact sensor's `open` is not writable.
        let (wr, rejected) = commands_for(&NodeInfo::default(), &cmd(json!({ "open": true })));
        assert!(wr.is_empty());
        assert_eq!(rejected, vec!["open".to_string()]);
    }

    #[test]
    fn brightness_writes_as_a_zwave_level() {
        let d = dimmer();
        let lvl = |c: Value| commands_for(&d, &cmd(c)).0;
        assert_eq!(lvl(json!({ "brightness_pct": 100 }))[0].value, json!(99));
        assert_eq!(lvl(json!({ "brightness_pct": 1 }))[0].value, json!(1));
        assert_eq!(lvl(json!({ "brightness_pct": 0 }))[0].value, json!(0));
        assert_eq!(lvl(json!({ "brightness_pct": 0.3 }))[0].value, json!(1));
    }

    #[test]
    fn on_restores_the_dimmer_s_last_level() {
        let d = dimmer();
        let wr = commands_for(&d, &cmd(json!({ "on": true }))).0;
        assert_eq!(wr, vec![w(38, "targetValue", None, json!(255), "on")]);
        let wr = commands_for(&d, &cmd(json!({ "on": false }))).0;
        assert_eq!(wr, vec![w(38, "targetValue", None, json!(0), "on")]);
        // With a level, the level says how on.
        let wr = commands_for(&d, &cmd(json!({ "on": true, "brightness_pct": 40 }))).0;
        assert_eq!(
            wr,
            vec![w(38, "targetValue", None, json!(40), "brightness_pct")]
        );
        // Off wins over a level sent with it.
        let wr = commands_for(&d, &cmd(json!({ "on": false, "brightness_pct": 40 }))).0;
        assert_eq!(wr, vec![w(38, "targetValue", None, json!(0), "on")]);
    }

    #[test]
    fn a_fade_goes_with_the_level() {
        let wr = commands_for(
            &dimmer(),
            &cmd(json!({ "brightness_pct": 50, "transition_ms": 1500 })),
        )
        .0;
        assert_eq!(wr.len(), 1);
        assert_eq!(wr[0].options, Some(json!({ "transitionDuration": "2s" })));
    }

    #[test]
    fn a_relay_is_switched_with_binary_switch() {
        let info = NodeInfo {
            binary_switch: true,
            ..Default::default()
        };
        let wr = commands_for(&info, &cmd(json!({ "on": true }))).0;
        assert_eq!(wr, vec![w(37, "targetValue", None, json!(true), "on")]);
        // A node nothing is known about yet still gets a relay write.
        let wr = commands_for(&NodeInfo::default(), &cmd(json!({ "on": false }))).0;
        assert_eq!(wr, vec![w(37, "targetValue", None, json!(false), "on")]);
        assert_eq!(
            one(37, "currentValue", None, json!(true)),
            ("on".into(), json!(true))
        );
    }

    #[test]
    fn setpoints_are_celsius_whatever_the_thermostat_uses() {
        let f = with_unit("67/0/setpoint/1", "°F");
        let out = tr_with(&f, 67, "setpoint", Some("1"), json!(68)).unwrap();
        assert_eq!(out, vec![("heat_setpoint".into(), json!(20))]);
        let out = tr(67, "setpoint", Some("2"), json!(24.5)).unwrap();
        assert_eq!(out, vec![("cool_setpoint".into(), json!(24.5))]);
        let (wr, _) = commands_for(&f, &cmd(json!({ "heat_setpoint": 20 })));
        assert_eq!(
            wr,
            vec![w(
                67,
                "setpoint",
                Some(json!(1)),
                json!(68),
                "heat_setpoint"
            )]
        );
        let (wr, _) = commands_for(&NodeInfo::default(), &cmd(json!({ "cool_setpoint": 24.5 })));
        assert_eq!(
            wr,
            vec![w(
                67,
                "setpoint",
                Some(json!(2)),
                json!(24.5),
                "cool_setpoint"
            )]
        );
    }

    #[test]
    fn thermostat_mode_action_and_fan_are_words() {
        assert_eq!(
            one(64, "mode", None, json!(1)),
            ("hvac_mode".into(), json!("heat"))
        );
        assert_eq!(
            one(64, "mode", None, json!(6)),
            ("hvac_mode".into(), json!("fan_only"))
        );
        assert_eq!(
            one(64, "mode", None, json!(13)),
            ("hvac_mode".into(), json!("away"))
        );
        assert_eq!(
            one(66, "state", None, json!(2)),
            ("hvac_action".into(), json!("cooling"))
        );
        assert_eq!(
            one(68, "mode", None, json!(1)),
            ("fan_mode".into(), json!("low"))
        );
        let info = NodeInfo::default();
        let (wr, _) = commands_for(&info, &cmd(json!({ "hvac_mode": "cool" })));
        assert_eq!(wr, vec![w(64, "mode", None, json!(2), "hvac_mode")]);
        // The old spelling is still accepted.
        let (wr, _) = commands_for(&info, &cmd(json!({ "mode": "off" })));
        assert_eq!(wr, vec![w(64, "mode", None, json!(0), "mode")]);
        let (wr, _) = commands_for(&info, &cmd(json!({ "fan_mode": "circulation" })));
        assert_eq!(wr, vec![w(68, "mode", None, json!(6), "fan_mode")]);
        let (wr, rejected) = commands_for(&info, &cmd(json!({ "hvac_mode": "warp" })));
        assert!(wr.is_empty());
        assert_eq!(rejected, vec!["hvac_mode".to_string()]);
    }

    #[test]
    fn supported_modes_come_from_metadata() {
        let mut info = NodeInfo::default();
        info.learn_metadata(
            "64/0/mode".into(),
            &json!({ "states": { "0": "Off", "1": "Heat", "2": "Cool", "11": "Energy heat" } }),
        );
        assert_eq!(
            info.options("hvac_mode").unwrap(),
            vec!["off", "heat", "cool", "energy_heat"]
        );
        assert!(info.options("fan_mode").is_none());
    }

    #[test]
    fn colour_is_rgb_and_kelvin() {
        let out = tr(
            51,
            "currentColor",
            None,
            json!({ "warmWhite": 255, "coldWhite": 0, "red": 10, "green": 20, "blue": 30 }),
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                ("color_rgb".into(), json!({ "r": 10, "g": 20, "b": 30 })),
                ("color_temp".into(), json!(2700)),
            ]
        );
        // Whites off: showing colour, so no white temperature.
        let out = tr(
            51,
            "currentColor",
            None,
            json!({ "warmWhite": 0, "coldWhite": 0, "red": 255, "green": 0, "blue": 0 }),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        let info = NodeInfo {
            color_components: ["warmWhite", "coldWhite", "red", "green", "blue"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        };
        let (wr, _) = commands_for(
            &info,
            &cmd(json!({ "color_rgb": { "r": 255, "g": 0, "b": 0 } })),
        );
        assert_eq!(
            wr[0].value,
            json!({ "red": 255, "green": 0, "blue": 0, "warmWhite": 0, "coldWhite": 0 })
        );
        let (wr, _) = commands_for(&info, &cmd(json!({ "color_temp": 6500 })));
        assert_eq!(
            wr[0].value,
            json!({ "warmWhite": 0, "coldWhite": 255, "red": 0, "green": 0, "blue": 0 })
        );
        // No white channels: no colour temperature to write.
        let rgb_only = NodeInfo {
            color_components: ["red", "green", "blue"].map(String::from).to_vec(),
            ..Default::default()
        };
        assert!(commands_for(&rgb_only, &cmd(json!({ "color_temp": 3000 })))
            .0
            .is_empty());
    }

    #[test]
    fn sensors_are_read_in_their_own_scale() {
        let f = with_unit("49/0/Air temperature", "°F");
        assert_eq!(
            tr_with(&f, 49, "Air temperature", None, json!(212)).unwrap(),
            vec![("temperature".into(), json!(100))]
        );
        let lux = with_unit("49/0/Illuminance", "Lux");
        assert_eq!(
            tr_with(&lux, 49, "Illuminance", None, json!(120)).unwrap(),
            vec![("illuminance".into(), json!(120))]
        );
        let pct = with_unit("49/0/Illuminance", "%");
        assert_eq!(
            tr_with(&pct, 49, "Illuminance", None, json!(40)).unwrap(),
            vec![("light_level_pct".into(), json!(40))]
        );
        let kpa = with_unit("49/0/Atmospheric pressure", "kPa");
        assert_eq!(
            tr_with(&kpa, 49, "Atmospheric pressure", None, json!(101.3)).unwrap(),
            vec![("pressure".into(), json!(1013))]
        );
        let ma = with_unit("49/0/Current", "mA");
        assert_eq!(
            tr_with(&ma, 49, "Current", None, json!(500)).unwrap(),
            vec![("current_a".into(), json!(0.5))]
        );
        // An unknown scale falls back to the raw synthetic name.
        let k = with_unit("49/0/Air temperature", "K");
        assert!(tr_with(&k, 49, "Air temperature", None, json!(290)).is_none());
    }

    #[test]
    fn old_sensor_names_are_gone() {
        assert_eq!(one(128, "level", None, json!(80)).0, "battery_pct");
        assert_eq!(one(48, "Door/Window", None, json!(true)).0, "open");
        assert_eq!(one(48, "Tamper", None, json!(true)).0, "tampered");
        assert_eq!(one(49, "CO2 level", None, json!(600)).0, "co2");
    }

    /// M7: the names zwave-js's Notifications table really uses.
    #[test]
    fn notification_variables_by_their_zwave_js_names() {
        assert_eq!(
            one(113, "Home Security", Some("Cover status"), json!(3)),
            ("tampered".into(), json!(true))
        );
        assert_eq!(
            one(113, "Home Security", Some("Cover status"), json!(0)),
            ("tampered".into(), json!(false))
        );
        assert_eq!(
            one(113, "Home Security", Some("Motion sensor status"), json!(8)),
            ("motion".into(), json!(true))
        );
        assert_eq!(
            one(113, "Smoke Alarm", Some("Sensor status"), json!(2)),
            ("smoke".into(), json!(true))
        );
        assert_eq!(
            one(113, "CO Alarm", Some("Sensor status"), json!(0)),
            ("co".into(), json!(false))
        );
        assert_eq!(
            one(113, "Water Alarm", Some("Sensor status"), json!(2)),
            ("water_detected".into(), json!(true))
        );
        assert_eq!(
            one(113, "Access Control", Some("Lock state"), json!(11)),
            ("jammed".into(), json!(true))
        );
        assert_eq!(
            one(113, "Access Control", Some("Lock state"), json!(0)),
            ("jammed".into(), json!(false))
        );
        assert_eq!(
            one(113, "Power Management", Some("Over-load status"), json!(8)),
            ("overload".into(), json!(true))
        );
        // Names that never existed are not aliased any more.
        assert!(tr(113, "Access Control", Some("Opening state"), json!(1)).is_none());
        assert!(tr(
            113,
            "Home Security",
            Some("Tampering, Product cover removed"),
            json!(3)
        )
        .is_none());
        assert!(tr(113, "Smoke Alarm", Some("Smoke sensor status"), json!(2)).is_none());
    }

    /// H4: only 255 is locked; 254 is "don't know", not locked.
    #[test]
    fn only_secured_is_locked() {
        let locked = |m: u64| one(98, "currentMode", None, json!(m)).1;
        assert_eq!(locked(255), json!(true));
        for unsecured in [0, 1, 16, 17, 32, 33] {
            assert_eq!(locked(unsecured), json!(false), "mode {unsecured}");
        }
        assert_eq!(locked(254), Value::Null);
        let (wr, _) = commands_for(&NodeInfo::default(), &cmd(json!({ "locked": true })));
        assert_eq!(wr, vec![w(98, "targetMode", None, json!(255), "locked")]);
        let (wr, _) = commands_for(&NodeInfo::default(), &cmd(json!({ "locked": false })));
        assert_eq!(wr, vec![w(98, "targetMode", None, json!(0), "locked")]);
    }

    #[test]
    fn a_lock_s_door_status_is_open() {
        assert_eq!(
            one(98, "doorStatus", None, json!("open")),
            ("open".into(), json!(true))
        );
        assert_eq!(
            one(98, "doorStatus", None, json!("closed")),
            ("open".into(), json!(false))
        );
    }

    #[test]
    fn auto_lock_writes_where_the_lock_takes_it() {
        let v3 = NodeInfo {
            lock: true,
            ..Default::default()
        };
        let (wr, _) = commands_for(
            &v3,
            &cmd(json!({ "lock_operation_type": 2, "lock_timeout_secs": 30 })),
        );
        assert_eq!(wr.len(), 2);
        assert!(v3.writable("lock_timeout_secs"));
        assert!(!v3.writable("lock_auto_relock_secs"));
        let (wr, rej) = commands_for(&v3, &cmd(json!({ "lock_operation_type": 7 })));
        assert!(wr.is_empty());
        assert_eq!(rej, vec!["lock_operation_type"]);
        let v4 = NodeInfo {
            lock: true,
            auto_relock: true,
            ..Default::default()
        };
        let (wr, _) = commands_for(&v4, &cmd(json!({ "lock_auto_relock_secs": 60 })));
        assert_eq!(
            wr,
            vec![w(
                98,
                "autoRelockTime",
                None,
                json!(60),
                "lock_auto_relock_secs"
            )]
        );
    }

    #[test]
    fn child_lock_is_protection() {
        assert_eq!(
            one(117, "local", None, json!(2)),
            ("child_lock".into(), json!(true))
        );
        assert_eq!(
            one(117, "local", None, json!(0)),
            ("child_lock".into(), json!(false))
        );
        let info = NodeInfo {
            protection: Some(vec![0, 1]),
            ..Default::default()
        };
        let (wr, _) = commands_for(&info, &cmd(json!({ "child_lock": true })));
        assert_eq!(wr[0].value, json!(1));
        let info = NodeInfo {
            protection: Some(vec![0, 1, 2]),
            ..Default::default()
        };
        let (wr, _) = commands_for(&info, &cmd(json!({ "child_lock": true })));
        assert_eq!(wr[0].value, json!(2));
        let (wr, _) = commands_for(&info, &cmd(json!({ "child_lock": false })));
        assert_eq!(wr[0].value, json!(0));
        assert!(!NodeInfo::default().writable("child_lock"));
    }

    #[test]
    fn meter_propertykey() {
        assert_eq!(
            one(50, "value", Some("65537"), json!(0.002)),
            ("energy_kwh".into(), json!(0.002))
        );
        assert_eq!(one(50, "value", Some("66049"), json!(120.5)).0, "power_w");
        assert_eq!(one(50, "value", Some("66561"), json!(120.077)).0, "voltage");
        assert_eq!(
            one(50, "value", Some("67073"), json!(0.95)).0,
            "power_factor"
        );
        // L4: rateType 0 (Meter v1/v2) is consumption too.
        assert_eq!(one(50, "value", Some("65536"), json!(1.5)).0, "energy_kwh");
        assert_eq!(one(50, "value", Some("66048"), json!(2.7)).0, "power_w");
    }

    #[test]
    fn unknown_cc_returns_none() {
        assert!(tr(999, "unknownProp", None, Value::Bool(true)).is_none());
    }

    #[test]
    fn metadata_and_ignored_keys_are_not_attributes() {
        let (wr, rejected) = commands_for(
            &dimmer(),
            &cmd(json!({ "_hc": {}, "correlation_id": "x", "transition_ms": 500 })),
        );
        assert!(wr.is_empty() && rejected.is_empty());
    }

    #[test]
    fn synthetic_name_matches_alias_shape() {
        assert_eq!(
            synthetic_attr_name(50, 0, "value", Some("65541")),
            "cc50_value_pk65541"
        );
        assert_eq!(
            synthetic_attr_name(49, 1, "Air temperature", None),
            "cc49_e1_air_temperature"
        );
        assert_eq!(
            synthetic_attr_name(113, 0, "Home Security[2]", Some("Sensor")),
            "cc113_home_security_2__pksensor"
        );
    }

    #[test]
    fn cc48_contact_sensor() {
        assert_eq!(
            one(48, "Door/Window", None, Value::Bool(false)),
            ("open".into(), json!(false))
        );
        assert_eq!(one(48, "Motion", None, Value::Bool(true)).0, "motion");
        assert!(tr(48, "Door/Window Status", None, Value::Bool(true)).is_none());
    }

    /// M6: 22 is open, and so are 0x1600 / 0x1601 — 22 with a parameter.
    #[test]
    fn cc113_access_control_door_state() {
        let ds = |v| one(113, "Access Control", Some("Door state"), json!(v));
        assert_eq!(ds(22), ("open".into(), json!(true)));
        assert_eq!(ds(5632), ("open".into(), json!(true)));
        assert_eq!(ds(5633), ("open".into(), json!(true)));
        assert_eq!(ds(23), ("open".into(), json!(false)));
        assert_eq!(ds(0), ("open".into(), json!(false)));
        let simple = |v| one(113, "Access Control", Some("Door state (simple)"), json!(v));
        assert_eq!(simple(22), ("open".into(), json!(true)));
        assert_eq!(simple(23), ("open".into(), json!(false)));
    }

    #[test]
    fn cc113_no_propertykey_does_not_match() {
        assert!(tr(113, "Access Control", None, json!(22)).is_none());
        assert!(tr(113, "Home Security", None, json!(1)).is_none());
    }

    #[test]
    fn node_info_is_learned_from_the_node() {
        let n = node(json!({
            "nodeId": 9,
            "deviceClass": {
                "generic": { "label": "Multilevel Switch" },
                "specific": { "label": "Motor Control Class C" }
            },
            "values": [
                { "commandClass": 38, "endpoint": 0, "property": "currentValue", "value": 10 },
                { "commandClass": 67, "endpoint": 0, "property": "setpoint", "propertyKey": 1,
                  "value": 70, "metadata": { "unit": "°F" } },
                { "commandClass": 51, "endpoint": 0, "property": "currentColor",
                  "value": { "red": 0, "green": 0, "blue": 0 } }
            ]
        }));
        let t = Translator::new();
        t.learn_node(&n);
        assert_eq!(
            t.translate(9, 38, 0, "currentValue", None, &json!(99))
                .unwrap(),
            vec![("position".into(), json!(100))]
        );
        assert_eq!(
            t.translate(9, 67, 0, "setpoint", Some("1"), &json!(50))
                .unwrap(),
            vec![("heat_setpoint".into(), json!(10))]
        );
        assert!(t.writable(9, "position"));
        assert!(t.writable(9, "color_rgb"));
        assert!(!t.writable(9, "color_temp"));
        t.forget_node(9);
        assert!(!t.writable(9, "position"));
    }

    /// M8: an outlet whose switch zwave-js reports only on endpoint 1 (its
    /// root values hidden) is that endpoint — `on`, writable, written there.
    #[test]
    fn one_endpoint_with_the_root_hidden_is_the_device() {
        let n = node(json!({
            "nodeId": 12,
            "values": [
                { "commandClass": 37, "endpoint": 1, "property": "currentValue", "value": true },
                { "commandClass": 37, "endpoint": 1, "property": "targetValue", "value": true },
                { "commandClass": 50, "endpoint": 1, "property": "value", "propertyKey": 66049, "value": 3.5,
                  "metadata": { "unit": "W" } },
                { "commandClass": 114, "endpoint": 0, "property": "manufacturerId", "value": 99 }
            ]
        }));
        let t = Translator::new();
        t.learn_node(&n);
        assert_eq!(
            t.translate(12, 37, 1, "currentValue", None, &json!(true)),
            Some(vec![("on".into(), json!(true))])
        );
        assert_eq!(
            t.translate(12, 50, 1, "value", Some("66049"), &json!(3.5)),
            Some(vec![("power_w".into(), json!(3.5))])
        );
        assert!(t.writable(12, "on"));
        let (wr, rej) = t.commands(12, &cmd(json!({ "on": false })));
        assert!(rej.is_empty());
        assert_eq!(wr.len(), 1);
        assert_eq!((wr[0].command_class, wr[0].endpoint), (37, 1));
    }

    /// M8: a real power strip — one channel per endpoint, each written where
    /// its value came from.
    #[test]
    fn several_endpoints_are_channels() {
        let n = node(json!({
            "nodeId": 13,
            "values": [
                { "commandClass": 37, "endpoint": 1, "property": "currentValue", "value": true },
                { "commandClass": 37, "endpoint": 2, "property": "currentValue", "value": false },
                { "commandClass": 50, "endpoint": 2, "property": "value", "propertyKey": 66049, "value": 0 }
            ]
        }));
        let t = Translator::new();
        t.learn_node(&n);
        assert_eq!(
            t.translate(13, 37, 2, "currentValue", None, &json!(false)),
            Some(vec![("ep2_on".into(), json!(false))])
        );
        assert_eq!(
            t.translate(13, 50, 2, "value", Some("66049"), &json!(0)),
            Some(vec![("ep2_power_w".into(), json!(0))])
        );
        assert!(t.writable(13, "ep1_on"));
        assert!(t.writable(13, "ep2_on"));
        assert!(!t.writable(13, "ep3_on"));
        assert!(!t.writable(13, "on"));
        let (wr, rej) = t.commands(13, &cmd(json!({ "ep2_on": true, "ep1_on": false })));
        assert!(rej.is_empty(), "{rej:?}");
        let mut got: Vec<(u32, Value, String)> = wr
            .iter()
            .map(|w| (w.endpoint, w.value.clone(), w.attribute.clone()))
            .collect();
        got.sort_by_key(|g| g.0);
        assert_eq!(
            got,
            vec![
                (1, json!(false), "ep1_on".to_string()),
                (2, json!(true), "ep2_on".to_string())
            ]
        );
        // An endpoint that is neither keeps its raw name.
        assert_eq!(
            t.synthetic_name(13, 49, 5, "Air temperature", None),
            "cc49_e5_air_temperature"
        );
    }

    #[test]
    fn ep_prefixes_parse() {
        assert_eq!(strip_ep_prefix("ep2_on"), Some((2, "on")));
        assert_eq!(strip_ep_prefix("ep12_power_w"), Some((12, "power_w")));
        assert_eq!(strip_ep_prefix("ep_on"), None);
        assert_eq!(strip_ep_prefix("ep2"), None);
        assert_eq!(strip_ep_prefix("energy_kwh"), None);
    }
}
