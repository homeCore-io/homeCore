//! What is wrong with a Hue device, as alerts a household can act on.
//!
//! Alerts are current state: each refresh says the whole list for a device,
//! and one that is no longer sent has cleared. The registry remembers what
//! was last said so an unchanged list is not republished every minute.

use plugin_sdk_rs::types::{AlertSeverity, PluginAlert, PluginAlertKind};
use serde_json::{Map, Value};

use crate::hue::models::HueLight;

// No battery alert here: homeCore raises its own from any device's
// `battery_pct`, and a second one from the plugin said it twice.

pub fn kinds() -> Vec<PluginAlertKind> {
    vec![
        PluginAlertKind {
            topic: "bridge_unreachable".into(),
            label: "Bridge not answering".into(),
            description: Some(
                "homeCore cannot reach a Hue bridge; its lights stop responding".into(),
            ),
            severity: AlertSeverity::Error,
            default_on: true,
        },
        PluginAlertKind {
            topic: "light_unreachable".into(),
            label: "Light not answering".into(),
            description: Some(
                "The bridge cannot reach a light — often switched off at the wall".into(),
            ),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: "update_ready".into(),
            label: "Software update waiting".into(),
            description: Some(
                "A Hue bridge, light or accessory has an update ready in the Hue app".into(),
            ),
            severity: AlertSeverity::Info,
            default_on: false,
        },
    ]
}

pub fn for_light(light: &HueLight) -> Vec<PluginAlert> {
    let mut out = Vec::new();
    if light.reachable == Some(false) {
        out.push(PluginAlert::new(
            "light_unreachable",
            "Not answering the bridge",
        ));
    }
    if light.update_state.as_deref() == Some("ready_to_install") {
        out.push(
            PluginAlert::new("update_ready", "A software update is ready")
                .key(light.software_version.clone().unwrap_or_default()),
        );
    }
    out
}

/// From what a sensor or switch publishes (all its facets together).
pub fn for_accessory(attrs: &Map<String, Value>) -> Vec<PluginAlert> {
    let mut out = Vec::new();
    if attrs
        .get("connectivity_status")
        .and_then(Value::as_str)
        .is_some_and(|s| s != "connected")
    {
        out.push(PluginAlert::new(
            "light_unreachable",
            "Not answering the bridge",
        ));
    }
    if attrs.get("update_state").and_then(Value::as_str) == Some("ready_to_install") {
        out.push(PluginAlert::new(
            "update_ready",
            "A software update is ready",
        ));
    }
    out
}

pub fn for_bridge(reachable: bool, host: &str, update_state: Option<&str>) -> Vec<PluginAlert> {
    let mut out = Vec::new();
    if !reachable {
        out.push(PluginAlert::new(
            "bridge_unreachable",
            format!("Not answering at {host}"),
        ));
    }
    if update_state == Some("ready_to_install") {
        out.push(PluginAlert::new(
            "update_ready",
            "A software update is ready",
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn topics(a: &[PluginAlert]) -> Vec<String> {
        a.iter().map(|a| a.topic.clone()).collect()
    }

    #[test]
    fn a_battery_is_left_to_homecore() {
        let low = json!({"battery_state": "low", "battery_pct": 12});
        assert!(for_accessory(low.as_object().unwrap()).is_empty());
        let lost = json!({"connectivity_status": "connectivity_issue"});
        assert_eq!(
            topics(&for_accessory(lost.as_object().unwrap())),
            ["light_unreachable"]
        );
    }

    #[test]
    fn every_kind_raised_is_declared() {
        let declared: Vec<String> = kinds().into_iter().map(|k| k.topic).collect();
        for t in ["bridge_unreachable", "light_unreachable", "update_ready"] {
            assert!(declared.contains(&t.to_string()), "{t}");
        }
    }
}
