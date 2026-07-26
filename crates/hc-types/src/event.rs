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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
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
        /// Total milliseconds for condition evaluation + action execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
        /// Correlation ID for tracing the full execution chain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
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
    /// A plugin sent a heartbeat on MQTT (internal only, not forwarded to WS clients).
    PluginHeartbeat {
        timestamp: DateTime<Utc>,
        plugin_id: String,
        /// Self-reported plugin version.
        version: Option<String>,
        /// SDK version the plugin was built against — value of
        /// `plugin-sdk-rs`'s `CARGO_PKG_VERSION` at compile time.
        /// Optional for backward compat: heartbeats from plugins built
        /// against pre-Phase-B SDKs (≤ 0.1.2) won't carry this field.
        /// `state_bridge` reads it on first heartbeat per plugin per
        /// session and warns if MAJOR/MINOR diverge from the version
        /// core was built with. Component versioning plan, Phase B.
        sdk_version: Option<String>,
        /// Plugin uptime in seconds.
        uptime_secs: Option<u64>,
        /// Number of devices managed by this plugin.
        device_count: Option<u32>,
    },
    /// A plugin's status changed (started, stopped, offline, etc.).
    PluginStatusChanged {
        timestamp: DateTime<Utc>,
        plugin_id: String,
        status: String,
        previous_status: String,
    },
    /// A plugin published (or republished) its capability manifest.
    /// Emitted when `homecore/plugins/{id}/capabilities` arrives. Consumers
    /// cache the manifest in `PluginRecord.capabilities` so the HTTP API and
    /// hc-mcp can serve it without re-reading MQTT.
    PluginCapabilities {
        timestamp: DateTime<Utc>,
        plugin_id: String,
        capabilities: crate::plugin_capabilities::Capabilities,
        /// The manifest's `config_schema` field (JSON Schema for the plugin's
        /// operator config), if present. Carried alongside — rather than inside —
        /// the frozen `Capabilities` type so the config editor can render a typed
        /// form. `None` when the plugin published no schema (→ raw-TOML fallback).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_schema: Option<serde_json::Value>,
        /// The manifest's `config_descriptor` field — the plugin's own,
        /// expressive description of its configuration (sections, field kinds,
        /// conditionals, data sources). Rides alongside the schema the same
        /// way. `None` when the plugin published none, in which case the client
        /// auto-derives a baseline descriptor from `config_schema`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_descriptor: Option<serde_json::Value>,
    },
    /// A device's human-readable name was changed at the source (plugin or user).
    DeviceNameChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        previous_name: String,
        current_name: String,
    },
    /// A battery-powered device's level dropped to or below the configured
    /// alert threshold. Synthesized by the battery watcher with hysteresis,
    /// so this fires once per crossing — not on every battery report.
    DeviceBatteryLow {
        timestamp: DateTime<Utc>,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        /// Battery percentage that triggered the latch (0–100).
        battery_pct: f64,
        /// Threshold value that was crossed (0–100).
        threshold_pct: f64,
    },
    /// A previously-low device's battery has climbed back above the recover
    /// band (threshold + recover_band_pct). Counterpart to `DeviceBatteryLow`.
    DeviceBatteryRecovered {
        timestamp: DateTime<Utc>,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        /// Battery percentage at the time the latch cleared (0–100).
        battery_pct: f64,
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
    /// A rule was triggered but its conditions did not pass.
    RuleEvaluationFailed {
        timestamp: DateTime<Utc>,
        rule_id: String,
        rule_name: String,
        trigger_type: String,
        /// Which condition (zero-indexed) failed.
        failed_condition_index: usize,
        /// Human-readable reason for the failure.
        reason: String,
        /// Milliseconds spent evaluating conditions.
        eval_ms: u64,
    },
    /// A rule action failed during execution.
    ActionFailed {
        timestamp: DateTime<Utc>,
        rule_id: String,
        rule_name: String,
        /// Zero-based index of the failed action.
        action_index: usize,
        /// Action variant name (e.g. "SetDeviceState", "CallService").
        action_type: String,
        error: String,
        /// Correlation ID linking this failure to the rule firing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// A command was published to a device's MQTT cmd topic.
    DeviceCommandSent {
        timestamp: DateTime<Utc>,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        /// The command payload sent to the device.
        command: serde_json::Value,
        /// What initiated this command ("rule", "scene", "api").
        source: String,
        /// Rule or scene ID that initiated the command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_id: Option<String>,
        /// Correlation ID for tracing the full command chain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    /// A hub mode was turned on or off.
    ModeChanged {
        timestamp: DateTime<Utc>,
        mode_id: String,
        mode_name: String,
        on: bool,
    },
    /// A timer device changed state (started, paused, resumed, cancelled, finished).
    TimerStateChanged {
        timestamp: DateTime<Utc>,
        timer_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timer_name: Option<String>,
        /// New timer state: "running", "paused", "finished", "cancelled", "idle".
        state: String,
        /// Remaining seconds (if applicable).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_secs: Option<u64>,
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
            | Event::SystemAlert { timestamp, .. }
            | Event::RuleEvaluationFailed { timestamp, .. }
            | Event::ActionFailed { timestamp, .. }
            | Event::DeviceCommandSent { timestamp, .. }
            | Event::ModeChanged { timestamp, .. }
            | Event::TimerStateChanged { timestamp, .. }
            | Event::PluginHeartbeat { timestamp, .. }
            | Event::PluginStatusChanged { timestamp, .. }
            | Event::PluginCapabilities { timestamp, .. }
            | Event::DeviceBatteryLow { timestamp, .. }
            | Event::DeviceBatteryRecovered { timestamp, .. } => *timestamp,
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
