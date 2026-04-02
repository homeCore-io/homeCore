//! Internal event bus types.
//!
//! All inter-crate communication flows through `Event` values.  The broker
//! bridge converts incoming MQTT messages into `Event::MqttMessage` variants;
//! the rule engine and state store consume and produce further event variants.

use crate::device::DeviceChange;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Every occurrence that flows through the HomeCore internal bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A device attribute changed value.
    DeviceStateChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        previous: HashMap<String, serde_json::Value>,
        current: HashMap<String, serde_json::Value>,
        /// Attribute keys whose values changed (added, updated, or removed).
        changed: Vec<String>,
        /// Provenance for this state transition.
        change: DeviceChange,
    },
    /// A device came online or went offline.
    DeviceAvailabilityChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        available: bool,
    },
    /// An automation rule fired successfully.
    RuleFired {
        timestamp: DateTime<Utc>,
        rule_id: String,
        rule_name: String,
        /// Trigger type that caused this rule to fire (e.g. `"DeviceStateChanged"`).
        trigger_type: String,
        /// Number of actions that were dispatched.
        action_count: usize,
    },
    /// A scene was activated.
    SceneActivated {
        timestamp: DateTime<Utc>,
        scene_id: String,
        scene_name: String,
    },
    /// A plugin registered itself with the broker.
    PluginRegistered {
        timestamp: DateTime<Utc>,
        plugin_id: String,
    },
    /// A plugin stopped responding.
    PluginOffline {
        timestamp: DateTime<Utc>,
        plugin_id: String,
    },
    /// A device's human-readable name was changed at the source (plugin or user).
    DeviceNameChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        previous_name: String,
        current_name: String,
    },
    /// A raw MQTT message arrived (used before topic routing is applied).
    MqttMessage {
        timestamp: DateTime<Utc>,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
    /// A user-defined event fired by an automation action.
    Custom {
        timestamp: DateTime<Utc>,
        event_type: String,
        payload: serde_json::Value,
    },
    /// A system-level alert (disk full, broker error, etc.).
    SystemAlert {
        timestamp: DateTime<Utc>,
        severity: AlertSeverity,
        message: String,
    },
}

impl Event {
    /// Return the UTC timestamp embedded in every variant.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Event::DeviceStateChanged { timestamp, .. }
            | Event::DeviceAvailabilityChanged { timestamp, .. }
            | Event::RuleFired { timestamp, .. }
            | Event::SceneActivated { timestamp, .. }
            | Event::PluginRegistered { timestamp, .. }
            | Event::PluginOffline { timestamp, .. }
            | Event::DeviceNameChanged { timestamp, .. }
            | Event::MqttMessage { timestamp, .. }
            | Event::Custom { timestamp, .. }
            | Event::SystemAlert { timestamp, .. } => *timestamp,
        }
    }
}

/// Severity levels for system alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}
