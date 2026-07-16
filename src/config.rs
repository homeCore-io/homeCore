use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hue::models::{BridgeTarget, DiscoveredBridge};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HuePluginConfig {
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub hue: HueConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
    #[serde(default)]
    pub bridges: Vec<BridgeConfig>,
}

/// JSON Schema of the operator config, published to core on the capability
/// manifest so the config editor can render a typed form. `None` when built
/// without the `schema` feature.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(HuePluginConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

impl HuePluginConfig {
    pub fn load(path: &str) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading config from {path}"))?;
        toml::from_str(&text).context("parsing config TOML")
    }

    // Paired-bridge app_keys are no longer written back to the config file
    // (D8): they're plugin-learned secrets that live in core's learned state.
    // See `build_bridge_state_delta` + `pairing::persist_app_key`.

    pub fn effective_bridges(&self, discovered: &[DiscoveredBridge]) -> Vec<BridgeTarget> {
        if !self.bridges.is_empty() {
            let resolved = self
                .bridges
                .iter()
                .filter_map(|cfg| cfg.to_target(discovered))
                .collect::<Vec<_>>();

            // If explicit bridge entries were provided but none could be resolved,
            // fall back to discovered bridges so startup can proceed.
            if !resolved.is_empty() {
                return resolved;
            }
        }

        discovered
            .iter()
            .map(|d| BridgeTarget {
                name: d.name.clone(),
                bridge_id: d.bridge_id.clone(),
                host: d.host.clone(),
                app_key: None,
                verify_tls: true,
                allow_self_signed: true,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_broker_host(),
            broker_port: default_broker_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HueConfig {
    #[serde(default = "default_true")]
    pub discovery_enabled: bool,
    #[serde(default = "default_true")]
    pub discovery_cloud_fallback: bool,
    #[serde(default = "default_discovery_timeout_secs")]
    pub discovery_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub eventstream_enabled: bool,
    #[serde(default = "default_eventstream_reconnect_secs")]
    pub eventstream_reconnect_secs: u64,
    #[serde(default = "default_resync_interval_secs")]
    pub resync_interval_secs: u64,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default = "default_true")]
    pub compact_motion_facets: bool,
    #[serde(default)]
    pub publish_grouped_lights: bool,
    #[serde(default)]
    pub publish_grouped_lights_for: Vec<String>,
    #[serde(default)]
    pub skip_grouped_lights_for: Vec<String>,
    #[serde(default)]
    pub publish_bridge_home: bool,
    #[serde(default)]
    pub publish_entertainment_configurations: bool,
    #[serde(default)]
    pub display: HueDisplayConfig,
}

impl Default for HueConfig {
    fn default() -> Self {
        Self {
            discovery_enabled: default_true(),
            discovery_cloud_fallback: default_true(),
            discovery_timeout_secs: default_discovery_timeout_secs(),
            eventstream_enabled: default_true(),
            eventstream_reconnect_secs: default_eventstream_reconnect_secs(),
            resync_interval_secs: default_resync_interval_secs(),
            heartbeat_secs: default_heartbeat_secs(),
            compact_motion_facets: default_true(),
            publish_grouped_lights: false,
            publish_grouped_lights_for: Vec::new(),
            skip_grouped_lights_for: Vec::new(),
            publish_bridge_home: false,
            publish_entertainment_configurations: false,
            display: HueDisplayConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HueDisplayConfig {
    #[serde(default)]
    pub temperature_unit: TemperatureUnit,
    #[serde(default)]
    pub illuminance_display: IlluminanceDisplay,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum TemperatureUnit {
    #[default]
    C,
    F,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum IlluminanceDisplay {
    #[default]
    Lux,
    Raw,
}

/// Merge learned-state bridge records (`{ "bridges": { "<id>": {app_key, host,
/// name} } }`, owned by core) into the effective target list: fill/override
/// `app_key` for bridges already targeted, and append learned bridges not present
/// (paired in a prior session). A config-provided `app_key` remains the fallback
/// when learned state has none. This is the D8 read side — learned secrets live in
/// core, operator inventory in the config file.
pub fn apply_learned_bridges(targets: &mut Vec<BridgeTarget>, learned: &serde_json::Value) {
    let Some(bridges) = learned.get("bridges").and_then(|b| b.as_object()) else {
        return;
    };
    // Hue reports bridge_id in a different case than config often stores it
    // (uppercase from the API vs lowercase in config.toml), so match
    // case-insensitively — same as `pairing::is_already_configured`.
    for t in targets.iter_mut() {
        if let Some(key) = bridges
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(&t.bridge_id))
            .and_then(|(_, r)| r.get("app_key"))
            .and_then(|v| v.as_str())
            .filter(|k| !k.is_empty())
        {
            t.app_key = Some(key.to_string());
        }
    }
    for (bridge_id, rec) in bridges {
        if targets
            .iter()
            .any(|t| t.bridge_id.eq_ignore_ascii_case(bridge_id))
        {
            continue;
        }
        let host = rec.get("host").and_then(|v| v.as_str()).unwrap_or("");
        if host.is_empty() {
            continue;
        }
        targets.push(BridgeTarget {
            name: rec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(bridge_id)
                .to_string(),
            bridge_id: bridge_id.clone(),
            host: host.to_string(),
            app_key: rec
                .get("app_key")
                .and_then(|v| v.as_str())
                .filter(|k| !k.is_empty())
                .map(String::from),
            verify_tls: true,
            allow_self_signed: true,
        });
    }
}

/// Whether two bridge identities refer to the same physical bridge: same
/// `bridge_id` (case-insensitive — Hue's API reports uppercase, config stores
/// lowercase) or the same non-empty host. Used to dedup a re-paired bridge
/// against the ones already running, without collapsing distinct bridges (which
/// differ in both id and host), so multiple bridges stay supported.
pub fn same_bridge(id_a: &str, host_a: &str, id_b: &str, host_b: &str) -> bool {
    (!id_a.is_empty() && id_a.eq_ignore_ascii_case(id_b))
        || (!host_a.is_empty() && host_a == host_b)
}

/// Build the learned-state write for a newly-paired bridge. Returns the full
/// `{ "bridges": { ... } }` document (core's merge is shallow at the top level,
/// so we send the whole `bridges` map to avoid dropping sibling bridges).
pub fn build_bridge_state_delta(
    current: &serde_json::Value,
    target: &BridgeTarget,
    app_key: &str,
) -> serde_json::Value {
    let mut bridges = current
        .get("bridges")
        .and_then(|b| b.as_object().cloned())
        .unwrap_or_default();
    bridges.insert(
        target.bridge_id.clone(),
        serde_json::json!({
            "app_key": app_key,
            "host": target.host,
            "name": target.name,
        }),
    );
    serde_json::json!({ "bridges": bridges })
}

/// Build the learned-state write that FORGETS a bridge: the full `bridges`
/// map minus `bridge_id` (matched case-insensitively — Hue reports uppercase,
/// config stores lowercase). Core's top-level merge replaces the `bridges` key,
/// so sending the whole (smaller) map drops just this bridge without disturbing
/// its siblings. The mirror of [`build_bridge_state_delta`], used by unpair to
/// clear a removed bridge's stored `app_key` so a restart can't resurrect it.
pub fn build_bridge_state_removal(
    current: &serde_json::Value,
    bridge_id: &str,
) -> serde_json::Value {
    let mut bridges = current
        .get("bridges")
        .and_then(|b| b.as_object().cloned())
        .unwrap_or_default();
    bridges.retain(|id, _| !id.eq_ignore_ascii_case(bridge_id));
    serde_json::json!({ "bridges": bridges })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BridgeConfig {
    pub name: String,
    #[serde(default)]
    pub bridge_id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub app_key: String,
    #[serde(default = "default_true")]
    pub verify_tls: bool,
    #[serde(default = "default_true")]
    pub allow_self_signed: bool,
}

impl BridgeConfig {
    fn to_target(&self, discovered: &[DiscoveredBridge]) -> Option<BridgeTarget> {
        let mut host = self.host.clone();
        let mut bridge_id = self.bridge_id.clone();

        if host.is_empty() || bridge_id.is_empty() {
            if let Some(found) = discovered.iter().find(|d| {
                (!self.bridge_id.is_empty() && d.bridge_id == self.bridge_id)
                    || (!self.host.is_empty() && d.host == self.host)
                    || d.name.eq_ignore_ascii_case(&self.name)
            }) {
                if host.is_empty() {
                    host = found.host.clone();
                }
                if bridge_id.is_empty() {
                    bridge_id = found.bridge_id.clone();
                }
            }
        }

        if host.is_empty() {
            return None;
        }

        if bridge_id.is_empty() {
            bridge_id = host.replace('.', "_");
        }

        Some(BridgeTarget {
            name: self.name.clone(),
            bridge_id,
            host,
            app_key: if self.app_key.trim().is_empty() {
                None
            } else {
                Some(self.app_key.clone())
            },
            verify_tls: self.verify_tls,
            allow_self_signed: self.allow_self_signed,
        })
    }
}

fn default_broker_host() -> String {
    "127.0.0.1".into()
}

fn default_broker_port() -> u16 {
    1883
}

fn default_plugin_id() -> String {
    "plugin.hue".into()
}

fn default_true() -> bool {
    true
}

fn default_resync_interval_secs() -> u64 {
    60
}

fn default_heartbeat_secs() -> u64 {
    30
}

fn default_discovery_timeout_secs() -> u64 {
    5
}

fn default_eventstream_reconnect_secs() -> u64 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_bridge_state_removal_drops_only_the_named_bridge() {
        let current = serde_json::json!({
            "bridges": {
                // Stored uppercase as the Hue API reports it.
                "001788FFFE6841B3": { "app_key": "k1", "host": "10.0.0.1", "name": "a" },
                "ecb5fafe112233": { "app_key": "k2", "host": "10.0.0.2", "name": "b" },
            }
        });

        // Request lowercase — must still match and remove the uppercase entry.
        let out = build_bridge_state_removal(&current, "001788fffe6841b3");
        let bridges = out.get("bridges").unwrap().as_object().unwrap();

        assert_eq!(bridges.len(), 1);
        assert!(bridges.contains_key("ecb5fafe112233"));
        assert!(!bridges
            .keys()
            .any(|k| k.eq_ignore_ascii_case("001788fffe6841b3")));
    }

    #[test]
    fn falls_back_to_discovered_when_configured_bridges_unresolved() {
        let cfg = HuePluginConfig {
            bridges: vec![BridgeConfig {
                name: "main".to_string(),
                bridge_id: String::new(),
                host: String::new(),
                app_key: String::new(),
                verify_tls: true,
                allow_self_signed: true,
            }],
            ..Default::default()
        };

        let discovered = vec![DiscoveredBridge {
            name: "Hue Bridge".to_string(),
            bridge_id: "bridge-1".to_string(),
            host: "10.0.0.10".to_string(),
        }];

        let effective = cfg.effective_bridges(&discovered);
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].host, "10.0.0.10");
        assert_eq!(effective[0].bridge_id, "bridge-1");
    }

    #[test]
    fn apply_learned_bridges_fills_key_and_appends_missing() {
        use crate::hue::models::BridgeTarget;
        let mut targets = vec![BridgeTarget {
            name: "main".into(),
            bridge_id: "bridge-1".into(),
            host: "10.0.0.10".into(),
            app_key: None, // no config key
            verify_tls: true,
            allow_self_signed: true,
        }];
        let learned = serde_json::json!({
            "bridges": {
                // Uppercase id (as the Hue API reports it) vs lowercase target —
                // must still match and fill the key, not append a duplicate.
                "BRIDGE-1": { "app_key": "K1", "host": "10.0.0.10", "name": "main" },
                "bridge-2": { "app_key": "K2", "host": "10.0.0.20", "name": "spare" }
            }
        });
        apply_learned_bridges(&mut targets, &learned);
        // Existing target gets its learned key (case-insensitive match).
        assert_eq!(targets[0].app_key.as_deref(), Some("K1"));
        // A prior-session bridge is appended from learned state.
        assert_eq!(targets.len(), 2);
        let spare = targets.iter().find(|t| t.bridge_id == "bridge-2").unwrap();
        assert_eq!(spare.host, "10.0.0.20");
        assert_eq!(spare.app_key.as_deref(), Some("K2"));
    }

    #[test]
    fn same_bridge_matches_case_insensitive_id_or_host_but_not_distinct() {
        // Re-pair: same physical bridge, id differs only in case.
        assert!(same_bridge(
            "001788FFFE6841B3",
            "10.0.10.23",
            "001788fffe6841b3",
            "10.0.10.23"
        ));
        // Same host, empty/unknown id on one side.
        assert!(same_bridge("", "10.0.10.23", "abc", "10.0.10.23"));
        // Distinct bridges (different id AND host) must NOT collapse —
        // multi-bridge support depends on this.
        assert!(!same_bridge(
            "bridge-a",
            "10.0.10.23",
            "bridge-b",
            "10.0.10.99"
        ));
        // Empty on both sides for a field never matches on that field alone.
        assert!(!same_bridge("", "", "x", "y"));
    }

    #[test]
    fn build_bridge_state_delta_preserves_sibling_bridges() {
        use crate::hue::models::BridgeTarget;
        let current = serde_json::json!({ "bridges": { "b1": { "app_key": "K1" } } });
        let target = BridgeTarget {
            name: "two".into(),
            bridge_id: "b2".into(),
            host: "10.0.0.20".into(),
            app_key: None,
            verify_tls: true,
            allow_self_signed: true,
        };
        let delta = build_bridge_state_delta(&current, &target, "K2");
        // Full bridges map (core merges shallow) — b1 kept, b2 added.
        assert_eq!(delta["bridges"]["b1"]["app_key"], "K1");
        assert_eq!(delta["bridges"]["b2"]["app_key"], "K2");
        assert_eq!(delta["bridges"]["b2"]["host"], "10.0.0.20");
    }

    #[cfg(feature = "schema")]
    #[test]
    fn config_schema_describes_operator_fields() {
        let schema = config_schema().expect("schema built with the schema feature");
        // A JSON Schema object with the top-level config sections as properties.
        let props = &schema["properties"];
        assert!(props.get("hue").is_some());
        assert!(props.get("bridges").is_some());
        assert!(props.get("homecore").is_some());
    }

    #[test]
    fn defaults_display_preferences() {
        let cfg = HuePluginConfig::default();
        assert_eq!(cfg.hue.display.temperature_unit, TemperatureUnit::C);
        assert_eq!(cfg.hue.display.illuminance_display, IlluminanceDisplay::Lux);
    }

    #[test]
    fn parses_display_preferences_from_toml() {
        let text = r#"
[hue]
[hue.display]
temperature_unit = "f"
illuminance_display = "raw"
"#;

        let cfg: HuePluginConfig = toml::from_str(text).expect("parse display config");
        assert_eq!(cfg.hue.display.temperature_unit, TemperatureUnit::F);
        assert_eq!(cfg.hue.display.illuminance_display, IlluminanceDisplay::Raw);
    }

    #[test]
    fn parses_group_publish_selectors() {
        let text = r#"
[hue]
publish_grouped_lights = false
publish_grouped_lights_for = ["Kitchen", "zone:downstairs"]
skip_grouped_lights_for = ["room:garage"]
"#;

        let cfg: HuePluginConfig = toml::from_str(text).expect("parse grouped-light config");
        assert_eq!(
            cfg.hue.publish_grouped_lights_for,
            vec!["Kitchen".to_string(), "zone:downstairs".to_string()]
        );
        assert_eq!(
            cfg.hue.skip_grouped_lights_for,
            vec!["room:garage".to_string()]
        );
    }
}
