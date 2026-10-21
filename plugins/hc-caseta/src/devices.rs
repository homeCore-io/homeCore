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
    /// The state last published from the bridge's own reports — what a
    /// refused command's optimistic echo is put back to.
    pub confirmed: Option<Value>,
    /// A Pico's buttons as they are now: configured, learned from a press,
    /// or the usual 5-button layout. Empty for anything else.
    pub buttons: Vec<u32>,
}

/// The usual Pico: On, Favorite, Off, Raise, Lower — what a Pico with no
/// buttons configured is taken to be until it presses something else.
pub const FIVE_BUTTON: [u32; 5] = [2, 3, 4, 5, 6];

/// The 4-button scene Pico's buttons.
pub const FOUR_BUTTON_SCENE: [u32; 4] = [8, 9, 10, 11];

/// What a Pico button nobody named is called, by its component number — the
/// names hc-lutron gives the same buttons, and for the 4-button scene Pico
/// the ones the Lutron app shows.
pub fn usual_button_name(component: u32) -> String {
    match component {
        2 => "On".into(),
        3 => "Favorite".into(),
        4 => "Off".into(),
        5 => "Raise".into(),
        6 => "Lower".into(),
        8 => "Button 1".into(),
        9 => "Button 2".into(),
        10 => "Button 3".into(),
        11 => "Off".into(),
        n => format!("Button {n}"),
    }
}

/// A Pico's buttons from its configuration: the list given, else the usual
/// 5-button layout.
pub fn configured_buttons(cfg: &DeviceConfig) -> Vec<u32> {
    if cfg.buttons.is_empty() {
        FIVE_BUTTON.to_vec()
    } else {
        let mut b = cfg.buttons.clone();
        b.sort_unstable();
        b.dedup();
        b
    }
}

