//! Device classification and state translation.
//!
//! Maps ISY node metadata and property values to HomeCore device types
//! and canonical JSON state, and translates HomeCore commands back to
//! ISY REST command codes.

use crate::isy::{IsyEvent, IsyNode};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Device classification
// ---------------------------------------------------------------------------

/// HomeCore device type for an ISY node.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceKind {
    Light,
    Switch,
    ContactSensor,
    MotionSensor,
    WaterSensor,
    BinarySensor,
    Sensor,
    Lock,
    Cover,
    Fan,
    Thermostat,
    Scene,
}

impl DeviceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Switch => "switch",
            Self::ContactSensor => "contact_sensor",
            Self::MotionSensor => "motion_sensor",
            Self::WaterSensor => "water_sensor",
            Self::BinarySensor => "binary_sensor",
            Self::Sensor => "sensor",
            Self::Lock => "lock",
            Self::Cover => "cover",
            Self::Fan => "fan",
            Self::Thermostat => "thermostat",
            Self::Scene => "scene",
        }
    }
}

/// Classify an ISY node into a HomeCore device type.
///
/// Detection priority:
/// 1. Groups → Scene
/// 2. Specific Insteon type-code prefixes (fan, thermostat, motor)
/// 3. UOM of the primary `ST` property
/// 4. Insteon category from type code
/// 5. Fallback: Switch
pub fn classify_node(node: &IsyNode) -> DeviceKind {
    if node.is_group {
        return DeviceKind::Scene;
    }

    let t = node.node_type.as_str();

    // Insteon FanLinc (category 1, sub 46)
    if t.starts_with("1.46.") {
        return DeviceKind::Fan;
    }
    // Insteon thermostat (category 5)
    if t.starts_with("5.") {
        return DeviceKind::Thermostat;
    }
    // Insteon motor/cover (category 14)
    if t.starts_with("14.") {
        return DeviceKind::Cover;
    }

    let st_uom = node
        .properties
        .get("ST")
        .map(|p| p.uom.as_str())
        .unwrap_or("");

    match st_uom {
        // UOM 51/100 = percentage — dimmable if category 1, else level sensor
        "51" | "100" => {
            if t.starts_with("1.") {
                DeviceKind::Light
            } else {
                DeviceKind::Sensor
            }
        }

        // UOM 78 = on/off binary
        "78" => {
            if t.starts_with("7.") || t.starts_with("16.") {
                classify_binary_sensor(node)
            } else if t.starts_with("2.") || t.starts_with("113.") {
                DeviceKind::Switch
            } else if t.starts_with("1.") {
                // Insteon switchlincs report UOM 78 but are still controllable
                DeviceKind::Switch
            } else {
                DeviceKind::Switch
            }
        }

        // UOM 2 = binary (0/1); UOM 25 = index — typically binary sensors
        "2" | "25" => classify_binary_sensor(node),

        // UOM 11 = deadbolt/lock
        "11" => DeviceKind::Lock,

        // UOM 97 = barrier/cover
        "97" => DeviceKind::Cover,

        _ => {
            if is_physical_uom(st_uom) {
                DeviceKind::Sensor
            } else if t.starts_with("1.") {
                DeviceKind::Light
            } else {
                // 2.* explicitly + everything else default to Switch.
                DeviceKind::Switch
            }
        }
    }
}

fn classify_binary_sensor(node: &IsyNode) -> DeviceKind {
    match binary_sensor_device_class(node.node_type.as_str()) {
        Some("motion") => DeviceKind::MotionSensor,
        Some("moisture") => DeviceKind::WaterSensor,
        Some("opening") => DeviceKind::ContactSensor,
        _ => DeviceKind::BinarySensor,
    }
}

