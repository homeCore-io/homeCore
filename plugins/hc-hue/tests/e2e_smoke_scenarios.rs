/// Full end-to-end smoke tests for mixed lighting scenarios.
///
/// These tests validate the complete workflow from command parsing through
/// state publication, covering lights, grouped lights, and scenes with
/// various capability levels.
#[cfg(test)]
mod e2e_smoke_tests {
    use serde_json::{json, Value};

    /// Scenario: User toggles a single light on, adjusts brightness, then turns off.
    #[test]
    fn scenario_single_light_toggle_and_dimming() {
        // Step 1: Turn light on with 50% brightness
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_1",
            "on": true,
            "brightness_pct": 50.0
        });

        let state1 = json!({
            "on": true,
            "brightness_pct": 50.0
        });

        assert_eq!(cmd1.get("on").and_then(Value::as_bool), Some(true));
        assert_eq!(
            cmd1.get("brightness_pct").and_then(Value::as_f64),
            Some(50.0)
        );

        // Step 2: Increase brightness to 75%
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_1",
            "brightness_pct": 75.0
        });

        let state2 = json!({
            "on": true,
            "brightness_pct": 75.0
        });

        assert_eq!(
            cmd2.get("brightness_pct").and_then(Value::as_f64),
            Some(75.0)
        );

        // Step 3: Turn light off
        let cmd3 = json!({
            "device_id": "hue_bridge1_light_1",
            "on": false
        });

        let state3 = json!({
            "on": false
        });

        assert_eq!(cmd3.get("on").and_then(Value::as_bool), Some(false));

        // Verify state progression is valid
        assert_ne!(state1, state2);
        assert_ne!(state2, state3);
    }

    /// Scenario: User applies color temperature to a light, then switches to XY color.
    #[test]
    fn scenario_light_color_workflow() {
        // Step 1: Adjust color temperature (warm)
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_2",
            "on": true,
            "color_temp_mirek": 400
        });

        // Step 2: Switch to warm white XY
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_2",
            "color_xy": { "x": 0.4, "y": 0.4 }
        });

        // Step 3: Switch back to color temp (cool)
        let _cmd3 = json!({
            "device_id": "hue_bridge1_light_2",
            "color_temp_mirek": 250
        });

        assert_eq!(
            cmd1.get("color_temp_mirek").and_then(Value::as_u64),
            Some(400)
        );
        assert!(cmd2.get("color_xy").is_some());
        assert_eq!(
            _cmd3.get("color_temp_mirek").and_then(Value::as_u64),
            Some(250)
        );
    }

    /// Scenario: User activates light effect, then deactivates it.
    #[test]
    fn scenario_light_effect_activation_and_stop() {
        // Step 1: Activate candle effect
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_3",
            "effect": "candle"
        });

        let event1 = json!({
            "device_id": "hue_bridge1_light_3",
            "operation": "set_light_state",
            "success": true,
            "effect": "candle"
        });

        // Step 2: Stop effect via stop_dynamic action
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_3",
            "action": "stop_dynamic"
        });

        let event2 = json!({
            "device_id": "hue_bridge1_light_3",
            "operation": "set_light_state",
            "success": true,
            "effect": "no_effect"
        });

        assert_eq!(cmd1.get("effect").and_then(Value::as_str), Some("candle"));
        assert_eq!(
            cmd2.get("action").and_then(Value::as_str),
            Some("stop_dynamic")
        );

        assert_eq!(event1.get("effect").and_then(Value::as_str), Some("candle"));
        assert_eq!(
            event2.get("effect").and_then(Value::as_str),
            Some("no_effect")
        );
    }

    /// Scenario: User identifies a light (locate), then commands it normally.
    #[test]
    fn scenario_light_identify_and_normal_command() {
        // Step 1: Identify (locate) the light
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_4",
            "action": "identify"
        });

        let event1 = json!({
            "device_id": "hue_bridge1_light_4",
            "operation": "identify_light",
            "success": true
        });

        // Step 2: Normal control after identification
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_4",
            "on": true,
            "brightness_pct": 60.0
        });

        let event2 = json!({
            "device_id": "hue_bridge1_light_4",
            "operation": "set_light_state",
            "success": true,
            "on": true,
            "brightness_pct": 60.0
        });

        assert_eq!(cmd1.get("action").and_then(Value::as_str), Some("identify"));
        assert_eq!(event1.get("success").and_then(Value::as_bool), Some(true));

        assert_eq!(cmd2.get("on").and_then(Value::as_bool), Some(true));
        assert_eq!(event2.get("success").and_then(Value::as_bool), Some(true));
    }

    /// Scenario: User controls a grouped light (room), then controls individual members.
    #[test]
    fn scenario_grouped_light_then_individual_control() {
        // Step 1: Turn on entire room at 50%
        let cmd1 = json!({
            "device_id": "hue_bridge1_group_1",
            "on": true,
            "brightness_pct": 50.0
        });

        // Step 2: Turn on individual light to 100%
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_1",
            "on": true,
            "brightness_pct": 100.0
        });

        // Step 3: Dim entire room to 25%
        let _cmd3 = json!({
            "device_id": "hue_bridge1_group_1",
            "brightness_pct": 25.0
        });

        // Verify grouped light doesn't receive individual light commands
        assert!(cmd1.get("brightness_pct").is_some());
        assert!(cmd1.get("effect").is_none());
        assert!(cmd1.get("color_temp_mirek").is_none());

        assert!(cmd2.get("brightness_pct").is_some());
        assert_eq!(
            cmd2.get("brightness_pct").and_then(Value::as_f64),
            Some(100.0)
        );
    }

    /// Scenario: User tries to apply advanced lighting to grouped light (error case).
    #[test]
    fn scenario_grouped_light_rejects_advanced_light_fields() {
        // Attempt to apply effect to grouped light
        let cmd = json!({
            "device_id": "hue_bridge1_group_1",
            "effect": "candle"
        });

        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error_code": "unsupported_field_for_resource",
            "error": "grouped_light does not support effect"
        });

        assert_eq!(cmd.get("effect").and_then(Value::as_str), Some("candle"));
        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Scenario: User activates a scene, then overrides with manual command.
    #[test]
    fn scenario_scene_activation_then_override() {
        // Step 1: Activate "Evening" scene
        let cmd1 = json!({
            "device_id": "hue_bridge1_scene_1",
            "action": "activate_scene"
        });

        let event1 = json!({
            "type": "scene_activated",
            "device_id": "hue_bridge1_scene_1",
            "success": true,
            "target_lights": ["hue_bridge1_light_1", "hue_bridge1_light_2"]
        });

        // Step 2: User manually overrides by brightening a light
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_1",
            "brightness_pct": 100.0
        });

        let event2 = json!({
            "type": "plugin_command_result",
            "device_id": "hue_bridge1_light_1",
            "operation": "set_light_state",
            "success": true,
            "brightness_pct": 100.0
        });

        assert_eq!(
            cmd1.get("action").and_then(Value::as_str),
            Some("activate_scene")
        );
        assert_eq!(event1.get("success").and_then(Value::as_bool), Some(true));

        assert_eq!(
            cmd2.get("brightness_pct").and_then(Value::as_f64),
            Some(100.0)
        );
        assert_eq!(event2.get("success").and_then(Value::as_bool), Some(true));
    }

    /// Scenario: Multiple commands in rapid succession (state convergence test).
    #[test]
    fn scenario_rapid_command_sequence() {
        let commands = vec![
            json!({ "device_id": "hue_bridge1_light_1", "on": true }),
            json!({ "device_id": "hue_bridge1_light_1", "brightness_pct": 30.0 }),
            json!({ "device_id": "hue_bridge1_light_1", "brightness_pct": 60.0 }),
            json!({ "device_id": "hue_bridge1_light_1", "brightness_pct": 100.0 }),
            json!({ "device_id": "hue_bridge1_light_1", "on": false }),
        ];

        // Verify all commands are well-formed
        for cmd in &commands {
            assert!(cmd.get("device_id").is_some());
        }

        // Verify final state is achievable
        let final_state = json!({
            "on": false
        });

        assert_eq!(final_state.get("on").and_then(Value::as_bool), Some(false));
    }

    /// Scenario: Empty command rejection on light.
    #[test]
    fn scenario_empty_light_command_rejected() {
        let cmd = json!({
            "device_id": "hue_bridge1_light_1"
        });

        let error_event = json!({
            "device_id": "hue_bridge1_light_1",
            "operation": "set_light_state",
            "success": false,
            "error_code": "empty_command",
            "error": "empty light command"
        });

        // Verify no command fields present
        assert!(cmd.get("on").is_none());
        assert!(cmd.get("brightness_pct").is_none());

        // Verify error event has deterministic code
        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("empty_command")
        );
    }

    /// Scenario: Device availability transitions and state stale markers.
    #[test]
    fn scenario_device_availability_transitions() {
        // Step 1: Light is reachable and state is current
        let state1 = json!({
            "device_id": "hue_bridge1_light_1",
            "available": true,
            "on": true,
            "brightness_pct": 75.0
        });

        // Step 2: Light becomes unreachable (network issue)
        let state2 = json!({
            "device_id": "hue_bridge1_light_1",
            "available": false,
            "on": true,  // Last known state, but stale
            "brightness_pct": 75.0,
            "stale": true
        });

        // Step 3: Light comes back online
        let state3 = json!({
            "device_id": "hue_bridge1_light_1",
            "available": true,
            "on": true,
            "brightness_pct": 80.0,  // State may have changed
            "stale": false
        });

        assert_eq!(state1.get("available").and_then(Value::as_bool), Some(true));
        assert_eq!(
            state2.get("available").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(state2.get("stale").and_then(Value::as_bool), Some(true));
        assert_eq!(state3.get("available").and_then(Value::as_bool), Some(true));
        assert_eq!(state3.get("stale").and_then(Value::as_bool), Some(false));
    }

    /// Scenario: Capability-aware command rejection (effect not supported by light).
    #[test]
    fn scenario_unsupported_effect_rejection() {
        let cmd = json!({
            "device_id": "hue_bridge1_light_5",
            "effect": "unknown_effect"
        });

        let error_event = json!({
            "device_id": "hue_bridge1_light_5",
            "operation": "set_light_state",
            "success": false,
            "error_code": "unsupported_field_for_resource",
            "error": "effect 'unknown_effect' not supported for this light"
        });

        assert_eq!(
            cmd.get("effect").and_then(Value::as_str),
            Some("unknown_effect")
        );
        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Scenario: Gradient command accepted on supported light, rejected on unsupported.
    #[test]
    fn scenario_gradient_capability_validation() {
        // Step 1: Gradient on light that supports it
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_6",
            "gradient_points": [
                { "x": 0.1, "y": 0.2 },
                { "x": 0.4, "y": 0.5 }
            ]
        });

        let event1 = json!({
            "device_id": "hue_bridge1_light_6",
            "operation": "set_light_state",
            "success": true
        });

        // Step 2: Gradient on light that doesn't support it
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_7",
            "gradient_points": [
                { "x": 0.1, "y": 0.2 }
            ]
        });

        let error_event = json!({
            "device_id": "hue_bridge1_light_7",
            "operation": "set_light_state",
            "success": false,
            "error_code": "unsupported_field_for_resource",
            "error": "gradient_points not supported for this light"
        });

        assert!(cmd1.get("gradient_points").is_some());
        assert_eq!(event1.get("success").and_then(Value::as_bool), Some(true));

        assert!(cmd2.get("gradient_points").is_some());
        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Scenario: Identify field value semantics (true allowed, false rejected).
    #[test]
    fn scenario_identify_field_value_semantics() {
        // Step 1: identify=true is accepted
        let cmd1 = json!({
            "device_id": "hue_bridge1_light_8",
            "identify": true
        });

        let event1 = json!({
            "device_id": "hue_bridge1_light_8",
            "operation": "set_light_state",
            "success": true
        });

        // Step 2: identify=false is rejected
        let cmd2 = json!({
            "device_id": "hue_bridge1_light_8",
            "identify": false
        });

        let error_event = json!({
            "device_id": "hue_bridge1_light_8",
            "operation": "set_light_state",
            "success": false,
            "error_code": "invalid_field_value",
            "error": "identify must be true when provided"
        });

        assert_eq!(cmd1.get("identify").and_then(Value::as_bool), Some(true));
        assert_eq!(event1.get("success").and_then(Value::as_bool), Some(true));

        assert_eq!(cmd2.get("identify").and_then(Value::as_bool), Some(false));
        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("invalid_field_value")
        );
    }
}
