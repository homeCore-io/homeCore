//! Plugin configuration — parsed from `config.toml`.
//!
//! # One bad thermostat is one bad thermostat
//!
//! The file is read entry by entry. A `[[thermostat]]` that cannot be used —
//! a missing field, a setpoint that is really °F, a heater asked to cool — is
//! left out and said (a [`SkippedEntry`], shown on the plugin page with its
//! fix); every other thermostat runs. It used to be that one such entry exited
//! the whole plugin, so a typo in one room stopped every room's heating.
//!
//! Only a file that is not TOML at all, or a `[homecore]` that cannot be read,
//! still stops the plugin: without them it cannot reach homeCore to say so.
//!
//! # Edits keep the rest of the file
//!
//! Adding, removing or fixing a thermostat from the page edits the
//! `thermostat` array in place ([`append_entry`], [`remove_entry`],
//! [`set_entry_setpoint`]) — comments, `[homecore]` and `[logging]` as they
//! were. It used to rewrite the whole file from memory, which could wipe the
//! connection settings when the file did not parse.

use anyhow::{anyhow, Context, Result};
use plugin_sdk_rs::logging::LoggingConfig;
use serde::{Deserialize, Serialize};

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

    Descriptor::new("plugin.thermostat")
        .title("Thermostat")
        .section(
            Section::new("thermostats", "Thermostats")
                .help(
                    "A thermostat here is software: it reads temperature from \
                     sensors you already have, compares it to a target, and \
                     switches a plug or switch on or off. The sensors and the switch \
                     can belong to any plugin. Temperatures are °C.",
                )
                .field(
                    // `thermostat`, not `thermostats` — the field is
                    // `#[serde(rename = "thermostat")]` so the TOML reads
                    // `[[thermostat]]`. The descriptor must name the wire key,
                    // or the editor writes somewhere the plugin never reads.
                    Field::table("thermostat")
                        .label("Thermostats")
                        .render("list")
                        .key_by("id")
                        .columns([
                            // Generated, never shown: it becomes the device id
                            // `thermostat_<id>`, which must never change.
                            Field::text("id").label("ID").generated(),
                            Field::text("name").label("Name").prompt_when_empty(),
                            Field::select("kind")
                                .label("Runs")
                                .option("ac", "An AC (cools)")
                                .option("heater", "A heater (heats)")
                                .option("fan", "A fan or other (runs when too warm)")
                                .help(
                                    "What the switch runs. An AC or a fan offers Cool and \
                                     Off; a heater Heat and Off. Left empty, it follows the \
                                     mode (heat → heater, otherwise AC).",
                                ),
                            Field::list("sensor_device_ids", "text")
                                .label("Sensors")
                                .prompt_when_empty()
                                .source(
                                    Source::core_resource("all_devices").capability("temperature"),
                                )
                                .help(
                                    "One or more devices reporting temperature. \
                                     Several are combined by the rule below.",
                                ),
                            Field::select("aggregation")
                                .label("Combine by")
                                .default("average")
                                .option("average", "Average")
                                .option("min", "Coldest")
                                .option("max", "Warmest"),
                            Field::select("mode")
                                .label("Mode")
                                .default("off")
                                .option("heat", "Heat")
                                .option("cool", "Cool")
                                .option("off", "Off")
                                .help(
                                    "Off turns the switch off once, then leaves it alone — \
                                     it can be used by hand.",
                                ),
                            Field::number("setpoint")
                                .label("Target")
                                .prompt_when_empty()
                                .min(-40)
                                .max(45)
                                .help(
                                    "The temperature to keep, in °C (68 °F is 20 °C). The \
                                     app shows °F if you prefer it.",
                                ),
                            Field::number("hysteresis")
                                .label("Swing")
                                .default(1.0)
                                .min(0)
                                .max(MAX_HYSTERESIS)
                                .help(
                                    "A °C difference, split either side of the target: 1 \
                                     switches on half a degree past it and off half a degree \
                                     back. Too small and the switch chatters.",
                                ),
                            Field::select("actuator_device_id")
                                .label("Switches")
                                .placeholder("Nothing")
                                .prompt_when_empty()
                                .source(Source::core_resource("all_devices").capability("switch"))
                                .help("The plug or switch turned on and off."),
                            Field::duration("min_on_secs")
                                .label("Once on, stays on at least")
                                .unit("secs")
                                .default(0)
                                .min(0)
                                .max(MAX_GUARD_SECS as i64)
                                .help(
                                    "Compressors are damaged by short cycling: an AC wants \
                                     180. 0 disables the guard.",
                                ),
                            Field::duration("min_off_secs")
                                .label("Once off, stays off at least")
                                .unit("secs")
                                .default(0)
                                .min(0)
                                .max(MAX_GUARD_SECS as i64)
                                .help("An AC wants 300. 0 disables the guard."),
                            Field::duration("max_run_secs")
                                .label("Longest run")
                                .unit("secs")
                                .min(0)
                                .max(MAX_RUN_LIMIT_SECS as i64)
                                .help(
                                    "Turned off after running this long without a break, \
                                     with an alert. Empty: 4 h for a heater, no limit \
                                     otherwise. 0: no limit.",
                                ),
                            Field::duration("sensor_timeout_secs")
                                .label("A sensor goes quiet after")
                                .unit("secs")
                                .min(60)
                                .max(MAX_SENSOR_TIMEOUT_SECS as i64)
                                .help("A reading older than this no longer counts. Empty: 2 h."),
                            Field::select("on_stale")
                                .label("With no reading")
                                .option("off", "Turn it off")
                                .option("hold", "Leave it as it is")
                                .help("What happens when no sensor has a current reading. Empty: off."),
                            Field::duration("override_secs")
                                .label("Switched by hand, left alone for")
                                .unit("secs")
                                .min(0)
                                .max(MAX_OVERRIDE_SECS as i64)
                                .help(
                                    "Someone flipping the switch is a manual override for \
                                     this long, or until the target or mode changes. Empty: \
                                     1 h. 0: take over again at once.",
                                ),
                        ]),
                ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_thermostat=debug"),
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
                .field(Field::secret("homecore.password").label("Broker password"))
                .field(
                    Field::duration("homecore.heartbeat_secs")
                        .label("Heartbeat")
                        .unit("secs")
                        .default(60)
                        .min(1),
                ),
        )
        .build()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Config {
    /// Optional, like every field inside it now is.
    ///
    /// It was mandatory, and its identity and broker fields had no defaults, so
    /// a config missing the section — or merely missing `plugin_id` — killed
    /// the plugin outright: "missing field", exit 1, before it could log or
    /// attempt anything.
    #[serde(default)]
    pub homecore: HomecoreSection,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default, rename = "thermostat")]
    pub thermostats: Vec<ThermostatEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomecoreSection {
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    /// MQTT credential. Empty = anonymous (dev broker default).
    /// The broker uses `plugin_id` as the username; this is just the password.
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

impl Default for HomecoreSection {
    fn default() -> Self {
        Self {
            plugin_id: default_plugin_id(),
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            password: String::new(),
            heartbeat_secs: default_heartbeat_secs(),
        }
    }
}

fn default_plugin_id() -> String {
    "plugin.thermostat".into()
}
fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_heartbeat_secs() -> u64 {
    60
}

/// One thermostat as the file has it.
///
/// Every field added after the first release is optional and left out when
/// absent, so an entry written before them serialises exactly as it did — the
/// runtime changes kept in learned state are matched against that form (see
/// `bridge::learned_entry`) and survive the upgrade.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThermostatEntry {
    pub id: String,
    pub name: String,

    #[serde(default)]
    pub sensor_device_ids: Vec<String>,
    #[serde(default = "default_sensor_attribute")]
    pub sensor_attribute: String,
    #[serde(default = "default_aggregation")]
    pub aggregation: String,

    pub setpoint: f64,
    #[serde(default = "default_hysteresis")]
    pub hysteresis: f64,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// `ac`, `heater` or `fan`; absent, it follows `mode` (see [`Kind::of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,

    #[serde(default)]
    pub actuator_device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator_on_cmd: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator_off_cmd: Option<serde_json::Value>,

    #[serde(default)]
    pub min_on_secs: u64,
    #[serde(default)]
    pub min_off_secs: u64,
    /// Longest run before it is forced off; absent: [`Kind::default_max_run_secs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_secs: Option<u64>,
    /// A reading older than this no longer counts; absent: 2 h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_timeout_secs: Option<u64>,
    /// `off` (the default) or `hold`: what to do with no current reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_stale: Option<String>,
    /// How long a hand flip of the switch is left alone; absent: 1 h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_secs: Option<u64>,
}

