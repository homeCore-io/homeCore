//! Plugin configuration — loaded from `config/config.toml`.
//!
//! See `config/config.toml.example` for a fully annotated reference.

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
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    Descriptor::new("plugin.zwave")
        .title("Z-Wave")
        .section(
            Section::new("server", "Z-Wave JS Server")
                .help(
                    "This plugin does not talk to the radio itself — it drives a \
                     zwave-js-server instance, which owns the USB stick. Run that \
                     first; these settings say where to find it.",
                )
                .field(
                    Field::url("server.url")
                        .label("Server URL")
                        .default("ws://localhost:3000")
                        .placeholder("ws://localhost:3000")
                        .required()
                        .help(
                            "WebSocket address of zwave-js-server. Use `ws://` on a \
                             trusted LAN, `wss://` if it sits behind TLS.",
                        ),
                )
                .field(
                    Field::int("server.schema_version")
                        .label("Schema version")
                        .default(32)
                        .min(0)
                        .help(
                            "Protocol version to negotiate. The value is clamped to \
                             the range the server advertises, so the default works \
                             with every current zwave-js — raise it only when a \
                             newer server exposes something this plugin needs.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("Z-Wave nodes")
                    .render("list")
                    .key_by("device_id")
                    .help(
                        "Every node in the mesh. Names arrive from zwave-js — edit \
                         one here to override what this house calls it, and assign \
                         it a room.",
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
                        .placeholder("info | debug | hc_zwave=debug"),
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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    /// Optional, like every field inside it.
    ///
    /// It used to be mandatory, and every field it holds already had a default,
    /// so the only thing a config could be missing was the section header — and
    /// missing it was fatal. `Config::load` returned "missing field `homecore`"
    /// and main exited 1 before logging anything else.
    ///
    /// That happened on a real house: hc-web's config editor wrote a document
    /// containing only the fields its form covered, `[homecore]` was not one of
    /// them, and this plugin then failed to start every 60 seconds for the
    /// better part of an hour. The editor bug is fixed and core no longer hands
    /// out configs it could not parse, but a plugin should not be one dropped
    /// section away from a restart loop when it has a working default for every
    /// value in it.
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub server: ServerConfig,
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

fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.zwave".into()
}

// ---------------------------------------------------------------------------
// [server] — zwave-js-server WebSocket endpoint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServerConfig {
    /// WebSocket URL of the zwave-js-server, e.g. `"ws://localhost:3000"`.
    #[serde(default = "default_server_url")]
    pub url: String,
    /// Schema version to negotiate. Clamped to the server's advertised min/max.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: default_server_url(),
            schema_version: default_schema_version(),
        }
    }
}

fn default_server_url() -> String {
    "ws://localhost:3000".into()
}
fn default_schema_version() -> u32 {
    32
}

#[cfg(test)]
mod tests {
    /// The config that took the plugin down, and must not any more.
    ///
    /// hc-web's editor wrote a document holding only the fields its form
    /// covered. `[homecore]` was not among them, and this plugin then refused
    /// to start — "missing field `homecore`", exit 1, every 60 seconds, for the
    /// better part of an hour, against a zwave-js-server that was up the whole
    /// time on the address the file already named.
    #[test]
    fn a_config_without_the_homecore_section_still_loads() {
        let cfg: Config = toml::from_str("[server]\nurl = \"ws://10.0.10.123:3000\"\n")
            .expect("a missing [homecore] must not be fatal");

        // The setting the operator actually cared about.
        assert_eq!(cfg.server.url, "ws://10.0.10.123:3000");
        // And the section that was missing falls back to the same values it
        // would have held had it been written out in full.
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.homecore.broker_port, 1883);
        assert_eq!(cfg.homecore.plugin_id, "plugin.zwave");
        assert!(cfg.homecore.password.is_empty());
    }

    /// An explicit section still wins — the default must not shadow real
    /// values, which is the way this kind of fix usually goes wrong.
    #[test]
    fn an_explicit_homecore_section_is_not_overridden_by_defaults() {
        let cfg: Config =
            toml::from_str("[homecore]\nbroker_host = \"10.0.0.5\"\nbroker_port = 8883\n").unwrap();
        assert_eq!(cfg.homecore.broker_host, "10.0.0.5");
        assert_eq!(cfg.homecore.broker_port, 8883);
        // Untouched keys in a present section still default.
        assert_eq!(cfg.homecore.plugin_id, "plugin.zwave");
    }

    /// A wholly empty file is a working plugin pointed at the defaults.
    #[test]
    fn an_empty_config_is_all_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.server.url, "ws://localhost:3000");
    }

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
