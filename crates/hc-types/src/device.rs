//! Device registry types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use uuid::Uuid;

fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// High-level origin classification for the most recent device change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceChangeKind {
    Homecore,
    Physical,
    External,
    #[default]
    Unknown,
}

/// Provenance metadata for a device change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceChange {
    /// Timestamp when HomeCore observed or issued the change.
    #[serde(default = "now_utc")]
    pub changed_at: DateTime<Utc>,
    /// Broad classification used by rules and UI.
    #[serde(default)]
    pub kind: DeviceChangeKind,
    /// Specific source label such as `api`, `rule`, `scene`, `script`,
    /// `timer_manager`, or plugin-reported values like `matter_controller`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional stable actor identifier when known (user id, rule id, scene id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional human-readable actor label when known (username, rule name, scene name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    /// Optional correlation id linking a command to a later echoed state update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl Default for DeviceChange {
    fn default() -> Self {
        Self::unknown()
    }
}

impl DeviceChange {
    pub fn unknown() -> Self {
        Self {
            changed_at: Utc::now(),
            kind: DeviceChangeKind::Unknown,
            source: None,
            actor_id: None,
            actor_name: None,
            correlation_id: None,
        }
    }

    pub fn homecore(source: impl Into<String>) -> Self {
        Self {
            changed_at: Utc::now(),
            kind: DeviceChangeKind::Homecore,
            source: Some(source.into()),
            actor_id: None,
            actor_name: None,
            correlation_id: None,
        }
    }

    pub fn physical(source: Option<String>) -> Self {
        Self {
            changed_at: Utc::now(),
            kind: DeviceChangeKind::Physical,
            source,
            actor_id: None,
            actor_name: None,
            correlation_id: None,
        }
    }

    pub fn external(source: impl Into<String>) -> Self {
        Self {
            changed_at: Utc::now(),
            kind: DeviceChangeKind::External,
            source: Some(source.into()),
            actor_id: None,
            actor_name: None,
            correlation_id: None,
        }
    }

    pub fn with_actor(mut self, actor_id: Option<String>, actor_name: Option<String>) -> Self {
        self.actor_id = actor_id;
        self.actor_name = actor_name;
        self
    }

    pub fn with_correlation_id(mut self, correlation_id: Option<String>) -> Self {
        self.correlation_id = correlation_id;
        self
    }
}

