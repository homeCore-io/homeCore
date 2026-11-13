//! What a scene sets each of its lights to, and which scene a light shows.
//!
//! A Hue scene is one action per light: `actions[]`, each a target light and
//! the on, dimming, colour or white it gets. Those are kept with the scene,
//! published on it as `lights`, sent one light at a time by `apply_scene`,
//! and compared with what each light reports so a light can say which scene
//! it is showing.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::models::{HueLight, HueScene, LightCommand};

/// How far a light may be from a scene's setting and still match it. The
/// bridge reports brightness to a fraction of a percent, rounds white to the
/// light's own steps and maps a colour into each light's gamut, so an exact
/// comparison would almost never hold.
pub const BRIGHTNESS_TOLERANCE_PCT: f64 = 3.0;
pub const MIREK_TOLERANCE: u16 = 8;
pub const XY_TOLERANCE: f64 = 0.02;

/// One light's part of a scene.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SceneLight {
    pub light_rid: String,
    /// The light's homeCore device id.
    pub device_id: String,
    pub on: Option<bool>,
    /// `dimming.brightness`, as the bridge has it.
    pub brightness_pct: Option<f64>,
    pub mirek: Option<u16>,
    pub xy: Option<(f64, f64)>,
    /// Gradient points, for a gradient lamp; empty otherwise.
    pub gradient: Vec<(f64, f64)>,
    pub effect: Option<String>,
    /// `dynamics.duration`: how long the scene takes to fade in.
    pub transition_ms: Option<u32>,
}

fn xy_of(v: &Value) -> Option<(f64, f64)> {
    Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?))
}

/// A scene's `actions[]`, one per light. Actions aimed at anything but a
/// light are skipped; `device_id_of` turns a light rid into its device id.
pub fn parse_actions(scene: &Value, device_id_of: impl Fn(&str) -> String) -> Vec<SceneLight> {
    let mut out = Vec::new();
    for a in scene
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let target = a.get("target");
        let rtype = target.and_then(|t| t.get("rtype")).and_then(Value::as_str);
        if rtype.is_some_and(|t| t != "light") {
            continue;
        }
        let Some(rid) = target.and_then(|t| t.get("rid")).and_then(Value::as_str) else {
            continue;
        };
        let Some(action) = a.get("action") else {
            continue;
        };
        let effect = action
            .get("effects_v2")
            .and_then(|e| e.get("action"))
            .and_then(|e| e.get("effect"))
            .or_else(|| action.get("effects").and_then(|e| e.get("effect")))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(SceneLight {
            light_rid: rid.to_string(),
            device_id: device_id_of(rid),
            on: action
                .get("on")
                .and_then(|o| o.get("on"))
                .and_then(Value::as_bool),
            brightness_pct: action
                .get("dimming")
                .and_then(|d| d.get("brightness"))
                .and_then(Value::as_f64),
            mirek: action
                .get("color_temperature")
                .and_then(|c| c.get("mirek"))
                .and_then(Value::as_u64)
                .map(|m| m.min(u64::from(u16::MAX)) as u16),
            xy: action
                .get("color")
                .and_then(|c| c.get("xy"))
                .and_then(xy_of),
            gradient: action
                .get("gradient")
                .and_then(|g| g.get("points"))
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| xy_of(p.get("color")?.get("xy")?))
                        .collect()
                })
                .unwrap_or_default(),
            effect,
            transition_ms: action
                .get("dynamics")
                .and_then(|d| d.get("duration"))
                .and_then(Value::as_u64)
                .map(|ms| ms.min(6_000_000) as u32),
        });
    }
    out
}

impl SceneLight {
    /// The command that gives this light its part of the scene and nothing
    /// else. `transition_ms` overrides the scene's own fade when given.
    pub fn command(&self, transition_ms: Option<u32>) -> LightCommand {
        let transition_ms = transition_ms.or(self.transition_ms);
        // Off is off: a level or a colour with it would light the lamp.
        if self.on == Some(false) {
            return LightCommand {
                on: Some(false),
                transition_ms,
                ..Default::default()
            };
        }
        let gradient = (self.gradient.len() > 1).then(|| self.gradient.clone());
        LightCommand {
            on: Some(true),
            brightness_pct: self.brightness_pct,
            color_temp_mirek: self.mirek,
            // A white and a colour together would leave the bridge to pick;
            // a scene sets one, and white wins if it somehow set both.
            color_xy: self
                .xy
                .filter(|_| self.mirek.is_none() && gradient.is_none()),
            gradient_points: gradient,
            effect: self.effect.clone(),
            transition_ms,
            ..Default::default()
        }
    }

