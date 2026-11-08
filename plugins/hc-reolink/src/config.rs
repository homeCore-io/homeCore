//! The plugin's config file.
//!
//! homeCore owns this file (`config/plugins/<plugin_id>.toml`) and restarts
//! the plugin when it changes. `[homecore]` and `[logging]` are every plugin's;
//! `[reolink]` is this one's: the cameras, NVRs and Home Hubs it signs in to,
//! how it watches them, and where live video comes from (`[reolink.go2rtc]`).

use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config JSON Schema, published on the capability manifest.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(Config)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// How the settings form presents this configuration (see the template).
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section};

    Descriptor::new("plugin.reolink")
        .title("Reolink")
        .section(
            Section::new("hosts", "Cameras and NVRs")
                .help(
                    "Each row is one address: a camera on its own, or an NVR or Home Hub, \
                     whose cameras arrive as channels. The plugin signs in once to each and \
                     finds out what it is.",
                )
                .field(
                    Field::table("reolink.hosts")
                        .label("Cameras and NVRs")
                        .key_by("id")
                        .columns([
                            Field::text("name").label("Name"),
                            Field::host("address").label("Address"),
                            Field::text("username").label("User name"),
                            Field::secret("password").label("Password"),
                            Field::toggle("https").label("HTTPS"),
                            Field::port("rtsp_port").label("RTSP port"),
                            Field::list("streams", "text").label("go2rtc streams"),
                            Field::list("streams_hd", "text").label("go2rtc streams, HD"),
                        ])
                        .help(
                            "The id names its cameras in homeCore for good (reolink_<id>, \
                             reolink_<id>_<channel> for an NVR's), so renaming the row is \
                             safe and changing its id is not.",
                        ),
                ),
        )
        .section(
            Section::new("watching", "Watching")
                .field(
                    Field::int("reolink.poll_secs")
                        .label("Ask for detections every")
                        .unit("s")
                        .default(2)
                        .help("How often each camera or NVR is asked what it sees now."),
                )
                .field(
                    Field::int("reolink.snapshots_kept")
                        .label("Snapshots kept per camera")
                        .default(20)
                        .help("A still is taken when a camera starts seeing something."),
                )
                .field(
                    Field::int("reolink.siren_times")
                        .label("The siren sounds")
                        .unit("times")
                        .default(3),
                ),
        )
        .section(
            Section::new("video", "Live video")
                .help(
                    "Live video comes from go2rtc. The app reaches it through its own server, \
                     for people signed in only.",
                )
                .field(Field::url("reolink.go2rtc.url").label("go2rtc address"))
                .field(
                    Field::toggle("reolink.go2rtc.add_streams")
                        .label("Add the cameras' streams to go2rtc")
                        .help(
                            "Adds reolink_<camera> and reolink_<camera>_hd to go2rtc and keeps \
                             them current. Off: name each camera's streams yourself.",
                        ),
                ),
        )
        .section(
            Section::new("logging", "Logging").field(
                Field::enumeration("logging.log_forward_level")
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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomeCoreSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub reolink: ReolinkSection,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoggingSection {
    #[serde(default = "default_forward_level")]
    pub log_forward_level: String,
}

fn default_forward_level() -> String {
    "info".into()
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            log_forward_level: default_forward_level(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReolinkSection {
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
    /// Seconds between asking each host what it sees.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "default_snapshots_kept")]
    pub snapshots_kept: usize,
    #[serde(default = "default_siren_times")]
    pub siren_times: u32,
    #[serde(default)]
    pub go2rtc: Go2rtcSection,
}

impl Default for ReolinkSection {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            poll_secs: default_poll_secs(),
            snapshots_kept: default_snapshots_kept(),
            siren_times: default_siren_times(),
            go2rtc: Go2rtcSection::default(),
        }
    }
}

fn default_poll_secs() -> u64 {
    2
}
fn default_snapshots_kept() -> usize {
    20
}
fn default_siren_times() -> u32 {
    3
}

/// One address: a camera on its own, or an NVR or Home Hub.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostEntry {
    /// Stable: its cameras' homeCore ids are made from it.
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Host name or IP, with `:port` if not the usual.
    pub address: String,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// HTTPS (self-signed, as Reolink's are) rather than plain HTTP.
    #[serde(default = "yes")]
    pub https: bool,
    /// Its RTSP port, for go2rtc; 554 unless it was changed.
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    /// go2rtc's names for its cameras' streams when the plugin doesn't add
    /// its own (`add_streams` off): the first camera's, the second's, …
    /// An empty name is a camera without one.
    #[serde(default)]
    pub streams: Vec<String>,
    /// The same for HD (main) streams, if go2rtc has them.
    #[serde(default)]
    pub streams_hd: Vec<String>,
}

fn default_rtsp_port() -> u16 {
    554
}

fn default_username() -> String {
    "admin".into()
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Go2rtcSection {
    /// go2rtc's API, as the plugin reaches it; empty, no live video.
    #[serde(default)]
    pub url: String,
    /// Add `reolink_…` streams to go2rtc and keep them current.
    #[serde(default = "yes")]
    pub add_streams: bool,
}

impl Default for Go2rtcSection {
    fn default() -> Self {
        Self {
            url: String::new(),
            add_streams: true,
        }
    }
}

fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
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
    fn a_minimal_config_loads_with_working_defaults() {
        let cfg: Config = toml::from_str(
            "[homecore]\nplugin_id = \"plugin.reolink\"\n\n[[reolink.hosts]]\nid = \"nvr\"\naddress = \"10.0.0.5\"\npassword = \"x\"\n",
        )
        .expect("should load");
        assert_eq!(cfg.reolink.poll_secs, 2);
        assert_eq!(cfg.reolink.hosts[0].username, "admin");
        assert!(cfg.reolink.hosts[0].https);
        assert!(cfg.reolink.go2rtc.add_streams);
        assert!(cfg.reolink.go2rtc.url.is_empty());
    }
}
