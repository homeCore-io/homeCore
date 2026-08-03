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

    Descriptor::new("plugin.ecowitt")
        .title("Ecowitt")
        .section(
            Section::new("receiver", "Push receiver")
                .help(
                    "The usual setup: point the console's \"custom server\" upload \
                     at this plugin and readings arrive as they are measured, with \
                     no polling.",
                )
                .field(
                    Field::host("ecowitt.bind_addr")
                        .label("Bind address")
                        .default("127.0.0.1")
                        .help(
                            "Loopback by default, on purpose. Ecowitt's upload \
                             protocol has no real authentication — the PASSKEY is \
                             just the gateway's MAC, sent in cleartext — so a \
                             LAN-reachable listener would accept forged readings \
                             from any host. Widen this only if the console must \
                             POST across the network, and pair it with an allow-list.",
                        ),
                )
                .field(
                    Field::port("ecowitt.listen_port")
                        .label("Listen port")
                        .default(8888)
                        .help("Must match the port set in the console's custom-server page."),
                )
                .field(
                    Field::list("ecowitt.allowed_source_ips", "host")
                        .label("Accept readings from")
                        .default(Vec::<String>::new())
                        .help(
                            "Empty accepts any source, which is fine while the bind \
                             address is loopback. Give the console a static DHCP \
                             lease first, or this entry goes stale and readings \
                             stop.",
                        ),
                )
                .field(
                    Field::note(
                        "The listener is reachable from the LAN. Add the console's \
                         IP above, or any host on the network can post fake weather.",
                    )
                    .visible_when(Cond::all([
                        Cond::ne("ecowitt.bind_addr", "127.0.0.1"),
                        Cond::not(Cond::truthy("ecowitt.allowed_source_ips")),
                    ])),
                ),
        )
        .section(
            Section::new("polling", "Polling")
                .help(
                    "An alternative to the push receiver for consoles that cannot \
                     reach this host, or that you would rather not open a port for. \
                     Leave the address empty to stay push-only.",
                )
                .field(
                    Field::host("ecowitt.gateway_ip")
                        .label("Console address")
                        .placeholder("Push only")
                        .help("Set this and the plugin polls the console instead of waiting for uploads."),
                )
                .field(
                    Field::duration("ecowitt.poll_interval_secs")
                        .label("Poll every")
                        .unit("secs")
                        .default(60)
                        .min(1)
                        .visible_when(Cond::truthy("ecowitt.gateway_ip"))
                        .help(
                            "Ecowitt sensors report on their own cadence — typically \
                             every 60s — so polling faster mostly re-reads the same \
                             values.",
                        ),
                )
                .field(
                    Field::list("ecowitt.manual_hosts", "host")
                        .label("Also probe these hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "Discovery finds consoles by UDP broadcast, which does \
                             not cross VLANs. List a console here only to reach one \
                             on another subnet that this host can still route to.",
                        ),
                ),
        )
        .section(
            Section::new("gateway", "Console sign-in")
                .help(
                    "Only needed to *write* settings back to the console. Reading \
                     measurements never requires this.",
                )
                .field(
                    Field::text("ecowitt.gateway_username")
                        .label("Username")
                        .default("admin")
                        .help(
                            "Most Ecowitt firmware hard-codes `admin` and checks \
                             only the password.",
                        ),
                )
                .field(
                    Field::secret("ecowitt.gateway_password")
                        .label("Password")
                        .help("Leave blank if the console's web UI has no password set."),
                ),
        )
        .section(
            Section::new("devices", "Devices")
                .field(
                    Field::text("ecowitt.device_prefix")
                        .label("Device ID prefix")
                        .default("ecowitt")
                        .help(
                            "Leads every device id this plugin creates \
                             (`ecowitt_outdoor_temp`). Changing it renames every \
                             device, which breaks rules that name the old ids — set \
                             it once, at install.",
                        ),
                )
                .field(
                    Field::table("devices")
                        .label("Sensors")
                        .render("list")
                        .key_by("device_id")
                        .help(
                            "Every sensor the console has reported. Give each one a \
                             name that means something and put it in a room.",
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
                        .placeholder("info | debug | hc_ecowitt=debug"),
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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub ecowitt: EcowittConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
}

impl Default for EcowittConfig {
    fn default() -> Self {
        Self {
            listen_port: default_listen_port(),
            bind_addr: default_bind_addr(),
            allowed_source_ips: Vec::new(),
            gateway_ip: None,
            manual_hosts: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            device_prefix: default_device_prefix(),
            gateway_username: default_gateway_username(),
            gateway_password: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config {path}: {e}"))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))
    }
}

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
    "plugin.ecowitt".into()
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EcowittConfig {
    /// Port for the HTTP server that receives POSTs from the gateway.
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// Address the HTTP receiver binds to. Defaults to loopback so the
    /// listener isn't reachable from the LAN by default — Ecowitt's
    /// "custom server" upload protocol carries no real authentication
    /// (PASSKEY is the gateway's MAC, sent in cleartext), so a 0.0.0.0
    /// bind would let any LAN host forge readings. Operators who need
    /// the gateway to POST directly across the network should set this
    /// to "0.0.0.0" (or a specific NIC) and pair it with
    /// `allowed_source_ips`.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Optional list of source IPs allowed to POST to /data/report.
    /// Empty (default) accepts any source — fine when `bind_addr` is
    /// loopback. When binding to a routable address, populate this with
    /// the gateway's IP to drop packets from anything else on the LAN.
    /// Pair with a static DHCP lease for the gateway so the entry
    /// doesn't go stale.
    #[serde(default)]
    pub allowed_source_ips: Vec<String>,
    /// Optional: gateway IP for polling mode.
    pub gateway_ip: Option<String>,
    /// Static console IPs to probe via HTTP whenever discovery runs
    /// (`discover_gateways`) or any action needs to resolve a gateway
    /// IP without one explicitly given.
    ///
    /// Use this when consoles live on a VLAN the homeCore host can
    /// route to but UDP broadcast (port 45000) can't reach. Each
    /// listed host is queried via `/get_device_info?` — successful
    /// responses are merged into the discovery results alongside any
    /// UDP-discovered consoles.
    #[serde(default)]
    pub manual_hosts: Vec<String>,
    /// Polling interval in seconds (only used when gateway_ip is set).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Prefix for HomeCore device IDs (default: "ecowitt").
    #[serde(default = "default_device_prefix")]
    pub device_prefix: String,
    /// Username for the gateway's local web UI. Most Ecowitt firmware
    /// hard-codes this to "admin" and only checks the password — kept
    /// here for forward-compatibility and so the field is visible in
    /// config for installations that need to override it.
    #[allow(dead_code)]
    #[serde(default = "default_gateway_username")]
    pub gateway_username: String,
    /// Password for the gateway's local web UI. Leave blank if the
    /// gateway has no password set. Required for `set_*` cgi-bin
    /// endpoints on firmware revisions that gate writes behind the
    /// web-UI login (e.g., GW1100 with a password configured).
    #[serde(default)]
    pub gateway_password: String,
}

fn default_listen_port() -> u16 {
    8888
}
fn default_bind_addr() -> String {
    "127.0.0.1".into()
}
fn default_poll_interval() -> u64 {
    60
}
fn default_device_prefix() -> String {
    "ecowitt".into()
}
fn default_gateway_username() -> String {
    "admin".into()
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