    /// Whether a light showing `now` is showing this.
    ///
    /// A light that is off matches no scene, even one that turns it off:
    /// "Relax" on a dark lamp says nothing a person can see. A setting that
    /// only switches a light on matches nothing either, for the same reason.
    pub fn matches(&self, now: &LightNow) -> bool {
        if !now.on || self.on == Some(false) {
            return false;
        }
        let mut said_something = false;
        if let Some(b) = self.brightness_pct {
            said_something = true;
            match now.brightness_pct {
                Some(n) if (n - b).abs() <= BRIGHTNESS_TOLERANCE_PCT => {}
                _ => return false,
            }
        }
        if let Some(m) = self.mirek {
            said_something = true;
            match now.mirek {
                Some(n) if n.abs_diff(m) <= MIREK_TOLERANCE => {}
                _ => return false,
            }
        } else if self.gradient.len() > 1 {
            said_something = true;
            let close = now.gradient.len() == self.gradient.len()
                && self
                    .gradient
                    .iter()
                    .zip(&now.gradient)
                    .all(|(a, b)| xy_close(*a, *b));
            if !close {
                return false;
            }
        } else if let Some(xy) = self.xy {
            said_something = true;
            // Showing white is not showing a colour, whatever xy it reports.
            match now.xy {
                Some(n) if now.mirek.is_none() && xy_close(xy, n) => {}
                _ => return false,
            }
        }
        if let Some(e) = self.effect.as_deref().filter(|e| *e != "no_effect") {
            said_something = true;
            if now.effect.as_deref() != Some(e) {
                return false;
            }
        }
        said_something
    }
}

fn xy_close(a: (f64, f64), b: (f64, f64)) -> bool {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() <= XY_TOLERANCE
}

/// What a light shows now, as far as a scene cares: kept from the last
/// refresh and moved along by each event.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LightNow {
    pub on: bool,
    pub brightness_pct: Option<f64>,
    /// `None` while it shows a colour (`mirek_valid: false`).
    pub mirek: Option<u16>,
    pub xy: Option<(f64, f64)>,
    pub gradient: Vec<(f64, f64)>,
    pub effect: Option<String>,
}

impl LightNow {
    pub fn of(light: &HueLight) -> Self {
        LightNow {
            on: light.on,
            brightness_pct: light.brightness_pct,
            mirek: light.color_temp_mirek,
            xy: light.color_xy,
            gradient: light.gradient_points.clone(),
            effect: light.effect.clone(),
        }
    }

    /// Fold in a `light` event, which carries only what changed.
    pub fn apply_event(&mut self, item: &Value) {
        if let Some(on) = item
            .get("on")
            .and_then(|o| o.get("on"))
            .and_then(Value::as_bool)
        {
            self.on = on;
        }
        if let Some(b) = item
            .get("dimming")
            .and_then(|d| d.get("brightness"))
            .and_then(Value::as_f64)
        {
            self.brightness_pct = Some(b);
        }
        if let Some(ct) = item.get("color_temperature") {
            match ct.get("mirek_valid").and_then(Value::as_bool) {
                Some(false) => self.mirek = None,
                _ => {
                    if let Some(m) = ct.get("mirek").and_then(Value::as_u64) {
                        self.mirek = Some(m.min(u64::from(u16::MAX)) as u16);
                    }
                }
            }
        }
        if let Some(xy) = item.get("color").and_then(|c| c.get("xy")).and_then(xy_of) {
            self.xy = Some(xy);
        }
        if let Some(points) = item
            .get("gradient")
            .and_then(|g| g.get("points"))
            .and_then(Value::as_array)
        {
            self.gradient = points
                .iter()
                .filter_map(|p| xy_of(p.get("color")?.get("xy")?))
                .collect();
        }
        if let Some(e) = item
            .get("effects_v2")
            .and_then(|e| e.get("status"))
            .and_then(|s| s.get("effect"))
            .or_else(|| item.get("effects").and_then(|e| e.get("status")))
            .and_then(Value::as_str)
        {
            self.effect = Some(e.to_string());
        }
    }
}

