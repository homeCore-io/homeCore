//! The plugin's config file.
//!
//! homeCore owns this file: it lives at `config/plugins/<plugin_id>.toml`, is
//! handed to the process as `argv[1]`, and core rewrites it and restarts this
//! one plugin when an operator edits it. Nothing here writes it back; tokens
//! go to core's learned state instead (see [`crate::auth`]).
//!
//! [`config_schema`] is derived from the structs; [`config_descriptor`] is the
//! form, and `descriptor_covers_every_schema_field` keeps them agreeing.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::auth::AuthSettings;

#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(Config)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    #[serde(default)]
    pub homecore: HomeCoreSection,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub mysa: MysaSection,
}

/// The broker connection. Every plugin has this section, spelled this way.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomeCoreSection {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

impl Default for HomeCoreSection {
    fn default() -> Self {
        Self {
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

/// This plugin's own settings.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MysaSection {
    /// How often to read every thermostat. One batch call reads them all.
    /// Mysa limits requests per app and per customer, and asks clients to
    /// expect `429` rather than a number; a minute is the plan in the access
    /// request.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Only publish these Mysa device ids. Empty means every thermostat the
    /// account shared.
    #[serde(default)]
    pub only_devices: Vec<String>,

    /// Where devices land in homeCore, when you want them all in one place.
    #[serde(default)]
    pub area: Option<String>,

    #[serde(default)]
    pub auth: AuthSection,

    #[serde(default)]
    pub endpoints: EndpointsSection,
}

impl Default for MysaSection {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            only_devices: Vec::new(),
            area: None,
            auth: AuthSection::default(),
            endpoints: EndpointsSection::default(),
        }
    }
}

/// How this plugin signs in. With nothing set it signs in as homeCore's own
/// Mysa client, once Mysa has issued it (see [`crate::auth`]).
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuthSection {
    /// Your own Mysa client id, if Mysa issued you one. Empty means homeCore's.
    #[serde(default)]
    pub client_id: Option<String>,

    /// Where Mysa sends the browser back. Empty means the homecore.io page.
    #[serde(default)]
    pub redirect_uri: Option<String>,

    /// The copy-the-address fallback. Empty means
    /// `http://127.0.0.1:8127/mysa/callback`.
    #[serde(default)]
    pub loopback_redirect_uri: Option<String>,
}

/// Where Mysa is. Only ever changed to point at a stand-in for testing.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EndpointsSection {
    /// Mysa's sign-in host. Empty means https://mysa-backend.mysa.cloud.
    #[serde(default)]
    pub identity_url: Option<String>,
    /// Mysa's API. Empty means https://consumer-api.mysa.cloud.
    #[serde(default)]
    pub api_url: Option<String>,
}

impl MysaSection {
    /// What [`crate::auth::Auth`] needs from the file.
    pub fn auth_settings(&self) -> AuthSettings {
        AuthSettings {
            client_id: self.auth.client_id.clone(),
            redirect_uri: self.auth.redirect_uri.clone(),
            loopback_redirect_uri: self.auth.loopback_redirect_uri.clone(),
            identity_base: self.endpoints.identity_url.clone(),
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
    "plugin.mysa".into()
}
fn default_poll_interval() -> u64 {
    60
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read config at {path}"))?;
        toml::from_str(&text).with_context(|| format!("could not parse config at {path}"))
    }
}

/// How the config editor should present all of that.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Cond, Descriptor, Field, Section};

    Descriptor::new("plugin.mysa")
        .title("Mysa")
        .section(
            Section::new("account", "Mysa account")
                .help(
                    "Sign in with your Mysa account using the \"Link Mysa account\" button at \
                     the top of this page. You sign in on Mysa's own site; homeCore never sees \
                     your password. Nothing below needs filling in.",
                )
                .field(
                    Field::text("mysa.auth.client_id")
                        .label("Your own client id")
                        .help(
                            "Leave empty to sign in as homeCore. Only set this if Mysa issued \
                             you a client of your own.",
                        ),
                )
                .field(
                    Field::url("mysa.auth.redirect_uri")
                        .label("Return address")
                        .visible_when(Cond::truthy("mysa.auth.client_id"))
                        .help(
                            "One registered against your own client, exactly, port included. \
                             Empty means homeCore's page on homecore.io, which only homeCore's \
                             client may use.",
                        ),
                )
                .field(
                    Field::url("mysa.auth.loopback_redirect_uri")
                        .label("Copy-the-address return")
                        .visible_when(Cond::truthy("mysa.auth.client_id"))
                        .help(
                            "For signing in by pasting the address back. Empty means \
                             http://127.0.0.1:8127/mysa/callback.",
                        ),
                ),
        )
        .section(
            Section::new("thermostats", "Thermostats")
                .field(
                    Field::duration("mysa.poll_interval_secs")
                        .label("Check every")
                        .unit("s")
                        .default(60)
                        .min(30)
                        .max(3600)
                        .help(
                            "One call reads every thermostat. This decides how soon a change \
                             made in the Mysa app shows up here.",
                        ),
                )
                .field(
                    Field::list("mysa.only_devices", "text")
                        .label("Only these Mysa device ids")
                        .help("Leave empty to publish every thermostat on the account."),
                )
                .field(
                    Field::text("mysa.area")
                        .label("Area")
                        .help("Where these devices land in homeCore. Optional."),
                ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_mysa=debug"),
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
                            "Whichever comes first, this or the schedule. \
                             0 disables size-based rotation.",
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
            Section::new("endpoints", "Mysa's addresses")
                .hidden()
                .help("For testing against a stand-in. Leave empty.")
                .field(Field::url("mysa.endpoints.identity_url").label("Sign-in host"))
                .field(Field::url("mysa.endpoints.api_url").label("API")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "schema")]
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("the schema feature is on in this build"),
            &config_descriptor(),
            &["homecore.plugin_id"],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }

    #[test]
    fn an_empty_config_loads_with_working_defaults() {
        let cfg: Config = toml::from_str("").expect("should load");
        assert_eq!(cfg.homecore.plugin_id, "plugin.mysa");
        assert_eq!(cfg.mysa.poll_interval_secs, 60);
        assert!(cfg.mysa.only_devices.is_empty());
        let s = cfg.mysa.auth_settings();
        assert!(s.client_id.is_none());
        assert!(s.identity_base.is_none());
    }

    #[test]
    fn an_own_client_and_a_stand_in_are_read() {
        let cfg: Config = toml::from_str(
            r#"
            [mysa.auth]
            client_id = "abc"
            [mysa.endpoints]
            identity_url = "http://127.0.0.1:9"
            api_url = "http://127.0.0.1:9"
            "#,
        )
        .expect("should load");
        let s = cfg.mysa.auth_settings();
        assert_eq!(s.client_id.as_deref(), Some("abc"));
        assert_eq!(s.identity_base.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(
            cfg.mysa.endpoints.api_url.as_deref(),
            Some("http://127.0.0.1:9")
        );
    }

    #[test]
    fn the_shipped_example_config_parses() {
        let text = include_str!("../config/config.toml.example");
        let cfg: Config = toml::from_str(text).expect("the example config must parse");
        assert_eq!(cfg.homecore.plugin_id, "plugin.mysa");
        assert_eq!(cfg.mysa.poll_interval_secs, 60);
    }
}
