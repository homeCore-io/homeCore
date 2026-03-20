//! Configuration types loaded from `config/config.toml`.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, Default)]
pub struct SonosConfig {
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub sonos: SonosSection,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

impl SonosConfig {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {path}"))?;
        toml::from_str(&text).context("parsing config TOML")
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct HomecoreConfig {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct SonosSection {
    /// How often to re-run SSDP discovery (seconds).
    #[serde(default = "default_discovery_interval_secs")]
    pub discovery_interval_secs: u64,
    /// How often to poll each speaker for state changes (seconds).
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// SSDP scan duration (seconds).
    #[serde(default = "default_discovery_timeout_secs")]
    pub discovery_timeout_secs: u64,
    /// Static IPs to probe in addition to SSDP.
    #[serde(default)]
    pub manual_hosts: Vec<String>,
}

impl Default for SonosSection {
    fn default() -> Self {
        Self {
            discovery_interval_secs: default_discovery_interval_secs(),
            poll_interval_secs:      default_poll_interval_secs(),
            discovery_timeout_secs:  default_discovery_timeout_secs(),
            manual_hosts:            vec![],
        }
    }
}

/// A pre-configured speaker entry.  Any speaker discovered via SSDP that
/// has a matching UUID will use these hc_id / name / area values instead of
/// the auto-generated ones.
#[derive(Deserialize, Clone, Debug)]
pub struct DeviceConfig {
    /// Sonos speaker UUID (e.g. "RINCON_347E5C3D12E401400").
    pub uuid: String,
    /// HomeCore device ID (e.g. "sonos_living_room").
    pub hc_id: String,
    /// Human-readable name surfaced in HomeCore.
    pub name: String,
    /// Optional room / area assignment.
    pub area: Option<String>,
}

// ── defaults ─────────────────────────────────────────────────────────────────

fn default_broker_host()            -> String { "127.0.0.1".into() }
fn default_broker_port()            -> u16    { 1883 }
fn default_plugin_id()              -> String { "plugin.sonos".into() }
fn default_discovery_interval_secs() -> u64   { 60 }
fn default_poll_interval_secs()     -> u64    { 5 }
fn default_discovery_timeout_secs() -> u64    { 5 }