fn default_sensor_attribute() -> String {
    "temperature".into()
}
fn default_aggregation() -> String {
    "average".into()
}
fn default_hysteresis() -> f64 {
    1.0
}
fn default_mode() -> String {
    "off".into()
}

/// What a thermostat's switch runs, which decides the modes it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ac,
    Heater,
    Fan,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ac" => Some(Kind::Ac),
            "heater" => Some(Kind::Heater),
            "fan" => Some(Kind::Fan),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ac => "ac",
            Kind::Heater => "heater",
            Kind::Fan => "fan",
        }
    }

    /// The kind an entry has: its own, or — for an entry from before kinds —
    /// what its mode says: `heat` is a heater, anything else an AC.
    pub fn of(entry: &ThermostatEntry) -> Self {
        entry
            .kind
            .as_deref()
            .and_then(Kind::parse)
            .unwrap_or(if entry.mode == "heat" {
                Kind::Heater
            } else {
                Kind::Ac
            })
    }

    /// The `hvac_mode`s it offers: an AC or a fan cools, a heater heats.
    pub fn modes(self) -> &'static [&'static str] {
        match self {
            Kind::Heater => &["heat", "off"],
            Kind::Ac | Kind::Fan => &["cool", "off"],
        }
    }

    /// The mode it runs in.
    pub fn active_mode(self) -> &'static str {
        self.modes()[0]
    }

    /// Words for a refusal: "An AC only cools".
    pub fn only(self) -> &'static str {
        match self {
            Kind::Ac => "An AC only cools",
            Kind::Heater => "A heater only heats",
            Kind::Fan => "A fan only runs when it is too warm",
        }
    }

    /// Short-cycle guards for a new thermostat: (min on, min off).
    pub fn default_guards(self) -> (u64, u64) {
        match self {
            Kind::Ac => (180, 300),
            Kind::Heater => (120, 120),
            Kind::Fan => (60, 60),
        }
    }

    /// The swing for a new thermostat, °C.
    pub fn default_hysteresis(self) -> f64 {
        match self {
            Kind::Fan => 2.0,
            Kind::Ac | Kind::Heater => 1.0,
        }
    }

    /// A heater is never left on longer than 4 h unless the file says so.
    pub fn default_max_run_secs(self) -> u64 {
        match self {
            Kind::Heater => 4 * 3600,
            Kind::Ac | Kind::Fan => 0,
        }
    }
}

