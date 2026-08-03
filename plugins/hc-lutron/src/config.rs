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
/// presented, which a JSON Schema cannot express: the repeater login as a
/// connection block, three hand-entered tables with typed pickers, units, and
/// prose. Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor`.
///
/// Coverage note (phase 6): a published descriptor is authoritative, so an
/// omitted key is uneditable. Every key is represented except
/// `homecore.plugin_id`, which is bootstrap identity fixed at install;
/// `descriptor_covers_every_schema_field` pins that.
///
/// `devices[].buttons` and `devices[].ccis` are `Vec<u32>` *inside* a table
/// row, which needed a list control in `_columnControl` (hc-web) — a cell
/// takes them comma-separated and parses back to a JSON array. Both are read
/// by the plugin (keypad LED state on connect, VCRX contact inputs), so
/// leaving them TOML-only would have been a real gap.
///
/// RA2 has no queryable device list over LIP, so all three tables are entered
/// by hand from the integration report.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    // Every table assigns a room the same way: the house's areas, with
    // free-text entry so a room can be named before it exists in core.
    let area_column = || {
        Field::select("area")
            .label("Room")
            .placeholder("Unassigned")
            .allow_create()
            .source(Source::core_resource("areas"))
    };

    Descriptor::new("plugin.lutron")
        .title("Lutron RadioRA 2")
        .section(
            Section::new("repeater", "Main Repeater")
                .field(Field::note(
                    "Connects to the RA2 Main Repeater over the Lutron Integration \
                     Protocol (telnet). Enable Telnet Support in RadioRA 2 Essentials \
                     or Inclusive under the repeater's integration settings.",
                ))
                .field(
                    Field::host("lutron.host")
                        .label("Repeater host")
                        .placeholder("10.0.0.x")
                        .help("IP address of the RA2 Main Repeater."),
                )
                .field(Field::port("lutron.port").label("Port").default(23))
                .field(
                    Field::text("lutron.username")
                        .label("Username")
                        .default("lutron"),
                )
                .field(
                    Field::secret("lutron.password")
                        .label("Password")
                        .help("Telnet login set in RadioRA 2 programming."),
                )
                .field(
                    Field::number("lutron.default_fade_secs")
                        .label("Default fade")
                        .unit("secs")
                        .default(1.0)
                        .min(0)
                        .help("Dimmer/shade transition time. Override per device in the table below."),
                )
                .field(
                    Field::duration("lutron.hold_threshold_ms")
                        .label("Hold threshold")
                        .unit("ms")
                        .default(500)
                        .min(1)
                        .help(
                            "How long a keypad or Pico button must stay down before it \
                             reports a hold instead of a press.",
                        ),
                )
                .field(
                    Field::duration("lutron.reconnect_delay_secs")
                        .label("Reconnect delay")
                        .unit("secs")
                        .default(5)
                        .min(1)
                        .help("Backoff before retrying a dropped repeater connection."),
                ),
        )
        .section(
            Section::new("devices", "Devices")
                .field(Field::note(
                    "The Main Repeater serves its whole design at \
                     http://{repeater}/DbXmlInfo.xml, so devices can be discovered \
                     rather than typed. RadioRA 2 declares each load's type, so \
                     imported rows arrive already classified.",
                ))
                .field(
                    Field::import("import_design")
                        .label("Discover from the repeater")
                        .targets(["devices", "scenes", "time_clocks"])
                        .placeholder(
                            "Leave empty to fetch from the repeater, or paste a \
                             DbXmlInfo.xml here",
                        )
                        .help(
                            "Press Import to read the design straight from the \
                             repeater. Paste a DbXmlInfo.xml instead if this machine \
                             cannot reach it. Rows are added below for review and are \
                             not saved until you save.",
                        ),
                )
                .field(
                    Field::table("devices")
                        .label("Devices")
                        // A real RA2 project is dozens of devices — this one
                        // discovers 46 — which a card apiece makes unreadable.
                        .render("list")
                        .group_by("area")
                        // Identity for discovery: re-running it updates nothing
                        // and duplicates nothing.
                        .key_by("integration_id")
                        .help("Each row maps an RA2 integration ID to a homeCore device.")
                        .columns([
                            Field::int("integration_id").label("Integration ID"),
                            Field::text("name").label("Name"),
                            // Discovery fills this from `OutputType`, so it is
                            // rarely blank — but a hand-added row still needs
                            // answering, and a device without a kind is one the
                            // plugin will skip.
                            Field::select("kind")
                                .label("Kind")
                                .prompt_when_empty()
                                .option("dimmer", "Dimmer")
                                .option("switch", "Switch")
                                .option("shade", "Shade")
                                .option("fan_control", "Fan")
                                .option("cco_pulsed", "Contact closure (pulsed)")
                                .option("keypad", "Keypad")
                                .option("pico", "Pico remote")
                                .option("occupancy_group", "Occupancy group")
                                .option("vcrx", "Visor control receiver"),
                            area_column(),
                            // Optional in `DeviceConfig` — empty inherits
                            // `lutron.default_fade_secs`, so no `.default()`.
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
                            // Keypad/Pico only. The LED component is derived
                            // (+80 per the Integration Guide), so only the
                            // button numbers are entered here.
                            Field::list("buttons", "int")
                                .label("Buttons")
                                .placeholder("1, 2, 3, 4, 5, 6")
                                .help(
                                    "Button component numbers, used to read each LED's \
                                     state on connect. Pico buttons start at 2.",
                                ),
                            Field::list("all_buttons", "int")
                                .label("All buttons")
                                .placeholder("2, 3, 4, 5, 6")
                                .help(
                                    "Every pressable button, LED or not — published as \
                                     available_buttons so a rule editor can name them. \
                                     Filled in by DbXML discovery; a Pico has no LEDs, so \
                                     this is the only list that ever contains its buttons.",
                                ),
                            Field::list("button_names", "string")
                                .label("Button engravings")
                                .placeholder("Overhead On, Overhead Off")
                                .help(
                                    "What is printed on each button, in the same order as \
                                     All buttons. Filled in by DbXML discovery; leave an \
                                     entry blank where a button has no engraving.",
                                ),
                            // VCRX only.
                            Field::list("ccis", "int")
                                .label("Contact inputs")
                                .placeholder("30, 31, 32, 33")
                                .help(
                                    "CCI component numbers that report open/closed. On a \
                                     VCRX: 30 Full/Security, 31 Flash, 32 Input 1, 33 Input 2.",
                                ),
                        ]),
                ),
        )
        .section(
            Section::new("scenes", "Scenes")
                .field(Field::note(
                    "Each scene is a phantom button on the Main Repeater, published as \
                     a homeCore scene that can be activated from a rule or the app.",
                ))
                .field(
                    Field::table("scenes")
                        .label("Scenes")
                        .render("list")
                        // No `group_by`: a scene is a phantom button on the
                        // repeater and belongs to no room, so `SceneConfig` has
                        // no area to group on.
                        .key_by("button_component")
                        .columns([
                            Field::text("name").label("Name"),
                            Field::int("main_repeater_id")
                                .label("Repeater ID")
                                .default(1)
                                .help("Integration ID of the Main Repeater — almost always 1."),
                            Field::int("button_component")
                                .label("Phantom button")
                                .help("Component number assigned to this scene in RadioRA 2 programming."),
                        ]),
                ),
        )
        .section(
            Section::new("timeclocks", "Timeclock events")
                .field(Field::note(
                    "Timeclock events can be enabled, disabled, or fired for testing. \
                     The Main Repeater has a single timeclock, so the ID is almost \
                     always 1 and events are addressed by their index within it.",
                ))
                .field(
                    Field::table("time_clocks")
                        .label("Timeclock events")
                        .render("list")
                        .group_by("area")
                        // No `key_by`: an event's identity is the pair
                        // (timeclock_id, event_index), which a single column
                        // cannot express — and the index is positional, so
                        // reordering events in RadioRA 2 silently repoints
                        // them. Better to have no identity than a wrong one
                        // that would let discovery overwrite the wrong row.
                        .columns([
                            Field::int("timeclock_id").label("Timeclock ID").default(1),
                            Field::int("event_index").label("Event index"),
                            Field::text("name").label("Name"),
                            area_column(),
                        ]),
                ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_lutron=debug"),
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
    pub lutron: LutronConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
    #[serde(default)]
    pub scenes: Vec<SceneConfig>,
    #[serde(default)]
    pub time_clocks: Vec<TimeclockConfig>,
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
    "plugin.lutron".into()
}

// ---------------------------------------------------------------------------
// Lutron RA2 connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LutronConfig {
    pub host: String,
    #[serde(default = "default_lip_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_fade_secs")]
    pub default_fade_secs: f64,
    #[serde(default = "default_hold_threshold_ms")]
    pub hold_threshold_ms: u64,
    #[serde(default = "default_reconnect_delay_secs")]
    pub reconnect_delay_secs: u64,
}

