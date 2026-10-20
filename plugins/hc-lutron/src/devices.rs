//! Device registry and state/command translation.

use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind, SceneConfig, TimeclockConfig};
use crate::lip::protocol::{
    cmd_device_led, cmd_pulse_output, cmd_set_level, cmd_shade_action, led_component_for_button,
};

// ---------------------------------------------------------------------------
// DeviceEntry
// ---------------------------------------------------------------------------

pub struct DeviceEntry {
    pub config: DeviceConfig,
    /// `lutron_{integration_id}`
    pub hc_id: String,
    /// The last level above zero the repeater reported, so `{"on": true}`
    /// restores it rather than jumping to full. LIP has no "on at last level"
    /// for a zone; the plugin is the only place that can remember it.
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

impl DeviceEntry {
    pub fn new(config: DeviceConfig) -> Self {
        let hc_id = format!("lutron_{}", config.integration_id);
        Self {
            config,
            hc_id,
            last_level: None,
        }
    }

    /// Note a level the repeater reported; only a level above zero is one
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

    /// HomeCore device_type string passed to the registration payload.
    pub fn homecore_device_type(&self) -> &str {
        match self.config.kind {
            DeviceKind::Dimmer => "light",
            DeviceKind::Switch => "switch",
            DeviceKind::Shade => "cover",
            DeviceKind::Keypad => "keypad",
            DeviceKind::Pico => "pico_remote",
            DeviceKind::OccupancyGroup => "occupancy_sensor",
            DeviceKind::Vcrx => "vcrx",
            DeviceKind::FanControl => "fan",
            // A momentary trigger, not a switch: it accepts `activate` and
            // never latches, which is what a scene already means here.
            DeviceKind::CcoPulsed => "scene",
        }
    }

    /// Whether this device is an OUTPUT that can be level-queried on connect.
    ///
    /// `CcoPulsed` is deliberately absent: the Integration Guide states plainly
    /// that "momentary outputs should not be queried", and a pulse has no
    /// resting level to report.
    pub fn is_output(&self) -> bool {
        matches!(
            self.config.kind,
            DeviceKind::Dimmer | DeviceKind::Switch | DeviceKind::Shade | DeviceKind::FanControl
        )
    }

    /// Whether this device is a GROUP (occupancy) that can be queried.
    pub fn is_group(&self) -> bool {
        matches!(self.config.kind, DeviceKind::OccupancyGroup)
    }

    /// Whether this device emits button press/release/hold events (Keypad, Pico, or VCRX).
    pub fn is_button_device(&self) -> bool {
        matches!(
            self.config.kind,
            DeviceKind::Keypad | DeviceKind::Pico | DeviceKind::Vcrx
        )
    }

    /// Whether this device has CCI (Contact Closure Input) components.
    #[allow(dead_code)]
    pub fn has_ccis(&self) -> bool {
        !self.config.ccis.is_empty()
    }

    /// CCI component numbers configured for this device.
    #[allow(dead_code)]
    pub fn cci_components(&self) -> &[u32] {
        &self.config.ccis
    }

    /// Whether a component number is a CCI on this device.
    pub fn is_cci_component(&self, component: u32) -> bool {
        self.config.ccis.contains(&component)
    }

    /// Button component numbers configured for this keypad (used to query LED state on connect).
    pub fn button_components(&self) -> &[u32] {
        &self.config.buttons
    }

    /// Effective fade time: per-device override or global default.
    pub fn fade_secs(&self, global: f64) -> f64 {
        self.config.fade_secs.unwrap_or(global)
    }

    // -----------------------------------------------------------------------
    // State translation: LIP level/state → HomeCore JSON
    // -----------------------------------------------------------------------

    /// Translate a LIP output level (0.0–100.0) to a HomeCore state patch.
    pub fn translate_output_state(&self, level: f64) -> Option<Value> {
        match self.config.kind {
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
            // Bands per the Integration Guide's Maestro Fan Speed Control
            // table. It prints Medium High as 56-75, leaving 51-55 undefined —
            // almost certainly a typo, so 51 is treated as medium-high here,
            // matching hc-caseta.
            DeviceKind::FanControl => {
                let speed = match level as u32 {
                    0 => "off",
                    1..=25 => "low",
                    26..=50 => "medium",
                    51..=75 => "medium-high",
                    _ => "high",
                };
                Some(serde_json::json!({
                    "on":        level > 0.0,
                    "speed":     speed,
                    "speed_pct": level_pct(level),
                }))
            }
            _ => None,
        }
    }

