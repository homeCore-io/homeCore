//! The plugin's config file.
//!
//! homeCore owns this file: it lives at `config/plugins/<plugin_id>.toml`, is
//! handed to the process as `argv[1]`, and core rewrites it and restarts this
//! one plugin when an operator edits it. Nothing here writes it back — tokens
//! go to core's learned state instead, for the reason spelled out in
//! [`crate::auth`].
//!
//! Two documents describe this to the config editor, and both are published on
//! the capability manifest:
//!
//! - [`config_schema`] — derived from the structs, so it cannot drift from what
//!   the plugin reads. Authoritative for what exists.
//! - [`config_descriptor`] — hand-written, about presentation. **Authoritative
//!   for the form**: a field it omits is not merely unlabelled, it is
//!   uneditable. `descriptor_covers_every_schema_field` at the bottom is what
//!   keeps that from happening quietly.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::auth::AuthSettings;
use crate::units::Scale;

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
    /// The SDK's own logging section, not a copy of it: `init_logging` reads
    /// this exact type, so a field here is a field the logger honours. A
    /// hand-rolled duplicate drifts, and the drift is silent — the form offers
    /// a rotation policy nothing applies.
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub nuheat: NuHeatSection,
}

/// The broker connection. Every plugin has this section, spelled this way.
///
/// Defaulted, unlike the template's: a missing section used to kill plugins
/// outright while every value in it had a working default. `plugin_id` keeps a
/// default here for the same reason the other shipped plugins do — core writes
/// the real one at install.
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
pub struct NuHeatSection {
    /// How often to ask NuHeat for the current state of every thermostat.
    ///
    /// A floor slab changes temperature over tens of minutes, so a fast poll
    /// buys nothing; the default is a compromise with wanting the UI to reflect
    /// a change made in the NuHeat app reasonably soon. Well inside the
    /// published rate limits either way — 1000 requests per 10 seconds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Which scale the limits in *this file* are written in. Published state
    /// is always °C; the app shows °F to whoever reads in it.
    #[serde(default)]
    pub config_scale: Scale,

    /// What a plain setpoint change means, with no mode alongside it.
    ///
    /// `false` — the default — makes it a temporary hold, so the thermostat's
    /// schedule reasserts itself later. `true` makes it permanent, which is
    /// what someone who does not use the schedule at all wants.
    #[serde(default)]
    pub setpoint_holds_permanently: bool,

    /// How long a temporary hold lasts when the command does not say.
    ///
    /// Unset leaves it to NuHeat, which resumes at the next scheduled event.
    /// Must be 1–23; NuHeat rejects anything longer.
    #[serde(default)]
    pub default_hold_hours: Option<i64>,

    /// Only publish these serial numbers. Empty means every thermostat on the
    /// account.
    #[serde(default)]
    pub only_serials: Vec<String>,

    /// The warmest this plugin will ask for, in [`Self::config_scale`] units.
    ///
    /// Floor coverings have their own limits well below the thermostat's 30 °C
    /// — engineered hardwood is usually rated to about 27 °C / 80 °F, and
    /// exceeding it damages the floor rather than the heater. NuHeat's app
    /// enforces nothing of the sort, so a rule with a bad number in it is
    /// otherwise free to cook the floor. Unset means the thermostat's own
    /// maximum.
    #[serde(default)]
    pub max_setpoint: Option<f64>,

    /// The coolest this plugin will ask for, in [`Self::config_scale`] units.
    /// Unset means the thermostat's own minimum.
    #[serde(default)]
    pub min_setpoint: Option<f64>,

    /// Where devices land in homeCore, when you want them all in one place.
    #[serde(default)]
    pub area: Option<String>,

    #[serde(default)]
    pub auth: AuthSection,

    #[serde(default)]
    pub endpoints: EndpointsSection,
}

impl Default for NuHeatSection {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            config_scale: Scale::default(),
            setpoint_holds_permanently: false,
            default_hold_hours: None,
            only_serials: Vec::new(),
            max_setpoint: None,
            min_setpoint: None,
            area: None,
            auth: AuthSection::default(),
            endpoints: EndpointsSection::default(),
        }
    }
}

