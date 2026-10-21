//! What a YoLink device says, in homeCore's words — pure, so every shape is
//! tested against what the hub really sends.
//!
//! **Two shapes of the same thing.** A real-time report carries the device's
//! fields flat (`{"state": "alert", "battery": 4, …}`); a sensor's `getState`
//! reply wraps them (`{"online": true, "state": {"state": "alert", "battery":
//! 4, …}, "reportAt": …}`). [`body`] flattens the second into the first, so
//! every translator reads one shape. That wrapper is why a leak sensor used to
//! read dry on every poll during a leak: the old code looked for a bool
//! `alarm` YoLink never sends, then for a flat `state`, and never under
//! `state.state`.
//!
//! A lock's `getState` is not wrapped — its `state` is `{"lock", "door"}` —
//! and an outlet's `state` is a plain string, so [`body`] only unwraps a
//! `state` object that holds the device's own fields.

use anyhow::{bail, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// DeviceKind — maps YoLink device types to HomeCore device types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceKind {
    /// Smart outlet (relay + optional power monitoring)
    Outlet,
    /// Same hardware class as Outlet
    SmartPlug,
    /// In-wall switch
    Switch,
    /// Multi-outlet power strip
    MultiOutlet,
    /// Door/window contact sensor
    DoorSensor,
    /// PIR motion sensor
    MotionSensor,
    /// Water leak sensor
    LeakSensor,
    /// Temperature (+ humidity) sensor
    THSensor,
    /// Vibration / shock sensor
    VibrationSensor,
    /// Smart lock (v1, YS7606)
    Lock,
    /// Smart lock v2 (YS7607: fingerprint, door sensor)
    LockV2,
    /// Siren / alarm
    Siren,
    /// Hub itself — not bridged as a device
    Hub,
    /// Anything not yet recognised
    Unknown(String),
}

impl DeviceKind {
    pub fn from_yolink_type(s: &str) -> Self {
        match s {
            "Outlet" => Self::Outlet,
            "SmartPlug" => Self::SmartPlug,
            "Switch" => Self::Switch,
            "MultiOutlet" => Self::MultiOutlet,
            "DoorSensor" => Self::DoorSensor,
            "MotionSensor" => Self::MotionSensor,
            "LeakSensor" => Self::LeakSensor,
            "THSensor" => Self::THSensor,
            "VibrationSensor" => Self::VibrationSensor,
            "Lock" => Self::Lock,
            "LockV2" => Self::LockV2,
            "Siren" => Self::Siren,
            "Hub" => Self::Hub,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// HomeCore device_type string (matched against the device-types catalog).
    pub fn homecore_device_type(&self) -> &str {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet => "switch",
            Self::DoorSensor => "contact_sensor",
            Self::MotionSensor => "motion_sensor",
            Self::LeakSensor => "water_sensor",
            Self::VibrationSensor => "vibration_sensor",
            Self::THSensor => "temperature_sensor",
            Self::Lock | Self::LockV2 => "lock",
            Self::Siren => "switch",
            Self::Hub | Self::Unknown(_) => "unknown",
        }
    }

    /// Whether this device kind should be registered and bridged.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Hub | Self::Unknown(_))
    }

    pub fn is_relay(&self) -> bool {
        matches!(
            self,
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet
        )
    }

    pub fn is_lock(&self) -> bool {
        matches!(self, Self::Lock | Self::LockV2)
    }

    /// Runs on batteries, sleeps, and checks in now and then — so silence is
    /// the only sign it has died. Relays are mains-powered and answer polls.
    pub fn is_battery(&self) -> bool {
        matches!(
            self,
            Self::DoorSensor
                | Self::MotionSensor
                | Self::LeakSensor
                | Self::THSensor
                | Self::VibrationSensor
                | Self::Lock
                | Self::LockV2
        )
    }

    /// Words for the page: "leak sensor", "lock".
    pub fn words(&self) -> &'static str {
        match self {
            Self::Outlet | Self::SmartPlug => "plug",
            Self::Switch | Self::MultiOutlet => "switch",
            Self::DoorSensor => "door sensor",
            Self::MotionSensor => "motion sensor",
            Self::LeakSensor => "leak sensor",
            Self::THSensor => "temperature sensor",
            Self::VibrationSensor => "vibration sensor",
            Self::Lock | Self::LockV2 => "lock",
            Self::Siren => "siren",
            Self::Hub => "hub",
            Self::Unknown(_) => "device",
        }
    }
}

