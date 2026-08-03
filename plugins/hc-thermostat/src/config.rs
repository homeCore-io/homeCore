//! Plugin configuration — parsed from `config.toml`.

use anyhow::{anyhow, Result};
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
                    "A thermostat here is virtual: it reads temperature from \
                     sensors you already have, compares it to a setpoint, and \
                     switches something on or off. The sensors and the switch can \
                     belong to any plugin.",
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
                            // Generated, never shown. This becomes the device
                            // id `thermostat_<id>`, which sounds like something
                            // to choose until you notice nobody types it: core
                            // assigns the device a canonical name from its area
                            // and Name (`hallway.upstairs`) and the rule
                            // resolver accepts that. Asking for an id only
                            // invented a second identifier that must never
                            // change.
                            Field::text("id").label("ID").generated(),
                            Field::text("name").label("Name").prompt_when_empty(),
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
                                .option("max", "Warmest")
                                .help(
                                    "How several sensors become one number. \
                                     Average suits a room with two sensors; coldest \
                                     suits \"heat until nowhere is cold\".",
                                ),
                            Field::select("mode")
                                .label("Mode")
                                .default("off")
                                .option("heat", "Heat")
                                .option("cool", "Cool")
                                .option("off", "Off")
                                .help("Off leaves the actuator alone entirely."),
                            Field::number("setpoint")
                                .label("Setpoint")
                                .prompt_when_empty()
                                .help(
                                    "The target temperature, in whatever unit the sensors report.",
                                ),
                            Field::number("hysteresis")
                                .label("Deadband")
                                .default(1.0)
                                .min(0)
                                .help(
                                    "How far past the setpoint before switching \
                                     back. Too small and the relay chatters around \
                                     the target; 1 degree is a sane start.",
                                ),
                            Field::select("actuator_device_id")
                                .label("Switches")
                                .placeholder("Nothing")
                                .prompt_when_empty()
                                .source(Source::core_resource("all_devices").capability("switch"))
                                .help("The device driven on and off — a relay, a switch, a valve."),
                            Field::duration("min_on_secs")
                                .label("Minimum on")
                                .unit("secs")
                                .default(0)
                                .min(0)
                                .help(
                                    "Hold it on at least this long once started. \
                                     Compressors and boilers are damaged by short \
                                     cycling; 0 disables the guard.",
                                ),
                            Field::duration("min_off_secs")
                                .label("Minimum off")
                                .unit("secs")
                                .default(0)
                                .min(0)
                                .help("Same guard in the other direction."),
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
    pub homecore: HomecoreSection,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default, rename = "thermostat")]
    pub thermostats: Vec<ThermostatEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomecoreSection {
    pub plugin_id: String,
    pub broker_host: String,
    pub broker_port: u16,
    /// MQTT credential. Empty = anonymous (dev broker default).
    /// The broker uses `plugin_id` as the username; this is just the password.
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

fn default_heartbeat_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ThermostatEntry {
    pub id: String,
    pub name: String,

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

    #[serde(default)]
    pub actuator_device_id: String,
    #[serde(default)]
    pub actuator_on_cmd: Option<serde_json::Value>,
    #[serde(default)]
    pub actuator_off_cmd: Option<serde_json::Value>,

    #[serde(default)]
    pub min_on_secs: u64,
    #[serde(default)]
    pub min_off_secs: u64,
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

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("Cannot read config file {path}: {e}"))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| anyhow!("Config parse error in {path}: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.homecore.plugin_id.is_empty() {
            return Err(anyhow!("homecore.plugin_id is required"));
        }
        for t in &self.thermostats {
            if t.id.is_empty() {
                return Err(anyhow!("thermostat id is required"));
            }
            if !matches!(t.mode.as_str(), "heat" | "cool" | "off") {
                return Err(anyhow!(
                    "thermostat {}: mode must be heat|cool|off (got {})",
                    t.id,
                    t.mode
                ));
            }
            if !matches!(t.aggregation.as_str(), "average" | "min" | "max") {
                return Err(anyhow!(
                    "thermostat {}: aggregation must be average|min|max (got {})",
                    t.id,
                    t.aggregation
                ));
            }
            if t.hysteresis < 0.0 {
                return Err(anyhow!(
                    "thermostat {}: hysteresis must be non-negative",
                    t.id
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod descriptor_tests {
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
                // Implied by the sensor picker. It only offers devices that
                // publish a `temperature` attribute, so which attribute to read
                // is already answered and asking again could only contradict
                // the choice. Kept in the struct, with its default, so a device
                // naming the reading something else stays reachable by editing
                // the TOML — unconfigurable and merely unprompted are different
                // things.
                "thermostat[].sensor_attribute",
                // Implied by the switch picker, which offers only binary
                // switches. The runtime already falls back to the HomeCore
                // Binary Switch convention — `{"on": true}` / `{"on": false}` —
                // so for every device now selectable these fields would restate
                // what the plugin does anyway. They remain in the struct as the
                // escape hatch for an actuator wanting a bespoke payload.
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