impl Default for LutronConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_lip_port(),
            username: default_username(),
            password: String::new(),
            default_fade_secs: default_fade_secs(),
            hold_threshold_ms: default_hold_threshold_ms(),
            reconnect_delay_secs: default_reconnect_delay_secs(),
        }
    }
}

fn default_lip_port() -> u16 {
    23
}
fn default_username() -> String {
    "lutron".into()
}
fn default_fade_secs() -> f64 {
    1.0
}
fn default_hold_threshold_ms() -> u64 {
    500
}
fn default_reconnect_delay_secs() -> u64 {
    5
}

// ---------------------------------------------------------------------------
// Device config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Dimmer,
    Switch,
    /// Motorized shade — published as HomeCore `cover`, stubbed for phase 2.
    Shade,
    /// Ceiling fan controller — discrete speeds over the same `#OUTPUT` levels
    /// a dimmer uses. RA2 calls it `CEILING_FAN_TYPE`.
    FanControl,
    /// A *pulsed* contact-closure output (`CCO_PULSED`) — a momentary relay,
    /// typically a garage door or gate trigger.
    ///
    /// Published as a `scene` rather than a switch, because that is what it
    /// behaves like: it accepts `{"activate": true}` and does not latch. A
    /// switch would give the UI a toggle that never stays on. *Maintained*
    /// CCOs are not a kind of their own — they latch, so `Switch` is already
    /// the correct model.
    CcoPulsed,
    /// Wall keypad — publishes button press/release/hold/double_click events and
    /// LED state; accepts set_led and press_button commands.
    Keypad,
    /// Pico wireless remote — publishes button events only; truly read-only
    /// (no LEDs, no outbound commands).  Pico button component numbers start at 2:
    ///   Button 1 = component 2, Button 2 = component 3,
    ///   Raise = component 5,    Lower = component 6.
    Pico,
    /// Occupancy sensor group — publishes occupied/vacant, read-only.
    OccupancyGroup,
    /// Visor Control Receiver (RR-VCRX) — 6 buttons with LEDs (like Keypad)
    /// plus Contact Closure Inputs (CCIs) that report open/closed state.
    /// Button LEDs use standard +80 offset (component 81-86).
    /// CCI components are typically 31-34.
    Vcrx,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    pub integration_id: u32,
    pub name: String,
    pub kind: DeviceKind,
    pub area: Option<String>,
    /// Per-device fade time override (seconds).  Falls back to lutron.default_fade_secs.
    pub fade_secs: Option<f64>,
    /// Invert cover position: false = Lutron native (0=open, 100=closed),
    /// true = inverted (0=closed, 100=open).
    #[serde(default)]
    pub invert_position: bool,
    /// Button component numbers on this keypad (e.g. [1, 2, 3, 4, 5, 6] for a
    /// 6-button seeTouch keypad).  Used to query each button's LED state on
    /// connect.  Ignored for non-keypad kinds.  Per the Lutron Integration Guide,
    /// LED component = button component + 80; this offset is applied automatically.
    #[serde(default)]
    pub buttons: Vec<u32>,
    /// Every button a person can press on this device, LED or not.
    ///
    /// Distinct from `buttons`, which is the LED-query list and is empty on a
    /// Pico. This is what gets published as `available_buttons` so a UI can
    /// name the buttons rather than asking for a component number. Populated
    /// by DbXML discovery; empty on a config written before that existed, in
    /// which case the device simply reports no catalogue.
    #[serde(default)]
    pub all_buttons: Vec<u32>,
    /// What Lutron engraved on each button, positionally matching
    /// `all_buttons`, empty where a button carries no engraving.
    ///
    /// Parallel rather than a map because that is what a config file can show
    /// legibly and a descriptor can render. Published alongside the numbers so
    /// a UI can offer "Overhead On" instead of "button 3".
    #[serde(default)]
    pub button_names: Vec<String>,
    /// CCI (Contact Closure Input) component numbers on a VCRX device.
    /// These report open/closed state via ~DEVICE press/release events.
    /// VCRX components are 30 (Full/Security), 31 (Flash), 32 (Input 1) and
    /// 33 (Input 2), per the Integration Guide.  Ignored for non-VCRX kinds.
    #[serde(default)]
    pub ccis: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Timeclock config