/// Return `true` for UOM codes that represent measured physical quantities.
fn is_physical_uom(uom: &str) -> bool {
    matches!(
        uom,
        "1" | "3"
            | "4"
            | "5"
            | "17"
            | "19"
            | "20"
            | "22"
            | "23"
            | "24"
            | "26"
            | "30"
            | "31"
            | "32"
            | "33"
            | "38"
            | "40"
            | "41"
            | "42"
            | "43"
            | "44"
            | "45"
            | "48"
            | "50"
            | "52"
            | "53"
            | "55"
            | "57"
            | "59"
            | "60"
            | "61"
            | "62"
            | "66"
            | "71"
            | "73"
            | "74"
            | "101"
            | "102"
            | "103"
            | "104"
    )
}

/// Derive the device class for a binary sensor from its Insteon type code.
pub fn binary_sensor_device_class(node_type: &str) -> Option<&'static str> {
    let t = node_type;
    if t.starts_with("16.1.")
        || t.starts_with("16.4.")
        || t.starts_with("16.5.")
        || t.starts_with("16.3.")
        || t.starts_with("16.22.")
    {
        return Some("motion");
    }
    if t.starts_with("16.8.") || t.starts_with("16.13.") || t.starts_with("16.14.") {
        return Some("moisture");
    }
    if t.starts_with("16.9.")
        || t.starts_with("16.6.")
        || t.starts_with("16.7.")
        || t.starts_with("16.2.")
        || t.starts_with("16.17.")
        || t.starts_with("16.20.")
        || t.starts_with("16.21.")
    {
        return Some("opening");
    }
    None
}

// ---------------------------------------------------------------------------
// UOM → human-readable unit
// ---------------------------------------------------------------------------