// ---------------------------------------------------------------------------
// What a model can do
// ---------------------------------------------------------------------------

/// Plugs that meter power, by model: YS6602 (Smart Plug with Power
/// Monitoring) and YS6614 (Mini Plug with energy monitoring). Any other plug
/// that ever reports a non-zero power is counted as metering too.
pub const METERING_MODELS: &[&str] = &["YS6602", "YS6614"];

/// Temperature sensors with no humidity element: the YS8004 weatherproof
/// probe (a freezer, a fridge, a pool). Their `humidity` is not a reading.
pub const TEMPERATURE_ONLY_MODELS: &[&str] = &["YS8004"];

fn model_in(model: Option<&str>, list: &[&str]) -> bool {
    model.is_some_and(|m| list.iter().any(|x| m.starts_with(x)))
}

/// What a device can report, from its model and from what it has reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    /// A plug with a power meter.
    pub metering: bool,
    /// A temperature probe with no humidity.
    pub temp_only: bool,
}

impl Caps {
    pub fn from_model(model: Option<&str>) -> Self {
        Self {
            metering: model_in(model, METERING_MODELS),
            temp_only: model_in(model, TEMPERATURE_ONLY_MODELS),
        }
    }
}

// ---------------------------------------------------------------------------
// Flattening
// ---------------------------------------------------------------------------

