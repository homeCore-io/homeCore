//! The plugin's settings file.
//!
//! homeCore owns it (`config/plugins/plugin.weather.toml`), hands its path to
//! the plugin as `argv[1]`, and restarts the plugin when it changes.
//!
//! **With no locations at all, the plugin reports the house's weather**, at
//! the coordinates homeCore already knows (`[location]`, the same place
//! sunrise and sunset come from). A location is added for anywhere else, by
//! coordinates, a postal code or a place's name.

use anyhow::{Context, Result};
use serde::Deserialize;

#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(Config)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// How the settings form is drawn. Every setting in the schema must be here
/// (see the coverage test), or it can't be changed from the app.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section};

    Descriptor::new("plugin.weather")
        .title("Weather")
        .section(
            Section::new("locations", "Places")
                .help(
                    "With none, the plugin reports the weather at the house, where \
                     homeCore's location says it is. Add a place for anywhere else, \
                     by its coordinates, a postal code and country, or its name.",
                )
                .field(
                    Field::table("weather.locations")
                        .label("Places")
                        .key_by("id")
                        .columns([
                            Field::text("name").label("Name"),
                            Field::number("latitude").label("Latitude"),
                            Field::number("longitude").label("Longitude"),
                            Field::text("postal_code").label("Postal code"),
                            Field::text("country").label("Country (two letters)"),
                            Field::text("query").label("Or its name"),
                            Field::enumeration("alerts")
                                .label("Warnings from")
                                .default("auto")
                                .option("auto", "Where it is")
                                .option("nws", "US National Weather Service")
                                .option("meteoalarm", "MeteoAlarm (Europe)")
                                .option("off", "None"),
                            Field::text("meteoalarm_area")
                                .label("MeteoAlarm region")
                                .help("Its code (DE804) or part of its name"),
                        ])
                        .help(
                            "The id names the place's device (weather_<id>) for good: \
                             automations and history are keyed to it.",
                        ),
                ),
        )
        .section(
            Section::new("alerts", "Warnings").field(
                Field::text("weather.contact")
                    .label("Contact for the weather service")
                    .placeholder("you@example.com")
                    .help(
                        "The US National Weather Service asks every program that reads \
                         its warnings to say who to contact about it. Sent only to \
                         api.weather.gov.",
                    ),
            ),
        )
        .section(
            Section::new("refresh", "How often")
                .field(
                    Field::int("weather.current_interval_secs")
                        .label("The weather now")
                        .unit("s")
                        .min(300)
                        .default(900)
                        .help("Open-Meteo updates it every 15 minutes."),
                )
                .field(
                    Field::int("weather.forecast_interval_secs")
                        .label("The forecast")
                        .unit("s")
                        .min(600)
                        .default(1800),
                )
                .field(
                    Field::int("weather.air_quality_interval_secs")
                        .label("Air quality")
                        .unit("s")
                        .min(600)
                        .default(900),
                )
                .field(
                    Field::int("weather.alerts_interval_secs")
                        .label("Warnings")
                        .unit("s")
                        .min(60)
                        .default(120),
                ),
        )
        .section(
            Section::new("services", "Services")
                .help("Where it asks. Change these only for a server of your own.")
                .field(Field::url("weather.forecast_url").label("Open-Meteo forecast"))
                .field(Field::url("weather.air_quality_url").label("Open-Meteo air quality"))
                .field(Field::url("weather.geocoding_url").label("Open-Meteo places"))
                .field(Field::url("weather.nws_url").label("National Weather Service"))
                .field(Field::url("weather.meteoalarm_url").label("MeteoAlarm")),
        )
        .section(
            Section::new("logging", "Logging").field(
                Field::enumeration("logging.forward_level")
                    .label("Forward to core")
                    .render("segmented")
                    .default("info")
                    .option("trace", "Trace")
                    .option("debug", "Debug")
                    .option("info", "Info")
                    .option("warn", "Warn")
                    .option("error", "Error"),
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

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomeCoreSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub weather: WeatherSection,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomeCoreSection {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoggingSection {
    #[serde(default = "default_forward_level")]
    pub forward_level: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            forward_level: default_forward_level(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WeatherSection {
    /// Who to contact about this program, for the NWS's User-Agent. Empty is
    /// allowed; the plugin then names only itself.
    #[serde(default)]
    pub contact: String,
    /// Places to report. Empty: the house.
    #[serde(default)]
    pub locations: Vec<LocationEntry>,
    #[serde(default = "d_current")]
    pub current_interval_secs: u64,
    #[serde(default = "d_forecast")]
    pub forecast_interval_secs: u64,
    #[serde(default = "d_air")]
    pub air_quality_interval_secs: u64,
    #[serde(default = "d_alerts")]
    pub alerts_interval_secs: u64,
    #[serde(default = "d_forecast_url")]
    pub forecast_url: String,
    #[serde(default = "d_air_url")]
    pub air_quality_url: String,
    #[serde(default = "d_geocoding_url")]
    pub geocoding_url: String,
    #[serde(default = "d_nws_url")]
    pub nws_url: String,
    #[serde(default = "d_meteoalarm_url")]
    pub meteoalarm_url: String,
}

impl Default for WeatherSection {
    fn default() -> Self {
        toml::from_str("").expect("every field has a default")
    }
}

/// One place. Give its coordinates, or a postal code (with its country), or
/// its name; with none, it is the house.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocationEntry {
    /// Names its device, `weather_<id>`, for good.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    #[serde(default)]
    pub postal_code: Option<String>,
    /// ISO 3166 two letters: "US", "DE". Narrows a postal code, and says
    /// which service has its warnings.
    #[serde(default)]
    pub country: Option<String>,
    /// A place's name, "Knoxville, TN", looked up once.
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub alerts: AlertSource,
    /// MeteoAlarm's region, by code or part of its name.
    #[serde(default)]
    pub meteoalarm_area: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum AlertSource {
    /// The US's own service in the US, MeteoAlarm in Europe, none elsewhere.
    #[default]
    Auto,
    Nws,
    Meteoalarm,
    Off,
}

fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_forward_level() -> String {
    "info".into()
}
fn d_current() -> u64 {
    900
}
fn d_forecast() -> u64 {
    1800
}
fn d_air() -> u64 {
    900
}
fn d_alerts() -> u64 {
    120
}
fn d_forecast_url() -> String {
    "https://api.open-meteo.com/v1/forecast".into()
}
fn d_air_url() -> String {
    "https://air-quality-api.open-meteo.com/v1/air-quality".into()
}
fn d_geocoding_url() -> String {
    "https://geocoding-api.open-meteo.com/v1/search".into()
}
fn d_nws_url() -> String {
    "https://api.weather.gov".into()
}
fn d_meteoalarm_url() -> String {
    "https://feeds.meteoalarm.org/api/v1/warnings".into()
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read config at {path}"))?;
        toml::from_str(&text).with_context(|| format!("could not parse config at {path}"))
    }
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
    fn a_minimal_config_is_the_house_with_public_services() {
        let cfg: Config =
            toml::from_str("[homecore]\nplugin_id = \"plugin.weather\"\n").expect("should load");
        assert!(cfg.weather.locations.is_empty());
        assert_eq!(cfg.weather.current_interval_secs, 900);
        assert_eq!(cfg.weather.alerts_interval_secs, 120);
        assert!(cfg
            .weather
            .forecast_url
            .starts_with("https://api.open-meteo.com"));
        assert!(cfg.weather.contact.is_empty(), "no contact is assumed");
    }

    #[test]
    fn a_place_by_postal_code() {
        let cfg: Config = toml::from_str(
            "[homecore]\nplugin_id = \"plugin.weather\"\n\
             [[weather.locations]]\nid = \"lake\"\nname = \"Lake house\"\n\
             postal_code = \"10115\"\ncountry = \"DE\"\nalerts = \"meteoalarm\"\n\
             meteoalarm_area = \"DE106\"\n",
        )
        .expect("should load");
        let l = &cfg.weather.locations[0];
        assert_eq!(l.alerts, AlertSource::Meteoalarm);
        assert_eq!(l.postal_code.as_deref(), Some("10115"));
    }
}

#[cfg(test)]
mod example_tests {
    #[test]
    fn the_example_loads() {
        let cfg: super::Config = toml::from_str(include_str!("../config/config.toml.example"))
            .expect("the example is a valid config");
        assert_eq!(cfg.homecore.plugin_id, "plugin.weather");
        assert!(cfg.weather.locations.is_empty(), "the example is the house");
    }
}
