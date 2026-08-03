//! Configuration types loaded from `config/config.toml`.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` without the `schema` feature.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(SonosConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, conditionals, live
/// data sources, and prose.
///
/// Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor` and the editor renders it directly
/// instead of guessing a form from the schema.
///
/// Note the Speakers section binds to the **live device registry** rather than
/// this file's `[[devices]]` array: naming and room assignment belong to the
/// device registry (core owns inventory), so those edits go to `/devices`.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Cond, Descriptor, Field, Section, Source};

    // Speakers bind to the live device registry rather than this file's
    // [[devices]] array: naming and room assignment belong to the device
    // registry (core owns inventory), so those edits go to /devices.
    let api_on = || Cond::truthy("api.enabled");
    let binds_all = || Cond::one_of("api.host", ["0.0.0.0", "::"]);

    Descriptor::new("plugin.sonos")
        .title("Sonos")
        .section(
            Section::new("discovery", "Discovery")
                .field(
                    Field::duration("sonos.discovery_interval_secs")
                        .label("Discovery interval")
                        .unit("secs")
                        .default(60)
                        .min(5)
                        .help("How often to re-run SSDP discovery."),
                )
                .field(
                    Field::duration("sonos.discovery_timeout_secs")
                        .label("Scan duration")
                        .unit("secs")
                        .default(5)
                        .min(1)
                        .help("How long each SSDP scan listens."),
                )
                .field(
                    Field::list("sonos.manual_hosts", "host")
                        .label("Manual hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "Static speaker IPs to probe in addition to SSDP \
                             — useful across subnets where multicast is dropped.",
                        ),
                ),
        )
        .section(
            Section::new("api", "HTTP API")
                .field(
                    Field::toggle("api.enabled")
                        .label("Enable HTTP API")
                        .default(true),
                )
                .field(
                    Field::note(
                        "A standalone web interface (independent of homeCore) for \
                         exploring each speaker — browse favorites and playlists, see \
                         now-playing and group state, and read diagnostics. Handy for \
                         content discovery and debugging.",
                    )
                    .visible_when(api_on()),
                )
                .field(
                    Field::link("Open web interface", "http://{client_host}:{api.port}/")
                        .help("Opens the Sonos HTTP API in a new tab.")
                        .visible_when(api_on()),
                )
                .field(
                    Field::host("api.host")
                        .label("Bind address")
                        .default("0.0.0.0")
                        .visible_when(api_on()),
                )
                .field(
                    Field::port("api.port")
                        .label("Port")
                        .default(5005)
                        .visible_when(api_on()),
                )
                .field(
                    Field::host("api.callback_host")
                        .label("Callback host")
                        .help("The LAN IP speakers reach for GENA event callbacks.")
                        .visible_when(api_on())
                        .required_when(binds_all()),
                )
                .field(
                    Field::note(
                        "When the API binds all interfaces (0.0.0.0), speakers need a \
                         concrete LAN IP to deliver event callbacks — set Callback host \
                         to this machine's address.",
                    )
                    // Both conditions: this advises about Callback host, which
                    // `api_on` hides. On `binds_all` alone the note outlived the
                    // field it describes — switching the API off left behind
                    // instructions pointing at a box no longer on screen.
                    .visible_when(Cond::all([api_on(), binds_all()])),
                ),
        )
        .section(
            Section::new("speakers", "Speakers").field(
                Field::table("devices")
                    .label("Speakers")
                    .render("cards")
                    .key_by("device_id")
                    .help("Every discovered speaker — set its name and room.")
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
                        .placeholder("info | debug | hc_sonos=debug"),
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

#[derive(Deserialize, Clone, Debug, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SonosConfig {
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub sonos: SonosSection,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

impl SonosConfig {
    pub fn load(path: &str) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config from {path}"))?;
        toml::from_str(&text).context("parsing config TOML")
    }
}

#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SonosSection {
    /// How often to re-run SSDP discovery (seconds).
    #[serde(default = "default_discovery_interval_secs")]
    pub discovery_interval_secs: u64,
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
            discovery_timeout_secs: default_discovery_timeout_secs(),
            manual_hosts: vec![],
        }
    }
}

/// A pre-configured speaker entry, matched to a discovered speaker by UUID.
///
/// **Only `hc_id` still takes effect.** It pins the speaker's homeCore device
/// id, which must stay stable because rules and dashboards reference it.
///
/// `name` and `area` are **ignored**: the label now follows what Sonos itself
/// reports, so renaming a speaker in the Sonos app reaches homeCore instead of
/// being masked by a value written here once. To pin a label or a room against
/// that sync, set the override in homeCore (`name_override` / `area_override`),
/// which this plugin never touches. The fields are retained so existing config
/// files keep parsing.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    /// Sonos speaker UUID (e.g. "RINCON_347E5C3D12E401400").
    pub uuid: String,
    /// HomeCore device ID (e.g. "sonos_living_room"). The only field still
    /// applied — it pins identity, which must stay stable for rules.
    pub hc_id: String,
    /// Ignored. Retained so existing config files keep parsing; the label now
    /// follows what Sonos reports. Pin one via homeCore's `name_override`.
    #[allow(
        dead_code,
        reason = "kept for config back-compat; superseded by name_override"
    )]
    pub name: String,
    /// Ignored. Retained for config back-compat; pin a room via homeCore's
    /// `area_override` instead.
    #[allow(
        dead_code,
        reason = "kept for config back-compat; superseded by area_override"
    )]
    pub area: Option<String>,
}

/// HTTP API configuration.  The API runs its own Axum server, completely
/// independent of HomeCore.  Disable with `enabled = false`.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiConfig {
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Set to false to disable the HTTP API entirely.
    #[serde(default = "default_api_enabled")]
    pub enabled: bool,
    /// The IP or hostname Sonos speakers can reach to deliver GENA NOTIFY
    /// callbacks.  Required when `host` is `"0.0.0.0"` (i.e. any interface).
    /// Example: `callback_host = "192.168.1.10"`.
    /// Defaults to `"127.0.0.1"` when not set (loopback only — useful for
    /// local testing; set to your LAN IP in production).
    pub callback_host: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
            enabled: default_api_enabled(),
            callback_host: None,
        }
    }
}

// ── defaults ─────────────────────────────────────────────────────────────────

fn default_api_host() -> String {
    "0.0.0.0".into()
}
fn default_api_port() -> u16 {
    5005
}
fn default_api_enabled() -> bool {
    true
}
fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.sonos".into()
}
fn default_discovery_interval_secs() -> u64 {
    60
}
fn default_discovery_timeout_secs() -> u64 {
    5
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable. This plugin is where that bit: it shipped with 2 of
    /// 6 `logging` fields declared, silently dropping the rest (`5bccebf`).
    /// The check lives in the SDK; it went missing here.
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity fixed at install, not an operator setting.
                "homecore.plugin_id",
                // The Speakers table binds to the live device registry, so its
                // edits go to /devices and never reach this file's [[devices]]
                // array. `uuid`/`hc_id` pin identity; `name`/`area` are inert,
                // retained only so existing config files keep parsing (use
                // homeCore's name_override / area_override instead).
                "devices[].uuid",
                "devices[].hc_id",
                "devices[].name",
                "devices[].area",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