#[derive(Debug, Clone)]
struct MatchScene {
    rid: String,
    device_id: String,
    name: String,
    group_rid: Option<String>,
    active: bool,
    lights: HashMap<String, SceneLight>,
}

/// The scene a light or a room is showing, as published.
#[derive(Debug, Clone, PartialEq)]
pub struct Showing {
    pub device_id: String,
    pub name: String,
}

/// `scene` and `scene_name` for a full state, null when nothing matches.
pub fn showing_attrs(showing: Option<&Showing>) -> Map<String, Value> {
    match showing_patch(showing) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `scene` and `scene_name` for a partial state: null clears them.
pub fn showing_patch(showing: Option<&Showing>) -> Value {
    match showing {
        Some(s) => json!({ "scene": s.device_id, "scene_name": s.name }),
        None => json!({ "scene": null, "scene_name": null }),
    }
}

/// What one scene event changed.
#[derive(Debug, Default, PartialEq)]
pub struct SceneEventChanges {
    /// Light device id → its new `scene` patch.
    pub lights: Vec<(String, Value)>,
    /// Room or zone rid → its new `scene` patch.
    pub groups: Vec<(String, Value)>,
    /// The scene's new settings, when the event carried them.
    pub actions: Option<Vec<SceneLight>>,
}

/// One bridge's scenes and lights, for telling which scene each light and
/// each room shows. Remembers what it last said so an event that changes
/// nothing republishes nothing.
#[derive(Debug, Clone, Default)]
pub struct SceneMatcher {
    /// In name order, so a tie between two scenes is settled the same way
    /// every time.
    scenes: Vec<MatchScene>,
    /// Light rid → what it shows.
    lights: HashMap<String, LightNow>,
    /// Light rid → its homeCore device id.
    light_ids: HashMap<String, String>,
    /// Light rid → the scene rid last said for it.
    said: HashMap<String, Option<String>>,
    /// Room or zone rid → the scene rid last said for it.
    group_said: HashMap<String, Option<String>>,
}

impl SceneMatcher {
    pub fn new(scenes: &[HueScene], lights: &[HueLight]) -> Self {
        let mut list: Vec<MatchScene> = scenes
            .iter()
            .filter(|s| !s.smart)
            .map(|s| MatchScene {
                rid: s.resource_id.clone(),
                device_id: s.device_id.clone(),
                name: s.name.clone(),
                group_rid: s.group_rid.clone(),
                active: s.active.unwrap_or(false),
                lights: s
                    .lights
                    .iter()
                    .map(|l| (l.light_rid.clone(), l.clone()))
                    .collect(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.rid.cmp(&b.rid)));
        SceneMatcher {
            scenes: list,
            lights: lights
                .iter()
                .map(|l| (l.resource_id.clone(), LightNow::of(l)))
                .collect(),
            light_ids: lights
                .iter()
                .map(|l| (l.resource_id.clone(), l.device_id.clone()))
                .collect(),
            said: HashMap::new(),
            group_said: HashMap::new(),
        }
    }

    /// Keep what was last said from an older matcher, so a refresh does not
    /// make every unchanged light speak again on its next event.
    pub fn carry_said(&mut self, older: &SceneMatcher) {
        self.said = older.said.clone();
        self.group_said = older.group_said.clone();
    }

    /// One light's part of one scene, for `apply_scene`. The error says
    /// which half is missing.
    pub fn part_of(&self, scene_rid: &str, light_rid: &str) -> Result<&SceneLight, String> {
        let scene = self
            .scenes
            .iter()
            .find(|s| s.rid == scene_rid)
            .ok_or_else(|| format!("no Hue scene '{scene_rid}' on this bridge"))?;
        scene
            .lights
            .get(light_rid)
            .ok_or_else(|| format!("the scene '{}' does not set this light", scene.name))
    }

    /// The scene this light shows. An active scene wins a tie with one that
    /// is not; between equals, the first by name.
    fn best_for(&self, light_rid: &str) -> Option<&MatchScene> {
        let now = self.lights.get(light_rid)?;
        let matching = |s: &&MatchScene| s.lights.get(light_rid).is_some_and(|a| a.matches(now));
        self.scenes
            .iter()
            .filter(|s| s.active)
            .find(matching)
            .or_else(|| self.scenes.iter().find(matching))
    }

    fn showing(s: &MatchScene) -> Showing {
        Showing {
            device_id: s.device_id.clone(),
            name: s.name.clone(),
        }
    }

    /// What a light shows, for its full state; recorded as said.
    pub fn light_showing(&mut self, light_rid: &str) -> Option<Showing> {
        let best = self
            .best_for(light_rid)
            .map(|s| (s.rid.clone(), Self::showing(s)));
        self.said.insert(
            light_rid.to_string(),
            best.as_ref().map(|(rid, _)| rid.clone()),
        );
        best.map(|(_, s)| s)
    }

    /// The scene a room or zone shows: one of its own the bridge reports
    /// active. Recorded as said.
    pub fn group_showing(&mut self, group_rid: &str) -> Option<Showing> {
        let best = self
            .scenes
            .iter()
            .find(|s| s.active && s.group_rid.as_deref() == Some(group_rid))
            .map(|s| (s.rid.clone(), Self::showing(s)));
        self.group_said.insert(
            group_rid.to_string(),
            best.as_ref().map(|(rid, _)| rid.clone()),
        );
        best.map(|(_, s)| s)
    }

    /// A light's patch when what it shows differs from what was last said.
    fn light_change(&mut self, light_rid: &str) -> Option<Value> {
        let before = self.said.get(light_rid).cloned().flatten();
        let now = self.light_showing(light_rid);
        let after = self.said.get(light_rid).cloned().flatten();
        (before != after).then(|| showing_patch(now.as_ref()))
    }

    fn group_change(&mut self, group_rid: &str) -> Option<Value> {
        let before = self.group_said.get(group_rid).cloned().flatten();
        let now = self.group_showing(group_rid);
        let after = self.group_said.get(group_rid).cloned().flatten();
        (before != after).then(|| showing_patch(now.as_ref()))
    }

    /// A light reported a change. Returns its new `scene` patch when what it
    /// shows changed; `None` for a light this matcher has never heard of.
    pub fn on_light_event(&mut self, light_rid: &str, item: &Value) -> Option<Value> {
        self.lights.get_mut(light_rid)?.apply_event(item);
        self.light_change(light_rid)
    }

    /// A scene reported a change: whether it is active, and sometimes its
    /// actions after an edit in the Hue app.
    pub fn on_scene_event(&mut self, scene_rid: &str, item: &Value) -> SceneEventChanges {
        let mut changes = SceneEventChanges::default();
        let ids = &self.light_ids;
        let Some(scene) = self.scenes.iter_mut().find(|s| s.rid == scene_rid) else {
            return changes;
        };
        let mut touched: Vec<String> = scene.lights.keys().cloned().collect();
        if let Some(status) = item
            .get("status")
            .and_then(|s| s.get("active"))
            .and_then(Value::as_str)
        {
            scene.active = status != "inactive";
        }
        if item.get("actions").is_some() {
            let actions = parse_actions(item, |rid| ids.get(rid).cloned().unwrap_or_default());
            scene.lights = actions
                .iter()
                .map(|l| (l.light_rid.clone(), l.clone()))
                .collect();
            touched.extend(scene.lights.keys().cloned());
            changes.actions = Some(actions);
        }
        let group = scene.group_rid.clone();
        touched.sort();
        touched.dedup();
        for rid in touched {
            let device_id = self.light_ids.get(&rid).cloned();
            if let (Some(patch), Some(device_id)) = (self.light_change(&rid), device_id) {
                changes.lights.push((device_id, patch));
            }
        }
        if let Some(group) = group {
            if let Some(patch) = self.group_change(&group) {
                changes.groups.push((group, patch));
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_id(rid: &str) -> String {
        format!("hue_b1_light_{rid}")
    }

    /// A scene as `GET /clip/v2/resource/scene` has it: three lights, one
    /// warm white, one in colour, one switched off.
    fn relax() -> Value {
        json!({
            "id": "s-relax",
            "type": "scene",
            "metadata": { "name": "Relax" },
            "group": { "rid": "r1", "rtype": "room" },
            "status": { "active": "static" },
            "actions": [
                {
                    "target": { "rid": "l1", "rtype": "light" },
                    "action": {
                        "on": { "on": true },
                        "dimming": { "brightness": 56.3 },
                        "color_temperature": { "mirek": 447 }
                    }
                },
                {
                    "target": { "rid": "l2", "rtype": "light" },
                    "action": {
                        "on": { "on": true },
                        "dimming": { "brightness": 80.0 },
                        "color": { "xy": { "x": 0.5612, "y": 0.4042 } },
                        "dynamics": { "duration": 400 }
                    }
                },
                {
                    "target": { "rid": "l3", "rtype": "light" },
                    "action": { "on": { "on": false } }
                },
                {
                    "target": { "rid": "p1", "rtype": "grouped_light" },
                    "action": { "on": { "on": true } }
                }
            ]
        })
    }

    #[test]
    fn reads_each_lights_part_and_skips_what_is_not_a_light() {
        let lights = parse_actions(&relax(), device_id);
        assert_eq!(lights.len(), 3);
        assert_eq!(
            lights[0],
            SceneLight {
                light_rid: "l1".into(),
                device_id: "hue_b1_light_l1".into(),
                on: Some(true),
                brightness_pct: Some(56.3),
                mirek: Some(447),
                ..Default::default()
            }
        );
        assert_eq!(lights[1].xy, Some((0.5612, 0.4042)));
        assert_eq!(lights[1].mirek, None);
        assert_eq!(lights[1].transition_ms, Some(400));
        assert_eq!(lights[2].on, Some(false));
        assert_eq!(lights[2].brightness_pct, None);
    }

    #[test]
    fn reads_gradients_and_effects() {
        let scene = json!({ "actions": [{
            "target": { "rid": "g1", "rtype": "light" },
            "action": {
                "on": { "on": true },
                "dimming": { "brightness": 100.0 },
                "gradient": { "points": [
                    { "color": { "xy": { "x": 0.2, "y": 0.3 } } },
                    { "color": { "xy": { "x": 0.6, "y": 0.35 } } }
                ] },
                "effects": { "effect": "candle" }
            }
        }]});
        let l = &parse_actions(&scene, device_id)[0];
        assert_eq!(l.gradient, vec![(0.2, 0.3), (0.6, 0.35)]);
        assert_eq!(l.effect.as_deref(), Some("candle"));
    }

    #[test]
    fn a_scene_with_no_actions_has_no_lights() {
        assert!(parse_actions(&json!({ "id": "s" }), device_id).is_empty());
    }

    #[test]
    fn the_command_sends_only_that_lights_part() {
        let lights = parse_actions(&relax(), device_id);
        let warm = lights[0].command(None);
        assert_eq!(warm.on, Some(true));
        assert_eq!(warm.brightness_pct, Some(56.3));
        assert_eq!(warm.color_temp_mirek, Some(447));
        assert_eq!(warm.color_xy, None);
        assert_eq!(warm.transition_ms, None);

        let colour = lights[1].command(None);
        assert_eq!(colour.color_xy, Some((0.5612, 0.4042)));
        assert_eq!(colour.transition_ms, Some(400));
        // A fade asked for wins over the scene's own.
        assert_eq!(lights[1].command(Some(0)).transition_ms, Some(0));

        let off = lights[2].command(None);
        assert_eq!(off.on, Some(false));
        assert_eq!(off.brightness_pct, None);
    }

    #[test]
    fn white_wins_when_a_scene_sets_both() {
        let l = SceneLight {
            mirek: Some(300),
            xy: Some((0.4, 0.4)),
            ..Default::default()
        };
        let c = l.command(None);
        assert_eq!(c.color_temp_mirek, Some(300));
        assert_eq!(c.color_xy, None);
    }

    fn warm_now() -> LightNow {
        LightNow {
            on: true,
            brightness_pct: Some(56.3),
            mirek: Some(447),
            xy: Some((0.51, 0.41)),
            ..Default::default()
        }
    }

    #[test]
    fn a_light_within_tolerance_matches() {
        let l = &parse_actions(&relax(), device_id)[0];
        assert!(l.matches(&warm_now()));
        let near = LightNow {
            brightness_pct: Some(56.3 + BRIGHTNESS_TOLERANCE_PCT - 0.1),
            mirek: Some(447 - MIREK_TOLERANCE),
            ..warm_now()
        };
        assert!(l.matches(&near));
    }

    #[test]
    fn a_light_outside_tolerance_does_not() {
        let l = &parse_actions(&relax(), device_id)[0];
        let brighter = LightNow {
            brightness_pct: Some(56.3 + BRIGHTNESS_TOLERANCE_PCT + 0.5),
            ..warm_now()
        };
        assert!(!l.matches(&brighter));
        let cooler = LightNow {
            mirek: Some(447 - MIREK_TOLERANCE - 1),
            ..warm_now()
        };
        assert!(!l.matches(&cooler));
        // Showing a colour is not showing white.
        let colour = LightNow {
            mirek: None,
            ..warm_now()
        };
        assert!(!l.matches(&colour));
    }

    #[test]
    fn colour_matches_by_distance_and_not_while_white() {
        let l = &parse_actions(&relax(), device_id)[1];
        let now = LightNow {
            on: true,
            brightness_pct: Some(80.0),
            mirek: None,
            xy: Some((0.5612 + 0.01, 0.4042 - 0.01)),
            ..Default::default()
        };
        assert!(l.matches(&now));
        assert!(!l.matches(&LightNow {
            xy: Some((0.5612 + 0.02, 0.4042 + 0.02)),
            ..now.clone()
        }));
        assert!(!l.matches(&LightNow {
            mirek: Some(300),
            ..now
        }));
    }

    #[test]
    fn an_off_light_matches_nothing() {
        let lights = parse_actions(&relax(), device_id);
        let off = LightNow {
            on: false,
            ..warm_now()
        };
        assert!(!lights[0].matches(&off));
        assert!(!lights[2].matches(&off));
    }

    #[test]
    fn a_setting_that_only_switches_on_matches_nothing() {
        let l = SceneLight {
            on: Some(true),
            ..Default::default()
        };
        assert!(!l.matches(&warm_now()));
    }

    #[test]
    fn an_effect_has_to_be_running() {
        let l = SceneLight {
            on: Some(true),
            brightness_pct: Some(56.3),
            effect: Some("candle".into()),
            ..Default::default()
        };
        assert!(!l.matches(&warm_now()));
        assert!(l.matches(&LightNow {
            effect: Some("candle".into()),
            ..warm_now()
        }));
    }

    #[test]
    fn events_move_the_light_along() {
        let mut now = warm_now();
        now.apply_event(&json!({ "dimming": { "brightness": 20.0 } }));
        assert_eq!(now.brightness_pct, Some(20.0));
        assert_eq!(now.mirek, Some(447));
        now.apply_event(&json!({
            "color": { "xy": { "x": 0.3, "y": 0.3 } },
            "color_temperature": { "mirek": null, "mirek_valid": false }
        }));
        assert_eq!(now.mirek, None);
        assert_eq!(now.xy, Some((0.3, 0.3)));
        now.apply_event(&json!({ "color_temperature": { "mirek": 250, "mirek_valid": true } }));
        assert_eq!(now.mirek, Some(250));
        now.apply_event(&json!({ "on": { "on": false } }));
        assert!(!now.on);
    }

    fn scene(rid: &str, name: &str, active: bool, actions: Value) -> HueScene {
        HueScene {
            bridge_id: "b1".into(),
            resource_id: rid.into(),
            device_id: format!("hue_b1_scene_{rid}"),
            name: name.into(),
            area: None,
            group_kind: Some("room".into()),
            active: Some(active),
            group_rid: Some("r1".into()),
            group_name: None,
            palette: vec![],
            status: None,
            dynamic_capable: false,
            smart: false,
            lights: parse_actions(&json!({ "actions": actions }), device_id),
        }
    }

    fn lamp(rid: &str, on: bool, bri: f64, mirek: u16) -> HueLight {
        HueLight {
            resource_id: rid.into(),
            device_id: device_id(rid),
            on,
            brightness_pct: Some(bri),
            color_temp_mirek: Some(mirek),
            color_xy: None,
            gradient_points: vec![],
            effect: None,
            ..HueLight::sample()
        }
    }

    fn white(rid: &str, bri: f64, mirek: u16) -> Value {
        json!({
            "target": { "rid": rid, "rtype": "light" },
            "action": {
                "on": { "on": true },
                "dimming": { "brightness": bri },
                "color_temperature": { "mirek": mirek }
            }
        })
    }

    #[test]
    fn a_light_says_the_scene_it_shows() {
        let scenes = [
            scene("s1", "Read", false, json!([white("l1", 100.0, 233)])),
            scene("s2", "Relax", false, json!([white("l1", 56.0, 447)])),
        ];
        let mut m = SceneMatcher::new(&scenes, &[lamp("l1", true, 56.0, 447)]);
        assert_eq!(
            m.light_showing("l1"),
            Some(Showing {
                device_id: "hue_b1_scene_s2".into(),
                name: "Relax".into()
            })
        );
        // Dimmed by hand: no longer any scene.
        let patch = m.on_light_event("l1", &json!({ "dimming": { "brightness": 30.0 } }));
        assert_eq!(patch, Some(json!({ "scene": null, "scene_name": null })));
        // A second change that still matches nothing says nothing.
        assert_eq!(
            m.on_light_event("l1", &json!({ "dimming": { "brightness": 31.0 } })),
            None
        );
        // Back to Read.
        let patch = m.on_light_event(
            "l1",
            &json!({
                "dimming": { "brightness": 100.0 },
                "color_temperature": { "mirek": 233, "mirek_valid": true }
            }),
        );
        assert_eq!(
            patch,
            Some(json!({ "scene": "hue_b1_scene_s1", "scene_name": "Read" }))
        );
        assert_eq!(m.on_light_event("unknown", &json!({})), None);
    }

    #[test]
    fn an_active_scene_wins_a_tie() {
        let same = || json!([white("l1", 56.0, 447)]);
        let scenes = [
            scene("s1", "Alpha", false, same()),
            scene("s2", "Beta", true, same()),
        ];
        let mut m = SceneMatcher::new(&scenes, &[lamp("l1", true, 56.0, 447)]);
        assert_eq!(m.light_showing("l1").unwrap().name, "Beta");
        assert_eq!(m.group_showing("r1").unwrap().name, "Beta");
        // Beta goes inactive: Alpha, first by name, now.
        let changes = m.on_scene_event("s2", &json!({ "status": { "active": "inactive" } }));
        assert_eq!(
            changes.lights,
            vec![(
                "hue_b1_light_l1".to_string(),
                json!({ "scene": "hue_b1_scene_s1", "scene_name": "Alpha" })
            )]
        );
        assert_eq!(
            changes.groups,
            vec![(
                "r1".to_string(),
                json!({ "scene": null, "scene_name": null })
            )]
        );
    }

    #[test]
    fn a_room_says_its_active_scene() {
        let scenes = [
            scene("s1", "Read", false, json!([white("l1", 100.0, 233)])),
            scene("s2", "Relax", true, json!([white("l1", 56.0, 447)])),
        ];
        let mut m = SceneMatcher::new(&scenes, &[lamp("l1", true, 10.0, 447)]);
        assert_eq!(m.group_showing("r1").unwrap().name, "Relax");
        assert_eq!(m.group_showing("r2"), None);
        let changes = m.on_scene_event("s1", &json!({ "status": { "active": "static" } }));
        // Both active for a moment: the first by name.
        assert_eq!(
            changes.groups,
            vec![(
                "r1".to_string(),
                json!({ "scene": "hue_b1_scene_s1", "scene_name": "Read" })
            )]
        );
        m.on_scene_event("s2", &json!({ "status": { "active": "inactive" } }));
        assert_eq!(m.group_showing("r1").unwrap().name, "Read");
    }

    #[test]
    fn an_edited_scene_is_matched_by_its_new_settings() {
        let scenes = [scene("s1", "Relax", false, json!([white("l1", 56.0, 447)]))];
        let mut m = SceneMatcher::new(&scenes, &[lamp("l1", true, 80.0, 300)]);
        assert_eq!(m.light_showing("l1"), None);
        let changes = m.on_scene_event("s1", &json!({ "actions": [white("l1", 80.0, 300)] }));
        assert_eq!(
            changes.lights,
            vec![(
                "hue_b1_light_l1".to_string(),
                json!({ "scene": "hue_b1_scene_s1", "scene_name": "Relax" })
            )]
        );
    }

    #[test]
    fn smart_scenes_are_not_matched() {
        let mut s = scene("s1", "Natural light", true, json!([white("l1", 56.0, 447)]));
        s.smart = true;
        let mut m = SceneMatcher::new(&[s], &[lamp("l1", true, 56.0, 447)]);
        assert_eq!(m.light_showing("l1"), None);
        assert_eq!(m.group_showing("r1"), None);
    }
}
