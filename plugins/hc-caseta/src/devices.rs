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
    /// The last level above zero the bridge reported, so `{"on": true}`
    /// restores it rather than jumping to full (a dimmer) or half (a fan).
    pub last_level: Option<f64>,
}

/// A LIP zone level (0–100, hundredths) as `brightness_pct` / `speed_pct`.
///
/// One decimal, clamped to 0–100, and never `0` while the zone is on: a level
/// of 0.4 % is a lit lamp, and 0 % means off.
pub fn level_pct(level: f64) -> f64 {
    let level = level.clamp(0.0, 100.0);
    let pct = (level * 10.0).round() / 10.0;
    if level > 0.0 {
        pct.max(1.0)
    } else {
        0.0
    }
}

/// The fade a command asks for, in seconds: `transition_ms`, or the older
/// `fade_secs` (still accepted, no longer declared), or the configured default.
pub fn command_fade_secs(cmd: &Value, default_secs: f64) -> f64 {
    if let Some(ms) = cmd.get("transition_ms").and_then(Value::as_f64) {
        return (ms / 1000.0).max(0.0);
    }
    if let Some(secs) = cmd.get("fade_secs").and_then(Value::as_f64) {
        return secs.max(0.0);
    }
    default_secs
}

/// The named fan speed a level falls in — the Maestro table's bands.
pub fn fan_speed(level: f64) -> &'static str {
    match level as u32 {
        0 => "off",
        1..=25 => "low",
        26..=50 => "medium",
        51..=75 => "medium-high",
        _ => "high",
    }
}

/// The level a named fan speed sets, or `None` for a speed the fan lacks.
pub fn fan_speed_level(speed: &str) -> Option<f64> {
    Some(match speed {
        "off" => 0.0,
        "low" => 25.0,
        "medium" => 50.0,
        "medium-high" => 75.0,
        "high" => 100.0,
        _ => return None,
    })
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
            last_level: None,
        })
    }

    /// Note a level the bridge reported; only a level above zero is one
    /// `{"on": true}` can come back to.
    pub fn remember_level(&mut self, level: f64) {
        if level > 0.0 {
            self.last_level = Some(level.min(100.0));
        }
    }

    /// The level `{"on": true}` with no level turns on to: the last one seen,
    /// or full when nothing has been seen since the plugin started.
    pub fn restore_level(&self) -> f64 {
        self.last_level.unwrap_or(100.0)
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
                "brightness_pct": level_pct(level),
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
                Some(
                    serde_json::json!({ "position": (pos.clamp(0.0, 100.0) * 10.0).round() / 10.0 }),
                )
            }
            DeviceKind::FanControl => Some(serde_json::json!({
                "on":    level > 0.0,
                "speed": fan_speed(level),
                "speed_pct": level_pct(level),
            })),
            _ => None,
        }
    }

    /// Translate an occupancy state to a HomeCore state patch.
    pub fn translate_occupancy_state(&self, occupied: bool) -> Value {
        serde_json::json!({ "occupied": occupied })
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore JSON → LIP wire command
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into LIP command strings.
    pub fn translate_command(&self, cmd: &Value, global_fade: f64) -> Vec<String> {
        let fade = command_fade_secs(cmd, self.fade_secs(global_fade));
        let id = self.config.integration_id;

        match self.kind {
            DeviceKind::Dimmer => {
                // `brightness` is not read: its scale (0–255 or 0–100) was
                // guessed from the number, and 50 meant two different levels.
                let level = if let Some(b) = cmd["brightness_pct"].as_f64() {
                    b.clamp(0.0, 100.0)
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on {
                        self.restore_level()
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
                    match fan_speed_level(speed) {
                        Some(level) => level,
                        None => return vec![],
                    }
                } else if let Some(pct) = cmd["speed_pct"].as_f64() {
                    pct.clamp(0.0, 100.0)
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on {
                        self.restore_level()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dev(kind: DeviceKind) -> DeviceEntry {
        DeviceEntry::new(DeviceConfig {
            integration_id: 7,
            name: "Test".into(),
            kind: Some(kind),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
        })
        .expect("a device")
    }

    #[test]
    fn levels_become_percentages_and_on_never_reads_zero() {
        assert_eq!(level_pct(0.0), 0.0);
        assert_eq!(level_pct(0.01), 1.0, "a lit lamp never reads 0 %");
        assert_eq!(level_pct(1.0), 1.0);
        assert_eq!(level_pct(37.46), 37.5);
        assert_eq!(level_pct(100.0), 100.0);
        assert_eq!(level_pct(120.0), 100.0);
        assert_eq!(level_pct(-1.0), 0.0);
    }

    /// Only `brightness_pct` — the 0–255 twin is gone, in both directions.
    #[test]
    fn a_dimmer_publishes_and_takes_one_brightness() {
        let d = dev(DeviceKind::Dimmer);
        assert_eq!(
            d.translate_output_state(25.0).unwrap(),
            json!({"on": true, "brightness_pct": 25.0})
        );
        assert!(d
            .translate_command(&json!({"brightness": 128}), 0.0)
            .is_empty());
        assert_eq!(
            d.translate_command(&json!({"brightness_pct": 40.5}), 0.0),
            vec!["#OUTPUT,7,1,40.50"]
        );
    }

    #[test]
    fn transition_ms_is_the_fade_and_fade_secs_still_works() {
        let d = dev(DeviceKind::Dimmer);
        let set = |cmd: Value| d.translate_command(&cmd, 1.0)[0].clone();
        assert_eq!(
            set(json!({"brightness_pct": 50, "transition_ms": 750})),
            "#OUTPUT,7,1,50.00,0.75"
        );
        assert_eq!(
            set(json!({"brightness_pct": 50, "transition_ms": 0})),
            "#OUTPUT,7,1,50.00"
        );
        assert_eq!(
            set(json!({"brightness_pct": 50, "fade_secs": 2})),
            "#OUTPUT,7,1,50.00,2"
        );
        assert_eq!(set(json!({"brightness_pct": 50})), "#OUTPUT,7,1,50.00,1");
    }

    /// Rule 5: `{"on": true}` comes back to the last level — a dimmer used to
    /// jump to 100 % and a fan to 50 %.
    #[test]
    fn on_restores_the_last_level() {
        let on = json!({"on": true});
        let mut d = dev(DeviceKind::Dimmer);
        assert_eq!(d.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,100.00"]);
        d.remember_level(35.0);
        d.remember_level(0.0);
        assert_eq!(d.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,35.00"]);

        let mut f = dev(DeviceKind::FanControl);
        assert_eq!(f.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,100.00"]);
        f.remember_level(25.0);
        assert_eq!(f.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,25.00"]);
    }

    #[test]
    fn fan_bands() {
        assert_eq!(fan_speed(0.0), "off");
        assert_eq!(fan_speed(25.0), "low");
        assert_eq!(fan_speed(50.0), "medium");
        assert_eq!(fan_speed(51.0), "medium-high");
        assert_eq!(fan_speed(100.0), "high");
        assert_eq!(fan_speed_level("medium-high"), Some(75.0));
        assert_eq!(fan_speed_level("turbo"), None);
    }

    #[test]
    fn occupancy_is_published_once_as_occupied() {
        assert_eq!(
            dev(DeviceKind::OccupancySensor).translate_occupancy_state(false),
            json!({"occupied": false})
        );
    }
}