/// The device's own fields, flat — unwrapping a `getState` reply whose
/// `state` object holds them, and keeping the wrapper's `online`,
/// `reportAt`, `loraInfo` and `deviceId` beside them.
pub fn body(data: &Value) -> Value {
    let Some(inner) = data.get("state").and_then(Value::as_object) else {
        return data.clone();
    };
    // A lock's `state` is `{lock, door}` — its own state, not a wrapper.
    let wrapper = [
        "state",
        "battery",
        "temperature",
        "online",
        "alarm",
        "alarmState",
    ]
    .iter()
    .any(|k| inner.contains_key(*k));
    if !wrapper {
        return data.clone();
    }
    let mut out: Map<String, Value> = inner.clone();
    if let Some(top) = data.as_object() {
        for (k, v) in top {
            if k != "state" {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    Value::Object(out)
}

/// A YoLink time: ms since the epoch, or an RFC 3339 string.
pub fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(ms) = v.as_i64() {
        return Utc.timestamp_millis_opt(ms).single();
    }
    let s = v.as_str()?;
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    s.parse::<i64>()
        .ok()
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn flag(v: &Value) -> Option<bool> {
    v.as_bool()
}

// ---------------------------------------------------------------------------
// A reading
// ---------------------------------------------------------------------------

/// An alert a lock reported: its `type` and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockAlert {
    pub kind: String,
    pub source: Option<String>,
}

/// What one report or poll said, beside the attributes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// Attributes, in homeCore's names. A `null` removes one.
    pub attrs: Map<String, Value>,
    /// The hub's `online`, where it said.
    pub online: Option<bool>,
    /// When the device itself last reported (`reportAt`).
    pub report_at: Option<DateTime<Utc>>,
    /// LoRa signal in dBm; YoLink sends 0 for "not measured".
    pub signal: Option<i64>,
    /// YoLink's 0–4 battery level.
    pub battery_level: Option<u8>,
    pub version: Option<String>,
    /// A lock's `alert` (sticky in `getState`: the last one it ever had).
    pub alert: Option<LockAlert>,
    /// A door sensor's "still open" reminder (`alertType: openRemind`).
    pub open_remind: bool,
    /// A leak sensor's faults, in words: "probe fault", "freezing".
    pub faults: Vec<&'static str>,
    /// An outlet's raw `power` (tenths of a watt), before it is decided
    /// whether the plug meters at all.
    pub power_raw: Option<f64>,
}

/// YoLink's 0–4 battery level, from a number or a string.
pub fn battery_level(b: &Value) -> Option<u8> {
    num(&b["battery"]).map(|n| n.clamp(0.0, 4.0) as u8)
}

/// YoLink's power is in tenths of a watt: the house's metered plug read
/// `37` while the YoLink app showed 3.7 W.
pub fn watts(raw: f64) -> f64 {
    round1(raw / 10.0)
}

/// Read one report or `getState` reply. `None` when it carries nothing this
/// kind understands (an `Outlet.powerReport` is read by
/// [`DeviceKind::power_report`] instead).
pub fn read(kind: &DeviceKind, caps: &Caps, data: &Value) -> Option<Reading> {
    let b = body(data);
    let mut r = Reading {
        online: flag(&b["online"]),
        report_at: parse_time(&b["reportAt"]),
        signal: b["loraInfo"]["signal"].as_i64().filter(|s| *s != 0),
        battery_level: battery_level(&b),
        version: b["version"].as_str().map(str::to_string),
        ..Default::default()
    };
    let a = &mut r.attrs;
    let state = b["state"].as_str();

    match kind {
        DeviceKind::Outlet
        | DeviceKind::SmartPlug
        | DeviceKind::Switch
        | DeviceKind::MultiOutlet => {
            a.insert("on".into(), json!(relay_is_on(state?)));
            r.power_raw = num(&b["power"]);
        }
        DeviceKind::Siren => {
            let on = state.map(relay_is_on).unwrap_or(false);
            a.insert("on".into(), json!(on));
            a.insert("alarm".into(), json!(b["alarm"].as_bool().unwrap_or(on)));
        }
        DeviceKind::DoorSensor => {
            match state? {
                "open" => a.insert("open".into(), json!(true)),
                "closed" | "close" => a.insert("open".into(), json!(false)),
                // "error": the magnet is not where it was learnt. Not a door
                // state; leave `open` as it was.
                _ => None,
            };
            r.open_remind = b["alertType"].as_str() == Some("openRemind") && state == Some("open");
        }
        DeviceKind::MotionSensor => {
            a.insert("motion".into(), json!(state? == "alert"));
        }
        DeviceKind::VibrationSensor => {
            a.insert("vibration".into(), json!(state? == "alert"));
        }
        DeviceKind::LeakSensor => {
            // "alert" is wet; "normal" is dry. (Water-peak mode says "full".)
            a.insert(
                "water_detected".into(),
                json!(matches!(state?, "alert" | "full")),
            );
            if let Some(t) = num(&b["devTemperature"]) {
                a.insert("temperature".into(), json!(round1(t)));
            }
            let alarm = &b["alarmState"];
            let probe = alarm["detectorError"].as_bool();
            let freeze = alarm["freezeError"].as_bool();
            if probe == Some(true) {
                r.faults.push("probe fault");
            }
            if freeze == Some(true) {
                r.faults.push("freezing");
            }
            if probe.is_some() || freeze.is_some() {
                a.insert("probe_fault".into(), json!(probe == Some(true)));
                a.insert("freeze_warning".into(), json!(freeze == Some(true)));
            }
        }
        DeviceKind::THSensor => {
            let t = num(&b["temperature"]);
            let h = num(&b["humidity"]);
            if t.is_none() && h.is_none() {
                return None;
            }
            if let Some(t) = t {
                a.insert("temperature".into(), json!(round2(t)));
            }
            // A probe has no humidity element; what it sends is not a
            // reading. Nor is an exact 0 % — no room or freezer is that dry.
            match h {
                Some(h) if !caps.temp_only && h > 0.0 => {
                    a.insert("humidity".into(), json!(round1(h)));
                }
                _ if caps.temp_only => {
                    a.insert("humidity".into(), Value::Null);
                }
                _ => {}
            }
            // The sensor's own limits and alarms, set in the YoLink app:
            // shown, not changed.
            if let Some(lo) = num(&b["tempLimit"]["min"]) {
                a.insert("temp_limit_low".into(), json!(round1(lo)));
            }
            if let Some(hi) = num(&b["tempLimit"]["max"]) {
                a.insert("temp_limit_high".into(), json!(round1(hi)));
            }
            if let Some(cold) = b["alarm"]["lowTemp"].as_bool() {
                a.insert("too_cold".into(), json!(cold));
            }
            if let Some(warm) = b["alarm"]["highTemp"].as_bool() {
                a.insert("too_warm".into(), json!(warm));
            }
        }
        DeviceKind::Lock => {
            // v1: `state` is the bolt — flat in reports, under `state.state`
            // in `getState` (which `body` has flattened).
            match state? {
                "locked" => a.insert("locked".into(), json!(true)),
                "unlocked" => a.insert("locked".into(), json!(false)),
                _ => None,
            };
            r.alert = lock_alert(&b);
        }
        DeviceKind::LockV2 => {
            let lock = b["state"]["lock"].as_str()?;
            a.insert("locked".into(), json!(lock == "locked"));
            match b["state"]["door"].as_str() {
                Some("open") => {
                    a.insert("open".into(), json!(true));
                }
                Some("closed") => {
                    a.insert("open".into(), json!(false));
                }
                _ => {}
            }
            if let Some(v) = b["attributes"]["autoLock"].as_u64() {
                a.insert("auto_lock_secs".into(), json!(v));
            }
            if let Some(v) = b["attributes"]["soundLevel"].as_u64() {
                a.insert("sound_level".into(), json!(v.min(3)));
            }
            r.alert = lock_alert(&b);
        }
        DeviceKind::Hub | DeviceKind::Unknown(_) => return None,
    }

    if kind.is_battery() {
        if let Some(level) = r.battery_level {
            a.insert("battery_pct".into(), json!(u64::from(level) * 25));
            // Level 1 is "25 %", which core's 20 % threshold never reaches —
            // so say low outright, as YoLink's own app does at one bar.
            let flagged = b["alarm"]["lowBattery"].as_bool() == Some(true)
                || b["lowBattery"].as_bool() == Some(true);
            a.insert("battery_low".into(), json!(level <= 1 || flagged));
        }
    }
    Some(r)
}

fn lock_alert(b: &Value) -> Option<LockAlert> {
    let kind = b["alert"]["type"]
        .as_str()
        .or_else(|| b["alertType"].as_str())?;
    Some(LockAlert {
        kind: kind.to_string(),
        source: b["alert"]["source"]
            .as_str()
            .or_else(|| b["source"].as_str())
            .map(str::to_string),
    })
}

/// YoLink uses "open" for on and "closed"/"close" for off in relays.
fn relay_is_on(state_str: &str) -> bool {
    matches!(state_str, "open" | "on")
}

impl DeviceKind {
    /// `Outlet.powerReport`: an hourly history, `{"watts": [{"time", "watt"}]}`.
    ///
    /// YoLink documents neither the event nor its `watt`'s unit, so it is not
    /// published as a reading — the plug's own `power` (in `getState` and
    /// state reports) is. It is still evidence that the plug meters.
    pub fn is_power_report(event: &str) -> bool {
        event.ends_with(".powerReport")
    }

    // -----------------------------------------------------------------------
    // Commands: HomeCore cmd JSON → YoLink calls
    // -----------------------------------------------------------------------

    /// The YoLink calls a command makes: `(method suffix, params)`, in order.
    ///
    /// Two shapes only — attribute writes (`{"locked": true}`,
    /// `{"auto_lock_secs": 30}`) — and each lock generation has its own:
    /// LockV2 takes `{"state": {"lock": "locked"}}` and its settings through
    /// `setAttributes`; the v1 lock takes `{"state": "lock"}`.
    pub fn translate_command(&self, cmd: &Value) -> Result<Vec<(&'static str, Value)>> {
        let obj = cmd
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("a command is an object of attributes"))?;
        let mut calls = Vec::new();
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet | Self::Siren => {
                let on = cmd["on"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("needs a boolean 'on'"))?;
                // YoLink uses "open" for on and "close" for off
                calls.push((
                    "setState",
                    json!({ "state": if on { "open" } else { "close" } }),
                ));
            }
            Self::LockV2 => {
                if let Some(v) = obj.get("locked") {
                    let locked = v
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("'locked' needs true or false"))?;
                    calls.push((
                        "setState",
                        json!({ "state": { "lock": if locked { "locked" } else { "unlocked" } } }),
                    ));
                }
                let mut attrs = Map::new();
                if let Some(v) = obj.get("auto_lock_secs") {
                    let secs = v
                        .as_u64()
                        .filter(|s| *s <= 3600)
                        .ok_or_else(|| anyhow::anyhow!("'auto_lock_secs' needs 0–3600 seconds"))?;
                    attrs.insert("autoLock".into(), json!(secs));
                }
                if let Some(v) = obj.get("sound_level") {
                    let level = v
                        .as_u64()
                        .filter(|s| *s <= 3)
                        .ok_or_else(|| anyhow::anyhow!("'sound_level' needs 0–3 (mute to high)"))?;
                    attrs.insert("soundLevel".into(), json!(level));
                }
                if !attrs.is_empty() {
                    calls.push(("setAttributes", Value::Object(attrs)));
                }
                if calls.is_empty() {
                    bail!("a lock takes 'locked', 'auto_lock_secs' or 'sound_level'");
                }
            }
            Self::Lock => {
                let locked = cmd["locked"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("needs a boolean 'locked'"))?;
                calls.push((
                    "setState",
                    json!({ "state": if locked { "lock" } else { "unlock" } }),
                ));
            }
            Self::DoorSensor
            | Self::MotionSensor
            | Self::LeakSensor
            | Self::THSensor
            | Self::VibrationSensor => {
                bail!("a {} only reports; it takes no commands", self.words())
            }
            Self::Hub | Self::Unknown(_) => {
                bail!("{:?} is not a supported device kind", self)
            }
        }
        Ok(calls)
    }
}

