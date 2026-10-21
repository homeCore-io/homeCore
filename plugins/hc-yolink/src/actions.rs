//! The plugin's actions and the kinds of alert it raises.

use plugin_sdk_rs::types::{
    Action, AlertSeverity, Capabilities, Concurrency, PluginAlertKind, RequiresRole,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::bridge::{self, Shared};

fn action(id: &str, label: &str, description: &str, role: RequiresRole) -> Action {
    Action {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        params: None,
        result: None,
        stream: false,
        cancelable: false,
        concurrency: Concurrency::default(),
        item_key: None,
        item_operations: None,
        requires_role: role,
        timeout_ms: None,
        danger: false,
    }
}

pub fn manifest() -> Capabilities {
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            action(
                "status",
                "Status",
                "The hub, the live event feed (connected, since, last event), and every \
                 device: when it last reported, its battery (0–4), its signal, whether it \
                 is reporting. From memory — it asks the hub nothing.",
                RequiresRole::ReadOnly,
            ),
            action(
                "poll_now",
                "Check every device now",
                "Ask the hub for every device's state now, a few seconds apart, instead of \
                 waiting for the hourly check.",
                RequiresRole::User,
            ),
            action(
                "rescan_devices",
                "Rescan devices",
                "Read the hub's device list again and republish each device's registration \
                 and schema.",
                RequiresRole::User,
            ),
        ],
    }
}

/// The alerts this plugin raises. A leak, a device offline and a low battery
/// are homeCore's own (from `water_detected`, availability and
/// `battery_low`), and are not repeated here.
pub fn alert_kinds() -> Vec<PluginAlertKind> {
    vec![
        PluginAlertKind {
            topic: "lock_failed".into(),
            label: "Lock didn't lock or unlock".into(),
            description: Some(
                "The lock tried and failed — the bolt jammed or the door was in the way. \
                 Clears the next time it locks or unlocks."
                    .into(),
            ),
            severity: AlertSeverity::Error,
            default_on: true,
        },
        PluginAlertKind {
            topic: "door_left_open".into(),
            label: "Door left open".into(),
            description: Some(
                "A door sensor (or a lock's door) has stayed open past its reminder, as set \
                 in the YoLink app. Clears when it closes."
                    .into(),
            ),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: "leak_sensor_fault".into(),
            label: "Leak sensor fault".into(),
            description: Some(
                "A leak sensor reports a probe fault or freezing: it may not notice water.".into(),
            ),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: "sensor_silent".into(),
            label: "Sensor not reporting".into(),
            description: Some(
                "A battery sensor has missed its check-ins and is shown offline. homeCore's \
                 own Offline alert says the same, so this one is off unless turned on."
                    .into(),
            ),
            severity: AlertSeverity::Warning,
            default_on: false,
        },
    ]
}

fn ok(v: Value) -> Value {
    let mut v = v;
    v["status"] = json!("ok");
    v
}

/// The SDK's custom handler. `None` for anything not ours.
pub fn handle(s: &Option<Arc<Shared>>, cmd: &Value) -> Option<Value> {
    let action = cmd.get("action")?.as_str()?;
    if !matches!(action, "status" | "poll_now" | "rescan_devices") {
        return None;
    }
    let Some(s) = s else {
        return Some(json!({
            "status": "ok",
            "configured": false,
            "devices": [],
            "feed": { "connected": false },
        }));
    };
    Some(match action {
        "status" => bridge::status(s),
        "poll_now" => {
            let busy = *s.polling.lock().unwrap_or_else(|e| e.into_inner());
            if !busy {
                s.poll_now.notify_one();
            }
            ok(json!({ "already": busy }))
        }
        "rescan_devices" => {
            s.rescan.notify_one();
            ok(json!({}))
        }
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_handled_is_declared() {
        let declared: Vec<String> = manifest().actions.into_iter().map(|a| a.id).collect();
        for id in ["status", "poll_now", "rescan_devices"] {
            assert!(declared.contains(&id.to_string()), "{id}");
            assert!(handle(&None, &json!({ "action": id })).is_some(), "{id}");
        }
        assert!(handle(&None, &json!({ "action": "ping" })).is_none());
    }

    /// Status is for anyone who can see devices; the rest touch the hub.
    #[test]
    fn status_is_read_only() {
        let m = manifest();
        let status = m.actions.iter().find(|a| a.id == "status").unwrap();
        assert!(matches!(status.requires_role, RequiresRole::ReadOnly));
    }

    #[test]
    fn alerts_do_not_repeat_cores() {
        let topics: Vec<String> = alert_kinds().into_iter().map(|k| k.topic).collect();
        for core in ["wet", "offline", "low_battery", "jammed"] {
            assert!(!topics.contains(&core.to_string()));
        }
    }
}
