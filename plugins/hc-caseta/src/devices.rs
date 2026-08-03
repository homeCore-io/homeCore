//! Device registry and state/command translation for Caseta Pro.

use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind, SceneConfig};
use crate::lip::protocol::{cmd_set_level, cmd_shade_action};

// ---------------------------------------------------------------------------
// DeviceEntry
// ---------------------------------------------------------------------------

pub struct DeviceEntry {
    pub config: DeviceConfig,
    /// `caseta_{integration_id}`
    pub hc_id: String,
    /// Resolved at construction — an entry only exists for a configured kind.
    pub kind: DeviceKind,
}

impl DeviceEntry {
    /// `None` when the row has no `kind` yet — an imported device the operator
    /// has not classified. The caller reports it rather than failing outright.
    pub fn new(config: DeviceConfig) -> Option<Self> {
        let kind = config.kind.clone()?;
        let hc_id = format!("caseta_{}", config.integration_id);
        Some(Self {
            config,
            hc_id,
            kind,
        })
    }

    /// HomeCore device_type string for registration.
    pub fn homecore_device_type(&self) -> &str {
        match self.kind {
            DeviceKind::Dimmer => "light",
            DeviceKind::Switch => "switch",
            DeviceKind::Shade => "cover",
            DeviceKind::FanControl => "fan",
            DeviceKind::Pico => "pico_remote",
            DeviceKind::OccupancySensor => "occupancy_sensor",
        }
    }

    /// Whether this device is an OUTPUT that can be level-queried on connect.
    pub fn is_output(&self) -> bool {
        matches!(
            self.kind,
            DeviceKind::Dimmer | DeviceKind::Switch | DeviceKind::Shade | DeviceKind::FanControl
        )
    }

    /// Whether this device is an occupancy group.
    pub fn is_group(&self) -> bool {
        matches!(self.kind, DeviceKind::OccupancySensor)
    }

    /// Whether this device emits button events (Pico remote).
    pub fn is_button_device(&self) -> bool {
        matches!(self.kind, DeviceKind::Pico)
    }

    /// Effective fade time: per-device override or global default.
    pub fn fade_secs(&self, global: f64) -> f64 {
        self.config.fade_secs.unwrap_or(global)
    }

    // -----------------------------------------------------------------------
    // State translation: LIP level → HomeCore JSON
    // -----------------------------------------------------------------------

    /// Translate a LIP output level (0.0–100.0) to a HomeCore state patch.
    pub fn translate_output_state(&self, level: f64) -> Option<Value> {
        match self.kind {
            DeviceKind::Dimmer => Some(serde_json::json!({
                "on":             level > 0.0,
                "brightness_pct": (level * 10.0).round() / 10.0,
                "brightness":     ((level / 100.0) * 255.0).round() as i64,
            })),
            DeviceKind::Switch => Some(serde_json::json!({
                "on": level > 0.0,
            })),
            DeviceKind::Shade => {
                let pos = if self.config.invert_position {
                    100.0 - level
                } else {
                    level
                };
                Some(serde_json::json!({ "position": (pos * 10.0).round() / 10.0 }))
            }
            DeviceKind::FanControl => {
                let speed = match level as u32 {
                    0 => "off",
                    1..=25 => "low",
                    26..=50 => "medium",
                    51..=75 => "medium-high",
                    _ => "high",
                };
                Some(serde_json::json!({
                    "on":    level > 0.0,
                    "speed": speed,
                    "speed_pct": (level * 10.0).round() / 10.0,
                }))
            }
            _ => None,
        }
    }

    /// Translate an occupancy state to a HomeCore state patch.
    pub fn translate_occupancy_state(&self, occupied: bool) -> Value {
        serde_json::json!({
            "occupied": occupied,
            "occupancy": occupied,
        })
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore JSON → LIP wire command
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into LIP command strings.
    pub fn translate_command(&self, cmd: &Value, global_fade: f64) -> Vec<String> {
        let fade = cmd["fade_secs"]
            .as_f64()
            .unwrap_or_else(|| self.fade_secs(global_fade));
        let id = self.config.integration_id;

        match self.kind {
            DeviceKind::Dimmer => {
                let level = if let Some(b) = cmd["brightness_pct"].as_f64() {
                    b.clamp(0.0, 100.0)
                } else if let Some(b) = cmd["brightness"].as_f64() {
                    if b > 100.0 {
                        ((b / 255.0) * 100.0).clamp(0.0, 100.0)
                    } else {
                        b.clamp(0.0, 100.0)
                    }
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on {
                        100.0
                    } else {
                        0.0
                    }
                } else {
                    return vec![];
                };
                vec![cmd_set_level(id, level, fade)]
            }

            DeviceKind::Switch => {
                let level = match cmd["on"].as_bool() {
                    Some(true) => 100.0,
                    Some(false) => 0.0,
                    None => return vec![],
                };
                vec![cmd_set_level(id, level, 0.0)]
            }

            DeviceKind::Shade => {
                if let Some(pos) = cmd["position"].as_f64() {
                    let level = if self.config.invert_position {
                        100.0 - pos
                    } else {
                        pos
                    };
                    vec![cmd_set_level(id, level.clamp(0.0, 100.0), 0.0)]
                } else if cmd["raise"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 2)]
                } else if cmd["lower"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 3)]
                } else if cmd["stop"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 4)]
                } else {
                    vec![]
                }
            }

            DeviceKind::FanControl => {
                let level = if let Some(speed) = cmd["speed"].as_str() {
                    match speed {
                        "off" => 0.0,
                        "low" => 25.0,
                        "medium" => 50.0,
                        "medium-high" => 75.0,
                        "high" => 100.0,
                        _ => return vec![],
                    }
                } else if let Some(pct) = cmd["speed_pct"].as_f64() {
                    pct.clamp(0.0, 100.0)
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on {
                        50.0
                    } else {
                        0.0
                    }
                } else {
                    return vec![];
                };
                vec![cmd_set_level(id, level, 0.0)]
            }

            // Pico and OccupancySensor are read-only.
            DeviceKind::Pico | DeviceKind::OccupancySensor => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SceneEntry
// ---------------------------------------------------------------------------

/// A phantom button on the Smart Bridge, published to homeCore as a scene.
pub struct SceneEntry {
    pub config: SceneConfig,
    /// `caseta_scene_{name_slug}`
    pub hc_id: String,
}

impl SceneEntry {
    pub fn new(config: SceneConfig) -> Self {
        let hc_id = config.hc_id();
        Self { config, hc_id }
    }
}