// ---------------------------------------------------------------------------
// A lock's history
// ---------------------------------------------------------------------------

/// Every `last_lock_action`, for the schema.
pub const LOCK_ACTIONS: &[(&str, &str)] = &[
    ("locked", "Locked"),
    ("unlocked", "Unlocked"),
    ("lock_failed", "Couldn't lock"),
    ("unlock_failed", "Couldn't unlock"),
    ("jammed", "Jammed"),
];

/// Every `last_lock_method`, for the schema.
pub const LOCK_METHODS: &[(&str, &str)] = &[
    ("fingerprint", "Fingerprint"),
    ("code", "Code on the keypad"),
    ("card", "Card or fob"),
    ("key", "Key"),
    ("button", "Button on the lock"),
    ("app", "YoLink app"),
    ("homecore", "homeCore"),
    ("auto", "By itself"),
    ("remote", "Remotely"),
];

/// YoLink's alert `source`, as a `last_lock_method`.
pub fn method_of(source: &str) -> Option<&'static str> {
    let s = source.to_ascii_lowercase();
    Some(match s.as_str() {
        "fingerprint" | "finger" => "fingerprint",
        "password" | "code" | "keypad" | "pwd" => "code",
        "card" | "fob" | "nfc" => "card",
        "key" | "mechanical" => "key",
        "button" | "manual" | "hand" | "knob" | "inside" => "button",
        "app" => "app",
        "auto" | "autolock" | "auto_lock" => "auto",
        "remote" | "server" | "cloud" | "api" | "openapi" => "remote",
        _ => return None,
    })
}

