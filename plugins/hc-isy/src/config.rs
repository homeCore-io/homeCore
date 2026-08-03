//! Plugin configuration — loaded from `config/config.toml`.

use anyhow::Result;
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

    Descriptor::new("plugin.isy")
        .title("ISY / IoX")
        .section(
            Section::new("controller", "Controller")
                .help(
                    "The ISY994 or eisy/Polisy that owns the Insteon and Z-Wave \
                     devices. This plugin signs in as an administrator to read the \
                     node list and subscribe to its event stream.",
                )
                .field(
                    Field::host("isy.host")
                        .label("Address")
                        .placeholder("192.168.1.50 or isy.local")
                        .required()
                        .help("Hostname or IP of the controller on your LAN."),
                )
                .field(
                    Field::port("isy.port")
                        .label("Port")
                        .default(80)
                        .help("80 for HTTP, 443 when TLS is on."),
                )
                .field(
                    Field::toggle("isy.tls")
                        .label("Use HTTPS")
                        .default(false)
                        .help(
                            "ISY controllers ship a self-signed certificate, so \
                             certificate verification is skipped when this is on — \
                             it encrypts the link but does not authenticate the \
                             controller.",
                        ),
                )
                .field(
                    Field::text("isy.username")
                        .label("Username")
                        .default("admin")
                        .required()
                        .help("Usually `admin`."),
                )
                .field(
                    Field::secret("isy.password")
                        .label("Password")
                        .required_when(Cond::truthy("isy.username")),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("ISY nodes")
                    .render("list")
                    .key_by("device_id")
                    .help(
                        "Every node read from the controller. Names arrive from the \
                         ISY — edit one here to override what this house calls it, \
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
                        .placeholder("info | debug | hc_isy=debug"),
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
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub isy: IsyConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config {path}: {e}"))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// [homecore] — MQTT broker connection and plugin identity
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
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
    "plugin.isy".into()
}

// ---------------------------------------------------------------------------
// [isy] — ISY/IoX controller connection
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IsyConfig {
    /// ISY/IoX controller hostname or IP address.
    #[serde(default)]
    pub host: String,

    /// HTTP port.  Default 80 (HTTP) or 443 (TLS).
    #[serde(default = "default_isy_port")]
    pub port: u16,

    /// ISY administrator username (usually "admin").
    #[serde(default)]
    pub username: String,

    /// ISY administrator password.
    #[serde(default)]
    pub password: String,

    /// Use HTTPS/WSS instead of HTTP/WS.
    /// The ISY typically uses a self-signed certificate; certificate
    /// verification is skipped when tls = true.
    #[serde(default)]
    pub tls: bool,
}

impl Default for IsyConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_isy_port(),
            username: String::new(),
            password: String::new(),
            tls: false,
        }
    }
}

fn default_isy_port() -> u16 {
    80
}

#[cfg(test)]
mod tests {
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
