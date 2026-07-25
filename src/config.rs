use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` when built without `schema`.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(RokuConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, conditionals, live
/// data sources, and prose. Published on the capability manifest; core serves
/// it at `GET /plugins/{id}/config/descriptor` and the editor renders it
/// directly.
///
/// Coverage note: a published descriptor is authoritative, so an omitted key is
/// uneditable. The Devices section binds to the **live device registry** rather
/// than this file's `[[devices]]` array — naming and room assignment belong to
/// the registry (core owns inventory), so those edits go to `/devices`. See
/// `descriptor_covers_every_schema_field` for the justified gaps.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Cond, Descriptor, Field, Section, Source};

    Descriptor::new("plugin.roku")
        .title("Roku")
        .section(
            Section::new("discovery", "Discovery")
                .help(
                    "Rokus announce themselves over SSDP, so devices on the same \
                     subnet are found without configuration. SSDP is link-local \
                     and does not cross VLANs — list an address under \
                     \"Cross-subnet hosts\" to reach one that routing can get to \
                     but multicast cannot.",
                )
                .field(
                    Field::toggle("roku.discovery_enabled")
                        .label("Auto-discover on the network")
                        .default(true),
                )
                .field(
                    Field::toggle("roku.auto_add_discovered")
                        .label("Register discovered devices automatically")
                        .default(true)
                        .visible_when(Cond::truthy("roku.discovery_enabled"))
                        .help(
                            "Off means discovery only reports what it finds; \
                             only devices listed below are registered.",
                        ),
                )
                .field(
                    Field::duration("roku.discovery_interval_secs")
                        .label("Re-scan every")
                        .unit("secs")
                        .default(900)
                        .min(60)
                        .visible_when(Cond::truthy("roku.discovery_enabled"))
                        .help("Also picks up address changes when DHCP moves a device."),
                )
                .field(
                    Field::duration("roku.discovery_timeout_secs")
                        .label("Listen for replies")
                        .unit("secs")
                        .default(4)
                        .min(1)
                        .max(10)
                        .visible_when(Cond::truthy("roku.discovery_enabled")),
                )
                .field(
                    Field::list("roku.manual_hosts", "host")
                        .label("Cross-subnet hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "IP or hostname of a Roku that SSDP cannot reach. \
                             Probed directly on every discovery sweep.",
                        ),
                ),
        )
        .section(
            Section::new("polling", "Polling")
                .help(
                    "Roku's control protocol has no push channel, so every state \
                     change is found by asking. Faster polling means state that \
                     tracks the remote more closely, at the cost of one HTTP \
                     round-trip per device per interval.",
                )
                .field(
                    Field::duration("roku.poll_interval_secs")
                        .label("Poll interval")
                        .unit("secs")
                        .default(10)
                        .min(1)
                        .help("Used while the device is powered on."),
                )
                .field(
                    Field::duration("roku.standby_poll_interval_secs")
                        .label("Standby poll interval")
                        .unit("secs")
                        .default(60)
                        .min(5)
                        .help(
                            "Used while the device reports standby. Nothing can \
                             change but the power state, so this is deliberately slower.",
                        ),
                )
                .field(
                    Field::duration("roku.request_timeout_secs")
                        .label("Request timeout")
                        .unit("secs")
                        .default(5)
                        .min(1)
                        .max(60),
                )
                .field(
                    Field::duration("roku.app_refresh_interval_secs")
                        .label("Refresh channel list every")
                        .unit("secs")
                        .default(3600)
                        .min(60)
                        .help(
                            "The installed-channel and TV-channel catalogues only \
                             change when someone adds a channel or re-scans the tuner.",
                        ),
                ),
        )
        .section(
            Section::new("control", "Control")
                .field(
                    Field::toggle("roku.wake_on_lan")
                        .label("Wake with Wake-on-LAN")
                        .default(true)
                        .help(
                            "A Roku TV that is fully off has no network stack, so \
                             the power-on command has nothing to reach. When this \
                             is on, powering on an unreachable device sends a \
                             magic packet to the MAC learned while it was last \
                             online.",
                        ),
                )
                .field(
                    Field::int("roku.key_hold_ms")
                        .label("Default key hold")
                        .unit("ms")
                        .default(500)
                        .min(50)
                        .max(10_000)
                        .help("How long a `key_hold` command presses a key when it doesn't say."),
                )
                .field(
                    Field::int("roku.type_delay_ms")
                        .label("Typing key delay")
                        .unit("ms")
                        .default(50)
                        .min(0)
                        .max(1000)
                        .help(
                            "Gap between characters when sending text. Roku's \
                             on-screen keyboards drop keys that arrive faster \
                             than the UI redraws.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("Roku devices")
                    .render("cards")
                    .key_by("device_id")
                    .help("Every Roku found by discovery or listed in config — set its name and room.")
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
                        .placeholder("info | debug | hc_roku=debug"),
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

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RokuConfig {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
    #[serde(default)]
    pub roku: RokuGlobalConfig,
    /// Explicitly configured devices. Optional — with discovery on, this
    /// is only needed to pin an id, to reach a device SSDP cannot see, or
    /// to run with `discovery_enabled = false`.
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
    "plugin.roku".into()
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
pub struct RokuGlobalConfig {
    /// Poll cadence while a device is powered on.
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Poll cadence while a device reports standby — nothing but the
    /// power state can change, so it is polled less often.
    #[serde(default = "default_standby_poll")]
    pub standby_poll_interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    /// How often the installed-channel and TV-channel catalogues are
    /// re-read. They change only when someone installs a channel or
    /// re-scans the tuner.
    #[serde(default = "default_app_refresh")]
    pub app_refresh_interval_secs: u64,
    #[serde(default = "default_true")]
    pub discovery_enabled: bool,
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
    #[serde(default = "default_discovery_timeout")]
    pub discovery_timeout_secs: u64,
    /// Register every discovered Roku, not just the configured ones.
    #[serde(default = "default_true")]
    pub auto_add_discovered: bool,
    /// Addresses probed directly on each sweep, for devices SSDP cannot
    /// reach (other VLANs, Wi-Fi segments that drop multicast).
    #[serde(default)]
    pub manual_hosts: Vec<String>,
    /// Fall back to a Wake-on-LAN magic packet when powering on a device
    /// that is not answering ECP.
    #[serde(default = "default_true")]
    pub wake_on_lan: bool,
    #[serde(default = "default_hold_ms")]
    pub key_hold_ms: u64,
    #[serde(default = "default_type_delay_ms")]
    pub type_delay_ms: u64,
}

fn default_poll() -> u64 {
    10
}
fn default_standby_poll() -> u64 {
    60
}
fn default_timeout() -> u64 {
    5
}
fn default_app_refresh() -> u64 {
    3600
}
fn default_discovery_interval() -> u64 {
    900
}
fn default_discovery_timeout() -> u64 {
    4
}
fn default_hold_ms() -> u64 {
    500
}
fn default_type_delay_ms() -> u64 {
    50
}
fn default_true() -> bool {
    true
}

impl Default for RokuGlobalConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll(),
            standby_poll_interval_secs: default_standby_poll(),
            request_timeout_secs: default_timeout(),
            app_refresh_interval_secs: default_app_refresh(),
            discovery_enabled: true,
            discovery_interval_secs: default_discovery_interval(),
            discovery_timeout_secs: default_discovery_timeout(),
            auto_add_discovered: true,
            manual_hosts: Vec::new(),
            wake_on_lan: true,
            key_hold_ms: default_hold_ms(),
            type_delay_ms: default_type_delay_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    /// IP address or hostname of the Roku.
    pub host: String,
    /// Stable homeCore device ID.
    pub hc_id: String,
    /// Human-readable display name.
    pub name: String,
    #[serde(default)]
    pub area: Option<String>,
    /// ECP port. Only worth setting when the device is reached through a
    /// port-forward rather than directly.
    #[serde(default)]
    pub port: Option<u16>,
    /// Per-device poll interval override (seconds).
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
}

impl RokuConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {path}"))?;
        toml::from_str(&content).with_context(|| format!("parsing config: {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[roku]` section is entirely optional — a config with nothing
    /// but broker settings must still start with working defaults.
    #[test]
    fn minimal_config_loads_with_defaults() {
        let cfg: RokuConfig = toml::from_str(
            r#"
            [homecore]
            broker_host = "127.0.0.1"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.homecore.plugin_id, "plugin.roku");
        assert_eq!(cfg.roku.poll_interval_secs, 10);
        assert!(cfg.roku.discovery_enabled);
        assert!(cfg.devices.is_empty());
    }

    #[test]
    fn devices_parse_with_optional_fields_absent() {
        let cfg: RokuConfig = toml::from_str(
            r#"
            [homecore]

            [[devices]]
            host  = "10.0.10.40"
            hc_id = "roku_living_room"
            name  = "Living Room Roku"
            "#,
        )
        .unwrap();
        let d = &cfg.devices[0];
        assert!(d.area.is_none());
        assert!(d.port.is_none());
        assert!(d.poll_interval_secs.is_none());
    }

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable. The check lives in the SDK; every schema leaf
    /// must be covered here or justified below.
    #[cfg(feature = "schema")]
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
                // either written by discovery or is a rare escape hatch:
                //
                //   host, hc_id        — discovered address and pinned identity
                //   port               — non-default ECP port (port-forwarding)
                //   poll_interval_secs — per-device override; the global one
                //                        covers every normal case
                //
                // The plugin does read all four (bridge.rs), so this is an
                // editing gap rather than dead config: changing them means
                // editing TOML.
                "devices[].host",
                "devices[].hc_id",
                "devices[].name",
                "devices[].area",
                "devices[].port",
                "devices[].poll_interval_secs",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