fn parse_changed_at(value: Option<&JsonValue>) -> DateTime<Utc> {
    value
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn classify_legacy_source(source: &str) -> DeviceChangeKind {
    match source {
        "physical" | "manual" => DeviceChangeKind::Physical,
        "api" | "rule" | "scene" | "script" | "builtin" => DeviceChangeKind::Homecore,
        _ if source.starts_with("homecore") => DeviceChangeKind::Homecore,
        _ => DeviceChangeKind::External,
    }
}

/// Add reserved HomeCore command metadata to a command payload.
///
/// Existing command shapes remain intact. Metadata is nested under `_hc.command`
/// and the top-level `correlation_id` is populated when absent for ecosystem
/// integrations that already understand that field.
pub fn with_command_change_metadata(payload: JsonValue, change: &DeviceChange) -> JsonValue {
    let mut payload = match payload {
        JsonValue::Object(map) => map,
        other => return other,
    };

    let mut hc = payload
        .remove("_hc")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    hc.insert(
        "command".to_string(),
        serde_json::to_value(change).unwrap_or(JsonValue::Null),
    );
    payload.insert("_hc".to_string(), JsonValue::Object(hc));

    if !payload.contains_key("correlation_id") {
        if let Some(correlation_id) = &change.correlation_id {
            payload.insert(
                "correlation_id".to_string(),
                JsonValue::String(correlation_id.clone()),
            );
        }
    }

    JsonValue::Object(payload)
}

/// Add reserved HomeCore state-change metadata to a state payload.
///
/// Plugins should use this when they know the provenance of the state update,
/// especially when echoing a HomeCore command back as a resulting state change.
pub fn with_state_change_metadata(payload: JsonValue, change: &DeviceChange) -> JsonValue {
    let mut payload = match payload {
        JsonValue::Object(map) => map,
        other => return other,
    };

    let mut hc = payload
        .remove("_hc")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    hc.insert(
        "change".to_string(),
        serde_json::to_value(change).unwrap_or(JsonValue::Null),
    );
    payload.insert("_hc".to_string(), JsonValue::Object(hc));

    if !payload.contains_key("correlation_id") {
        if let Some(correlation_id) = &change.correlation_id {
            payload.insert(
                "correlation_id".to_string(),
                JsonValue::String(correlation_id.clone()),
            );
        }
    }

    JsonValue::Object(payload)
}

/// Extract HomeCore command provenance metadata from an inbound command payload.
pub fn extract_change_from_command_payload(payload: &JsonValue) -> Option<DeviceChange> {
    if let Some(change) = payload
        .get("_hc")
        .and_then(|v| v.get("command"))
        .cloned()
        .and_then(|v| serde_json::from_value::<DeviceChange>(v).ok())
    {
        return Some(change);
    }

    let source = payload.get("source").and_then(|v| v.as_str())?;
    Some(DeviceChange {
        changed_at: parse_changed_at(payload.get("timestamp")),
        kind: classify_legacy_source(source),
        source: Some(source.to_string()),
        actor_id: None,
        actor_name: None,
        correlation_id: payload
            .get("correlation_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Resolve a command payload into a concrete change record.
///
/// If the payload already contains HomeCore command metadata, it is preserved.
/// Otherwise the command is still treated as HomeCore-originated with the
/// provided fallback source label.
pub fn change_from_command_payload(payload: &JsonValue, fallback_source: &str) -> DeviceChange {
    extract_change_from_command_payload(payload)
        .unwrap_or_else(|| DeviceChange::homecore(fallback_source.to_string()))
}

/// Extract HomeCore state provenance metadata from an inbound state payload.
///
/// Reserved `_hc.change` metadata is preferred. Legacy top-level `origin`,
/// `correlation_id`, and `timestamp` fields are also recognized.
pub fn extract_change_from_state_payload(payload: &JsonValue) -> Option<DeviceChange> {
    if let Some(change) = payload
        .get("_hc")
        .and_then(|v| v.get("change"))
        .cloned()
        .and_then(|v| serde_json::from_value::<DeviceChange>(v).ok())
    {
        return Some(change);
    }

    let source = payload.get("origin").and_then(|v| v.as_str())?;
    Some(DeviceChange {
        changed_at: parse_changed_at(payload.get("timestamp")),
        kind: classify_legacy_source(source),
        source: Some(source.to_string()),
        actor_id: None,
        actor_name: None,
        correlation_id: payload
            .get("correlation_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Canonical state snapshot for a single device.
///
/// The `attributes` map holds all device-specific key/value pairs described
/// by the device's capability schema (e.g. `"on"`, `"brightness"`, `"color_temp"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    /// Stable, unique identifier assigned at plugin registration time.
    pub device_id: String,
    /// Stable, rule-facing canonical name assigned by HomeCore.
    ///
    /// Unlike `name`, this is intended to stay stable even if the display
    /// label changes. Rules may use this value instead of the plugin-owned
    /// `device_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_name: Option<String>,
    /// Optional UI-facing status icon override selected by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_icon: Option<String>,
    /// Human-readable label (from plugin registration or user override).
    pub name: String,
    /// The plugin that owns this device.
    pub plugin_id: String,
    /// Optional area/room assignment.
    pub area: Option<String>,
    /// Canonical device type tag from plugin registration
    /// (e.g. "switch", "light", "motion_sensor", "virtual_switch").
    /// `None` for devices registered before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    /// Whether the device is currently reachable.
    pub available: bool,
    /// All reported attributes and their current values.
    pub attributes: HashMap<String, serde_json::Value>,
    /// Timestamp of the most recent state update.
    pub last_seen: DateTime<Utc>,
    /// Provenance of the most recent meaningful state update, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change: Option<DeviceChange>,
}

impl DeviceState {
    /// Create a minimal `DeviceState` with empty attributes.
    pub fn new(
        device_id: impl Into<String>,
        name: impl Into<String>,
        plugin_id: impl Into<String>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            canonical_name: None,
            status_icon: None,
            name: name.into(),
            plugin_id: plugin_id.into(),
            area: None,
            device_type: None,
            available: false,
            attributes: HashMap::new(),
            last_seen: Utc::now(),
            last_change: None,
        }
    }
}

/// Registration payload a plugin sends on first connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRegistration {
    pub device_id: String,
    pub plugin_id: String,
    pub name: String,
    pub area: Option<String>,
    /// JSON Schema object describing the device's capabilities.
    pub capabilities: serde_json::Value,
}

/// A logical grouping of devices (room, zone, floor, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Area {
    pub id: Uuid,
    pub name: String,
    pub device_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_metadata_round_trips() {
        let change = DeviceChange::homecore("api")
            .with_actor(Some("user-1".into()), Some("john".into()))
            .with_correlation_id(Some("corr-123".into()));
        let payload = with_command_change_metadata(json!({ "on": true }), &change);

        assert_eq!(
            extract_change_from_command_payload(&payload),
            Some(change.clone())
        );
        assert_eq!(payload.get("on"), Some(&json!(true)));
        assert_eq!(payload.get("correlation_id"), Some(&json!("corr-123")));
    }

    #[test]
    fn state_change_extracts_legacy_origin_metadata() {
        let payload = json!({
            "on": true,
            "origin": "physical",
            "correlation_id": "corr-456",
            "timestamp": "2026-04-01T12:00:00Z"
        });

        let change = extract_change_from_state_payload(&payload).expect("change metadata");
        assert_eq!(change.kind, DeviceChangeKind::Physical);
        assert_eq!(change.source.as_deref(), Some("physical"));
        assert_eq!(change.correlation_id.as_deref(), Some("corr-456"));
    }
}
