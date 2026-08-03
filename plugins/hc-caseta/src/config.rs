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

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: the bridge auth as a proper
/// connection block, a device table with a typed `kind` picker, units, and
/// prose. Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor`.
///
/// Coverage note (phase 6): a published descriptor is authoritative, so an
/// omitted key is uneditable. Every `Config` key is represented except two,
/// both deliberate: `homecore.plugin_id` is bootstrap identity fixed at
/// install, and `devices[].buttons` (Pico/keypad button-component numbers) is
/// reserved for future use — unread by this plugin today, so it stays in TOML
/// rather than shipping a column that does nothing.
///
/// Caséta has no queryable device list (unlike RadioRA 2), so the Devices
/// table writes the `[[devices]]` array directly — filled either by hand or
/// from a pasted integration report via the `import` field (see [`crate::import`]).
/// `descriptor_covers_every_schema_field` pins all of it, table columns
/// included.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    Descriptor::new("plugin.caseta")
        .title("Lutron Caséta")
        .section(
            Section::new("bridge", "Bridge")
                .field(Field::note(
                    "The Caséta Smart Bridge PRO (not the standard bridge) exposes \
                     the Telnet integration this plugin uses. Enable it in the \
                     Lutron app under Advanced → Integration.",
                ))
                .field(
                    Field::host("caseta.host")
                        .label("Bridge host")
                        .placeholder("10.0.0.x")
                        .help("IP address of the Caséta Smart Bridge PRO."),
                )
                .field(Field::port("caseta.port").label("Port").default(23))
                .field(
                    Field::text("caseta.username")
                        .label("Username")
                        .default("lutron"),
                )
                .field(
                    Field::secret("caseta.password")
                        .label("Password")
                        .help("Integration login. Factory default is lutron / integration."),
                )
                .field(
                    Field::number("caseta.default_fade_secs")
                        .label("Default fade")
                        .unit("secs")
                        .default(1.0)
                        .min(0)
                        .help("Dimmer/shade transition time. Override per device in the table below."),
                )
                .field(
                    Field::duration("caseta.reconnect_delay_secs")
                        .label("Reconnect delay")
                        .unit("secs")
                        .default(5)
                        .min(1)
                        .help("Backoff before retrying a dropped bridge connection."),
                ),
        )
        .section(
            Section::new("devices", "Devices")
                .field(Field::note(
                    "Caséta has no queryable device list, so devices are added by \
                     integration ID. Rather than typing them, paste the integration \
                     report the Lutron app emails you — it carries every ID, name \
                     and room.",
                ))
                .field(
                    Field::import("import_integration_report")
                        .label("Paste integration report")
                        .targets(["devices", "scenes"])
                        .placeholder("{ \"LIPIdList\": { \"Devices\": [...], \"Zones\": [...] } }")
                        .help(
                            "Lutron app → Settings → Advanced → Integration → Send \
                             Integration Report. Rows are added below for review and \
                             are not saved until you save.",
                        ),
                )
                .field(
                    Field::table("devices")
                        .label("Devices")
                        // A card per device is unreadable past a handful, and
                        // an integration report lands nine at once.
                        .render("list")
                        .group_by("area")
                        // Identity for the importer: re-pasting a report
                        // updates nothing and duplicates nothing.
                        .key_by("integration_id")
                        .help("Each row maps a Caséta integration ID to a homeCore device.")
                        .columns([
                            Field::int("integration_id").label("Integration ID"),
                            Field::text("name").label("Name"),
                            // The report carries no load type, so an imported
                            // row arrives without one. Flag it rather than
                            // guess — and rather than block the save, since the
                            // plugin skips such a device and logs it.
                            Field::select("kind")
                                .label("Kind")
                                .prompt_when_empty()
                                .option("dimmer", "Dimmer")
                                .option("switch", "Switch")
                                .option("shade", "Shade")
                                .option("fan_control", "Fan")
                                .option("pico", "Pico remote")
                                .option("occupancy_sensor", "Occupancy sensor"),
                            Field::select("area")
                                .label("Room")
                                .placeholder("Unassigned")
                                .allow_create()
                                .source(Source::core_resource("areas")),
                            // Optional in `DeviceConfig` — empty inherits
                            // `caseta.default_fade_secs`, so no `.default()`.
                            Field::number("fade_secs")
                                .label("Fade")
                                .unit("secs")
                                .min(0)
                                .placeholder("Default"),
                            // Shades only, but the renderer evaluates
                            // `visible_when` against the whole config, never a
                            // table row, so the column is always shown.
                            Field::toggle("invert_position")
                                .label("Invert position")
                                .default(false),
                        ]),
                ),
        )
        .section(
            Section::new("scenes", "Scenes")
                .field(Field::note(
                    "Scenes are the Smart Bridge's phantom buttons, programmed in the \
                     Lutron app. Activating one here presses it, exactly as a wall \
                     control would.",
                ))
                .field(
                    Field::table("scenes")
                        .label("Scenes")
                        .render("list")
                        .key_by("button_component")
                        .columns([
                            Field::text("name").label("Name"),
                            Field::int("button_component")
                                .label("Phantom button")
                                .min(1)
                                .max(100)
                                .help("Component number shown in the integration report."),
                            Field::int("bridge_id")
                                .label("Bridge ID")
                                .default(1)
                                .help("Integration ID of the Smart Bridge — almost always 1."),
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
                        .placeholder("info | debug | hc_caseta=debug"),
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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub caseta: CasetaConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
    #[serde(default)]
    pub scenes: Vec<SceneConfig>,
}

impl Default for CasetaConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_lip_port(),
            username: default_username(),
            password: default_password(),
            default_fade_secs: default_fade_secs(),
            reconnect_delay_secs: default_reconnect_delay_secs(),
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
    "plugin.caseta".into()
}

// ---------------------------------------------------------------------------
// Caseta Pro bridge connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CasetaConfig {
    pub host: String,
    #[serde(default = "default_lip_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_password")]
    pub password: String,
    /// Default fade time for dimmers (seconds).
    #[serde(default = "default_fade_secs")]
    pub default_fade_secs: f64,
    /// Delay between reconnection attempts (seconds).
    #[serde(default = "default_reconnect_delay_secs")]
    pub reconnect_delay_secs: u64,
}

fn default_lip_port() -> u16 {
    23
}
fn default_username() -> String {
    "lutron".into()
}
fn default_password() -> String {
    "integration".into()
}
fn default_fade_secs() -> f64 {
    1.0
}
fn default_reconnect_delay_secs() -> u64 {
    5
}

// ---------------------------------------------------------------------------
// Scene config
// ---------------------------------------------------------------------------

/// A scene stored on the Smart Bridge as a *phantom button*.
///
/// The bridge exposes 100 of them (integration ID 1 in the LIP integration
/// report). Activating one is a press/release pair on its component, exactly
/// as a physical button would be — the bridge then runs whatever the Lutron
/// app programmed onto it.
///
/// HomeCore device ID: `caseta_scene_{name_slug}`
/// Commands accepted:  `{ "activate": true }`
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SceneConfig {
    pub name: String,
    /// Integration ID of the Smart Bridge itself — almost always 1.
    #[serde(default = "default_bridge_id")]
    pub bridge_id: u32,
    /// Phantom button component number (1-100) assigned in the Lutron app.
    pub button_component: u32,
    /// Optional HomeCore area tag.
    pub area: Option<String>,
}

fn default_bridge_id() -> u32 {
    1
}

impl SceneConfig {
    /// HomeCore device ID: `caseta_scene_{name_slug}`.
    pub fn hc_id(&self) -> String {
        let slug = self
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect::<String>();
        format!("caseta_scene_{slug}")
    }
}

// ---------------------------------------------------------------------------
// Device config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// Dimmable light — brightness 0-100 with optional fade.
    Dimmer,
    /// Non-dimmable load — on/off only.
    Switch,
    /// Motorized shade — position 0-100.
    Shade,
    /// Ceiling fan control — speed levels (off/low/medium/medium-high/high).
    FanControl,
    /// Pico wireless remote — publishes button events only (read-only).
    Pico,
    /// Occupancy sensor — publishes occupied/vacant (read-only).
    OccupancySensor,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    pub integration_id: u32,
    pub name: String,
    /// Absent until the operator picks one.
    ///
    /// The integration report carries no load type, so an imported row arrives
    /// without a kind. Requiring it here would mean a freshly imported config
    /// refused to parse and took the whole plugin offline; instead such a row
    /// is skipped at startup, named in the log, and works the moment a kind is
    /// chosen.
    #[serde(default)]
    pub kind: Option<DeviceKind>,
    pub area: Option<String>,
    /// Per-device fade time override (seconds).  Falls back to caseta.default_fade_secs.
    pub fade_secs: Option<f64>,
    /// Invert shade position: false = Lutron native (0=open, 100=closed),
    /// true = inverted (0=closed, 100=open).
    #[serde(default)]
    pub invert_position: bool,
    /// Pico button component numbers (e.g. [2, 3, 4, 5, 6]).
    ///
    /// Published as `available_buttons` so a rule editor can name the buttons
    /// of a Pico rather than asking for a component number. Filled in by the
    /// integration report import; empty on a config written before that
    /// carried the numbers through, in which case no catalogue is published.
    #[serde(default)]
    pub buttons: Vec<u32>,
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;

    /// An imported row has no `kind` until the operator picks one, and the
    /// integration report cannot supply it. Making `kind` mandatory meant the
    /// first save after an import failed to parse — `missing field \`kind\`` —
    /// and took the whole plugin offline, holiday lights and all.
    #[test]
    fn a_device_without_a_kind_still_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [homecore]
            [caseta]
            host = "10.0.0.5"

            [[devices]]
            integration_id = 2
            name = "Holiday Lights 1"
            area = "Living Room"

            [[devices]]
            integration_id = 6
            name = "Pico"
            kind = "pico"
            "#,
        )
        .expect("an unclassified device must not break the config");
        assert_eq!(cfg.devices.len(), 2);
        assert!(cfg.devices[0].kind.is_none());
        assert_eq!(cfg.devices[1].kind, Some(DeviceKind::Pico));
    }

    /// …and such a row is simply not registered, rather than guessed at.
    #[test]
    fn an_unclassified_device_builds_no_entry() {
        let cfg: Config = toml::from_str(
            r#"
            [homecore]
            [caseta]
            host = "10.0.0.5"

            [[devices]]
            integration_id = 2
            name = "Holiday Lights 1"
            "#,
        )
        .unwrap();
        assert!(crate::devices::DeviceEntry::new(cfg.devices[0].clone()).is_none());
    }

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable (the hc-sonos logging bug, `5bccebf`). The check
    /// lives in the SDK; every schema leaf must be covered or justified.
    ///
    /// The SDK descends into arrays of objects, so the Devices table is
    /// checked column by column too.
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity fixed at install, not an operator setting.
                "homecore.plugin_id",
                // Reserved for future use (Pico/keypad button components) and
                // unread by the plugin today — a column would do nothing.
                "devices[].buttons",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
