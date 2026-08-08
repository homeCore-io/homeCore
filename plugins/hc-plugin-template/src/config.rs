//! The plugin's config file.
//!
//! homeCore owns this file. It lives at `config/plugins/<plugin_id>.toml` under
//! homeCore's home directory and is handed to the plugin as `argv[1]`, so all a
//! plugin does is read the path it was given. When an operator edits it — in
//! the web UI, over the API, or by hand — core rewrites the file and restarts
//! this one plugin, so a fresh process always sees the new values.
//!
//! Keep `[homecore]` exactly as it is. Everything else is yours.

use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config **JSON Schema**, published on the capability manifest.
///
/// Core serves it at `GET /plugins/{id}/config/schema`. It is authoritative for
/// what *exists* and for core-side validation. Derived from the structs below,
/// so it cannot drift from what this plugin actually reads.
///
/// `None` when built without the `schema` feature — a leaner binary whose
/// config editor falls back to a raw TOML textarea.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(Config)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// *presented*: sections, units, prose, which fields are secret. A JSON Schema
/// cannot say any of that, so without a descriptor the editor has to guess a
/// form from types alone.
///
/// Core serves it at `GET /plugins/{id}/config/descriptor`. Publish both: the
/// schema stays authoritative for existence and validation, the descriptor
/// annotates intent.
///
/// **A published descriptor is authoritative for the form.** Anything it omits
/// becomes uneditable in the UI, however plainly the schema declares it — which
/// is why `descriptor_covers_every_schema_field` below is not optional
/// boilerplate. Every shipped plugin has that test; hc-sonos shipped a
/// descriptor missing its logging section once and the settings simply vanished
/// from the page.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section};

    Descriptor::new("plugin.template")
        .title("Template")
        .section(
            Section::new("devices", "Devices")
                .help("Each row becomes one virtual light in homeCore.")
                .field(
                    Field::table("template.devices")
                        .label("Devices")
                        .key_by("id")
                        .columns([Field::text("name").label("Name")])
                        .help(
                            "The id identifies the device to homeCore forever — \
                             rules and history are keyed to it, so renaming the \
                             row is safe but changing its id orphans both.",
                        ),
                ),
        )
        .section(
            Section::new("logging", "Logging").field(
                Field::enumeration("logging.forward_level")
                    .label("Forward to core")
                    .render("segmented")
                    .default("info")
                    .help(
                        "Minimum level forwarded to homeCore's live log stream; \
                         anything below is written to stderr only.",
                    )
                    // Exactly the levels `MqttLogLayer` parses. Anything else
                    // falls through to info, so offering an option it does not
                    // understand would quietly do the wrong thing.
                    .option("trace", "Trace")
                    .option("debug", "Debug")
                    .option("info", "Info")
                    .option("warn", "Warn")
                    .option("error", "Error"),
            ),
        )
        // Hidden, not omitted. Core writes these at install and an operator has
        // no reason to touch them — but a hidden field is still *covered*, so
        // the coverage check stays honest and the values remain reachable.
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
    /// Deliberately **not** `#[serde(default)]`, unlike every shipped plugin.
    ///
    /// The others made this optional because a missing section killed them
    /// outright while every value in it had a working default — hc-zwave spent
    /// the better part of an hour in a restart loop over exactly that.
    ///
    /// A template is the one place that must not follow. `plugin_id` is the
    /// single value whoever copies this has to set, and it has no defensible
    /// default: guessing one lets a new plugin register under the wrong name
    /// and publish onto another plugin's topics, which is far worse than
    /// refusing to start. Loud and missing beats quiet and wrong for identity.
    pub homecore: HomeCoreSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub template: TemplateSection,
}

/// What this plugin sends to homeCore's live log stream.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoggingSection {
    /// Minimum level forwarded over MQTT. An operator can also change this at
    /// runtime from the UI, without restarting the plugin.
    #[serde(default = "default_forward_level")]
    pub forward_level: String,
}

fn default_forward_level() -> String {
    "info".into()
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            forward_level: default_forward_level(),
        }
    }
}

/// The broker connection. Every plugin has this section, spelled this way.
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

/// Your plugin's own settings. Rename the section and replace the fields.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TemplateSection {
    /// The devices this plugin should publish. Empty is the normal state right
    /// after install, and the plugin says so with a notice rather than looking
    /// healthy and doing nothing.
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceEntry {
    /// homeCore device id. Stable — rules and history are keyed to it, so
    /// changing it later orphans both.
    pub id: String,
    pub name: String,
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

    /// The check every shipped plugin runs, and the reason to copy it along
    /// with everything else here.
    ///
    /// The editor renders the descriptor *instead of* deriving a form from the
    /// schema, so a config field the descriptor forgets is not merely
    /// unlabelled — it is unreachable, silently, while the schema still
    /// declares it and the plugin still reads it. Adding a field to the structs
    /// above and forgetting the descriptor is the easiest possible mistake;
    /// this turns it into a failing test instead of a support question.
    #[cfg(feature = "schema")]
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("the schema feature is on in this build"),
            &config_descriptor(),
            &[
                // Bootstrap identity, fixed when core installs the plugin.
                // Editing it at runtime would re-point the plugin at another
                // plugin's topics, so it is deliberately not a form field.
                "homecore.plugin_id",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }

    /// Pins the deliberate decision documented on `Config::homecore`.
    ///
    /// Every shipped plugin defaults this section, because a missing one used
    /// to be fatal while every value in it had a working default. A template
    /// must not: `plugin_id` has no defensible default, and guessing one lets a
    /// new plugin publish onto an existing plugin's topics. If someone "fixes"
    /// this by adding `#[serde(default)]`, this test is what tells them it was
    /// a choice.
    #[test]
    fn a_config_without_an_identity_refuses_to_load() {
        let err = toml::from_str::<Config>("").expect_err("plugin_id has no default");
        assert!(
            err.to_string().contains("homecore"),
            "the error must name the missing section: {err}"
        );
    }

    #[test]
    fn a_minimal_config_loads_with_working_defaults() {
        let cfg: Config =
            toml::from_str("[homecore]\nplugin_id = \"plugin.mine\"\n").expect("should load");
        assert_eq!(cfg.homecore.plugin_id, "plugin.mine");
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.homecore.broker_port, 1883);
        assert_eq!(cfg.logging.forward_level, "info");
        assert!(cfg.template.devices.is_empty());
    }
}