/// The largest swing taken, °C.
pub const MAX_HYSTERESIS: f64 = 5.0;
/// The longest short-cycle guard taken, either way.
pub const MAX_GUARD_SECS: u64 = 3600;
/// The longest "longest run".
pub const MAX_RUN_LIMIT_SECS: u64 = 24 * 3600;
pub const MIN_SENSOR_TIMEOUT_SECS: u64 = 60;
pub const MAX_SENSOR_TIMEOUT_SECS: u64 = 24 * 3600;
pub const DEFAULT_SENSOR_TIMEOUT_SECS: u64 = 2 * 3600;
pub const MAX_OVERRIDE_SECS: u64 = 24 * 3600;
pub const DEFAULT_OVERRIDE_SECS: u64 = 3600;

impl ThermostatEntry {
    pub fn kind(&self) -> Kind {
        Kind::of(self)
    }
    pub fn sensor_timeout_secs(&self) -> u64 {
        self.sensor_timeout_secs
            .unwrap_or(DEFAULT_SENSOR_TIMEOUT_SECS)
            .clamp(MIN_SENSOR_TIMEOUT_SECS, MAX_SENSOR_TIMEOUT_SECS)
    }
    pub fn max_run_secs(&self) -> u64 {
        self.max_run_secs
            .unwrap_or_else(|| self.kind().default_max_run_secs())
            .min(MAX_RUN_LIMIT_SECS)
    }
    pub fn override_secs(&self) -> u64 {
        self.override_secs
            .unwrap_or(DEFAULT_OVERRIDE_SECS)
            .min(MAX_OVERRIDE_SECS)
    }
    /// Whether it turns the switch off when no reading is current.
    pub fn off_when_stale(&self) -> bool {
        self.on_stale.as_deref() != Some("hold")
    }
    pub fn device_id(&self) -> String {
        format!("thermostat_{}", self.id)
    }

