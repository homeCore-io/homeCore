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
    use plugin_sdk_rs::config_descriptor::{Cond, Descriptor, Field, Section};

    Descriptor::new("plugin.reolink")
        .title("Reolink")
        .section(
            Section::new("hosts", "Cameras and NVRs")
                .help(
                    "Add a row for each Reolink address on your network: a camera on its own, \
                     or an NVR or Home Hub, whose cameras come with it. The plugin signs in \
                     once to each and finds out what it is and what each camera can do. \
                     Usually only the name, address, user name and password need filling in.",
                )
                .field(
                    Field::table("reolink.hosts")
                        .label("Cameras and NVRs")
                        .key_by("name")
                        .columns([
                            Field::text("name")
                                .label("Name")
                                .placeholder("Driveway")
                                .help(
                                    "What it's called here. Its cameras' device ids are made \
                                     from it (reolink_driveway), so choose it before you write \
                                     rules: renaming it later gives them new ids.",
                                ),
                            Field::host("address")
                                .label("Address")
                                .required()
                                .placeholder("192.168.1.20")
                                .help("Its IP address or host name on your network."),
                            Field::text("username")
                                .label("User name")
                                .default("admin")
                                .help(
                                    "An account on the camera or NVR. One of its own is best: \
                                     Reolink allows only a few sign-ins at once.",
                                ),
                            Field::secret("password").label("Password").required(),
                            Field::toggle("https").label("HTTPS").default(true).help(
                                "Leave it on unless HTTPS is turned off on the device. If \
                                     it can't sign in and the password is right, try it off.",
                            ),
                            Field::port("rtsp_port")
                                .label("RTSP port")
                                .default(554)
                                .help(
                                    "Where it serves video, for go2rtc. Change it only if you \
                                     changed it on the device.",
                                ),
                            Field::port("push_port")
                                .label("Push port")
                                .default(9000)
                                .help(
                                    "Where it tells the plugin the moment it sees something \
                                     (Reolink's own protocol, as its app uses). Change it only \
                                     if you changed it on the device.",
                                ),
                            Field::list("streams", "text")
                                .label("go2rtc streams")
                                .placeholder("driveway")
                                .visible_when(Cond::falsy("reolink.go2rtc.add_streams"))
                                .help(
                                    "Names of streams already in your go2rtc config, one per \
                                     camera. For an NVR, the first is channel 1. A camera with \
                                     no name shows snapshots.",
                                ),
                            Field::list("streams_hd", "text")
                                .label("go2rtc streams, HD")
                                .placeholder("driveway_hd")
                                .visible_when(Cond::falsy("reolink.go2rtc.add_streams"))
                                .help(
                                    "The same for high-definition streams, if you have them. \
                                     Leave it empty if not.",
                                ),
                        ]),
                ),
        )
        .section(
            Section::new("watching", "Watching")
                .field(
                    Field::toggle("reolink.push")
                        .label("Events as they happen")
                        .default(true)
                        .help(
                            "Reolink's push (port 9000): motion and detections arrive at once. \
                             Off, or when a camera doesn't push, they're asked for instead.",
                        ),
                )
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
                        .default(3)
                        .min(1)
                        .max(10)
                        .help("For its \"sound\" action, about five seconds each."),
                )
                .field(
                    Field::int("reolink.siren_max_secs")
                        .label("A siren turned on stops after")
                        .unit("s")
                        .default(60)
                        .min(5)
                        .max(600)
                        .help(
                            "A siren turned on sounds until it's turned off, \
                             or this long at most.",
                        ),
                ),
        )
        .section(
            Section::new("video", "Live video")
                .help(
                    "Live video comes from go2rtc. The app reaches it through its own server, \
                     for people signed in only.",
                )
                .field(
                    Field::url("reolink.go2rtc.url")
                        .label("go2rtc address")
                        .placeholder("http://127.0.0.1:1984")
                        .help(
                            "go2rtc's address as this plugin reaches it, port 1984 unless \
                             changed. Empty: cameras show snapshots, no live video.",
                        ),
                )
                .field(
                    Field::toggle("reolink.go2rtc.add_streams")
                        .label("Add the cameras' streams to go2rtc")
                        .default(true)
                        .help(
                            "Adds reolink_<camera> and reolink_<camera>_hd to go2rtc and keeps \
                             them current, and touches no other stream. Off: name your own \
                             go2rtc streams in each camera or NVR's row instead.",
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
    /// The longest a siren sounds before the plugin stops it, in seconds
    /// (5 to 600): none is left sounding when whoever started it is gone.
    #[serde(default = "default_siren_max_secs")]
    pub siren_max_secs: u64,
    /// Events the moment they happen, by Reolink's push (port 9000), with
    /// asking kept behind it. Off: asked every `poll_secs` only.
    #[serde(default = "yes")]
    pub push: bool,
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
            siren_max_secs: default_siren_max_secs(),
            push: true,
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
fn default_siren_max_secs() -> u64 {
    60
}
fn default_siren_times() -> u32 {
    3
}

/// One address: a camera on its own, or an NVR or Home Hub.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostEntry {
    /// Stable: its cameras' homeCore ids are made from it. Left out (a row
    /// added in the form), it's made from the name, else the address.
    #[serde(default)]
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
    /// Its push port (Baichuan); 9000 unless it was changed.
    #[serde(default = "default_push_port")]
    pub push_port: u16,
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

fn default_push_port() -> u16 {
    9000
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
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("could not parse config at {path}"))?;
        cfg.fill_ids();
        Ok(cfg)
    }

    /// A row without an id gets one from its name, else its address, so a
    /// row added in the form can never stop the plugin.
    pub fn fill_ids(&mut self) {
        for h in &mut self.reolink.hosts {
            if h.id.trim().is_empty() {
                let from = if h.name.trim().is_empty() {
                    &h.address
                } else {
                    &h.name
                };
                h.id = crate::camera::slug(from);
            }
        }
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
            // A row's id is set only in the file; the plugin makes one from the name.
            &["homecore.plugin_id", "reolink.hosts[].id"],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }

    #[test]
    fn a_row_without_an_id_gets_one() {
        let mut cfg: Config = toml::from_str(
            "[homecore]\nplugin_id = \"plugin.reolink\"\n\n[[reolink.hosts]]\nname = \"Front Deck\"\naddress = \"10.0.0.5\"\n\n[[reolink.hosts]]\naddress = \"10.0.0.6\"\n",
        )
        .expect("loads without ids");
        cfg.fill_ids();
        assert_eq!(cfg.reolink.hosts[0].id, "front_deck");
        assert_eq!(cfg.reolink.hosts[1].id, "10_0_0_6");
    }

    #[test]
    fn a_minimal_config_loads_with_working_defaults() {
        let cfg: Config = toml::from_str(
            "[homecore]\nplugin_id = \"plugin.reolink\"\n\n[[reolink.hosts]]\nid = \"nvr\"\naddress = \"10.0.0.5\"\npassword = \"x\"\n",
        )
        .expect("should load");
        assert_eq!(cfg.reolink.poll_secs, 2);
        assert_eq!(cfg.reolink.siren_max_secs, 60);
        assert_eq!(cfg.reolink.hosts[0].username, "admin");
        assert!(cfg.reolink.hosts[0].https);
        assert!(cfg.reolink.go2rtc.add_streams);
        assert!(cfg.reolink.go2rtc.url.is_empty());
    }
}
