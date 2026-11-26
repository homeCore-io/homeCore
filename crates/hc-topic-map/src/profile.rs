//! Ecosystem profile deserialization.
//!
//! Mirrors the TOML structure of `config/profiles/examples/*.toml`.
//! Each profile file contains one `[ecosystem]` block with nested
//! `[[ecosystem.state_topics]]`, `[[ecosystem.availability_topics]]`,
//! and `[[ecosystem.cmd_topics]]` arrays.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Top-level file wrapper
// ---------------------------------------------------------------------------

/// Root of a profile TOML file.
#[derive(Debug, Deserialize)]
pub struct ProfileFile {
    pub ecosystem: EcosystemProfile,
}

// ---------------------------------------------------------------------------
// Ecosystem profile
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EcosystemProfile {
    /// Unique name, e.g. "zigbee2mqtt".
    pub name: String,
    pub description: Option<String>,
    /// Prefix prepended to the captured device variable to form homeCore device IDs.
    /// e.g. "zigbee_" → device ID "zigbee_{friendly_name}".
    pub prefix: String,

    #[serde(default)]
    pub state_topics: Vec<StateTopicConfig>,
    #[serde(default)]
    pub availability_topics: Vec<AvailabilityTopicConfig>,
    #[serde(default)]
    pub cmd_topics: Vec<CmdTopicConfig>,
    /// What individual devices are, keyed by homeCore device ID. Overrides a
    /// state topic's `device_type` and `name` for that one device.
    #[serde(default)]
    pub devices: HashMap<String, DeviceConfig>,
}

// ---------------------------------------------------------------------------
// One device
// ---------------------------------------------------------------------------

/// What one device is: its type, which gives it a schema from the device type
/// catalog, and the name homeCore shows for it.
#[derive(Debug, Deserialize, serde::Serialize, Clone, Default, PartialEq, Eq)]
pub struct DeviceConfig {
    pub device_type: Option<String>,
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// State topic
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StateTopicConfig {
    /// MQTT topic pattern with `{var}` captures.
    pub pattern: String,
    /// Override the homeCore device ID template.
    /// Defaults to `{prefix}{device}` if not set.
    /// Supports `{var}` substitution from the topic captures.
    pub device_id: Option<String>,
    /// For scalar payloads: wrap the value under this attribute name.
    /// e.g. `attribute = "on"` turns `"1"` into `{"on": "1"}` before coercion.
    pub attribute: Option<String>,
    /// Rename ecosystem attribute keys → homeCore canonical names.
    /// Also supports dot-notation source keys for nested JSON fields:
    /// `"aenergy.total" = "energy_kwh"`.
    #[serde(default)]
    pub field_map: HashMap<String, String>,
    /// Coercions keyed by homeCore attribute name (after renaming).
    #[serde(default)]
    pub coerce: HashMap<String, String>,
    /// Auto-detect scalar type (string → bool/int/float).
    #[serde(default)]
    pub coerce_scalar: bool,
    /// Force partial-update semantics for this topic regardless of other settings.
    /// When true, the result is a JSON merge-patch rather than a full state replace.
    /// Useful for field_map-based topics (like nodeInfo) that extract a single field
    /// from a larger JSON payload and must not wipe out other device attributes.
    pub partial: Option<bool>,
    /// Map raw scalar string → typed output value. Applied when `attribute` is set,
    /// after `coerce_scalar` and `coerce`. The lookup key is the stringified coerced
    /// value (e.g. integer 1 → key "1").
    #[serde(default)]
    pub value_map: HashMap<String, Value>,
    /// Optional Rhai function name for fully custom payload transformation.
    pub transform: Option<String>,
    /// What the devices on this topic are, e.g. `"switch"`. A device with a
    /// type is registered with it and gets that type's schema from the
    /// device type catalog, so the app can show its controls.
    pub device_type: Option<String>,
    /// The name homeCore shows for a new device, with `{var}` substitution
    /// from the topic captures. Defaults to the device ID.
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Availability topic
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct AvailabilityTopicConfig {
    /// MQTT topic pattern.
    pub pattern: String,
    /// If set, extract this key from a JSON payload before value_map lookup.
    pub json_field: Option<String>,
    /// Payload format hint: "raw_bool" | "raw_string".
    /// raw_bool  — payload bytes are literally `true` or `false`.
    /// raw_string — payload is a plain string matched against value_map.
    pub payload: Option<String>,
    /// Map raw string/bool values to homeCore `available: bool`.
    #[serde(default)]
    pub value_map: HashMap<String, bool>,
}

// ---------------------------------------------------------------------------
// Command topic
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CmdTopicConfig {
    /// homeCore cmd topic pattern to match (source side).
    pub source: String,
    /// Native device command topic template (target side).
    pub target: Option<String>,
    /// Extract only this attribute from the homeCore JSON cmd payload,
    /// publishing just its scalar value (rather than the full JSON object).
    pub attribute: Option<String>,
    /// Rename homeCore canonical attribute names → ecosystem keys.
    #[serde(default)]
    pub field_map: HashMap<String, String>,
    /// Coercions applied after renaming, keyed by ecosystem key.
    #[serde(default)]
    pub coerce: HashMap<String, String>,
    /// For Shelly Gen2 RPC: the JSON-RPC method name.
    pub rpc_method: Option<String>,
    /// For Shelly Gen2 RPC: the component `id` parameter.
    pub rpc_id: Option<u32>,
    /// Optional Rhai function for custom cmd payload transformation.
    pub transform: Option<String>,
}
