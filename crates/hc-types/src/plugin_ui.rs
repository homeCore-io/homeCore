//! A plugin's own screens — the `ui` section of its capability manifest.
//!
//! A plugin can ship web components with itself: a page for itself, a panel
//! for its devices. The plugin carries its UI — a built folder beside its
//! program, in whatever package installs it — and serves the files itself,
//! over the management channel; homeCore passes them through to the app
//! (`GET /plugins/{id}/ui/{build}/{file}`) and keeps none. A screen is
//! therefore always the one the running program carries.
//!
//! Carried alongside the frozen `Capabilities` type, like `alert_kinds`.

use serde::{Deserialize, Serialize};

/// The version of the app's `host` interface this UI was built against. The
/// app loads a UI only when it offers that version.
pub const HOST_API: u32 = 1;

/// What a plugin's UI is and where it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUi {
    /// The `host` interface version it was built against.
    pub host_api: u32,
    /// The plugin version it came with — the plugin's own.
    pub version: String,
    /// A hash of the UI's files, so each build has its own address and can
    /// be cached for good: a rebuilt UI is a new `build`, even under the same
    /// version. Set by the SDK from the folder it serves.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build: String,
    /// The module to load, relative to the bundle's root.
    #[serde(default = "index_js")]
    pub entry: String,
    /// Where its components go.
    #[serde(default)]
    pub slots: Vec<UiSlot>,
}

fn index_js() -> String {
    "index.js".into()
}

/// One place in the app a plugin's component fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSlot {
    /// `plugin.page` (its page, in place of the generic action list) or
    /// `device.panel` (its devices' panel, in place of the generic controls).
    pub slot: String,
    /// The custom element the module defines for it (`hc-ecowitt-page`).
    pub element: String,
    /// For `device.panel`: only devices of these types; every one of its
    /// devices when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_types: Vec<String>,
}

/// The slots the app knows.
pub const SLOTS: &[&str] = &["plugin.page", "device.panel"];

impl PluginUi {
    /// Whether it can be offered at all: a version, a module inside the
    /// bundle, and at least one slot the app knows, each naming a valid
    /// custom element. Slots that don't pass are dropped; `None` when none
    /// is left.
    pub fn validated(mut self) -> Option<Self> {
        if self.version.trim().is_empty()
            || !relative_path(&self.entry)
            || !self.build.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        self.slots
            .retain(|s| SLOTS.contains(&s.slot.as_str()) && custom_element(&s.element));
        (!self.slots.is_empty()).then_some(self)
    }
}

/// A custom element name: lowercase, starting with a letter, with a hyphen.
fn custom_element(name: &str) -> bool {
    name.contains('-')
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.' || c == '_'
        })
}

/// A path that stays inside the bundle.
pub fn relative_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && p.split('/').all(|seg| !seg.is_empty() && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_ui_keeps_the_slots_the_app_knows_and_drops_the_rest() {
        let ui: PluginUi = serde_json::from_value(json!({
            "host_api": 1,
            "version": "0.1.18",
            "slots": [
                {"slot": "plugin.page", "element": "hc-ecowitt-page"},
                {"slot": "device.panel", "element": "hc-ecowitt-station", "device_types": ["weather_station"]},
                {"slot": "dashboard.card", "element": "hc-ecowitt-card"},
                {"slot": "plugin.page", "element": "NoHyphen"}
            ]
        }))
        .unwrap();
        assert_eq!(ui.entry, "index.js");
        let ui = ui.validated().unwrap();
        assert_eq!(ui.slots.len(), 2);
        assert_eq!(ui.slots[1].device_types, vec!["weather_station"]);
    }

    #[test]
    fn a_ui_with_nowhere_to_go_or_a_module_outside_its_bundle_is_none() {
        let base = |entry: &str, slots: serde_json::Value| -> Option<PluginUi> {
            serde_json::from_value::<PluginUi>(json!({
                "host_api": 1, "version": "1.0.0", "entry": entry, "slots": slots
            }))
            .unwrap()
            .validated()
        };
        let page = json!([{"slot": "plugin.page", "element": "hc-x-page"}]);
        assert!(base("index.js", page.clone()).is_some());
        assert!(base("../index.js", page.clone()).is_none());
        assert!(base("/etc/index.js", page.clone()).is_none());
        assert!(base("index.js", json!([])).is_none());
    }
}