/// What a lock alert `type` is, if it is a lock action or a failure:
/// `UnLockFailed` → `unlock_failed`.
pub fn alert_action(kind: &str) -> Option<&'static str> {
    let k = kind.to_ascii_lowercase().replace(['_', ' '], "");
    Some(match k.as_str() {
        "unlockfailed" => "unlock_failed",
        "lockfailed" => "lock_failed",
        k if k.contains("jam") => "jammed",
        "unlock" | "unlocked" => "unlocked",
        "lock" | "locked" => "locked",
        _ => return None,
    })
}

/// A lock alert that says the door was left open.
pub fn alert_is_door_open(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    k.contains("dooropen") || k.contains("openremind") || k.contains("door_open")
}

/// "Couldn't unlock", "Door left open", else YoLink's own word.
pub fn alert_words(kind: &str) -> String {
    if alert_is_door_open(kind) {
        return "Door left open".into();
    }
    match alert_action(kind) {
        Some(a) => LOCK_ACTIONS
            .iter()
            .find(|(v, _)| *v == a)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| kind.to_string()),
        None => kind.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Rounding helpers
// ---------------------------------------------------------------------------

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// Unit tests — real shapes (ids scrubbed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(kind: DeviceKind, data: Value) -> Map<String, Value> {
        read(&kind, &Caps::default(), &data)
            .expect("a reading")
            .attrs
    }

    /// The house's own `Outlet.getState` (the plug with nothing drawing).
    const OUTLET_GETSTATE: &str = r#"{"delay":{"ch":1,"off":0,"on":0},"loraInfo":{"devNetType":"A","gatewayId":"","gateways":1,"signal":0},"power":0,"state":"closed","time":{},"tz":-4,"version":"0433","watt":0}"#;
    /// The house's own `LockV2.getState` (Laundry Room Lock).
    const LOCKV2_GETSTATE: &str = r#"{"alert":{"source":"Fingerprint","type":"UnLockFailed"},"attributes":{"autoLock":0,"enableSetButton":true,"openRemind":0,"rlSet":"right","soundLevel":2},"battery":2,"loraInfo":{"devNetType":"A","gatewayId":"","gateways":1,"signal":0},"loraP2PHash":0,"state":{"door":"closed","lock":"unlocked"},"tz":-4,"version":"1707"}"#;

    fn leak_getstate(state: &str) -> Value {
        json!({
            "online": true,
            "state": {
                "alarmState": { "stayError": false, "detectorError": false, "freezeError": false, "reminder": false },
                "state": state, "battery": "4", "beep": true, "devTemperature": 18,
                "interval": 0, "sensitivity": "high", "sensorMode": "WaterLeak",
                "stateChangedAt": 1785716447119i64, "supportChangeMode": true, "version": "0605"
            },
            "reportAt": "2026-10-21T10:00:00.000Z",
            "deviceId": "aa00000000000011"
        })
    }

    /// Fix 1: `getState` nests the state; "alert" is wet. Every poll during a
    /// leak used to publish dry.
    #[test]
    fn a_leak_reads_wet_through_the_getstate_wrapper() {
        let a = attrs(DeviceKind::LeakSensor, leak_getstate("alert"));
        assert_eq!(a["water_detected"], json!(true));
        let a = attrs(DeviceKind::LeakSensor, leak_getstate("normal"));
        assert_eq!(a["water_detected"], json!(false));
        assert_eq!(a["battery_pct"], json!(100));
        assert_eq!(a["temperature"], json!(18.0));
        assert_eq!(a["probe_fault"], json!(false));
        assert!(a.get("leak").is_none());
    }

    #[test]
    fn a_leak_reads_wet_from_a_flat_report() {
        let a = attrs(
            DeviceKind::LeakSensor,
            json!({ "state": "alert", "battery": 3, "alarmState": { "detectorError": false, "freezeError": true } }),
        );
        assert_eq!(a["water_detected"], json!(true));
        assert_eq!(a["freeze_warning"], json!(true));
        let r = read(
            &DeviceKind::LeakSensor,
            &Caps::default(),
            &json!({ "state": "normal", "alarmState": { "detectorError": true } }),
        )
        .unwrap();
        assert_eq!(r.faults, vec!["probe fault"]);
    }

    /// The wrapper's `reportAt` and `online` come through the flattening.
    #[test]
    fn report_time_and_online_survive_the_wrapper() {
        let r = read(
            &DeviceKind::LeakSensor,
            &Caps::default(),
            &leak_getstate("normal"),
        )
        .unwrap();
        assert_eq!(r.online, Some(true));
        assert_eq!(
            r.report_at.unwrap().to_rfc3339(),
            "2026-10-21T10:00:00+00:00"
        );
        assert_eq!(r.battery_level, Some(4));
    }

    /// Fix 2: vibration and motion are "alert"/"normal", flat or nested — not
    /// a bool `alarm`.
    #[test]
    fn vibration_and_motion_fire_on_alert() {
        assert_eq!(
            attrs(
                DeviceKind::VibrationSensor,
                json!({ "state": "alert", "battery": 4 })
            )["vibration"],
            json!(true)
        );
        assert_eq!(
            attrs(DeviceKind::VibrationSensor, json!({ "state": "normal" }))["vibration"],
            json!(false)
        );
        let nested = json!({ "state": { "online": true, "state": "alert", "battery": "4" }, "reportAt": "2026-10-21T10:00:00Z" });
        assert_eq!(
            attrs(DeviceKind::VibrationSensor, nested)["vibration"],
            json!(true)
        );
        assert_eq!(
            attrs(DeviceKind::MotionSensor, json!({ "state": "alert" }))["motion"],
            json!(true)
        );
        assert_eq!(
            attrs(
                DeviceKind::MotionSensor,
                json!({ "online": true, "state": { "state": "normal" } })
            )["motion"],
            json!(false)
        );
        // The old shape is not what YoLink sends, and means nothing now.
        assert!(read(
            &DeviceKind::MotionSensor,
            &Caps::default(),
            &json!({ "alarm": true })
        )
        .is_none());
    }

    #[test]
    fn door_open_closed_and_left_open() {
        let r = read(
            &DeviceKind::DoorSensor,
            &Caps::default(),
            &json!({ "state": "open", "alertType": "openRemind", "battery": 3 }),
        )
        .unwrap();
        assert_eq!(r.attrs["open"], json!(true));
        assert!(r.open_remind);
        assert_eq!(r.attrs["battery_pct"], json!(75));
        let nested = json!({ "online": true, "state": { "state": "closed", "battery": "4", "openRemindDelay": 600 }, "reportAt": "2026-10-21T10:00:00Z" });
        assert_eq!(attrs(DeviceKind::DoorSensor, nested)["open"], json!(false));
        assert!(attrs(DeviceKind::DoorSensor, json!({ "state": "error" }))
            .get("open")
            .is_none());
    }

    /// Fix 9: a v1 lock's `getState` nests `state.state`.
    #[test]
    fn a_v1_lock_reads_through_the_wrapper() {
        let a = attrs(
            DeviceKind::Lock,
            json!({ "state": { "state": "locked", "battery": 3 }, "version": "0101", "tz": -4 }),
        );
        assert_eq!(a["locked"], json!(true));
        assert_eq!(a["battery_pct"], json!(75));
        assert_eq!(
            attrs(DeviceKind::Lock, json!({ "state": "unlocked" }))["locked"],
            json!(false)
        );
    }

    #[test]
    fn the_houses_lockv2_getstate() {
        let r = read(
            &DeviceKind::LockV2,
            &Caps::default(),
            &serde_json::from_str(LOCKV2_GETSTATE).unwrap(),
        )
        .unwrap();
        assert_eq!(r.attrs["locked"], json!(false));
        assert_eq!(r.attrs["open"], json!(false));
        assert_eq!(r.attrs["auto_lock_secs"], json!(0));
        assert_eq!(r.attrs["sound_level"], json!(2));
        assert_eq!(r.attrs["battery_pct"], json!(50));
        assert_eq!(r.attrs["battery_low"], json!(false));
        assert_eq!(
            r.alert,
            Some(LockAlert {
                kind: "UnLockFailed".into(),
                source: Some("Fingerprint".into())
            })
        );
        assert_eq!(r.signal, None, "0 is not a measurement");
        // The lock's `state` object is its own state, not a wrapper.
        assert_eq!(
            body(&serde_json::from_str(LOCKV2_GETSTATE).unwrap())["state"]["lock"],
            json!("unlocked")
        );
    }

    /// Fix 10: level 1 is low (core's 20 % never fires at "25 %").
    #[test]
    fn battery_level_one_is_low() {
        for (raw, pct, low) in [(0, 0, true), (1, 25, true), (2, 50, false), (4, 100, false)] {
            let a = attrs(
                DeviceKind::DoorSensor,
                json!({ "state": "open", "battery": raw }),
            );
            assert_eq!(a["battery_pct"], json!(pct), "raw {raw}");
            assert_eq!(a["battery_low"], json!(low), "raw {raw}");
        }
        // The sensor's own flag counts too.
        let a = attrs(
            DeviceKind::THSensor,
            json!({ "temperature": 3.0, "battery": 3, "alarm": { "lowBattery": true } }),
        );
        assert_eq!(a["battery_low"], json!(true));
        // And a string level, as the docs type it.
        assert_eq!(
            attrs(
                DeviceKind::LeakSensor,
                json!({ "state": "normal", "battery": "1" })
            )["battery_pct"],
            json!(25)
        );
    }

    /// Fix 4: `power` is tenths of a watt; the plug's own reading decides.
    #[test]
    fn power_is_tenths_of_a_watt() {
        let r = read(
            &DeviceKind::Outlet,
            &Caps::default(),
            &json!({ "state": "open", "power": 37, "watt": 12 }),
        )
        .unwrap();
        assert_eq!(r.power_raw, Some(37.0));
        assert_eq!(watts(37.0), 3.7);
        assert_eq!(watts(0.0), 0.0);
        assert_eq!(watts(15000.0), 1500.0);
        // Nothing about watts is published here: whether the plug meters
        // decides, in the bridge.
        assert!(r.attrs.get("power_w").is_none());
        assert!(r.attrs.get("energy_kwh").is_none());
        let a = read(
            &DeviceKind::Outlet,
            &Caps::default(),
            &serde_json::from_str(OUTLET_GETSTATE).unwrap(),
        )
        .unwrap();
        assert_eq!(a.attrs["on"], json!(false));
        assert_eq!(a.version.as_deref(), Some("0433"));
    }

    /// Fix 16: a probe publishes no humidity (and clears an old one).
    #[test]
    fn a_probe_has_no_humidity() {
        let probe = Caps::from_model(Some("YS8004-UC"));
        assert!(probe.temp_only);
        let r = read(&DeviceKind::THSensor, &probe, &json!({ "state": { "temperature": -14.3, "humidity": 52.0, "battery": 2, "tempLimit": { "min": -20, "max": -5 }, "alarm": { "lowTemp": false, "highTemp": false } }, "reportAt": "2026-10-21T10:00:00Z" })).unwrap();
        assert_eq!(r.attrs["temperature"], json!(-14.3));
        assert_eq!(r.attrs["humidity"], Value::Null);
        assert_eq!(r.attrs["temp_limit_low"], json!(-20.0));
        assert_eq!(r.attrs["temp_limit_high"], json!(-5.0));
        assert_eq!(r.attrs["too_warm"], json!(false));
        let room = read(
            &DeviceKind::THSensor,
            &Caps::from_model(Some("YS8003-UC")),
            &json!({ "temperature": 20.5, "humidity": 47.0 }),
        )
        .unwrap();
        assert_eq!(room.attrs["humidity"], json!(47.0));
        // An exact 0 % is not a reading either.
        let zero = read(
            &DeviceKind::THSensor,
            &Caps::default(),
            &json!({ "temperature": 20.5, "humidity": 0 }),
        )
        .unwrap();
        assert!(zero.attrs.get("humidity").is_none());
    }

    #[test]
    fn metering_models() {
        assert!(Caps::from_model(Some("YS6602-UC")).metering);
        assert!(Caps::from_model(Some("YS6614-UC")).metering);
        assert!(!Caps::from_model(Some("YS6604-UC")).metering);
        assert!(!Caps::from_model(None).metering);
    }

    /// Fix 3: LockV2 and v1 each in their own shape.
    #[test]
    fn lock_commands_per_generation() {
        let v2 = DeviceKind::LockV2
            .translate_command(&json!({ "locked": true }))
            .unwrap();
        assert_eq!(
            v2,
            vec![("setState", json!({ "state": { "lock": "locked" } }))]
        );
        let v2 = DeviceKind::LockV2
            .translate_command(&json!({ "locked": false }))
            .unwrap();
        assert_eq!(v2[0].1, json!({ "state": { "lock": "unlocked" } }));
        let v1 = DeviceKind::Lock
            .translate_command(&json!({ "locked": false }))
            .unwrap();
        assert_eq!(v1, vec![("setState", json!({ "state": "unlock" }))]);
        let set = DeviceKind::LockV2
            .translate_command(&json!({ "auto_lock_secs": 30, "sound_level": 1 }))
            .unwrap();
        assert_eq!(
            set,
            vec![("setAttributes", json!({ "autoLock": 30, "soundLevel": 1 }))]
        );
        assert!(DeviceKind::LockV2
            .translate_command(&json!({ "sound_level": 4 }))
            .is_err());
        assert!(DeviceKind::LockV2.translate_command(&json!({})).is_err());
        assert!(DeviceKind::Lock
            .translate_command(&json!({ "sound_level": 1 }))
            .is_err());
    }

    #[test]
    fn relay_commands() {
        assert_eq!(
            DeviceKind::Outlet
                .translate_command(&json!({ "on": true }))
                .unwrap()[0]
                .1,
            json!({ "state": "open" })
        );
        assert_eq!(
            DeviceKind::Switch
                .translate_command(&json!({ "on": false }))
                .unwrap()[0]
                .1,
            json!({ "state": "close" })
        );
        assert!(DeviceKind::DoorSensor
            .translate_command(&json!({ "on": true }))
            .is_err());
    }

    #[test]
    fn lock_alert_words() {
        assert_eq!(alert_action("UnLockFailed"), Some("unlock_failed"));
        assert_eq!(alert_action("LockFailed"), Some("lock_failed"));
        assert_eq!(alert_words("UnLockFailed"), "Couldn't unlock");
        assert!(alert_is_door_open("DoorOpenRemind"));
        assert_eq!(method_of("Fingerprint"), Some("fingerprint"));
        assert_eq!(method_of("App"), Some("app"));
        assert_eq!(method_of("Button"), Some("button"));
        assert_eq!(method_of("whatever"), None);
    }

    #[test]
    fn times_parse_either_way() {
        assert!(parse_time(&json!(1785716447119i64)).is_some());
        assert!(parse_time(&json!("2026-10-21T10:00:00.000Z")).is_some());
        assert!(parse_time(&json!("soon")).is_none());
    }
}
