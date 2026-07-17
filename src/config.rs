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

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub lutron: LutronConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
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
    /// CCI (Contact Closure Input) component numbers on a VCRX device.
    /// These report open/closed state via ~DEVICE press/release events.
    /// Typical values: [31, 32, 33, 34].  Ignored for non-VCRX kinds.
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