// ---------------------------------------------------------------------------

/// A single schedulable event on a Lutron RadioRA2 timeclock.
///
/// The RA2 main repeater supports one timeclock (ID 1).  Events are addressed
/// by index within that timeclock.  Two operations are supported:
///   - Enable/Disable: `#TIMECLOCK,{id},6,{event_index},{1=Enable|2=Disable}`
///   - Execute (test trigger): `#TIMECLOCK,{id},5,{event_index}`
///
/// HomeCore device ID: `lutron_tc_{timeclock_id}_{event_index}`
/// State published:    `{ "enabled": true|false }`
/// Commands accepted:  `{ "enable": true|false }`, `{ "execute": true }`
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimeclockConfig {
    /// Lutron timeclock integration ID (almost always 1 for the Main Repeater).
    pub timeclock_id: u32,
    /// Event index within the timeclock (as assigned in RadioStar programming).
    pub event_index: u32,
    /// Human-readable name for this timeclock event.
    pub name: String,
    /// Optional HomeCore area tag.
    pub area: Option<String>,
}

impl TimeclockConfig {
    pub fn hc_id(&self) -> String {
        format!("lutron_tc_{}_{}", self.timeclock_id, self.event_index)
    }
}

// ---------------------------------------------------------------------------
// Scene config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SceneConfig {
    pub name: String,
    /// Integration ID of the Main Repeater — almost always 1.
    pub main_repeater_id: u32,
    /// Phantom button component number assigned in RadioStar.
    pub button_component: u32,
}

impl SceneConfig {
    /// HomeCore device ID: `lutron_scene_{name_slug}`.
    pub fn hc_id(&self) -> String {
        let slug = self
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect::<String>();
        format!("lutron_scene_{slug}")
    }
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable (the class of bug that dropped four hc-sonos logging
    /// settings, `5bccebf`). The check lives in the SDK; every leaf must be in
    /// the descriptor or a justified omission. It descends into arrays of
    /// objects, so the three tables are checked column by column too.
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            // Bootstrap identity fixed at install, not an operator setting.
            &["homecore.plugin_id"],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}
