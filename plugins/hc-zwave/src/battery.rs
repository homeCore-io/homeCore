//! Whether a node's battery is low: `battery_low`.
//!
//! homeCore's alerts raise a low battery from `battery_pct` at its own line
//! (20 % unless changed) or from `battery_low: true`. This decides the second.
//!
//! **The device's own warning.** zwave-js 15.10 stopped keeping Battery CC's
//! `isLow` value — a "battery low" report is now a `notification` event
//! (`ccId` 128, `eventType: "battery low"`) — and later releases delete the
//! old values on startup. So the warning is taken from the event, and also
//! from what remains a value: Battery CC v2's `rechargeOrReplace` and
//! Notification CC's Power Management "Battery level status". `isLow` is
//! still read for an older zwave-js.
//!
//! **A lock earlier.** A lock stops answering well before its battery reads
//! empty, so a lock's battery is low at [`LOCK_LOW_PCT`] or below — earlier
//! than homeCore's line for everything else.
//!
//! **Over when it reads healthy.** A warning is an event, not a state: it
//! stays said until the battery reports above [`HEALTHY_ABOVE_PCT`] (new
//! batteries) with no warning value still standing.

use crate::translator::property_key_str;
use crate::types::NodeState;
use serde_json::Value;

/// A lock's battery is low at or below this.
pub const LOCK_LOW_PCT: f64 = 30.0;
/// A warning is over once the battery reads above this.
pub const HEALTHY_ABOVE_PCT: f64 = 30.0;

/// Battery CC.
const BATTERY: u32 = 128;
/// Notification CC.
const NOTIFICATION: u32 = 113;

/// Power Management "Battery level status": replace soon, replace now,
/// charge soon, charge now.
const LEVEL_STATUS_LOW: [u64; 4] = [0x0a, 0x0b, 0x0e, 0x0f];

/// Whether a value feeds this decision, so a change to it re-decides.
pub fn is_input(cc: u32, property: &str, property_key: Option<&str>) -> bool {
    match cc {
        BATTERY => matches!(property, "level" | "isLow" | "rechargeOrReplace"),
        NOTIFICATION => {
            property == "Power Management" && property_key == Some("Battery level status")
        }
        _ => false,
    }
}

/// Whether a `notification` event is the device's battery warning.
pub fn is_warning_event(cc: Option<u64>, args: &Value) -> bool {
    cc == Some(u64::from(BATTERY))
        && args.get("eventType").and_then(Value::as_str) == Some("battery low")
}

/// `Some(true)` low, `Some(false)` healthy, `None` nothing new to say (a
/// device that warned and still reads low keeps its warning).
pub fn low(node: &NodeState, is_lock: bool) -> Option<bool> {
    let mut level = None;
    let mut warned = false;
    for v in &node.values {
        let Some(raw) = v.value.as_ref() else {
            continue;
        };
        let pk = v.property_key.as_ref().and_then(property_key_str);
        match (v.command_class, v.property.as_str(), pk.as_deref()) {
            (BATTERY, "level", _) => level = raw.as_f64().or(level),
            (BATTERY, "isLow", _) => warned |= raw.as_bool() == Some(true),
            (BATTERY, "rechargeOrReplace", _) => warned |= raw.as_u64().is_some_and(|n| n > 0),
            (NOTIFICATION, "Power Management", Some("Battery level status")) => {
                warned |= raw.as_u64().is_some_and(|n| LEVEL_STATUS_LOW.contains(&n));
            }
            _ => {}
        }
    }
    if warned {
        return Some(true);
    }
    let level = level?;
    if is_lock && level <= LOCK_LOW_PCT {
        Some(true)
    } else if level > HEALTHY_ABOVE_PCT {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeValue;
    use serde_json::json;

    fn node(values: &[(u32, &str, Option<&str>, Value)]) -> NodeState {
        let mut n: NodeState = serde_json::from_value(json!({ "nodeId": 23 })).unwrap();
        n.values = values
            .iter()
            .map(|(cc, prop, pk, v)| NodeValue {
                command_class: *cc,
                endpoint: 0,
                property: (*prop).into(),
                property_key: pk.map(|k| json!(k)),
                value: Some(v.clone()),
                metadata: None,
            })
            .collect();
        n
    }

    #[test]
    fn a_lock_is_low_from_thirty_percent() {
        let at = |pct| node(&[(128, "level", None, json!(pct))]);
        assert_eq!(low(&at(23), true), Some(true));
        assert_eq!(low(&at(30), true), Some(true));
        assert_eq!(low(&at(31), true), Some(false));
    }

    #[test]
    fn anything_else_is_left_to_homecores_line() {
        let at = |pct| node(&[(128, "level", None, json!(pct))]);
        assert_eq!(low(&at(23), false), None);
        assert_eq!(low(&at(10), false), None);
        assert_eq!(low(&at(80), false), Some(false));
        assert_eq!(low(&node(&[]), false), None);
    }

    #[test]
    fn the_devices_own_warning_values() {
        let healthy = (128, "level", None, json!(90));
        assert_eq!(
            low(
                &node(&[healthy.clone(), (128, "isLow", None, json!(true))]),
                false
            ),
            Some(true)
        );
        assert_eq!(
            low(
                &node(&[healthy.clone(), (128, "rechargeOrReplace", None, json!(1))]),
                false
            ),
            Some(true)
        );
        assert_eq!(
            low(
                &node(&[healthy.clone(), (128, "rechargeOrReplace", None, json!(0))]),
                false
            ),
            Some(false)
        );
        let status = |n| {
            (
                113,
                "Power Management",
                Some("Battery level status"),
                json!(n),
            )
        };
        assert_eq!(
            low(&node(&[healthy.clone(), status(0x0a)]), false),
            Some(true)
        );
        assert_eq!(
            low(&node(&[healthy.clone(), status(0x0b)]), false),
            Some(true)
        );
        assert_eq!(low(&node(&[healthy, status(0)]), false), Some(false));
    }

    #[test]
    fn the_battery_low_event_as_zwave_js_sends_it() {
        // zwave-js 15.10+: node.emit("notification", endpoint, 128,
        // { eventType: "battery low", urgency }).
        let args = json!({ "eventType": "battery low", "urgency": 1 });
        assert!(is_warning_event(Some(128), &args));
        assert!(!is_warning_event(Some(113), &args));
        assert!(!is_warning_event(
            Some(128),
            &json!({ "eventType": "other" })
        ));
    }

    #[test]
    fn inputs() {
        assert!(is_input(128, "level", None));
        assert!(is_input(128, "isLow", None));
        assert!(is_input(128, "rechargeOrReplace", None));
        assert!(is_input(
            113,
            "Power Management",
            Some("Battery level status")
        ));
        assert!(!is_input(113, "Power Management", Some("Over-load status")));
        assert!(!is_input(128, "temperature", None));
    }
}
