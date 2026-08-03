use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` when built without `schema`.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(WledConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, live data sources, and
/// prose. Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor` and the editor renders it directly.
///
/// Coverage note (phase 6): a published descriptor is authoritative, so an
/// omitted key is uneditable. The Devices section binds to the **live device
/// registry** rather than this file's `[[devices]]` array — naming and room
/// assignment belong to the registry (core owns inventory, per the
/// device-identity model), so those edits go to `/devices`.
///
/// That leaves the array's own keys unreachable from the form: `host` and
/// `hc_id` (written by mDNS discovery and the Discover action) and the
/// per-device `poll_interval_secs` override, where the global poll interval
/// covers the common case. The plugin *does* read all three, so correcting a
/// stale host means editing TOML — a real gap, listed as justified in
/// `descriptor_covers_every_schema_field`. `homecore.plugin_id` is bootstrap
/// identity fixed at install.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    Descriptor::new("plugin.wled")
        .title("WLED")
        .section(
            Section::new("discovery", "Discovery")
                .field(
                    Field::duration("wled.poll_interval_secs")
                        .label("Poll interval")
                        .unit("secs")
                        .default(30)
                        .min(1)
                        .help("How often to poll each WLED device for state. WLED has no push API, so state is read on this cadence."),
                )
                .field(
                    Field::list("wled.discovery_hosts", "host")
                        .label("Cross-subnet hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "Local WLEDs are found automatically over mDNS, which \
                             can't cross VLANs. List an IP/hostname here only to \
                             reach a WLED on another subnet — leave empty on a flat \
                             network.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("WLED devices")
                    .render("cards")
                    .key_by("device_id")
                    .help("Every WLED found by mDNS or the Discover action — set its name and room.")
                    .source(
                        Source::core_resource("devices")
                            .item_key("device_id")
                            .labels("name", "device_id"),
                    )
                    .columns([
                        Field::text("name").label("Name"),
                        Field::select("area")
                            .label("Room")
                            .placeholder("Unassigned")
                            .allow_create()
                            .source(Source::core_resource("areas")),
                    ]),
            ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_wled=debug"),
                )
                .field(
                    Field::enumeration("logging.log_forward_level")
                        .label("Forward to core")
                        .render("segmented")
                        .default("info")
                        .help(
                            "Minimum level forwarded to homeCore over MQTT; \
                             anything below is written locally only.",
                        )
                        .option("off", "Off")
                        .option("error", "Error")
                        .option("warn", "Warn")
                        .option("info", "Info")
                        .option("debug", "Debug"),
                )
                .field(
                    Field::enumeration("logging.rotation")
                        .label("Rotate")
                        .render("segmented")
                        .default("daily")
                        .option("hourly", "Hourly")
                        .option("daily", "Daily")
                        .option("weekly", "Weekly")
                        .option("never", "Never"),
                )
                .field(
                    Field::int("logging.max_size_mb")
                        .label("Rotate at size")
                        .unit("MB")
                        .default(100)
                        .min(0)
                        .help("Whichever comes first, this or the schedule. 0 disables size-based rotation."),
                )
                .field(
                    Field::int("logging.prune_after_days")
                        .label("Prune after")
                        .unit("days")
                        .default(0)
                        .min(0)
                        .help("Delete rotated files older than this. 0 = never prune."),
                )
                .field(
                    Field::toggle("logging.compress")
                        .label("Compress rotated files")
                        .default(true),
                ),
        )
        .section(
            Section::new("connection", "Connection")
                .hidden()
                .field(Field::host("homecore.broker_host").label("Broker host"))
                .field(Field::port("homecore.broker_port").label("Broker port"))
                .field(Field::secret("homecore.password").label("Broker password")),
        )
        .build()
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WledConfig {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub wled: WledGlobalConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomecoreConfig {
    #[serde(default = "default_host")]
    pub broker_host: String,
    #[serde(default = "default_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.wled".into()
}

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_host(),
            broker_port: default_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WledGlobalConfig {
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// OPTIONAL cross-subnet fallback for `discover_devices`. Local-subnet
    /// WLEDs are found automatically over mDNS (`_wled._tcp.local`), which is
    /// link-local and cannot cross VLANs — so list an IP/hostname here only to
    /// reach a WLED on a subnet the homeCore host can route to but mDNS can't.
    /// Each listed host is queried once via `/json/nodes` and its WLED-Sync
    /// peer list is merged into the result. Leave empty on a flat network.
    #[serde(default)]
    pub discovery_hosts: Vec<String>,
}

fn default_poll() -> u64 {
    30
}

impl Default for WledGlobalConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll(),
            discovery_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    /// IP address or hostname of the WLED device.
    pub host: String,
    /// Stable homeCore device ID.
    pub hc_id: String,
    /// Human-readable display name.
    pub name: String,
    #[serde(default)]
    pub area: Option<String>,
    /// Per-device polling interval override (seconds).
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
}

impl WledConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {path}"))?;
        toml::from_str(&content).with_context(|| format!("parsing config: {path}"))
    }
}
#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable (the hc-sonos logging bug, `5bccebf`). The check
    /// lives in the SDK; every schema leaf must be covered or justified.
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity fixed at install, not an operator setting.
                "homecore.plugin_id",
                // The Devices table binds to the live registry, so naming and
                // room assignment go to core (which owns inventory) and never
                // reach this file's [[devices]] array. What remains there is
                // written by mDNS discovery and the Discover action:
                //
                //   host, hc_id          — discovered address and pinned identity
                //   poll_interval_secs   — per-device override, TOML-only today
                //
                // The plugin does read them (bridge.rs, main.rs), so this is a
                // genuine editing gap rather than dead config: correcting a
                // stale host means editing TOML. Revisit if that bites.
                "devices[].host",
                "devices[].hc_id",
                "devices[].poll_interval_secs",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
