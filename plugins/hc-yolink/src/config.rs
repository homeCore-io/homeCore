use anyhow::{bail, Result};
use serde::Deserialize;

/// Operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` without the `schema` feature.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(Config)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin-authored config descriptor, published on the capability manifest.
///
/// The editor renders this *instead of* deriving a form from the JSON Schema,
/// so it is authoritative: a key omitted here cannot be edited from the UI at
/// all. `descriptor_covers_every_schema_field` below holds that line.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Cond, Descriptor, Field, Section, Source};

    Descriptor::new("plugin.yolink")
        .title("YoLink")
        .section(
            Section::new("mode", "Connection")
                .help(
                    "YoLink hubs come in two flavours. Cloud works with every hub \
                     but routes through YoLink's servers, so it needs internet. \
                     Local talks straight to a YS1606 on your LAN and keeps \
                     working when the internet does not — it is the better choice \
                     if you have that hub.",
                )
                .field(
                    Field::enumeration("yolink.mode")
                        .label("Talk to the hub via")
                        .render("segmented")
                        .default("cloud")
                        .option("cloud", "YoLink cloud")
                        .option("local", "Local hub"),
                ),
        )
        .section(
            Section::new("cloud", "YoLink cloud account")
                .visible_when(Cond::eq("yolink.mode", "cloud"))
                .help(
                    "From the YoLink app: Account → Advanced Settings → Personal \
                     Access Credentials.",
                )
                .field(
                    Field::text("yolink.cloud.uaid")
                        .label("UAID")
                        .required_when(Cond::eq("yolink.mode", "cloud"))
                        .help("The User Access Credential ID — not your account email."),
                )
                .field(
                    Field::secret("yolink.cloud.secret_key")
                        .label("Secret key")
                        .required_when(Cond::eq("yolink.mode", "cloud")),
                )
                .field(
                    Field::url("yolink.cloud.api_url")
                        .label("API URL")
                        .default("https://api.yosmart.com")
                        .help("Only change this if YoLink moves their endpoint."),
                )
                .field(
                    Field::host("yolink.cloud.mqtt_host")
                        .label("MQTT host")
                        .default("mqtt.api.yosmart.com"),
                )
                .field(
                    Field::port("yolink.cloud.mqtt_port")
                        .label("MQTT port")
                        .default(8003),
                ),
        )
        .section(
            Section::new("local", "Local hub")
                .visible_when(Cond::eq("yolink.mode", "local"))
                .help(
                    "From the YoLink app: Local Hub → Local Network → Integrations \
                     → Local API. All three credentials come from that one screen.",
                )
                .field(
                    Field::host("yolink.local.hub_ip")
                        .label("Hub address")
                        .placeholder("192.168.1.60")
                        .required_when(Cond::eq("yolink.mode", "local"))
                        .help(
                            "Give the hub a static DHCP lease — if its address \
                             moves, the plugin cannot find it.",
                        ),
                )
                .field(
                    Field::text("yolink.local.client_id")
                        .label("Client ID")
                        .required_when(Cond::eq("yolink.mode", "local")),
                )
                .field(
                    Field::secret("yolink.local.client_secret")
                        .label("Client secret")
                        .required_when(Cond::eq("yolink.mode", "local")),
                )
                .field(
                    Field::text("yolink.local.net_id")
                        .label("Network ID")
                        .required_when(Cond::eq("yolink.mode", "local"))
                        .help("On the same credentials screen; local access fails without it."),
                )
                .field(
                    Field::port("yolink.local.api_port")
                        .label("API port")
                        .default(1080),
                )
                .field(
                    Field::port("yolink.local.mqtt_port")
                        .label("MQTT port")
                        .default(18080),
                ),
        )
        .section(
            Section::new("polling", "Polling")
                .help(
                    "Devices push their state over MQTT as it changes. Polling is \
                     only a safety net for reports that went missing, so these \
                     intervals are deliberately slow — polling hard mostly \
                     re-reads what you already know and irritates the hub.",
                )
                .field(
                    Field::duration("yolink.poll_interval_secs")
                        .label("Full refresh every")
                        .unit("secs")
                        .default(3600)
                        .min(1),
                )
                .field(
                    Field::duration("yolink.inventory_interval_secs")
                        .label("Re-scan for new devices every")
                        .unit("secs")
                        .min(1)
                        .placeholder("Same as full refresh")
                        .help(
                            "How often to look for newly paired devices. Lower it \
                             to about 300 while you are adding hardware, so it \
                             appears without a restart.",
                        ),
                )
                .field(
                    Field::duration("yolink.poll_device_delay_ms")
                        .label("Wait between devices")
                        .unit("ms")
                        .default(1000)
                        .min(0)
                        .help(
                            "Paced on purpose. Polling a hub too fast returns \
                             `000201 Cannot connect to device` — that is rate \
                             limiting, not a broken device.",
                        ),
                )
                .field(
                    Field::duration("yolink.initial_fetch_delay_secs")
                        .label("Settle before first fetch")
                        .unit("secs")
                        .default(10)
                        .min(0)
                        .help(
                            "Lets the MQTT connection stabilise before adding HTTP \
                             load at startup. 0 skips the initial fetch and relies \
                             on reports plus the periodic refresh.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices")
                .field(
                    Field::enumeration("yolink.temperature_unit")
                        .label("Report temperatures in")
                        .render("segmented")
                        .default("F")
                        .option("F", "Fahrenheit")
                        .option("C", "Celsius")
                        .help(
                            "Devices report in their own unit; this is the unit \
                             everything is converted to before publishing.",
                        ),
                )
                .field(
                    Field::table("devices")
                        .label("YoLink devices")
                        .render("list")
                        .key_by("device_id")
                        .help(
                            "Everything paired to the hub. Names arrive from YoLink \
                             — edit one here to override what this house calls it, \
                             and assign it a room.",
                        )
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
                        .placeholder("info | debug | hc_yolink=debug"),
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
                        .help(
                            "Whichever comes first, this or the schedule. 0 disables \
                             size-based rotation.",
                        ),
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
// Top-level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub yolink: YolinkConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config file {path}: {e}"))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        // Minimal/unconfigured bootstrap (e.g. the sandbox writes just a
        // `[homecore]` block): with neither a cloud nor a local section the
        // operator hasn't entered credentials yet. Allow startup so the plugin
        // can publish its operator-config schema before creds are filled in.
        if self.yolink.cloud.is_none() && self.yolink.local.is_none() {
            return Ok(());
        }
        match self.yolink.mode {
            Mode::Cloud if self.yolink.cloud.is_none() => {
                bail!("[yolink.cloud] section is required when mode = \"cloud\"");
            }
            Mode::Local if self.yolink.local.is_none() => {
                bail!("[yolink.local] section is required when mode = \"local\"");
            }
            _ => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HomeCore broker connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
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

fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.yolink".into()
}

// ---------------------------------------------------------------------------
// YoLink settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Cloud,
    Local,
}

/// Display unit for temperatures reported to HomeCore.
///
/// YoLink devices report their own unit in the `tempUnit` field; this plugin
/// always converts to the configured unit before publishing.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TemperatureUnit {
    /// Degrees Celsius
    #[serde(rename = "C")]
    C,
    /// Degrees Fahrenheit (default)
    #[serde(rename = "F")]
    #[default]
    F,
}

impl TemperatureUnit {
    /// Convert a Celsius value to the target unit.
    pub fn convert_celsius(&self, c: f64) -> f64 {
        match self {
            TemperatureUnit::C => c,
            TemperatureUnit::F => c * 9.0 / 5.0 + 32.0,
        }
    }

    /// Convert a Fahrenheit value to the target unit.
    pub fn convert_fahrenheit(&self, f: f64) -> f64 {
        match self {
            TemperatureUnit::F => f,
            TemperatureUnit::C => (f - 32.0) * 5.0 / 9.0,
        }
    }

    /// Short label used in published state JSON, e.g. `"F"` or `"C"`.
    pub fn label(&self) -> &'static str {
        match self {
            TemperatureUnit::C => "C",
            TemperatureUnit::F => "F",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct YolinkConfig {
    pub mode: Mode,

    /// How often to poll all devices for a full state refresh (seconds).
    /// MQTT events are the primary delivery mechanism; this is a safety true-up.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// How often to re-scan the hub's device inventory (seconds).
    /// Lower this (e.g. 300) so newly-paired devices appear without a restart.
    /// Defaults to `poll_interval_secs` when unset.
    #[serde(default)]
    pub inventory_interval_secs: Option<u64>,

    /// Delay between individual device polls (milliseconds).
    /// Prevents 000201 "Cannot connect to device" rate-limit errors from the hub.
    #[serde(default = "default_poll_device_delay_ms")]
    pub poll_device_delay_ms: u64,

    /// Delay before starting the initial background state fetch (seconds).
    /// Gives the YoLink MQTT connection time to stabilize before adding HTTP
    /// load to the hub.  Set to 0 to disable (rely on MQTT reports + periodic poll).
    #[serde(default = "default_initial_fetch_delay_secs")]
    pub initial_fetch_delay_secs: u64,

    /// Unit used when publishing temperature values to HomeCore.
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,

    pub cloud: Option<CloudConfig>,
    pub local: Option<LocalConfig>,
}

impl Default for YolinkConfig {
    fn default() -> Self {
        Self {
            // No credentials configured yet — the operator picks a mode and
            // fills in cloud/local before the plugin can talk to a hub. This
            // default just lets the config parse from a `[homecore]`-only file
            // so the operator-config schema can be published first.
            mode: Mode::Cloud,
            poll_interval_secs: default_poll_interval(),
            inventory_interval_secs: None,
            poll_device_delay_ms: default_poll_device_delay_ms(),
            initial_fetch_delay_secs: default_initial_fetch_delay_secs(),
            temperature_unit: TemperatureUnit::default(),
            cloud: None,
            local: None,
        }
    }
}

fn default_poll_interval() -> u64 {
    3600
}
fn default_poll_device_delay_ms() -> u64 {
    1000
}
fn default_initial_fetch_delay_secs() -> u64 {
    10
}

// ---------------------------------------------------------------------------
// Cloud mode (YS1603 / YS1605)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CloudConfig {
    /// User Access Credential ID (from YoLink App → Account → Personal Access Credentials)
    pub uaid: String,
    /// UAC secret key
    pub secret_key: String,

    #[serde(default = "default_cloud_api_url")]
    pub api_url: String,
    #[serde(default = "default_cloud_mqtt_host")]
    pub mqtt_host: String,
    #[serde(default = "default_cloud_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_cloud_api_url() -> String {
    "https://api.yosmart.com".into()
}
fn default_cloud_mqtt_host() -> String {
    "mqtt.api.yosmart.com".into()
}
fn default_cloud_mqtt_port() -> u16 {
    8003
}

// ---------------------------------------------------------------------------
// Local mode (YS1606 Local Hub only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalConfig {
    /// Local IP address (or hostname) of the YS1606 hub on the LAN
    pub hub_ip: String,

    /// Client ID from YoLink App → Local Hub → Local Network → Integrations → Local API
    pub client_id: String,
    /// Client secret from the same screen
    pub client_secret: String,
    /// Network ID from the same credentials screen — required for local hub access
    pub net_id: String,

    /// HTTP port of the local hub's API server
    #[serde(default = "default_local_api_port")]
    pub api_port: u16,

    /// MQTT port of the local hub's broker
    #[serde(default = "default_local_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_local_api_port() -> u16 {
    1080
}
fn default_local_mqtt_port() -> u16 {
    18080
}

// ---------------------------------------------------------------------------
// Resolved endpoint bundle (computed once in main, passed around)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub token_url: String,
    pub api_base_url: String,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub client_id: String,
    pub client_secret: String,
    /// Local hub Net ID (local mode only; empty string for cloud mode).
    /// Used to build the MQTT topic prefix: `ylsubnet/{net_id}/+/report`
    pub net_id: String,
}