/// What a press of `component` says about a Pico believed to have `current`,
/// or `None` when it is one of them already.
///
/// A press of 8–11 is a 4-button scene Pico; a press of 2–6 on a Pico taken
/// for a 4-button one is a 5-button Pico after all; anything else is added.
/// A 2-button Pico (2 and 4) cannot be told from a 5-button one this way —
/// both have those buttons — so it is set in Settings.
pub fn learn_buttons(current: &[u32], component: u32) -> Option<Vec<u32>> {
    if current.contains(&component) || component == 0 {
        return None;
    }
    let only = |set: &[u32]| current.iter().all(|b| set.contains(b));
    let mut next = if FOUR_BUTTON_SCENE.contains(&component) && only(&FIVE_BUTTON) {
        FOUR_BUTTON_SCENE.to_vec()
    } else if FIVE_BUTTON.contains(&component) && only(&FOUR_BUTTON_SCENE) {
        FIVE_BUTTON.to_vec()
    } else {
        let mut v = current.to_vec();
        v.push(component);
        v
    };
    next.sort_unstable();
    next.dedup();
    Some(next)
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
///
/// Rounded up, so a level between 0 and 1 — a fan that is running — is
/// `low`, never `off`.
pub fn fan_speed(level: f64) -> &'static str {
    match level.clamp(0.0, 100.0).ceil() as u32 {
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
        let buttons = if kind == DeviceKind::Pico {
            configured_buttons(&config)
        } else {
            vec![]
        };
        Some(Self {
            config,
            hc_id,
            kind,
            last_level: None,
            confirmed: None,
            buttons,
        })
    }

    /// Each of a Pico's buttons with its name: the one configured for it,
    /// else its usual name.
    pub fn buttons_with_labels(&self) -> Vec<(u32, String)> {
        // `button_names` follows `buttons` in the order they were written.
        let given: std::collections::HashMap<u32, &str> = self
            .config
            .buttons
            .iter()
            .zip(&self.config.button_names)
            .map(|(&b, n)| (b, n.trim()))
            .filter(|(_, n)| !n.is_empty())
            .collect();
        self.buttons
            .iter()
            .map(|&b| {
                let name = given
                    .get(&b)
                    .map_or_else(|| usual_button_name(b), |n| n.to_string());
                (b, name)
            })
            .collect()
    }

    /// The name of one button, as `buttons_with_labels` has it.
    pub fn button_name(&self, component: u32) -> String {
        self.buttons_with_labels()
            .into_iter()
            .find(|(b, _)| *b == component)
            .map_or_else(|| usual_button_name(component), |(_, n)| n)
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
                // A level ramping until told to stop: the same OUTPUT actions
                // a shade's travel uses.
                for (verb, action) in [("raise", 2), ("lower", 3), ("stop", 4)] {
                    if cmd[verb].as_bool() == Some(true) {
                        return vec![cmd_shade_action(id, action)];
                    }
                }
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

            // A Pico is pressed as if by hand: press, then release (the
            // release is sent separately, a moment later — see the bridge).
            DeviceKind::Pico => match cmd["press"].as_u64() {
                Some(b) if self.buttons.contains(&(b as u32)) => {
                    vec![crate::lip::protocol::cmd_device_action(id, b as u32, 3)]
                }
                _ => vec![],
            },
            DeviceKind::OccupancySensor => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SceneEntry
// ---------------------------------------------------------------------------

/// A phantom button on the Smart Bridge, published to homeCore as a scene.
pub struct SceneEntry {
    pub config: SceneConfig,
    /// `caseta_scene_{name_slug}` — of the name it had when first seen; see
    /// [`scene_ids`].
    pub hc_id: String,
}

impl SceneEntry {
    pub fn with_id(config: SceneConfig, hc_id: String) -> Self {
        Self { config, hc_id }
    }
}

/// A scene's lasting key: the phantom button it is, not its name.
pub fn scene_key(cfg: &SceneConfig) -> String {
    format!("{}:{}", cfg.bridge_id, cfg.button_component)
}

/// Each scene's device id, keyed by its phantom button rather than its name.
///
/// The id was always `caseta_scene_{slug of the name}`, so renaming a scene
/// in the Lutron app (and re-importing) made a new device and stranded every
/// automation on the old one. Now the id a button was first given is
/// remembered (`known`, `bridge_id:button` → id) and kept through renames;
/// a button seen for the first time gets the slug of its name, which is what
/// every existing scene already has — so nothing changes id on upgrade. Two
/// buttons whose names slug alike get the button number added to the second.
///
/// Returns the ids, in `scenes` order, and the map to remember.
pub fn scene_ids(
    scenes: &[SceneConfig],
    known: &std::collections::BTreeMap<String, String>,
) -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut remembered = std::collections::BTreeMap::new();
    // Remembered ones first, so a new scene can't take a renamed one's id.
    for s in scenes {
        if let Some(id) = known.get(&scene_key(s)) {
            taken.insert(id.clone());
        }
    }
    let ids = scenes
        .iter()
        .map(|s| {
            let key = scene_key(s);
            let id = match known.get(&key) {
                Some(id) => id.clone(),
                None => {
                    let mut id = s.hc_id();
                    if taken.contains(&id) {
                        id = format!("{id}_{}", s.button_component);
                    }
                    taken.insert(id.clone());
                    id
                }
            };
            remembered.insert(key, id.clone());
            id
        })
        .collect();
    (ids, remembered)
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
            button_names: vec![],
        })
        .expect("a device")
    }

    fn pico(buttons: Vec<u32>, names: Vec<&str>) -> DeviceEntry {
        DeviceEntry::new(DeviceConfig {
            integration_id: 9,
            name: "Remote".into(),
            kind: Some(DeviceKind::Pico),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons,
            button_names: names.into_iter().map(String::from).collect(),
        })
        .expect("a Pico")
    }

    /// "Holiday Lights Remote 1" has no buttons configured: it is the usual
    /// 5-button Pico, named as hc-lutron names the same buttons.
    #[test]
    fn a_pico_with_no_buttons_is_the_usual_five_button() {
        let p = pico(vec![], vec![]);
        assert_eq!(p.buttons, FIVE_BUTTON);
        let names: Vec<String> = p
            .buttons_with_labels()
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(names, ["On", "Favorite", "Off", "Raise", "Lower"]);
    }

    #[test]
    fn configured_buttons_and_names_win() {
        let p = pico(vec![4, 2], vec!["Lights off", ""]);
        assert_eq!(p.buttons, [2, 4]);
        assert_eq!(p.button_name(4), "Lights off");
        assert_eq!(p.button_name(2), "On", "a blank name keeps the usual one");
        let s = pico(vec![8, 9, 10, 11], vec!["Bright", "Entertain", "Relax"]);
        assert_eq!(s.button_name(8), "Bright");
        assert_eq!(s.button_name(11), "Off");
    }

    /// A press of 8–11 on a Pico taken for a 5-button one is a 4-button
    /// scene Pico; a press of a known button teaches nothing.
    #[test]
    fn presses_teach_a_picos_layout() {
        assert_eq!(
            learn_buttons(&FIVE_BUTTON, 9),
            Some(FOUR_BUTTON_SCENE.to_vec())
        );
        assert_eq!(learn_buttons(&FIVE_BUTTON, 2), None);
        assert_eq!(
            learn_buttons(&FOUR_BUTTON_SCENE, 3),
            Some(FIVE_BUTTON.to_vec())
        );
        assert_eq!(learn_buttons(&[2, 4], 5), Some(vec![2, 4, 5]));
        assert_eq!(learn_buttons(&[2, 4], 8), Some(FOUR_BUTTON_SCENE.to_vec()));
        assert_eq!(learn_buttons(&[2, 4, 5, 6], 12), Some(vec![2, 4, 5, 6, 12]));
    }

    #[test]
    fn a_pico_press_is_a_device_press_of_one_of_its_buttons() {
        let p = pico(vec![], vec![]);
        assert_eq!(
            p.translate_command(&json!({"press": 3}), 0.0),
            vec!["#DEVICE,9,3,3"]
        );
        assert!(p.translate_command(&json!({"press": 9}), 0.0).is_empty());
        assert!(p.translate_command(&json!({"on": true}), 0.0).is_empty());
    }

    #[test]
    fn a_dimmer_ramps_and_stops() {
        let d = dev(DeviceKind::Dimmer);
        assert_eq!(
            d.translate_command(&json!({"raise": true}), 0.0),
            vec!["#OUTPUT,7,2"]
        );
        assert_eq!(
            d.translate_command(&json!({"lower": true}), 0.0),
            vec!["#OUTPUT,7,3"]
        );
        assert_eq!(
            d.translate_command(&json!({"stop": true}), 0.0),
            vec!["#OUTPUT,7,4"]
        );
    }

    fn scene(name: &str, button: u32) -> SceneConfig {
        SceneConfig {
            name: name.into(),
            bridge_id: 1,
            button_component: button,
            area: None,
        }
    }

    /// The house's four scenes keep the ids their automations use, and a
    /// rename in the Lutron app keeps the id it had.
    #[test]
    fn scene_ids_survive_upgrade_and_rename() {
        let house = [
            scene("Solstice Lights Off", 2),
            scene("Solstice Lights On", 1),
            scene("Sonos Play", 3),
            scene("Sonos Stop", 4),
        ];
        let (ids, known) = scene_ids(&house, &Default::default());
        assert_eq!(
            ids,
            [
                "caseta_scene_solstice_lights_off",
                "caseta_scene_solstice_lights_on",
                "caseta_scene_sonos_play",
                "caseta_scene_sonos_stop"
            ]
        );
        let renamed = [scene("Speakers Play", 3), scene("Sonos Play", 9)];
        let (ids, _) = scene_ids(&renamed, &known);
        assert_eq!(
            ids[0], "caseta_scene_sonos_play",
            "renamed, same button, same id"
        );
        assert_eq!(
            ids[1], "caseta_scene_sonos_play_9",
            "a new button never takes it"
        );
    }

    #[test]
    fn a_running_fan_below_one_percent_is_low() {
        assert_eq!(fan_speed(0.5), "low");
        assert_eq!(fan_speed(0.0), "off");
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
