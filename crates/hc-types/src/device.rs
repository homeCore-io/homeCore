//! Device registry types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

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
    /// Human-readable label (from plugin registration or user override).
    pub name: String,
    /// The plugin that owns this device.
    pub plugin_id: String,
    /// Optional area/room assignment.
    pub area: Option<String>,
    /// Device type tag from plugin registration (e.g. "switch", "scene", "binary_sensor").
    /// `None` for devices registered before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    /// Whether the device is currently reachable.
    pub available: bool,
    /// All reported attributes and their current values.
    pub attributes: HashMap<String, serde_json::Value>,
    /// Timestamp of the most recent state update.
    pub last_seen: DateTime<Utc>,
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
            name: name.into(),
            plugin_id: plugin_id.into(),
            area: None,
            device_type: None,
            available: false,
            attributes: HashMap::new(),
            last_seen: Utc::now(),
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