    /// Bring numbers past their sane maxima down to them. Returns what was
    /// changed, in words, for a notice.
    pub fn cap(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        if self.hysteresis > MAX_HYSTERESIS {
            said.push(format!(
                "swing {} °C is more than {MAX_HYSTERESIS} °C",
                self.hysteresis
            ));
            self.hysteresis = MAX_HYSTERESIS;
        }
        for (name, v) in [
            ("minimum on", &mut self.min_on_secs),
            ("minimum off", &mut self.min_off_secs),
        ] {
            if *v > MAX_GUARD_SECS {
                said.push(format!("{name} {v} s is more than {MAX_GUARD_SECS} s"));
                *v = MAX_GUARD_SECS;
            }
        }
        said
    }

    /// Why this entry can't run, if it can't. `None` is good.
    pub fn problem(&self) -> Option<Problem> {
        let say = |s: String| {
            Some(Problem {
                message: s,
                fix: None,
            })
        };
        if let Some(p) = id_problem(&self.id) {
            return say(p);
        }
        if let Some(k) = &self.kind {
            if Kind::parse(k).is_none() {
                return say(format!("kind must be ac, heater or fan (got {k})"));
            }
        }
        if !matches!(self.mode.as_str(), "heat" | "cool" | "off") {
            return say(format!(
                "mode must be heat, cool or off (got {})",
                self.mode
            ));
        }
        let kind = self.kind();
        if !kind.modes().contains(&self.mode.as_str()) {
            return say(format!(
                "{} — mode {} isn't one it has ({})",
                kind.only(),
                self.mode,
                kind.modes().join(" or ")
            ));
        }
        if !matches!(self.aggregation.as_str(), "average" | "min" | "max") {
            return say(format!(
                "aggregation must be average, min or max (got {})",
                self.aggregation
            ));
        }
        if self.hysteresis < 0.0 || !self.hysteresis.is_finite() {
            return say("the swing (hysteresis) must be 0 or more".into());
        }
        if let Some(s) = &self.on_stale {
            if !matches!(s.as_str(), "off" | "hold") {
                return say(format!("on_stale must be off or hold (got {s})"));
            }
        }
        if let Some(p) = setpoint_problem(self.setpoint) {
            return Some(p);
        }
        if self.mode != "off" && self.sensor_device_ids.is_empty() {
            return say(format!(
                "it is set to {} but reads no sensor — add one under Sensors",
                self.mode
            ));
        }
        None
    }
}

/// Why an entry can't run, and the one-press fix when there is one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<SetpointFix>,
}

/// "Make it 68 °F (20 °C)": the setpoint as written, read as °F, in °C.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SetpointFix {
    pub fahrenheit: f64,
    pub celsius: f64,
}

/// The setpoints a thermostat accepts, °C.
///
/// A number above this is almost certainly a °F setpoint left from before —
/// 68 °C would run a heater without end — so it is refused rather than obeyed.
pub const SETPOINT_RANGE: std::ops::RangeInclusive<f64> = -40.0..=45.0;

/// °F to °C, to the nearest half degree — the step a thermostat takes.
pub fn f_to_c_half(f: f64) -> f64 {
    ((f - 32.0) * 5.0 / 9.0 * 2.0).round() / 2.0
}

/// Why `setpoint` cannot be used, if it cannot — with the °F reading of it
/// when it looks like one.
pub fn setpoint_problem(setpoint: f64) -> Option<Problem> {
    if SETPOINT_RANGE.contains(&setpoint) {
        return None;
    }
    if (46.0..=110.0).contains(&setpoint) {
        let c = f_to_c_half(setpoint);
        return Some(Problem {
            message: format!(
                "its target is {setpoint}, which looks like °F ({setpoint} °F is {c} °C) — \
                 thermostats keep °C, and {setpoint} °C would run without end"
            ),
            fix: Some(SetpointFix {
                fahrenheit: setpoint,
                celsius: c,
            }),
        });
    }
    Some(Problem {
        message: format!(
            "its target {setpoint} is outside {}–{} °C",
            SETPOINT_RANGE.start(),
            SETPOINT_RANGE.end()
        ),
        fix: None,
    })
}

/// Why `id` can't be a thermostat's id: it becomes the device id
/// `thermostat_<id>` and an MQTT topic level, so no `/ + #`, no spaces.
pub fn id_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("it has no id".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some(format!(
            "its id \"{id}\" may hold only letters, digits, _ and - (no spaces, / + or #)"
        ));
    }
    None
}

/// An id from a name: "Den Heater" → `den_heater`.
pub fn id_from_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() {
        "thermostat".into()
    } else {
        out
    }
}

/// A `[[thermostat]]` that was left out, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedEntry {
    /// Its place among the file's `[[thermostat]]` tables, from 0.
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<SetpointFix>,
}