/// Map an ISY UOM code to a human-readable unit string.
pub fn uom_unit(uom: &str) -> Option<&'static str> {
    match uom {
        "1" => Some("A"),
        "4" => Some("°C"),
        "5" => Some("°C"),
        "17" => Some("°F"),
        "19" => Some("°F"),
        "20" => Some("ft/s"),
        "22" => Some("Hz"),
        "23" => Some("inHg"),
        "30" => Some("lx"),
        "31" => Some("lm"),
        "32" => Some("V"),
        "33" => Some("W"),
        "38" => Some("W/m²"),
        "40" => Some("kWh"),
        "41" => Some("mm/hr"),
        "42" => Some("hPa"),
        "43" => Some("kg"),
        "45" => Some("%"),
        "48" => Some("kΩ"),
        "50" => Some("mph"),
        "51" => Some("%"),
        "52" => Some("kPa"),
        "53" => Some("psi"),
        "57" => Some("UV index"),
        "62" => Some("m/s"),
        "66" => Some("µg/m³"),
        "71" => Some("mbar"),
        "73" => Some("W"),
        "74" => Some("%"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// State translation: ISyNode → HomeCore JSON
// ---------------------------------------------------------------------------

/// Build a full HomeCore device state JSON from an [`IsyNode`] whose
/// `properties` map has been populated from both `/rest/nodes` and
/// `/rest/status`.
pub fn node_to_state(node: &IsyNode, kind: &DeviceKind) -> Value {
    let st = node.properties.get("ST");
    let st_value = st.map(|p| p.value).unwrap_or(0);
    let st_uom = st.map(|p| p.uom.as_str()).unwrap_or("");

    match kind {
        DeviceKind::Light => {
            let brightness = st_value.clamp(0, 255) as u8;
            let brightness_pct = (brightness as u32 * 100 / 255) as u8;
            json!({
                "on":             brightness > 0,
                "brightness":     brightness,
                "brightness_pct": brightness_pct,
            })
        }

        DeviceKind::Switch => {
            json!({ "on": st_value > 0 })
        }

        DeviceKind::ContactSensor => {
            let open = st_value > 0;
            // One name per reading. `contact` carried the same value under a
            // name that conventionally means the OPPOSITE — a closed contact
            // circuit is a shut door — so a client keying on the name read the
            // sensor backwards, and every attribute list showed the door twice.
            json!({ "open": open })
        }

        DeviceKind::MotionSensor => {
            let motion = st_value > 0;
            // `occupancy` was the same reading again. A PIR reports motion;
            // occupancy is an inference from it that this plugin does not make.
            json!({ "motion": motion })
        }

        DeviceKind::WaterSensor => {
            let leak = st_value <= 0;
            // `water_detected` rather than `leak`: it is the name a live rule
            // already triggers on, and hc-yolink now reports the same one, so a
            // rule reads the same across both plugins.
            json!({ "water_detected": leak })
        }

        DeviceKind::BinarySensor => {
            // Insteon leak/moisture sensors report ON when DRY → invert.
            let t = node.node_type.as_str();
            let inverted =
                t.starts_with("16.8.") || t.starts_with("16.13.") || t.starts_with("16.14.");
            let raw_on = st_value > 0;
            let on = if inverted { !raw_on } else { raw_on };

            let mut obj = json!({ "on": on });
            if let Some(dc) = binary_sensor_device_class(t) {
                obj["device_class"] = json!(dc);
            }
            obj
        }

        DeviceKind::Sensor => {
            let value = st.map(|p| p.as_f64()).unwrap_or(0.0);
            let unit = uom_unit(st_uom).unwrap_or("");
            json!({ "value": value, "unit": unit })
        }

        DeviceKind::Lock => {
            // UOM 11: 0 = unlocked, 100 = locked, 101 = unknown, 102 = problem
            json!({ "locked": st_value == 100 })
        }

        DeviceKind::Cover => {
            let position = (st_value.clamp(0, 255) as u32 * 100 / 255) as u8;
            let state = if st_value == 0 { "closed" } else { "open" };
            json!({ "position": position, "state": state })
        }

        DeviceKind::Fan => {
            let (on, speed) = fan_speed_from_value(st_value);
            json!({ "on": on, "speed": speed })
        }

        DeviceKind::Thermostat => {
            let mut obj = serde_json::Map::new();

            // Operating state from ST (0=idle, 1=heating, 2=cooling)
            obj.insert(
                "state".into(),
                json!(match st_value {
                    1 => "heating",
                    2 => "cooling",
                    _ => "idle",
                }),
            );

            if let Some(p) = node.properties.get("CLITEMP") {
                obj.insert("temperature".into(), json!(p.as_f64()));
            }
            if let Some(p) = node.properties.get("CLIMD") {
                obj.insert("hvac_mode".into(), json!(hvac_mode_str(p.value)));
            }
            if let Some(p) = node.properties.get("CLISPH") {
                obj.insert("target_temp_heat".into(), json!(p.as_f64()));
            }
            if let Some(p) = node.properties.get("CLISPC") {
                obj.insert("target_temp_cool".into(), json!(p.as_f64()));
            }
            if let Some(p) = node.properties.get("CLIFAN") {
                obj.insert(
                    "fan_mode".into(),
                    json!(if p.value == 7 { "on" } else { "auto" }),
                );
            }

            Value::Object(obj)
        }

        DeviceKind::Scene => {
            // Scenes have no persistent state; always report off at startup.
            json!({ "on": false })
        }
    }
}

// ---------------------------------------------------------------------------
// State translation: IsyEvent → HomeCore JSON patch
// ---------------------------------------------------------------------------

/// Translate an ISY real-time event into a HomeCore partial state JSON patch.
/// Returns `None` if the event should be ignored for this device kind.
pub fn event_to_patch(event: &IsyEvent, kind: &DeviceKind) -> Option<Value> {
    let v = event.value;
    let real = event.real_value();

    match (kind, event.control.as_str()) {
        // ── Light ──────────────────────────────────────────────────────────
        (DeviceKind::Light, "ST") | (DeviceKind::Light, "DON") | (DeviceKind::Light, "DFON") => {
            let brightness = v.clamp(0, 255) as u8;
            let brightness_pct = (brightness as u32 * 100 / 255) as u8;
            Some(json!({
                "on":             brightness > 0,
                "brightness":     brightness,
                "brightness_pct": brightness_pct,
            }))
        }
        (DeviceKind::Light, "DOF") | (DeviceKind::Light, "DFOF") => {
            Some(json!({ "on": false, "brightness": 0, "brightness_pct": 0 }))
        }

        // ── Switch ─────────────────────────────────────────────────────────
        (DeviceKind::Switch, "ST") | (DeviceKind::Switch, "DON") | (DeviceKind::Switch, "DFON") => {
            Some(json!({ "on": v > 0 }))
        }
        (DeviceKind::Switch, "DOF") | (DeviceKind::Switch, "DFOF") => Some(json!({ "on": false })),

        // ── Contact Sensor ────────────────────────────────────────────────
        (DeviceKind::ContactSensor, "ST")
        | (DeviceKind::ContactSensor, "DON")
        | (DeviceKind::ContactSensor, "DOF") => {
            let open = v > 0;
            Some(json!({ "open": open }))
        }

        // ── Motion Sensor ─────────────────────────────────────────────────
        (DeviceKind::MotionSensor, "ST")
        | (DeviceKind::MotionSensor, "DON")
        | (DeviceKind::MotionSensor, "DOF") => {
            let motion = v > 0;
            Some(json!({ "motion": motion }))
        }

        // ── Water Sensor ──────────────────────────────────────────────────
        (DeviceKind::WaterSensor, "ST")
        | (DeviceKind::WaterSensor, "DON")
        | (DeviceKind::WaterSensor, "DOF") => {
            let leak = v <= 0;
            Some(json!({ "water_detected": leak }))
        }

        // ── Binary Sensor ──────────────────────────────────────────────────
        (DeviceKind::BinarySensor, "ST")
        | (DeviceKind::BinarySensor, "DON")
        | (DeviceKind::BinarySensor, "DOF") => Some(json!({ "on": v > 0 })),

        // ── Sensor ─────────────────────────────────────────────────────────
        (DeviceKind::Sensor, "ST") => {
            let unit = uom_unit(&event.uom).unwrap_or("");
            Some(json!({ "value": real, "unit": unit }))
        }

        // ── Fan ────────────────────────────────────────────────────────────
        (DeviceKind::Fan, "ST") | (DeviceKind::Fan, "DON") => {
            let (on, speed) = fan_speed_from_value(v);
            Some(json!({ "on": on, "speed": speed }))
        }
        (DeviceKind::Fan, "DOF") => Some(json!({ "on": false, "speed": "off" })),

        // ── Lock ───────────────────────────────────────────────────────────
        (DeviceKind::Lock, "ST") => Some(json!({ "locked": v == 100 })),

        // ── Cover ──────────────────────────────────────────────────────────
        (DeviceKind::Cover, "ST") | (DeviceKind::Cover, "DON") => {
            let position = (v.clamp(0, 255) as u32 * 100 / 255) as u8;
            Some(json!({ "position": position, "state": if v == 0 { "closed" } else { "open" } }))
        }
        (DeviceKind::Cover, "DOF") => Some(json!({ "position": 0u8, "state": "closed" })),

        // ── Scene ──────────────────────────────────────────────────────────
        (DeviceKind::Scene, "ST") | (DeviceKind::Scene, "DON") => Some(json!({ "on": v > 0 })),
        (DeviceKind::Scene, "DOF") => Some(json!({ "on": false })),

        // ── Thermostat ─────────────────────────────────────────────────────
        (DeviceKind::Thermostat, "ST") => {
            Some(json!({ "state": match v { 1 => "heating", 2 => "cooling", _ => "idle" } }))
        }
        (DeviceKind::Thermostat, "CLITEMP") => Some(json!({ "temperature": real })),
        (DeviceKind::Thermostat, "CLIMD") => Some(json!({ "hvac_mode": hvac_mode_str(v) })),
        (DeviceKind::Thermostat, "CLISPH") => Some(json!({ "target_temp_heat": real })),
        (DeviceKind::Thermostat, "CLISPC") => Some(json!({ "target_temp_cool": real })),
        (DeviceKind::Thermostat, "CLIFAN") => {
            Some(json!({ "fan_mode": if v == 7 { "on" } else { "auto" } }))
        }

        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Command translation: HomeCore JSON → ISY REST command(s)
// ---------------------------------------------------------------------------

/// The ISY REST command to send for a given HomeCore command payload.
pub struct IsyCmd {
    pub cmd: &'static str,
    pub value: Option<u32>,
}

/// Translate a HomeCore command JSON payload into one or more ISY commands.
///
/// Most device types produce a single command.  Thermostats can produce
/// multiple (one per attribute being set).
pub fn cmd_to_isy(payload: &Value, kind: &DeviceKind) -> Vec<IsyCmd> {
    let Some(obj) = payload.as_object() else {
        return vec![];
    };

    match kind {
        DeviceKind::Light => {
            if obj.get("on").and_then(Value::as_bool) == Some(false) {
                return vec![IsyCmd {
                    cmd: "DOF",
                    value: None,
                }];
            }
            if let Some(br) = obj.get("brightness").and_then(Value::as_u64) {
                return vec![IsyCmd {
                    cmd: "DON",
                    value: Some(br.clamp(0, 255) as u32),
                }];
            }
            if let Some(pct) = obj.get("brightness_pct").and_then(Value::as_u64) {
                let v = (pct.clamp(0, 100) as u32 * 255 / 100).min(255);
                return vec![IsyCmd {
                    cmd: "DON",
                    value: Some(v),
                }];
            }
            if obj.get("on").and_then(Value::as_bool) == Some(true) {
                return vec![IsyCmd {
                    cmd: "DON",
                    value: None,
                }];
            }
            vec![]
        }

        DeviceKind::Switch | DeviceKind::Scene => match obj.get("on").and_then(Value::as_bool) {
            Some(true) => vec![IsyCmd {
                cmd: "DON",
                value: None,
            }],
            Some(false) => vec![IsyCmd {
                cmd: "DOF",
                value: None,
            }],
            None => vec![],
        },

        DeviceKind::Fan => {
            if obj.get("on").and_then(Value::as_bool) == Some(false) {
                return vec![IsyCmd {
                    cmd: "DOF",
                    value: None,
                }];
            }
            match obj.get("speed").and_then(Value::as_str) {
                Some("off") => vec![IsyCmd {
                    cmd: "DOF",
                    value: None,
                }],
                Some("low") => vec![IsyCmd {
                    cmd: "DON",
                    value: Some(63),
                }],
                Some("medium") => vec![IsyCmd {
                    cmd: "DON",
                    value: Some(127),
                }],
                Some("high") => vec![IsyCmd {
                    cmd: "DON",
                    value: Some(255),
                }],
                _ => {
                    if obj.get("on").and_then(Value::as_bool) == Some(true) {
                        vec![IsyCmd {
                            cmd: "DON",
                            value: Some(127),
                        }] // default medium
                    } else {
                        vec![]
                    }
                }
            }
        }

        DeviceKind::Lock => match obj.get("locked").and_then(Value::as_bool) {
            Some(true) => vec![IsyCmd {
                cmd: "LOCK",
                value: None,
            }],
            Some(false) => vec![IsyCmd {
                cmd: "UNLOCK",
                value: None,
            }],
            None => vec![],
        },

        DeviceKind::Cover => {
            if let Some(pos) = obj.get("position").and_then(Value::as_u64) {
                let v = (pos.clamp(0, 100) as u32 * 255 / 100).min(255);
                return vec![IsyCmd {
                    cmd: "DON",
                    value: Some(v),
                }];
            }
            match obj.get("state").and_then(Value::as_str) {
                Some("open") => vec![IsyCmd {
                    cmd: "DON",
                    value: Some(255),
                }],
                Some("closed") => vec![IsyCmd {
                    cmd: "DOF",
                    value: None,
                }],
                _ => vec![],
            }
        }

        DeviceKind::Thermostat => {
            // Thermostats accept multiple independent property commands.
            let mut cmds = Vec::new();
            if let Some(hs) = obj.get("target_temp_heat").and_then(Value::as_f64) {
                // ISY expects temperature × 10
                cmds.push(IsyCmd {
                    cmd: "CLISPH",
                    value: Some((hs * 10.0) as u32),
                });
            }
            if let Some(cs) = obj.get("target_temp_cool").and_then(Value::as_f64) {
                cmds.push(IsyCmd {
                    cmd: "CLISPC",
                    value: Some((cs * 10.0) as u32),
                });
            }
            if let Some(mode) = obj.get("hvac_mode").and_then(Value::as_str) {
                let v = match mode {
                    "off" => 0,
                    "heat" => 1,
                    "cool" => 2,
                    "auto" => 3,
                    _ => return cmds,
                };
                cmds.push(IsyCmd {
                    cmd: "CLIMD",
                    value: Some(v),
                });
            }
            if let Some(fan) = obj.get("fan_mode").and_then(Value::as_str) {
                cmds.push(IsyCmd {
                    cmd: "CLIFAN",
                    value: Some(if fan == "on" { 7 } else { 0 }),
                });
            }
            cmds
        }

        // Read-only device types — ignore commands
        DeviceKind::ContactSensor
        | DeviceKind::MotionSensor
        | DeviceKind::WaterSensor
        | DeviceKind::BinarySensor
        | DeviceKind::Sensor => vec![],
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fan_speed_from_value(value: i64) -> (bool, &'static str) {
    match value {
        0 => (false, "off"),
        1..=84 => (true, "low"),
        85..=168 => (true, "medium"),
        _ => (true, "high"),
    }
}

fn hvac_mode_str(value: i64) -> &'static str {
    match value {
        1 => "heat",
        2 => "cool",
        3 => "auto",
        _ => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isy::{IsyEvent, IsyNode, IsyProperty};
    use std::collections::HashMap;

    fn binary_node(node_type: &str, value: i64) -> IsyNode {
        let mut properties = HashMap::new();
        properties.insert(
            "ST".to_string(),
            IsyProperty {
                value,
                formatted: value.to_string(),
                uom: "78".to_string(),
                prec: 0,
            },
        );

        IsyNode {
            address: "1".to_string(),
            name: "Test".to_string(),
            node_type: node_type.to_string(),
            is_group: false,
            enabled: true,
            properties,
        }
    }

    #[test]
    fn classifies_opening_sensor_as_contact_sensor() {
        let node = binary_node("16.9.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::ContactSensor);
    }

    #[test]
    fn classifies_motion_sensor_as_motion_sensor() {
        let node = binary_node("16.1.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::MotionSensor);
    }

    #[test]
    fn classifies_moisture_sensor_as_water_sensor() {
        let node = binary_node("16.8.1.0", 255);
        assert_eq!(classify_node(&node), DeviceKind::WaterSensor);
    }

    #[test]
    fn water_sensor_state_inverts_dry_signal() {
        let node = binary_node("16.8.1.0", 255);
        let state = node_to_state(&node, &DeviceKind::WaterSensor);
        assert_eq!(state["water_detected"], json!(false));
        assert!(state.get("leak").is_none(), "the alias is gone");
    }

    #[test]
    fn water_sensor_event_patch_inverts_active_signal() {
        let event = IsyEvent {
            control: "ST".to_string(),
            node_addr: "1".to_string(),
            value: 0,
            uom: "78".to_string(),
            prec: 0,
        };

        let patch = event_to_patch(&event, &DeviceKind::WaterSensor).expect("patch");
        assert_eq!(patch["water_detected"], json!(true));
        assert!(patch.get("leak").is_none(), "the alias is gone");
    }
}