    /// Translate an occupancy state to a HomeCore state patch.
    pub fn translate_occupancy_state(&self, occupied: bool) -> Value {
        serde_json::json!({ "occupied": occupied })
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore JSON → LIP wire command string
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into one or more LIP command strings.
    /// Returns an empty Vec if the command is not applicable to this device type.
    pub fn translate_command(&self, cmd: &Value, global_fade: f64) -> Vec<String> {
        let fade = command_fade_secs(cmd, self.fade_secs(global_fade));
        let id = self.config.integration_id;

        match self.config.kind {
            // Discrete speeds ride the same zone-level command a dimmer uses;
            // the guide's own example sets Medium High with `#OUTPUT,1,1,75`.
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
                        self.restore_level()
                    } else {
                        0.0
                    }
                } else {
                    return vec![];
                };
                vec![cmd_set_level(id, level, fade)]
            }
            // Momentary: one pulse, no level, nothing to turn back off.
            DeviceKind::CcoPulsed => {
                if cmd["activate"].as_bool() == Some(true) || cmd["on"].as_bool() == Some(true) {
                    vec![cmd_pulse_output(id)]
                } else {
                    vec![]
                }
            }
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

            DeviceKind::Keypad => {
                // set_led: {"set_led": {"button": 3, "state": 1}}
                // state: 0=off, 1=on, 2=normal-flash (1 Hz), 3=rapid-flash (10 Hz)
                // LED component = button + 80 (Lutron Integration Guide, universal offset)
                if let Some(set_led) = cmd.get("set_led") {
                    let button = set_led["button"].as_u64().unwrap_or(0) as u32;
                    let state = set_led["state"].as_u64().unwrap_or(0).min(3) as u8;
                    if button > 0 {
                        return vec![cmd_device_led(id, led_component_for_button(button), state)];
                    }
                }
                // press_button requires an async press+release with a delay and is handled
                // in bridge.rs (same pattern as phantom button scene activation).
                vec![]
            }

            DeviceKind::Vcrx => {
                // VCRX supports set_led (same as Keypad, +80 offset).
                // press_button is handled in bridge.rs.
                // CCIs are read-only inputs — no outbound commands.
                if let Some(set_led) = cmd.get("set_led") {
                    let button = set_led["button"].as_u64().unwrap_or(0) as u32;
                    let state = set_led["state"].as_u64().unwrap_or(0).min(3) as u8;
                    if button > 0 {
                        return vec![cmd_device_led(id, led_component_for_button(button), state)];
                    }
                }
                vec![]
            }

            // Pico is truly read-only — no commands accepted.
            // OccupancyGroup is read-only.
            DeviceKind::Pico | DeviceKind::OccupancyGroup => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SceneEntry
// ---------------------------------------------------------------------------

pub struct SceneEntry {
    pub config: SceneConfig,
    pub hc_id: String,
    /// Whether this scene's phantom button has an LED that reports back.
    ///
    /// `None` until the repeater answers the startup LED query. A phantom
    /// button on the main repeater normally has an LED — the ones that do not
    /// are scenes tied to a Pico, which has no LEDs at all — so unknown is
    /// treated as reporting, and only an explicit 255 ("no LED assigned")
    /// turns it into `Some(false)`.
    ///
    /// Assuming the other way round meant every scene in the house declared no
    /// status for the second between connecting and the query coming back, on
    /// every reconnect.
    pub reports_state: Option<bool>,
    /// What the last published schema claimed, so a schema is republished when
    /// the answer changes it and not on every LED event.
    pub declared_status: bool,
}

impl SceneEntry {
    pub fn new(config: SceneConfig) -> Self {
        let hc_id = config.hc_id();
        Self {
            config,
            hc_id,
            reports_state: None,
            declared_status: true,
        }
    }