impl SkippedEntry {
    /// Its notice code: one per entry, so each clears by itself.
    pub fn code(&self) -> String {
        format!(
            "config_entry_skipped:{}",
            self.id.clone().unwrap_or_else(|| self.index.to_string())
        )
    }
    /// What it is called, for a person.
    pub fn called(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.id.clone())
            .unwrap_or_else(|| format!("thermostat {}", self.index + 1))
    }
}

/// What reading the file found.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub skipped: Vec<SkippedEntry>,
    /// Numbers brought down to their maxima, per thermostat id.
    pub capped: Vec<(String, Vec<String>)>,
}

impl Config {
    /// Read the file, keeping the thermostats that can run and saying the rest.
    pub fn load(path: &str) -> Result<Loaded> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("Cannot read config file {path}: {e}"))?;
        Self::parse(&text).map_err(|e| anyhow!("Config parse error in {path}: {e}"))
    }

    pub fn parse(text: &str) -> Result<Loaded> {
        let mut table: toml::Table = toml::from_str(text)?;
        let raw = table.remove("thermostat");
        let mut config: Config = toml::Value::Table(table).try_into()?;
        if config.homecore.plugin_id.is_empty() {
            return Err(anyhow!("homecore.plugin_id is required"));
        }
        let raw: Vec<toml::Value> = match raw {
            None => Vec::new(),
            Some(toml::Value::Array(a)) => a,
            Some(_) => return Err(anyhow!("`thermostat` must be [[thermostat]] tables")),
        };
        let mut skipped = Vec::new();
        let mut capped = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (index, v) in raw.into_iter().enumerate() {
            let id = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
            let name = v.get("name").and_then(|x| x.as_str()).map(str::to_string);
            let skip = |problem: String, fix: Option<SetpointFix>| SkippedEntry {
                index,
                id: id.clone(),
                name: name.clone(),
                problem,
                fix,
            };
            let mut entry: ThermostatEntry = match v.clone().try_into() {
                Ok(e) => e,
                Err(e) => {
                    skipped.push(skip(plain_toml_error(&e.to_string()), None));
                    continue;
                }
            };
            if let Some(p) = entry.problem() {
                skipped.push(skip(p.message, p.fix));
                continue;
            }
            if !seen.insert(entry.id.clone()) {
                skipped.push(skip(
                    format!(
                        "another thermostat above it already has the id \"{}\"",
                        entry.id
                    ),
                    None,
                ));
                continue;
            }
            let said = entry.cap();
            if !said.is_empty() {
                capped.push((entry.id.clone(), said));
            }
            config.thermostats.push(entry);
        }
        Ok(Loaded {
            config,
            skipped,
            capped,
        })
    }
}

/// toml's error, first line only: "missing field `setpoint`".
fn plain_toml_error(e: &str) -> String {
    e.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(e)
        .trim()
        .to_string()
}

/// A thermostat asked for from the page (`add_thermostat`): what the person
/// chose, with the rest filled in for its kind.
#[derive(Debug, Clone, Deserialize)]
pub struct AddRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub sensor_device_ids: Vec<String>,
    #[serde(default)]
    pub aggregation: Option<String>,
    pub setpoint: f64,
    #[serde(default)]
    pub hysteresis: Option<f64>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub actuator_device_id: String,
    #[serde(default)]
    pub min_on_secs: Option<u64>,
    #[serde(default)]
    pub min_off_secs: Option<u64>,
    #[serde(default)]
    pub max_run_secs: Option<u64>,
    #[serde(default)]
    pub sensor_timeout_secs: Option<u64>,
    #[serde(default)]
    pub on_stale: Option<String>,
    #[serde(default)]
    pub override_secs: Option<u64>,
}

