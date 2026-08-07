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

#[derive(Debug, Deserialize)]
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
pub struct TemplateSection {
    /// The devices this plugin should publish. Empty is the normal state right
    /// after install, and the plugin says so with a notice rather than looking
    /// healthy and doing nothing.
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
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
