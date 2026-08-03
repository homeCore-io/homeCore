use serde_json::Value;

use crate::hue::models::{AccessoryCommand, LightCommand};

#[derive(Debug, Clone)]
pub enum PluginCommand {
    Refresh,
    PairBridge,
    SetAvailability(bool),
    SetLightState(LightCommand),
    SetAccessoryState(AccessoryCommand),
    ActivateScene {
        scene_id: Option<String>,
    },
    SetEntertainmentActive {
        config_id: Option<String>,
        active: bool,
    },
    Raw(Value),
}

pub fn parse_homecore_command(payload: Value) -> PluginCommand {
    if let Some(action) = payload.get("action").and_then(Value::as_str) {
        if action == "refresh" {
            return PluginCommand::Refresh;
        }
        if action == "pair_bridge" {
            return PluginCommand::PairBridge;
        }
        if action == "activate_scene" {
            let scene_id = payload
                .get("scene_id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            return PluginCommand::ActivateScene { scene_id };
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
        let scene_id = payload
            .get("scene_id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return PluginCommand::ActivateScene { scene_id };
    }

    let mut light = LightCommand::default();
    let mut found_light_field = false;

    if let Some(on) = payload.get("on").and_then(Value::as_bool) {
        light.on = Some(on);
        found_light_field = true;
    }

    if let Some(brightness) = payload.get("brightness_pct").and_then(Value::as_f64) {
        light.brightness_pct = Some(brightness.clamp(0.0, 100.0));
        found_light_field = true;
    }

    if let Some(mirek) = payload.get("color_temp_mirek").and_then(Value::as_u64) {
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

    if let Some(speed) = payload
        .get("dynamic_speed")
        .or_else(|| payload.get("effect_speed"))
        .and_then(Value::as_f64)
    {
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
}