impl AddRequest {
    /// The entry it makes, with an id not already in `taken`.
    pub fn into_entry(self, taken: &dyn Fn(&str) -> bool) -> Result<ThermostatEntry, String> {
        let kind = Kind::parse(&self.kind)
            .ok_or_else(|| format!("kind must be ac, heater or fan (got {})", self.kind))?;
        if self.name.trim().is_empty() {
            return Err("it needs a name".into());
        }
        let id = match self.id.filter(|s| !s.is_empty()) {
            Some(id) => {
                if taken(&id) {
                    return Err(format!("a thermostat with the id \"{id}\" already exists"));
                }
                id
            }
            None => {
                let base = id_from_name(&self.name);
                let mut id = base.clone();
                let mut n = 2;
                while taken(&id) {
                    id = format!("{base}_{n}");
                    n += 1;
                }
                id
            }
        };
        let (on, off) = kind.default_guards();
        let mut e = ThermostatEntry {
            id,
            name: self.name.trim().to_string(),
            sensor_device_ids: self.sensor_device_ids,
            sensor_attribute: default_sensor_attribute(),
            aggregation: self.aggregation.unwrap_or_else(default_aggregation),
            setpoint: self.setpoint,
            hysteresis: self.hysteresis.unwrap_or_else(|| kind.default_hysteresis()),
            mode: self.mode.unwrap_or_else(|| kind.active_mode().to_string()),
            kind: Some(kind.as_str().to_string()),
            actuator_device_id: self.actuator_device_id,
            actuator_on_cmd: None,
            actuator_off_cmd: None,
            min_on_secs: self.min_on_secs.unwrap_or(on),
            min_off_secs: self.min_off_secs.unwrap_or(off),
            max_run_secs: self.max_run_secs,
            sensor_timeout_secs: self.sensor_timeout_secs,
            on_stale: self.on_stale,
            override_secs: self.override_secs,
        };
        if let Some(p) = e.problem() {
            return Err(p.message);
        }
        e.cap();
        Ok(e)
    }
}

// ── Editing the file in place ────────────────────────────────────────────────

fn read_doc(path: &str) -> Result<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    text.parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{path} is not TOML"))
}

fn write_doc(path: &str, doc: &toml_edit::DocumentMut) -> Result<()> {
    let tmp = format!("{path}.partial");
    std::fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {tmp}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {path}"))?;
    Ok(())
}

