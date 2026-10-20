use serde_json::Value;

use crate::hue::models::{AccessoryCommand, LightCommand, SceneRecall, Signal};

#[derive(Debug, Clone)]
pub enum PluginCommand {
    Refresh,
    PairBridge,
    SetAvailability(bool),
    SetLightState(LightCommand),
    SetAccessoryState(AccessoryCommand),
    ActivateScene {
        scene_id: Option<String>,
        recall: SceneRecall,
    },
    /// Save a room's or zone's lights as they are as a new scene.
    SaveScene {
        name: String,
    },
    RenameScene {
        name: String,
    },
    DeleteScene,
    SetEntertainmentActive {
        config_id: Option<String>,
        active: bool,
    },
    Raw(Value),
}

/// How long a change takes: `transition_ms`, in milliseconds; Hue takes at
/// most a little under two hours. `duration_ms` is the old spelling, still
/// taken quietly from older rules but never declared.
fn transition_ms(payload: &Value) -> Option<u32> {
    payload
        .get("transition_ms")
        .or_else(|| payload.get("duration_ms"))
        .and_then(Value::as_f64)
        .map(|ms| ms.clamp(0.0, 6_000_000.0) as u32)
}

/// How long a `signal` flashes for, in milliseconds: `duration_secs` as
/// declared, or the old `duration_ms` quietly. Hue takes up to 65 534 s.
fn signal_ms(payload: &Value) -> Option<u32> {
    payload
        .get("duration_secs")
        .and_then(Value::as_f64)
        .map(|s| s * 1000.0)
        .or_else(|| payload.get("duration_ms").and_then(Value::as_f64))
        .map(|ms| ms.clamp(0.0, 65_534_000.0) as u32)
}

fn recall_of(payload: &Value) -> SceneRecall {
    SceneRecall {
        dynamic: payload
            .get("dynamic")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        transition_ms: transition_ms(payload),
        brightness_pct: payload
            .get("brightness_pct")
            .and_then(Value::as_f64)
            .map(|b| b.clamp(1.0, 100.0)),
    }
}

