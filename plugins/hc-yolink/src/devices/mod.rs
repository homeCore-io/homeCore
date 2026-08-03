use anyhow::{bail, Result};
use serde_json::Value;

use crate::config::TemperatureUnit;

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
    /// Temperature + humidity sensor
    THSensor,
    /// Vibration / shock sensor
    VibrationSensor,
    /// Smart lock
    Lock,
    /// Smart lock v2 variant
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

    // -----------------------------------------------------------------------
    // State translation: YoLink report/state data → HomeCore state JSON
    // -----------------------------------------------------------------------

    /// Translate a real-time MQTT report into a HomeCore state *patch*.
    ///
    /// Separate from [`Self::translate_state`] because a report is not a
    /// snapshot. Most events carry the device's state and translate identically,
    /// but some carry only a measurement, and those have no `state` field at
    /// all — [`translate_state`](Self::translate_state) correctly refuses them,
    /// because publishing a stateless object as a full retained state would
    /// erase everything it omits.
    ///
    /// Patches have no such problem: they merge. So the report path can accept
    /// events the snapshot path must not.
    pub fn translate_report(
        &self,
        event: &str,
        data: &Value,
        temp_unit: &TemperatureUnit,
    ) -> Option<Value> {
        // `Outlet.powerReport` is the only one of these seen in the wild, and it
        // arrives hourly from every metered plug. It was logged as an
        // untranslatable payload every time — 137 warnings in the retained logs,
        // all of them the plug doing exactly what it is supposed to.
        if event.ends_with(".powerReport") {
            return translate_power_report(data);
        }
        self.translate_state(data, temp_unit)
    }

    /// Translate device data from a YoLink report or getState response into a
    /// HomeCore-compatible state JSON object.
    ///
    /// Returns `None` if the data cannot be translated (e.g. unrecognised
    /// payload shape).
    pub fn translate_state(&self, data: &Value, temp_unit: &TemperatureUnit) -> Option<Value> {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch => translate_switch(data),
            Self::MultiOutlet => translate_multi_outlet(data),
            Self::DoorSensor => translate_door_sensor(data),
            Self::MotionSensor => translate_motion_sensor(data),
            Self::LeakSensor => translate_leak_sensor(data),
            Self::THSensor => translate_th_sensor(data, temp_unit),
            Self::VibrationSensor => translate_vibration_sensor(data),
            Self::Lock | Self::LockV2 => translate_lock(data),
            Self::Siren => translate_siren(data),
            Self::Hub | Self::Unknown(_) => None,
        }
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore cmd JSON → (YoLink method suffix, params)
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into a YoLink API method suffix
    /// (appended to the device type, e.g. "setState") and its params object.
    pub fn translate_command(&self, cmd: &Value) -> Result<(&'static str, Value)> {
        match self {
            Self::Outlet | Self::SmartPlug | Self::Switch | Self::MultiOutlet => {
                let on = cmd["on"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'on' field"))?;
                // YoLink uses "open" for on and "close" for off
                Ok((
                    "setState",
                    serde_json::json!({ "state": if on { "open" } else { "close" } }),
                ))
            }
            Self::Siren => {
                let on = cmd["on"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'on' field"))?;
                Ok((
                    "setState",
                    serde_json::json!({ "state": if on { "open" } else { "close" } }),
                ))
            }
            Self::Lock | Self::LockV2 => {
                let locked = cmd["locked"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("cmd missing boolean 'locked' field"))?;
                Ok((
                    "setState",
                    serde_json::json!({ "state": if locked { "lock" } else { "unlock" } }),
                ))
            }
            Self::DoorSensor
            | Self::MotionSensor
            | Self::LeakSensor
            | Self::THSensor
            | Self::VibrationSensor => {
                bail!("{:?} is read-only; commands are not supported", self)
            }
            Self::Hub | Self::Unknown(_) => {
                bail!("{:?} is not a supported device kind", self)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-type translation helpers
// ---------------------------------------------------------------------------

/// YoLink uses "open" for on/active and "close" for off/inactive in relay devices.
fn relay_is_on(state_str: &str) -> bool {
    matches!(state_str, "open" | "on")
}

/// Extract the device state string, handling both payload shapes:
/// - MQTT reports: `data["state"] = "locked"` (flat string)
/// - getState API: `data["state"] = { "state": "locked", ... }` (nested object)
fn state_str(data: &Value) -> Option<&str> {
    data["state"]["state"]
        .as_str()
        .or_else(|| data["state"].as_str())
}

/// Battery level as a percentage (0–100).
/// YoLink reports battery on a 0–4 scale (4 = full); we convert to percent.
/// Looks in `data["state"]["battery"]` (getState) and `data["battery"]` (MQTT report).
fn battery_pct(data: &Value) -> Option<Value> {
    let b = data["state"]["battery"]
        .as_u64()
        .or_else(|| data["battery"].as_u64())?;
    let pct = (b * 25).min(100);
    Some(Value::Number(pct.into()))
}

fn translate_switch(data: &Value) -> Option<Value> {
    let on = relay_is_on(state_str(data)?);
    let mut out = serde_json::json!({ "on": on });

    // Power monitoring fields (Outlet with power meter)
    if let Some(w) = data["power"].as_f64().or_else(|| data["watt"].as_f64()) {
        out["power_w"] = serde_json::json!(round1(w));
    }
    if let Some(kwh) = data["electricity"].as_f64() {
        out["energy_kwh"] = serde_json::json!(round3(kwh));
    }

    Some(out)
}

/// `Outlet.powerReport` — an hourly power history, and nothing else.
///
/// ```json
/// { "watts": [ { "time": 1785716447119, "watt": 0 }, ... ] }
/// ```
///
/// There is no `state` here, so this says nothing about whether the outlet is
/// on; the patch deliberately carries only `power_w`, leaving `on` to the
/// events and polls that actually know.
///
/// The newest sample is picked by timestamp rather than by position. The hub
/// has always sent them newest-first, but nothing documents that, and reading
/// index 0 on a hub that changed its mind would publish an hours-old reading as
/// current — a wrong number, which is worse than the missing one this replaces.
fn translate_power_report(data: &Value) -> Option<Value> {
    let newest = data["watts"]
        .as_array()?
        .iter()
        .max_by_key(|s| s["time"].as_i64().unwrap_or(i64::MIN))?;
    let w = newest["watt"].as_f64()?;
    Some(serde_json::json!({ "power_w": round1(w) }))
}

fn translate_multi_outlet(data: &Value) -> Option<Value> {
    // MultiOutlet state may be an array of per-outlet states.
    // For now expose the overall state as a single on/off.
    translate_switch(data)
}

fn translate_door_sensor(data: &Value) -> Option<Value> {
    // state == "open" → door open, "close" or "closed" → closed
    let open = state_str(data).map(|s| s == "open")?;
    // Just `open`. This used to publish `contact` alongside it carrying the
    // SAME value, which is the opposite of what `contact` conventionally means
    // — a closed contact circuit is a shut door — so every client that keyed
    // on the name read the sensor backwards. Two names for one reading is also
    // two entries in every attribute list, twice in a group's member list, and
    // one more thing for a rule author to choose between with no difference
    // between the choices.
    //
    // `device_type` is still `contact_sensor`; only the redundant attribute is
    // gone. A full state publish replaces the stored map, so the key clears
    // itself on the next report.
    let mut out = serde_json::json!({ "open": open });
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_motion_sensor(data: &Value) -> Option<Value> {
    // YoLink MotionSensor: `alarm: true` = motion detected
    let motion = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .unwrap_or(false);
    let mut out = serde_json::json!({ "motion": motion });
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_leak_sensor(data: &Value) -> Option<Value> {
    // `alarm: true` or state == "alert" → leak detected
    let leak = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .or_else(|| data["state"].as_str().map(|s| s == "alert"))
        .unwrap_or(false);
    // One name, and it is `water_detected` rather than the shorter `leak`:
    // that is the one a live rule already triggers on, and breaking a working
    // rule to win a naming preference is the wrong trade. See the note on
    // `translate_door_sensor` for why two names for one reading is a cost.
    let mut out = serde_json::json!({ "water_detected": leak });
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_th_sensor(data: &Value, unit: &TemperatureUnit) -> Option<Value> {
    // Temperature value (raw)
    let raw_temp = data["temperature"]
        .as_f64()
        .or_else(|| data["state"]["temperature"].as_f64())?;

    let humidity = data["humidity"]
        .as_f64()
        .or_else(|| data["state"]["humidity"].as_f64())?;

    // Device reports its own unit in "tempUnit": "℃" or "℉"
    // Check both data-level and state-level.
    let raw_unit = data["tempUnit"]
        .as_str()
        .or_else(|| data["state"]["tempUnit"].as_str())
        .unwrap_or("℃");

    // Determine raw unit and convert to the configured output unit.
    let temp_out = if raw_unit.contains('C') || raw_unit == "℃" {
        unit.convert_celsius(raw_temp)
    } else {
        // Assume Fahrenheit
        unit.convert_fahrenheit(raw_temp)
    };

    let mut out = serde_json::json!({
        "temperature":      round1(temp_out),
        "temperature_unit": unit.label(),
        "humidity_pct":     round1(humidity),
    });
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_vibration_sensor(data: &Value) -> Option<Value> {
    let vibration = data["alarm"]
        .as_bool()
        .or_else(|| data["state"]["alarm"].as_bool())
        .unwrap_or(false);
    let mut out = serde_json::json!({ "vibration": vibration });
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }
    Some(out)
}

fn translate_lock(data: &Value) -> Option<Value> {
    // LockV2 MQTT/getState: data["state"] = {"lock": "locked"/"unlocked", "door": "open"/"closed"}
    // Legacy Lock flat MQTT: data["state"] = "locked"/"unlocked" (string)
    let lock_str = data["state"]["lock"]
        .as_str()
        .or_else(|| data["state"].as_str())?;
    let locked = lock_str == "locked";
    let mut out = serde_json::json!({ "locked": locked });

    // Battery: top-level for LockV2 (0–4 scale → %)
    if let Some(b) = battery_pct(data) {
        out["battery"] = b;
    }

    // Door contact sensor (separate from the bolt)
    if let Some(door) = data["state"]["door"].as_str() {
        out["door_open"] = serde_json::json!(door == "open");
    }

    // Last alert (e.g. "UnLockFailed", "LockFailed", "DoorOpenAlarm")
    if let Some(alert) = data["alert"]["type"].as_str() {
        out["last_alert"] = serde_json::json!(alert);
    }

    // Lock configuration attributes
    if let Some(v) = data["attributes"]["autoLock"].as_u64() {
        out["auto_lock_secs"] = serde_json::json!(v);
    }
    if let Some(v) = data["attributes"]["soundLevel"].as_u64() {
        out["sound_level"] = serde_json::json!(v);
    }

    Some(out)
}

fn translate_siren(data: &Value) -> Option<Value> {
    let on = state_str(data).map(relay_is_on).unwrap_or(false);
    let alarm = data["alarm"].as_bool().unwrap_or(on);
    Some(serde_json::json!({ "on": on, "alarm": alarm }))
}

// ---------------------------------------------------------------------------
// Rounding helpers
// ---------------------------------------------------------------------------

/// Round to 1 decimal place.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Round to 3 decimal places.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact payload from the live plug, which used to produce a warning
    /// and no state at all.
    #[test]
    fn outlet_power_report_publishes_watts() {
        let data = json!({ "watts": [
            { "time": 1785716447119i64, "watt": 41.5 },
            { "time": 1785712847119i64, "watt": 12.0 },
            { "time": 1785709247119i64, "watt": 3.0 },
        ]});
        let patch = DeviceKind::Outlet
            .translate_report("Outlet.powerReport", &data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(patch["power_w"], json!(41.5));
        // Says nothing about the relay: a power report does not know.
        assert!(patch.get("on").is_none());
    }

    /// Newest by timestamp, not by position — the hub's ordering is not a
    /// documented guarantee.
    #[test]
    fn outlet_power_report_picks_newest_not_first() {
        let data = json!({ "watts": [
            { "time": 1785709247119i64, "watt": 3.0 },
            { "time": 1785716447119i64, "watt": 41.5 },
        ]});
        let patch = DeviceKind::Outlet
            .translate_report("Outlet.powerReport", &data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(patch["power_w"], json!(41.5));
    }

    #[test]
    fn outlet_power_report_empty_is_none() {
        let data = json!({ "watts": [] });
        assert!(DeviceKind::Outlet
            .translate_report("Outlet.powerReport", &data, &TemperatureUnit::F)
            .is_none());
    }

    /// A state-carrying report still goes through the snapshot translator.
    #[test]
    fn ordinary_report_still_translates_state() {
        let data = json!({ "state": "open", "power": 12.5 });
        let patch = DeviceKind::Outlet
            .translate_report("Outlet.StatusChange", &data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(patch["on"], json!(true));
    }

    /// A power report must never reach the snapshot path: publishing it as a
    /// full retained state would erase `on`.
    #[test]
    fn power_report_is_not_a_snapshot() {
        let data = json!({ "watts": [{ "time": 1i64, "watt": 5.0 }] });
        assert!(DeviceKind::Outlet
            .translate_state(&data, &TemperatureUnit::F)
            .is_none());
    }

    #[test]
    fn outlet_on() {
        let data = json!({ "state": "open", "power": 12.5, "electricity": 0.031 });
        let state = DeviceKind::Outlet
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["on"], json!(true));
        assert_eq!(state["power_w"], json!(12.5));
        assert_eq!(state["energy_kwh"], json!(0.031));
    }

    #[test]
    fn outlet_off() {
        let data = json!({ "state": "close" });
        let state = DeviceKind::Outlet
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["on"], json!(false));
    }

    #[test]
    fn door_open() {
        let data = json!({ "state": "open", "battery": 3 });
        let state = DeviceKind::DoorSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["open"], json!(true));
        assert!(state.get("contact").is_none(), "the alias is gone");
        assert_eq!(state["battery"], json!(75)); // 3/4 = 75%
    }

    #[test]
    fn door_open_nested_getstate() {
        // getState response: data["state"] is a nested object
        let data = json!({ "state": { "state": "open", "battery": 4 }, "online": true });
        let state = DeviceKind::DoorSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["open"], json!(true));
        assert!(state.get("contact").is_none(), "the alias is gone");
        assert_eq!(state["battery"], json!(100));
    }

    #[test]
    fn door_closed() {
        let data = json!({ "state": "close", "battery": 4 });
        let state = DeviceKind::DoorSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["open"], json!(false));
        assert!(state.get("contact").is_none(), "the alias is gone");
    }

    #[test]
    fn motion_detected() {
        let data = json!({ "alarm": true, "battery": 2 });
        let state = DeviceKind::MotionSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["motion"], json!(true));
    }

    /// One key, not two.
    ///
    /// This asserted both `leak` and `water_detected` — the "canonical and
    /// legacy" pair. Two names for one reading meant the sensor appeared twice
    /// in every attribute list and offered two identical choices to anyone
    /// writing a rule. `water_detected` is the survivor because a live rule
    /// already triggers on it.
    #[test]
    fn a_leak_sensor_reports_one_name_for_one_reading() {
        let data = json!({ "alarm": true, "battery": 2 });
        let state = DeviceKind::LeakSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["water_detected"], json!(true));
        assert!(state.get("leak").is_none(), "the alias is gone");
    }

    #[test]
    fn th_celsius_device_to_fahrenheit_output() {
        let data = json!({ "temperature": 22.5, "humidity": 65.2, "tempUnit": "℃", "battery": 4 });
        let state = DeviceKind::THSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        // 22.5 °C → 72.5 °F
        assert_eq!(state["temperature"], json!(72.5));
        assert_eq!(state["temperature_unit"], json!("F"));
        assert_eq!(state["humidity_pct"], json!(65.2));
    }

    #[test]
    fn th_celsius_device_to_celsius_output() {
        let data = json!({ "temperature": 22.5, "humidity": 65.2, "tempUnit": "℃", "battery": 4 });
        let state = DeviceKind::THSensor
            .translate_state(&data, &TemperatureUnit::C)
            .unwrap();
        assert_eq!(state["temperature"], json!(22.5));
        assert_eq!(state["temperature_unit"], json!("C"));
    }

    #[test]
    fn th_fahrenheit_device_to_fahrenheit_output() {
        let data = json!({ "temperature": 72.5, "humidity": 50.0, "tempUnit": "℉" });
        let state = DeviceKind::THSensor
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["temperature"], json!(72.5));
    }

    #[test]
    fn th_fahrenheit_device_to_celsius_output() {
        let data = json!({ "temperature": 72.5, "humidity": 50.0, "tempUnit": "℉" });
        let state = DeviceKind::THSensor
            .translate_state(&data, &TemperatureUnit::C)
            .unwrap();
        // 72.5 °F → 22.5 °C
        assert_eq!(state["temperature"], json!(22.5));
    }

    #[test]
    fn lockv2_unlocked_with_door_open() {
        // Real LockV2 MQTT/getState format
        let data = json!({
            "state": { "lock": "unlocked", "door": "open" },
            "battery": 4,
            "alert": { "source": "Fingerprint", "type": "UnLockFailed" },
            "attributes": { "autoLock": 10, "soundLevel": 2 }
        });
        let state = DeviceKind::LockV2
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["locked"], json!(false));
        assert_eq!(state["door_open"], json!(true));
        assert_eq!(state["battery"], json!(100)); // 4 * 25 = 100%
        assert_eq!(state["last_alert"], json!("UnLockFailed"));
        assert_eq!(state["auto_lock_secs"], json!(10));
        assert_eq!(state["sound_level"], json!(2));
    }

    #[test]
    fn lockv2_locked_door_closed() {
        let data = json!({
            "state": { "lock": "locked", "door": "closed" },
            "battery": 3
        });
        let state = DeviceKind::LockV2
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["locked"], json!(true));
        assert_eq!(state["door_open"], json!(false));
        assert_eq!(state["battery"], json!(75)); // 3 * 25 = 75%
    }

    #[test]
    fn lock_flat_string_state() {
        // Legacy Lock flat format: data["state"] is a string
        let data = json!({ "state": "locked", "battery": 3 });
        let state = DeviceKind::Lock
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["locked"], json!(true));
        assert_eq!(state["battery"], json!(75));
    }

    #[test]
    fn lock_battery_zero_pct() {
        let data = json!({ "state": { "lock": "locked", "door": "closed" }, "battery": 0 });
        let state = DeviceKind::LockV2
            .translate_state(&data, &TemperatureUnit::F)
            .unwrap();
        assert_eq!(state["battery"], json!(0));
    }

    #[test]
    fn switch_cmd_on() {
        let cmd = json!({ "on": true });
        let (method, params) = DeviceKind::Switch.translate_command(&cmd).unwrap();
        assert_eq!(method, "setState");
        assert_eq!(params["state"], json!("open"));
    }

    #[test]
    fn switch_cmd_off() {
        let cmd = json!({ "on": false });
        let (_method, params) = DeviceKind::Switch.translate_command(&cmd).unwrap();
        assert_eq!(params["state"], json!("close"));
    }

    #[test]
    fn sensor_cmd_rejected() {
        let cmd = json!({});
        assert!(DeviceKind::DoorSensor.translate_command(&cmd).is_err());
    }
}