fn thermostats_mut(doc: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::ArrayOfTables> {
    if !doc.contains_key("thermostat") {
        doc.insert(
            "thermostat",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    doc["thermostat"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("`thermostat` in the file is not [[thermostat]] tables"))
}

/// Add a `[[thermostat]]` at the end of the file's list.
pub fn append_entry(path: &str, entry: &ThermostatEntry) -> Result<()> {
    let mut doc = read_doc(path)?;
    let one: toml_edit::DocumentMut = toml::to_string(entry)?.parse()?;
    let table = one.as_table().clone();
    thermostats_mut(&mut doc)?.push(table);
    write_doc(path, &doc)
}

/// Remove the `[[thermostat]]` with this id. Returns whether one was there.
pub fn remove_entry(path: &str, id: &str) -> Result<bool> {
    let mut doc = read_doc(path)?;
    let list = thermostats_mut(&mut doc)?;
    let before = list.len();
    list.retain(|t| t.get("id").and_then(|v| v.as_str()) != Some(id));
    let removed = list.len() != before;
    if removed {
        write_doc(path, &doc)?;
    }
    Ok(removed)
}

/// Set the setpoint of the `index`th `[[thermostat]]` — checked against its
/// id when the caller knows it, so a file edited meanwhile isn't changed in
/// the wrong place.
pub fn set_entry_setpoint(path: &str, index: usize, id: Option<&str>, celsius: f64) -> Result<()> {
    if setpoint_problem(celsius).is_some() {
        return Err(anyhow!("{celsius} °C is not a target a thermostat takes"));
    }
    let mut doc = read_doc(path)?;
    let list = thermostats_mut(&mut doc)?;
    let t = list
        .get_mut(index)
        .ok_or_else(|| anyhow!("the file has no thermostat {}", index + 1))?;
    if let Some(id) = id {
        if t.get("id").and_then(|v| v.as_str()) != Some(id) {
            return Err(anyhow!(
                "the file changed — thermostat {} is no longer \"{id}\"",
                index + 1
            ));
        }
    }
    t["setpoint"] = toml_edit::value(celsius);
    write_doc(path, &doc)
}

#[cfg(test)]
mod descriptor_tests {
    use super::*;

    /// The descriptor is authoritative — the editor renders it instead of
    /// deriving a form from the schema — so anything it omits becomes
    /// uneditable from the UI.
    #[test]
    #[cfg(feature = "schema")]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity, fixed at install time.
                "homecore.plugin_id",
                // Implied by the sensor picker, which offers only devices
                // with a `temperature`; kept in the struct for a device that
                // names its reading otherwise.
                "thermostat[].sensor_attribute",
                // Custom switch payloads: the file only, never a form or a
                // command (anything else lets a user-role caller send
                // anything to any device).
                "thermostat[].actuator_on_cmd",
                "thermostat[].actuator_off_cmd",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_without_the_homecore_section_still_loads() {
        let cfg = Config::parse("")
            .expect("[homecore] must be optional")
            .config;
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.homecore.broker_port, 1883);
        assert_eq!(cfg.homecore.plugin_id, "plugin.thermostat");
    }

    #[test]
    fn an_explicit_homecore_section_beats_the_defaults() {
        let cfg = Config::parse("[homecore]\nbroker_host = \"10.0.0.5\"\n")
            .unwrap()
            .config;
        assert_eq!(cfg.homecore.broker_host, "10.0.0.5");
        assert_eq!(cfg.homecore.plugin_id, "plugin.thermostat");
    }

    const HOUSE: &str = r#"
[homecore]
plugin_id = "plugin.thermostat"

[[thermostat]]
id = "master_bedroom_ac_thermostat"
name = "Master Bedroom AC Thermostat"
sensor_device_ids = ["yolink_a"]
setpoint = 68.0
mode = "cool"
actuator_device_id = "zwave_39"

[[thermostat]]
id = "office"
name = "Office"
sensor_device_ids = ["yolink_b"]
setpoint = 20.0
mode = "cool"
actuator_device_id = "zwave_38"

[[thermostat]]
name = "No id"
setpoint = 20.0

[[thermostat]]
id = "office"
name = "Office again"
sensor_device_ids = ["yolink_b"]
setpoint = 20.0

[[thermostat]]
id = "den"
name = "Den"
kind = "heater"
mode = "cool"
sensor_device_ids = ["s"]
setpoint = 20.0
"#;

    /// H7: one bad entry is one skipped entry — the rest run.
    #[test]
    fn an_entry_that_cannot_run_is_skipped_and_the_rest_load() {
        let l = Config::parse(HOUSE).unwrap();
        let ids: Vec<_> = l.config.thermostats.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["office"]);
        assert_eq!(l.skipped.len(), 4);
        let f = &l.skipped[0];
        assert_eq!(f.index, 0);
        assert_eq!(f.called(), "Master Bedroom AC Thermostat");
        assert!(f.problem.contains("looks like °F"), "{}", f.problem);
        assert!(f.problem.contains("68 °F is 20 °C"), "{}", f.problem);
        assert_eq!(
            f.fix,
            Some(SetpointFix {
                fahrenheit: 68.0,
                celsius: 20.0
            })
        );
        assert!(l.skipped[1].problem.contains("missing field"));
        assert!(l.skipped[2].problem.contains("already has the id"));
        assert!(l.skipped[3].problem.contains("A heater only heats"));
    }

    #[test]
    fn fahrenheit_reads_to_the_nearest_half_degree() {
        assert_eq!(f_to_c_half(68.0), 20.0);
        assert_eq!(f_to_c_half(70.0), 21.0);
        assert_eq!(f_to_c_half(45.0), 7.0);
        assert!(setpoint_problem(45.0).is_none());
        assert!(setpoint_problem(-41.0).unwrap().fix.is_none());
    }

    #[test]
    fn ids_are_topic_safe() {
        assert!(id_problem("office_ac").is_none());
        for bad in ["", "a b", "a/b", "a+b", "a#b"] {
            assert!(id_problem(bad).is_some(), "{bad}");
        }
        assert_eq!(id_from_name("Den Heater!"), "den_heater");
        assert_eq!(id_from_name("  "), "thermostat");
    }

    /// H8: no kind — it follows the mode, and an old entry keeps working.
    #[test]
    fn an_entry_without_a_kind_follows_its_mode() {
        let l = Config::parse(
            "[[thermostat]]\nid=\"a\"\nname=\"A\"\nsetpoint=20.0\nmode=\"heat\"\nsensor_device_ids=[\"s\"]\n\
             [[thermostat]]\nid=\"b\"\nname=\"B\"\nsetpoint=20.0\nmode=\"cool\"\nsensor_device_ids=[\"s\"]\n\
             [[thermostat]]\nid=\"c\"\nname=\"C\"\nsetpoint=20.0\n",
        )
        .unwrap();
        let k: Vec<_> = l.config.thermostats.iter().map(|t| t.kind()).collect();
        assert_eq!(k, [Kind::Heater, Kind::Ac, Kind::Ac]);
        assert_eq!(l.config.thermostats[0].max_run_secs(), 4 * 3600);
        assert_eq!(l.config.thermostats[1].max_run_secs(), 0);
    }

    /// An entry from before the new fields serialises as it always did, so
    /// runtime changes kept against it survive the upgrade.
    #[test]
    fn an_old_entry_serialises_without_the_new_fields() {
        let e: ThermostatEntry = toml::from_str(
            "id=\"a\"\nname=\"A\"\nsetpoint=20.0\nsensor_device_ids=[\"s\"]\nactuator_device_id=\"x\"\n",
        )
        .unwrap();
        let v = serde_json::to_value(&e).unwrap();
        for k in [
            "kind",
            "max_run_secs",
            "sensor_timeout_secs",
            "on_stale",
            "override_secs",
            "actuator_on_cmd",
        ] {
            assert!(v.get(k).is_none(), "{k}");
        }
    }

    /// M3: limits are capped, and said.
    #[test]
    fn numbers_past_their_maxima_are_capped() {
        let l = Config::parse(
            "[[thermostat]]\nid=\"a\"\nname=\"A\"\nsetpoint=20.0\nhysteresis=9.0\nmin_on_secs=99999\n",
        )
        .unwrap();
        let t = &l.config.thermostats[0];
        assert_eq!(t.hysteresis, MAX_HYSTERESIS);
        assert_eq!(t.min_on_secs, MAX_GUARD_SECS);
        assert_eq!(l.capped[0].1.len(), 2);
    }

    #[test]
    fn heat_or_cool_with_no_sensor_is_refused() {
        let l =
            Config::parse("[[thermostat]]\nid=\"a\"\nname=\"A\"\nsetpoint=20.0\nmode=\"cool\"\n")
                .unwrap();
        assert!(l.skipped[0].problem.contains("reads no sensor"));
    }

    #[test]
    fn a_new_thermostat_gets_the_guards_for_its_kind() {
        let none = |_: &str| false;
        let r: AddRequest = serde_json::from_value(serde_json::json!({
            "name": "Office AC", "kind": "ac", "setpoint": 20.0,
            "sensor_device_ids": ["s"], "actuator_device_id": "zwave_38"
        }))
        .unwrap();
        let e = r.into_entry(&none).unwrap();
        assert_eq!((e.id.as_str(), e.mode.as_str()), ("office_ac", "cool"));
        assert_eq!((e.min_on_secs, e.min_off_secs), (180, 300));
        let r: AddRequest = serde_json::from_value(serde_json::json!({
            "name": "Office AC", "kind": "heater", "setpoint": 20.0, "sensor_device_ids": ["s"]
        }))
        .unwrap();
        let taken = |id: &str| id == "office_ac";
        let e = r.into_entry(&taken).unwrap();
        assert_eq!((e.id.as_str(), e.mode.as_str()), ("office_ac_2", "heat"));
        assert_eq!((e.min_on_secs, e.min_off_secs), (120, 120));
        let r: AddRequest = serde_json::from_value(serde_json::json!({
            "name": "X", "kind": "ac", "setpoint": 20.0
        }))
        .unwrap();
        assert!(r.into_entry(&none).unwrap_err().contains("reads no sensor"));
    }

    /// M6: edits touch the thermostat list and nothing else.
    #[test]
    fn edits_keep_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("hc-thermo-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        let path = path.to_str().unwrap();
        std::fs::write(
            path,
            "# kept\n[homecore]\npassword = \"secret\" # kept too\n\n[[thermostat]]\nid = \"a\"\nname = \"A\"\nsetpoint = 68.0\n",
        )
        .unwrap();
        set_entry_setpoint(path, 0, Some("a"), 20.0).unwrap();
        assert!(set_entry_setpoint(path, 0, Some("b"), 20.0).is_err());
        let r: AddRequest = serde_json::from_value(serde_json::json!({
            "name": "B", "kind": "ac", "setpoint": 21.0, "sensor_device_ids": ["s"]
        }))
        .unwrap();
        append_entry(path, &r.into_entry(&|_| false).unwrap()).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("# kept\n"));
        assert!(text.contains("password = \"secret\" # kept too"));
        let l = Config::parse(&text).unwrap();
        assert_eq!(l.config.thermostats.len(), 2);
        assert_eq!(l.config.thermostats[0].setpoint, 20.0);
        assert_eq!(l.config.homecore.password, "secret");
        assert!(remove_entry(path, "a").unwrap());
        assert!(!remove_entry(path, "a").unwrap());
        let l = Config::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(l.config.thermostats[0].id, "b");
        std::fs::remove_dir_all(&dir).ok();
    }
}