impl Endpoints {
    pub fn from_config(cfg: &YolinkConfig) -> Result<Self> {
        match cfg.mode {
            Mode::Cloud => {
                let c = cfg.cloud.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("[yolink.cloud] section is required when mode = \"cloud\"")
                })?;
                Ok(Self {
                    token_url: format!("{}/open/yolink/token", c.api_url),
                    api_base_url: c.api_url.clone(),
                    mqtt_host: c.mqtt_host.clone(),
                    mqtt_port: c.mqtt_port,
                    client_id: c.uaid.clone(),
                    client_secret: c.secret_key.clone(),
                    net_id: String::new(),
                })
            }
            Mode::Local => {
                let l = cfg.local.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("[yolink.local] section is required when mode = \"local\"")
                })?;
                let base = format!("http://{}:{}", l.hub_ip, l.api_port);
                Ok(Self {
                    token_url: format!("{}/open/yolink/token", base),
                    api_base_url: base,
                    mqtt_host: l.hub_ip.clone(),
                    mqtt_port: l.mqtt_port,
                    client_id: l.client_id.clone(),
                    client_secret: l.client_secret.clone(),
                    net_id: l.net_id.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    /// The descriptor is authoritative — the editor renders it instead of
    /// deriving a form from the schema — so anything it omits becomes
    /// uneditable from the UI. That is a silent regression, not a compile
    /// error, which is why this test exists rather than a code-review habit.
    #[test]
    #[cfg(feature = "schema")]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity, fixed at install time. Editing it would
                // rename the plugin out from under core's records.
                "homecore.plugin_id",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
