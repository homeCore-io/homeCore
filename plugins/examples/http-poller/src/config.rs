//! Configuration types for the http-poller.
//!
//! Loaded from a TOML file.  See `http-poller.example.toml` for a fully
//! annotated reference.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// Top-level config file structure.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub plugin: PluginSection,
    /// One entry per device to poll.  TOML key is `[[poller]]`.
    #[serde(rename = "poller", default)]
    pub pollers: Vec<PollerConfig>,
}

impl AppConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config file '{path}': {e}"))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("TOML parse error in '{path}': {e}"))
    }
}

/// MQTT broker and plugin identity.
#[derive(Debug, Deserialize)]
pub struct PluginSection {
    #[serde(default = "default_plugin_id")]
    pub id: String,
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default)]
    pub password: String,
}

impl Default for PluginSection {
    fn default() -> Self {
        Self {
            id: default_plugin_id(),
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            password: String::new(),
        }
    }
}

fn default_plugin_id() -> String {
    "plugin.http-poller".into()
}
fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}

/// Configuration for a single polled device.
#[derive(Debug, Clone, Deserialize)]
pub struct PollerConfig {
    /// HomeCore device ID (e.g. `sensor.outdoor_weather`).
    pub device_id: String,
    /// Human-readable device name shown in the UI.
    pub name: String,
    /// URL to poll.
    pub url: String,
    /// Seconds between polls. Default: 30.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Per-request HTTP timeout in seconds. Default: 10.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Extra HTTP headers (API keys, auth tokens, etc.).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Device capability schema published to HomeCore at startup.
    /// If omitted, an empty schema is used (HomeCore still accepts the device).
    #[serde(default)]
    pub capabilities: Value,
    /// Dot-notation path mappings: `target_attr = "response.path"`.
    /// Supports nested keys (`main.temp`) and array indexing (`weather[0].description`).
    /// Ignored if `transform` is also set.
    #[serde(default)]
    pub field_map: HashMap<String, String>,
    /// Rhai script evaluated with `response` in scope.
    /// Must evaluate to a Rhai map (`#{ ... }`), which becomes the device state.
    /// Takes precedence over `field_map` when both are present.
    pub transform: Option<String>,
}

fn default_interval() -> u64 {
    30
}
fn default_timeout() -> u64 {
    10
}