/// Which scene a room, zone or light is told to show: `scene_id` as
/// declared — the Hue scene's id or its homeCore device id — or `scene`
/// quietly.
fn scene_of(payload: &Value) -> Option<String> {
    payload
        .get("scene_id")
        .or_else(|| payload.get("scene"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn xy_list(v: Option<&Value>) -> Vec<(f64, f64)> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some((
                        p.get("x")?.as_f64()?.clamp(0.0, 1.0),
                        p.get("y")?.as_f64()?.clamp(0.0, 1.0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_homecore_command(payload: Value) -> PluginCommand {
    if let Some(action) = payload.get("action").and_then(Value::as_str) {
        if action == "refresh" {
            return PluginCommand::Refresh;
        }
        if action == "pair_bridge" {
            return PluginCommand::PairBridge;
        }
        // `activate` is the declared id — the same one hc-lutron declares, so a
        // client has one action for "run this scene" whatever ran it.
        // `activate_scene` is what this plugin has always accepted and what
        // older rules send.
        if action == "activate_scene" || action == "activate" {
            return PluginCommand::ActivateScene {
                scene_id: scene_of(&payload),
                recall: recall_of(&payload),
            };
        }
        let name = || {
            payload
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
        };
        if action == "save_scene" {
            if let Some(name) = name() {
                return PluginCommand::SaveScene { name };
            }
        }
        if action == "rename" {
            if let Some(name) = name() {
                return PluginCommand::RenameScene { name };
            }
        }
        if action == "delete" {
            return PluginCommand::DeleteScene;
        }
        if action == "breathe" {
            return PluginCommand::SetLightState(LightCommand {
                breathe: true,
                ..Default::default()
            });
        }
        if action == "signal" {
            let kind = payload
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("on_off")
                .to_string();
            return PluginCommand::SetLightState(LightCommand {
                signal: Some(Signal {
                    duration_ms: signal_ms(&payload).unwrap_or(5_000),
                    colors: xy_list(payload.get("colors")),
                    kind,
                }),
                ..Default::default()
            });
        }
        if action == "identify" {
            let light = LightCommand {
                identify: Some(true),
                ..Default::default()
            };
            return PluginCommand::SetLightState(light);
        }
        if action == "stop_dynamic" {
            let light = LightCommand {
                effect: Some("no_effect".to_string()),
                dynamic_speed: Some(0.0),
                ..Default::default()
            };
            return PluginCommand::SetLightState(light);
        }
        if action == "activate_entertainment" {
            let config_id = payload
                .get("config_id")
                .or_else(|| payload.get("entertainment_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
            return PluginCommand::SetEntertainmentActive {
                config_id,
                active: true,
            };
        }
        if action == "deactivate_entertainment" {
            let config_id = payload
                .get("config_id")
                .or_else(|| payload.get("entertainment_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
            return PluginCommand::SetEntertainmentActive {
                config_id,
                active: false,
            };
        }
    }

    if let Some(online) = payload.get("online").and_then(Value::as_bool) {
        return PluginCommand::SetAvailability(online);
    }

    // Unified scene activation payload — same as {"action":"activate_scene"} but
    // allows a single activation call across Hue and Lutron scene devices.
    if payload.get("activate").and_then(Value::as_bool) == Some(true) {
        return PluginCommand::ActivateScene {
            scene_id: scene_of(&payload),
            recall: recall_of(&payload),
        };
    }

    let mut light = LightCommand::default();
    let mut found_light_field = false;

    if let Some(on) = payload.get("on").and_then(Value::as_bool) {
        light.on = Some(on);
        found_light_field = true;
    }

    // 0 % is off. Hue reads a brightness of 0 as its dimmest step, not off,
    // so it is sent as off; any other level turns the light on with it.
    if let Some(brightness) = payload.get("brightness_pct").and_then(Value::as_f64) {
        if brightness <= 0.0 {
            light.on = Some(false);
        } else {
            light.brightness_pct = Some(brightness.min(100.0));
            light.on.get_or_insert(true);
        }
        found_light_field = true;
    }

    // `color_temp` in kelvin is what is declared. A mirek write from
    // something older is still taken, quietly; kelvin wins when both come.
    if let Some(kelvin) = payload.get("color_temp").and_then(Value::as_f64) {
        light.color_temp_mirek = Some(crate::translator::mirek_from_kelvin(kelvin));
        found_light_field = true;
    } else if let Some(mirek) = payload.get("color_temp_mirek").and_then(Value::as_u64) {
        light.color_temp_mirek = Some(mirek.clamp(153, 500) as u16);
        found_light_field = true;
    }

    if let Some(color_xy) = payload.get("color_xy").and_then(Value::as_object) {
        let x = color_xy.get("x").and_then(Value::as_f64);
        let y = color_xy.get("y").and_then(Value::as_f64);
        if let (Some(x), Some(y)) = (x, y) {
            light.color_xy = Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
            found_light_field = true;
        }
    }

    if let Some(effect) = payload
        .get("effect")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        light.effect = Some(effect.to_string());
        found_light_field = true;
    }

    if let Some(speed) = payload.get("dynamic_speed").and_then(Value::as_f64) {
        light.dynamic_speed = Some(speed.clamp(0.0, 1.0));
        found_light_field = true;
    }

    if let Some(points) = payload.get("gradient_points").and_then(Value::as_array) {
        let parsed = points
            .iter()
            .filter_map(|point| {
                let obj = point.as_object()?;
                let x = obj.get("x")?.as_f64()?.clamp(0.0, 1.0);
                let y = obj.get("y")?.as_f64()?.clamp(0.0, 1.0);
                Some((x, y))
            })
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            light.gradient_points = Some(parsed);
            found_light_field = true;
        }
    }

    if let Some(identify) = payload.get("identify").and_then(Value::as_bool) {
        light.identify = Some(identify);
        found_light_field = true;
    }

    if let Some(step) = payload.get("brightness_step").and_then(Value::as_f64) {
        light.brightness_step = Some(step.clamp(-100.0, 100.0));
        found_light_field = true;
    }

    // A fade goes with a change, never alone.
    if found_light_field {
        light.transition_ms = transition_ms(&payload);
    }

    if found_light_field {
        return PluginCommand::SetLightState(light);
    }

    let mut accessory = AccessoryCommand::default();
    let mut found_accessory_field = false;

    if let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) {
        accessory.enabled = Some(enabled);
        found_accessory_field = true;
    }

    if let Some(sensitivity) = payload
        .get("motion_sensitivity")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(0, 2) as u8)
    {
        accessory.motion_sensitivity = Some(sensitivity);
        found_accessory_field = true;
    }

    if found_accessory_field {
        return PluginCommand::SetAccessoryState(accessory);
    }

    PluginCommand::Raw(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The mirror of the declaration: `activate` is the action id a scene's
    /// schema offers, so the parser has to accept it. `activate_scene` is what
    /// this plugin has always taken and what older rules send.
    #[test]
    fn both_spellings_of_activation_reach_the_same_command() {
        for payload in [
            json!({ "action": "activate" }),
            json!({ "action": "activate_scene" }),
            json!({ "activate": true }),
        ] {
            match parse_homecore_command(payload.clone()) {
                PluginCommand::ActivateScene { .. } => {}
                other => panic!("expected ActivateScene for {payload}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parses_accessory_enabled_command() {
        let cmd = parse_homecore_command(json!({ "enabled": true }));
        match cmd {
            PluginCommand::SetAccessoryState(accessory) => {
                assert_eq!(accessory.enabled, Some(true));
                assert_eq!(accessory.motion_sensitivity, None);
            }
            _ => panic!("expected SetAccessoryState"),
        }
    }

    #[test]
    fn clamps_motion_sensitivity_to_supported_range() {
        let cmd = parse_homecore_command(json!({ "motion_sensitivity": 99 }));
        match cmd {
            PluginCommand::SetAccessoryState(accessory) => {
                assert_eq!(accessory.motion_sensitivity, Some(2));
            }
            _ => panic!("expected SetAccessoryState"),
        }
    }

    #[test]
    fn action_refresh_takes_precedence() {
        let cmd = parse_homecore_command(json!({
            "action": "refresh",
            "enabled": false,
            "motion_sensitivity": 1
        }));
        match cmd {
            PluginCommand::Refresh => {}
            _ => panic!("expected Refresh"),
        }
    }

    #[test]
    fn parses_pair_bridge_action() {
        let cmd = parse_homecore_command(json!({ "action": "pair_bridge" }));
        match cmd {
            PluginCommand::PairBridge => {}
            _ => panic!("expected PairBridge"),
        }
    }

    #[test]
    fn parses_entertainment_actions() {
        let activate = parse_homecore_command(json!({
            "action": "activate_entertainment",
            "config_id": "abc"
        }));
        match activate {
            PluginCommand::SetEntertainmentActive { config_id, active } => {
                assert_eq!(config_id.as_deref(), Some("abc"));
                assert!(active);
            }
            _ => panic!("expected SetEntertainmentActive(activate)"),
        }

        let deactivate = parse_homecore_command(json!({
            "action": "deactivate_entertainment"
        }));
        match deactivate {
            PluginCommand::SetEntertainmentActive { config_id, active } => {
                assert!(config_id.is_none());
                assert!(!active);
            }
            _ => panic!("expected SetEntertainmentActive(deactivate)"),
        }
    }

    #[test]
    fn parses_advanced_light_fields() {
        let cmd = parse_homecore_command(json!({
            "effect": "candle",
            "dynamic_speed": 1.5,
            "gradient_points": [
                { "x": 0.1, "y": 0.2 },
                { "x": 2.0, "y": -1.0 }
            ]
        }));

        match cmd {
            PluginCommand::SetLightState(light) => {
                assert_eq!(light.effect.as_deref(), Some("candle"));
                assert_eq!(light.dynamic_speed, Some(1.0));
                assert_eq!(light.gradient_points.as_ref().map(Vec::len), Some(2));
                let second = light.gradient_points.unwrap()[1];
                assert_eq!(second, (1.0, 0.0));
            }
            _ => panic!("expected SetLightState"),
        }
    }

    #[test]
    fn parses_stop_dynamic_action_as_light_command() {
        let cmd = parse_homecore_command(json!({ "action": "stop_dynamic" }));

        match cmd {
            PluginCommand::SetLightState(light) => {
                assert_eq!(light.effect.as_deref(), Some("no_effect"));
                assert_eq!(light.dynamic_speed, Some(0.0));
                assert!(light.on.is_none());
                assert!(light.brightness_pct.is_none());
            }
            _ => panic!("expected SetLightState"),
        }
    }

    #[test]
    fn parses_identify_action_as_light_command() {
        let cmd = parse_homecore_command(json!({ "action": "identify" }));

        match cmd {
            PluginCommand::SetLightState(light) => {
                assert_eq!(light.identify, Some(true));
                assert!(light.effect.is_none());
                assert!(light.dynamic_speed.is_none());
            }
            _ => panic!("expected SetLightState"),
        }
    }

    fn light(payload: Value) -> LightCommand {
        match parse_homecore_command(payload.clone()) {
            PluginCommand::SetLightState(l) => l,
            other => panic!("expected SetLightState for {payload}, got {other:?}"),
        }
    }

    /// 0 % is off — Hue would read a brightness of 0 as its dimmest step.
    #[test]
    fn zero_brightness_is_off() {
        let l = light(json!({ "brightness_pct": 0 }));
        assert_eq!(l.on, Some(false));
        assert_eq!(l.brightness_pct, None);
        let l = light(json!({ "on": true, "brightness_pct": 0 }));
        assert_eq!(l.on, Some(false));
        let l = light(json!({ "brightness_pct": -3 }));
        assert_eq!(l.on, Some(false));
    }

    #[test]
    fn a_level_turns_the_light_on_and_tops_out_at_100() {
        let l = light(json!({ "brightness_pct": 1 }));
        assert_eq!((l.on, l.brightness_pct), (Some(true), Some(1.0)));
        let l = light(json!({ "brightness_pct": 0.5 }));
        assert_eq!((l.on, l.brightness_pct), (Some(true), Some(0.5)));
        let l = light(json!({ "brightness_pct": 100 }));
        assert_eq!(l.brightness_pct, Some(100.0));
        let l = light(json!({ "brightness_pct": 140 }));
        assert_eq!(l.brightness_pct, Some(100.0));
        // An explicit off with a level stays off.
        let l = light(json!({ "on": false, "brightness_pct": 40 }));
        assert_eq!(l.on, Some(false));
    }

    #[test]
    fn kelvin_is_declared_and_mirek_still_taken_quietly() {
        assert_eq!(
            light(json!({ "color_temp": 2000 })).color_temp_mirek,
            Some(500)
        );
        assert_eq!(
            light(json!({ "color_temp": 6500 })).color_temp_mirek,
            Some(154)
        );
        assert_eq!(
            light(json!({ "color_temp_mirek": 366 })).color_temp_mirek,
            Some(366)
        );
        assert_eq!(
            light(json!({ "color_temp_mirek": 9 })).color_temp_mirek,
            Some(153)
        );
        // Kelvin wins when both come.
        let l = light(json!({ "color_temp": 2000, "color_temp_mirek": 153 }));
        assert_eq!(l.color_temp_mirek, Some(500));
    }

    #[test]
    fn a_fade_is_transition_ms_and_duration_ms_is_still_taken() {
        let l = light(json!({ "on": true, "transition_ms": 400 }));
        assert_eq!(l.transition_ms, Some(400));
        let l = light(json!({ "on": true, "duration_ms": 700 }));
        assert_eq!(l.transition_ms, Some(700));
        let l = light(json!({ "on": true, "transition_ms": 10_000_000 }));
        assert_eq!(l.transition_ms, Some(6_000_000));
        match parse_homecore_command(json!({ "action": "activate", "transition_ms": 2000 })) {
            PluginCommand::ActivateScene { recall, .. } => {
                assert_eq!(recall.transition_ms, Some(2000))
            }
            other => panic!("expected ActivateScene, got {other:?}"),
        }
    }

    #[test]
    fn effect_speed_is_not_hues_speed() {
        // `effect_speed` is WLED's 0–255; Hue's own idea is `dynamic_speed`.
        assert!(matches!(
            parse_homecore_command(json!({ "effect_speed": 0.5 })),
            PluginCommand::Raw(_)
        ));
    }

    #[test]
    fn a_signal_runs_for_duration_secs() {
        let secs = |p: Value| match parse_homecore_command(p) {
            PluginCommand::SetLightState(LightCommand {
                signal: Some(s), ..
            }) => s.duration_ms,
            other => panic!("expected a signal, got {other:?}"),
        };
        assert_eq!(
            secs(json!({ "action": "signal", "duration_secs": 8 })),
            8_000
        );
        assert_eq!(
            secs(json!({ "action": "signal", "duration_ms": 3000 })),
            3_000
        );
        assert_eq!(secs(json!({ "action": "signal" })), 5_000);
    }

    #[test]
    fn a_group_names_its_scene_with_scene_id_or_scene() {
        for payload in [
            json!({ "action": "activate", "scene_id": "abc" }),
            json!({ "action": "activate", "scene": "abc" }),
        ] {
            match parse_homecore_command(payload.clone()) {
                PluginCommand::ActivateScene { scene_id, .. } => {
                    assert_eq!(scene_id.as_deref(), Some("abc"), "{payload}")
                }
                other => panic!("expected ActivateScene, got {other:?}"),
            }
        }
    }
}