/// How this plugin signs in. Everything here is optional: with nothing set it
/// signs in as homeCore's own NuHeat client (see [`crate::auth`]).
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuthSection {
    /// Your own NuHeat client id, if you have one. Empty means homeCore's.
    #[serde(default)]
    pub client_id: Option<String>,

    /// Only for your own confidential client. homeCore's needs none.
    #[serde(default)]
    pub client_secret: Option<String>,

    /// Where NuHeat sends the browser back. Empty means the homecore.io page
    /// registered against homeCore's client.
    #[serde(default)]
    pub redirect_uri: Option<String>,

    /// The copy-the-address fallback. Empty means
    /// `http://127.0.0.1:8127/nuheat/callback`, also registered.
    #[serde(default)]
    pub loopback_redirect_uri: Option<String>,
}

/// Where NuHeat is. Only ever changed to point at a stand-in for testing.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EndpointsSection {
    /// NuHeat's identity server. Empty means https://identity.mynuheat.com.
    #[serde(default)]
    pub identity_url: Option<String>,
    /// NuHeat's API. Empty means https://api.mynuheat.com.
    #[serde(default)]
    pub api_url: Option<String>,
}

impl NuHeatSection {
    /// What [`crate::auth::Auth`] needs from the file.
    pub fn auth_settings(&self) -> AuthSettings {
        AuthSettings {
            client_id: self.auth.client_id.clone(),
            client_secret: self.auth.client_secret.clone(),
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
    "plugin.nuheat".into()
}
fn default_poll_interval() -> u64 {
    120
}

impl NuHeatSection {
    /// The configured limits, converted out of whatever scale they were
    /// written in and intersected with what the thermostat accepts.
    pub fn setpoint_limits(&self) -> crate::units::SetpointLimits {
        crate::units::SetpointLimits::new(
            self.min_setpoint.map(|v| self.config_scale.to_celsius(v)),
            self.max_setpoint.map(|v| self.config_scale.to_celsius(v)),
        )
    }
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

    Descriptor::new("plugin.nuheat")
        .title("NuHeat")
        .section(
            Section::new("account", "NuHeat account")
                .help(
                    "Sign in with your NuHeat account using the \"Link NuHeat account\" \
                     button at the top of this page. You sign in on NuHeat's own site; \
                     homeCore never sees your password. Nothing below needs filling in.",
                )
                .field(
                    Field::text("nuheat.auth.client_id")
                        .label("Your own client id")
                        .help(
                            "Leave empty to sign in as homeCore, which NuHeat registered for \
                             everyone. Only set this if NuHeat issued you a client of your own.",
                        ),
                )
                .field(
                    Field::secret("nuheat.auth.client_secret")
                        .label("Your own client secret")
                        .visible_when(Cond::truthy("nuheat.auth.client_id"))
                        .help("Only for your own client, and only if NuHeat gave it a secret."),
                )
                .field(
                    Field::url("nuheat.auth.redirect_uri")
                        .label("Return address")
                        .visible_when(Cond::truthy("nuheat.auth.client_id"))
                        .help(
                            "One registered against your own client, exactly. Empty means \
                             homeCore's page on homecore.io, which only homeCore's client \
                             may use.",
                        ),
                )
                .field(
                    Field::url("nuheat.auth.loopback_redirect_uri")
                        .label("Copy-the-address return")
                        .visible_when(Cond::truthy("nuheat.auth.client_id"))
                        .help(
                            "For signing in by pasting the address back. Empty means \
                             http://127.0.0.1:8127/nuheat/callback.",
                        ),
                ),
        )
        .section(
            Section::new("thermostats", "Thermostats")
                .field(
                    Field::duration("nuheat.poll_interval_secs")
                        .label("Check every")
                        .unit("s")
                        .default(120)
                        .min(30)
                        .max(3600)
                        .help(
                            "A floor slab changes over tens of minutes, so there is little to \
                             gain below a minute or two. This mostly decides how quickly a \
                             change made in the NuHeat app shows up here.",
                        ),
                )
                .field(
                    Field::list("nuheat.only_serials", "text")
                        .label("Only these serial numbers")
                        .help(
                            "Leave empty to publish every thermostat on the account. \
                             Listing serials here hides the rest.",
                        ),
                )
                .field(
                    Field::text("nuheat.area")
                        .label("Area")
                        .help("Where these devices land in homeCore. Optional."),
                ),
        )
        .section(
            Section::new("behaviour", "Behaviour")
                .field(
                    Field::toggle("nuheat.setpoint_holds_permanently")
                        .label("A new target holds indefinitely")
                        .default(false)
                        .help(
                            "Off: changing the target holds it for a while and then the \
                             thermostat's schedule takes over again. On: it holds until \
                             something changes it. Turn this on if you do not use the \
                             NuHeat schedule.",
                        ),
                )
                .field(
                    Field::int("nuheat.default_hold_hours")
                        .label("Hold for")
                        .unit("hours")
                        .min(1)
                        // NuHeat's own cap. Offering 24 would produce a control
                        // whose top value the API rejects.
                        .max(23)
                        .visible_when(Cond::falsy("nuheat.setpoint_holds_permanently"))
                        .help(
                            "How long a hold lasts when nothing says otherwise. Leave empty \
                             to let NuHeat resume at its next scheduled event. NuHeat allows \
                             at most 23 hours.",
                        ),
                )
                .field(
                    Field::enumeration("nuheat.config_scale")
                        .label("Temperatures in this form")
                        .render("segmented")
                        .default("celsius")
                        .option("celsius", "°C")
                        .option("fahrenheit", "°F")
                        .help(
                            "Only affects numbers you type here. Thermostats always report \
                             in °C, and the app shows them in your own units.",
                        ),
                )
                .field(
                    Field::number("nuheat.max_setpoint")
                        .label("Never go above")
                        .help(
                            "A safety limit for the floor covering, which usually has a much \
                             lower maximum than the thermostat does — engineered hardwood is \
                             typically rated to about 27 °C / 80 °F. Anything asking for more \
                             is held at this value. Leave empty for the thermostat's own \
                             maximum.",
                        ),
                )
                .field(
                    Field::number("nuheat.min_setpoint")
                        .label("Never go below")
                        .help("Leave empty for the thermostat's own minimum."),
                ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_nuheat=debug"),
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
            Section::new("endpoints", "NuHeat's addresses")
                .hidden()
                .help("For testing against a stand-in. Leave empty.")
                .field(Field::url("nuheat.endpoints.identity_url").label("Identity server"))
                .field(Field::url("nuheat.endpoints.api_url").label("API")),
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

    /// The editor renders the descriptor *instead of* deriving a form from the
    /// schema, so a field the descriptor forgets is unreachable while the
    /// schema still declares it and the plugin still reads it.
    #[cfg(feature = "schema")]
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("the schema feature is on in this build"),
            &config_descriptor(),
            &[
                // Bootstrap identity, fixed when core installs the plugin.
                // Editing it would re-point this plugin at another's topics.
                "homecore.plugin_id",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }

    #[test]
    fn a_minimal_config_loads_with_working_defaults() {
        let cfg: Config =
            toml::from_str("[homecore]\nplugin_id = \"plugin.nuheat\"\n").expect("should load");
        assert_eq!(cfg.homecore.plugin_id, "plugin.nuheat");
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.nuheat.poll_interval_secs, 120);
        assert!(cfg.nuheat.only_serials.is_empty());
        assert_eq!(cfg.nuheat.default_hold_hours, None);
    }

    /// A fresh install has no `[nuheat]` section at all, and needs none:
    /// homeCore's own client signs in.
    #[test]
    fn an_empty_config_signs_in_as_homecore() {
        let cfg: Config = toml::from_str("").expect("should load");
        assert_eq!(cfg.homecore.plugin_id, "plugin.nuheat");
        let s = cfg.nuheat.auth_settings();
        assert!(s.client_id.is_none());
        assert!(s.client_secret.is_none());
        assert!(s.identity_base.is_none());
    }

    #[test]
    fn an_own_client_and_a_stand_in_are_read() {
        let cfg: Config = toml::from_str(
            r#"
            [nuheat.auth]
            client_id = "abc"
            redirect_uri = "https://example.invalid/cb"
            [nuheat.endpoints]
            identity_url = "http://127.0.0.1:9"
            api_url = "http://127.0.0.1:9"
            "#,
        )
        .expect("should load");
        let s = cfg.nuheat.auth_settings();
        assert_eq!(s.client_id.as_deref(), Some("abc"));
        assert_eq!(s.identity_base.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(
            cfg.nuheat.endpoints.api_url.as_deref(),
            Some("http://127.0.0.1:9")
        );
    }

    /// The example config is what an operator copies, so it has to parse and
    /// to agree with the defaults the descriptor advertises.
    #[test]
    fn the_shipped_example_config_parses() {
        let text = include_str!("../config/config.toml.example");
        let cfg: Config = toml::from_str(text).expect("the example config must parse");
        assert_eq!(cfg.homecore.plugin_id, "plugin.nuheat");
        assert_eq!(cfg.nuheat.poll_interval_secs, 120);
    }
}
