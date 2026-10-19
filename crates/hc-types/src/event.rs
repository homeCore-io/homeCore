//! Internal event bus types.
//!
//! All inter-crate communication flows through `Event` values.  The broker
//! bridge converts incoming MQTT messages into `Event::MqttMessage` variants;
//! the rule engine and state store consume and produce further event variants.

use crate::device::{Area, DeviceChange, DeviceState};
use crate::rule::{Rule, Scene};
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
        /// Conditions the plugin is currently reporting about itself — the
        /// full set each beat, replacing whatever core held. Empty for
        /// plugins on SDKs that predate notices, which is the same shape as
        /// "nothing to report", so no special case is needed.
        /// See [`crate::plugin_notice`].
        #[serde(default)]
        notices: Vec<crate::plugin_notice::PluginNotice>,
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
        /// The dashboard widgets the manifest declares, already validated —
        /// core drops any it cannot promise every client will draw, so a
        /// consumer of this event never has to ask again.
        ///
        /// Carried alongside rather than inside `Capabilities` for the same
        /// reason `config_schema` is: `spec = "1"` is frozen, and adding a
        /// field to it would source-break every plugin that builds a manifest
        /// literally — including the ones outside this repo. Typed all the
        /// same, because unlike a config schema a descriptor has a validator,
        /// and a `Value` here would mean every reader running it again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        widgets: Vec<crate::widget_descriptor::WidgetDescriptor>,
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
        /// The kinds of alert the plugin can raise (`alert_kinds` on the
        /// manifest), carried alongside for the same reason as `widgets`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        alert_kinds: Vec<crate::alert::PluginAlertKind>,
        /// The plugin's own screens (`ui` on the manifest), already
        /// validated; carried alongside for the same reason as `widgets`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ui: Option<crate::plugin_ui::PluginUi>,
    },
    /// A plugin said which of its alerts are true now for one device — or,
    /// when `device_id` is the plugin's own id, for itself. The whole list:
    /// one it no longer sends has cleared.
    PluginAlerts {
        timestamp: DateTime<Utc>,
        plugin_id: String,
        device_id: String,
        alerts: Vec<crate::alert::PluginAlert>,
    },
    /// A device's human-readable name was changed at the source (plugin or user).
    DeviceNameChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        previous_name: String,
        current_name: String,
    },
    /// A device appeared in the registry — first registered, or created by
    /// a manager (a timer, a switch, a mode). Carries the whole record, so a
    /// client holding the house can add it without a second request.
    DeviceAdded {
        timestamp: DateTime<Utc>,
        device: Box<DeviceState>,
    },
    /// A device left the registry.
    DeviceRemoved {
        timestamp: DateTime<Utc>,
        device_id: String,
    },
    /// Something about a device other than what it reports changed: its name
    /// or name override, its room, its type, `ui_hint`, icon, button names,
    /// hardware identity. Carries the whole record as it now stands.
    ///
    /// **This is what lets two open screens agree.** Metadata edits used to
    /// be saved without telling anybody, so a wall panel went on showing a
    /// device under its old name, in its old room, until it was reloaded.
    /// Raised by the state store on any write that changes these fields,
    /// whoever made it, so no edit path can forget to.
    ///
    /// [`Event::DeviceNameChanged`] still fires for a rename at the source,
    /// for the rules and logs that watch for it.
    DeviceMetadataChanged {
        timestamp: DateTime<Utc>,
        device: Box<DeviceState>,
    },
    /// A room was created, renamed, or gained an alias. Carries the room as
    /// it now stands, with its devices as of the change. A device moving
    /// between rooms is a [`Event::DeviceMetadataChanged`] on the device.
    RoomChanged {
        timestamp: DateTime<Utc>,
        room: Area,
    },
    /// A room was deleted — or merged into another, in which case its devices
    /// are now in `merged_into` and its slugs answer to that room.
    RoomRemoved {
        timestamp: DateTime<Utc>,
        room_id: uuid::Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merged_into: Option<uuid::Uuid>,
    },
    /// A homeCore scene was created or edited. Plugin scenes (Hue, Lutron,
    /// ISY) are devices, and change through the device events.
    SceneChanged {
        timestamp: DateTime<Utc>,
        scene: Scene,
    },
    /// A homeCore scene was deleted.
    SceneRemoved {
        timestamp: DateTime<Utc>,
        scene_id: uuid::Uuid,
    },
    /// A rule was created or changed through the API — as written, the way
    /// `GET /automations/{id}` returns it — so a screen showing it, or an
    /// editor holding a draft of it, knows someone else changed it.
    RuleChanged {
        timestamp: DateTime<Utc>,
        rule: Box<Rule>,
    },
    /// A rule was deleted through the API.
    RuleRemoved {
        timestamp: DateTime<Utc>,
        rule_id: uuid::Uuid,
    },
    /// A device's capability schema changed — what it declares it reports and
    /// accepts, not what it is currently reporting.
    ///
    /// **Schemas are not static, which is the reason this exists.** A Lutron
    /// phantom scene upgrades its schema a second after the bridge connects,
    /// once its LED query is answered; hc-ecowitt republishes whenever a
    /// sensor's attribute set changes; hc-hue republishes when an auxiliary
    /// device gains a facet; hc-zwave republishes on rescan. A client that
    /// renders controls from the schema — which is the point of publishing
    /// one — would otherwise show a scene with no status until somebody
    /// reloaded the page.
    ///
    /// Carries the declared attribute names rather than the whole schema, so a
    /// client can tell whether it cares before refetching. The schema itself
    /// is at `GET /devices/{id}/schema`.
    DeviceSchemaChanged {
        timestamp: DateTime<Utc>,
        device_id: String,
        /// Every attribute the new schema declares, sorted.
        attributes: Vec<String>,
        /// Ids of every action it declares, sorted.
        actions: Vec<String>,
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
    /// homeCore raised an alert (H4), or changed what an active one says.
    AlertRaised {
        timestamp: DateTime<Utc>,
        alert: crate::alert::Alert,
        /// An alert already active that now says something else (its words,
        /// or that someone has seen it) — not a new one. Automations that
        /// start "when an alert is raised" start only on a new one.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        update: bool,
    },
    /// An alert's condition ended.
    AlertCleared {
        timestamp: DateTime<Utc>,
        alert_id: String,
        device_id: String,
        /// The alert as it was, so a rule can tell which kind cleared.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alert: Option<Box<crate::alert::Alert>>,
    },
    /// Someone marked an active alert seen.
    AlertAcknowledged {
        timestamp: DateTime<Utc>,
        alert: crate::alert::Alert,
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
            | Event::DeviceAdded { timestamp, .. }
            | Event::DeviceRemoved { timestamp, .. }
            | Event::DeviceMetadataChanged { timestamp, .. }
            | Event::RoomChanged { timestamp, .. }
            | Event::RoomRemoved { timestamp, .. }
            | Event::SceneChanged { timestamp, .. }
            | Event::SceneRemoved { timestamp, .. }
            | Event::RuleChanged { timestamp, .. }
            | Event::RuleRemoved { timestamp, .. }
            | Event::DeviceSchemaChanged { timestamp, .. }
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
            | Event::DeviceBatteryRecovered { timestamp, .. }
            | Event::AlertRaised { timestamp, .. }
            | Event::AlertCleared { timestamp, .. }
            | Event::AlertAcknowledged { timestamp, .. }
            | Event::PluginAlerts { timestamp, .. } => *timestamp,
        }
    }
}

/// Severity levels for system alerts.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}