    /// Whether this scene should declare a readable `on`.
    ///
    /// Unknown counts as yes: see [`Self::reports_state`].
    pub fn declares_status(&self) -> bool {
        self.reports_state != Some(false)
    }
}

// ---------------------------------------------------------------------------
// TimeclockEntry
// ---------------------------------------------------------------------------

pub struct TimeclockEntry {
    pub config: TimeclockConfig,
    pub hc_id: String,
}

impl TimeclockEntry {
    pub fn new(config: TimeclockConfig) -> Self {
        let hc_id = config.hc_id();
        Self { config, hc_id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phantom button on the main repeater normally has an LED. The
    /// exception is a scene tied to a Pico, which has none — and the repeater
    /// says so with 255. So unknown means reporting, and only that answer
    /// retires the scene's `on`.
    #[test]
    fn a_scene_is_assumed_to_report_until_the_repeater_says_otherwise() {
        let mut scene = SceneEntry::new(SceneConfig {
            name: "Deck On".into(),
            main_repeater_id: 1,
            button_component: 5,
        });
        assert!(scene.declares_status(), "unknown is not a denial");

        scene.reports_state = Some(false); // the repeater answered 255
        assert!(!scene.declares_status());

        scene.reports_state = Some(true);
        assert!(scene.declares_status());
    }

    use crate::config::DeviceConfig;

    fn dev(kind: DeviceKind) -> DeviceEntry {
        DeviceEntry::new(DeviceConfig {
            integration_id: 7,
            name: "Test".into(),
            kind,
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
            all_buttons: vec![],
            button_names: vec![],
            ccis: vec![],
        })
    }

    /// Bands from the Integration Guide's Maestro Fan Speed Control table.
    #[test]
    fn fan_levels_map_to_named_speeds() {
        let f = dev(DeviceKind::FanControl);
        let speed = |lvl: f64| f.translate_output_state(lvl).unwrap()["speed"].clone();
        assert_eq!(speed(0.0), "off");
        assert_eq!(speed(25.0), "low");
        assert_eq!(speed(50.0), "medium");
        assert_eq!(speed(75.0), "medium-high");
        assert_eq!(speed(100.0), "high");
    }

    #[test]
    fn fan_speed_commands_use_the_guides_levels() {
        let f = dev(DeviceKind::FanControl);
        // The guide's own worked example: Medium High is #OUTPUT,<id>,1,75.
        let cmds = f.translate_command(&serde_json::json!({"speed": "medium-high"}), 0.0);
        assert!(cmds[0].starts_with("#OUTPUT,7,1,75"), "got {:?}", cmds);
        // An unknown speed is refused rather than guessed at.
        assert!(f
            .translate_command(&serde_json::json!({"speed": "turbo"}), 0.0)
            .is_empty());
    }

    /// A pulsed CCO is action 6 with no parameters — *not* a level. Levelling
    /// it would latch a maintained CCO, which is a different device.
    #[test]
    fn pulsed_cco_pulses_and_never_latches() {
        let c = dev(DeviceKind::CcoPulsed);
        assert_eq!(
            c.translate_command(&serde_json::json!({"activate": true}), 0.0),
            vec!["#OUTPUT,7,6".to_string()]
        );
        assert_eq!(
            c.translate_command(&serde_json::json!({"on": true}), 0.0),
            vec!["#OUTPUT,7,6".to_string()]
        );
        // There is no "off" for a momentary contact.
        assert!(c
            .translate_command(&serde_json::json!({"on": false}), 0.0)
            .is_empty());
    }

    /// "Momentary outputs should not be queried" — Integration Guide.
    #[test]
    fn a_pulsed_cco_is_not_queried_on_connect() {
        assert!(!dev(DeviceKind::CcoPulsed).is_output());
        assert!(dev(DeviceKind::FanControl).is_output());
    }

    #[test]
    fn levels_become_percentages_and_on_never_reads_zero() {
        assert_eq!(level_pct(0.0), 0.0);
        assert_eq!(level_pct(0.01), 1.0, "a lit lamp never reads 0 %");
        assert_eq!(level_pct(0.4), 1.0);
        assert_eq!(level_pct(1.0), 1.0);
        assert_eq!(level_pct(37.46), 37.5);
        assert_eq!(level_pct(100.0), 100.0);
        assert_eq!(level_pct(100.5), 100.0);
        assert_eq!(level_pct(-3.0), 0.0);
    }

    /// Only `brightness_pct` — the 0–255 twin is gone.
    #[test]
    fn a_dimmer_publishes_one_brightness() {
        let d = dev(DeviceKind::Dimmer);
        let s = d.translate_output_state(25.0).unwrap();
        assert_eq!(s, serde_json::json!({"on": true, "brightness_pct": 25.0}));
        let off = d.translate_output_state(0.0).unwrap();
        assert_eq!(off, serde_json::json!({"on": false, "brightness_pct": 0.0}));
    }

    /// `brightness` guessed its scale from the number; it is no longer read.
    #[test]
    fn the_guessed_scale_brightness_is_not_accepted() {
        let d = dev(DeviceKind::Dimmer);
        assert!(d
            .translate_command(&serde_json::json!({"brightness": 128}), 0.0)
            .is_empty());
        assert_eq!(
            d.translate_command(&serde_json::json!({"brightness_pct": 40.5}), 0.0),
            vec!["#OUTPUT,7,1,40.50".to_string()]
        );
    }

    #[test]
    fn transition_ms_is_the_fade_and_fade_secs_still_works() {
        let d = dev(DeviceKind::Dimmer);
        let set = |cmd| d.translate_command(&cmd, 1.0)[0].clone();
        assert_eq!(
            set(serde_json::json!({"brightness_pct": 50, "transition_ms": 500})),
            "#OUTPUT,7,1,50.00,0.5"
        );
        assert_eq!(
            set(serde_json::json!({"brightness_pct": 50, "transition_ms": 0})),
            "#OUTPUT,7,1,50.00"
        );
        assert_eq!(
            set(serde_json::json!({"brightness_pct": 50, "fade_secs": 2})),
            "#OUTPUT,7,1,50.00,2"
        );
        // transition_ms wins when both are sent.
        assert_eq!(
            set(serde_json::json!({"brightness_pct": 50, "transition_ms": 250, "fade_secs": 9})),
            "#OUTPUT,7,1,50.00,0.25"
        );
        // Neither: the configured default.
        assert_eq!(
            set(serde_json::json!({"brightness_pct": 50})),
            "#OUTPUT,7,1,50.00,1"
        );
    }

    /// Rule 5: `{"on": true}` comes back to the last level, not to full.
    #[test]
    fn on_restores_the_last_level() {
        let mut d = dev(DeviceKind::Dimmer);
        let on = serde_json::json!({"on": true});
        // Nothing seen yet: full.
        assert_eq!(d.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,100.00"]);
        d.remember_level(35.0);
        d.remember_level(0.0); // turned off — not a level to come back to
        assert_eq!(d.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,35.00"]);
        // A level sent with it still wins.
        assert_eq!(
            d.translate_command(&serde_json::json!({"on": true, "brightness_pct": 80}), 0.0),
            vec!["#OUTPUT,7,1,80.00"]
        );

        let mut f = dev(DeviceKind::FanControl);
        assert_eq!(f.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,100.00"]);
        f.remember_level(25.0);
        assert_eq!(f.translate_command(&on, 0.0), vec!["#OUTPUT,7,1,25.00"]);
    }

    #[test]
    fn occupancy_is_published_once_as_occupied() {
        let g = dev(DeviceKind::OccupancyGroup);
        assert_eq!(
            g.translate_occupancy_state(true),
            serde_json::json!({"occupied": true})
        );
    }

    #[test]
    fn new_kinds_publish_sensible_device_types() {
        assert_eq!(dev(DeviceKind::FanControl).homecore_device_type(), "fan");
        // Momentary trigger, so a scene rather than a switch that never latches.
        assert_eq!(dev(DeviceKind::CcoPulsed).homecore_device_type(), "scene");
    }
}
