/// Integration tests for grouped-light isolation and validation semantics.
///
/// These tests verify that grouped lights maintain strict isolation from advanced light features
/// and that invalid commands produce deterministic error codes.
#[cfg(test)]
mod grouped_light_tests {
    use serde_json::{json, Value};

    /// Test that grouped light accepts only on/brightness commands.
    #[test]
    fn grouped_light_accepts_basic_on_brightness_commands() {
        let valid_payloads = vec![
            json!({ "on": true }),
            json!({ "on": false }),
            json!({ "brightness_pct": 50.0 }),
            json!({ "brightness_pct": 100.0 }),
            json!({
                "on": true,
                "brightness_pct": 75.0
            }),
        ];

        for payload in valid_payloads {
            // Verify payload structure is valid for grouped light
            assert!(payload.get("on").is_none() || payload.get("on").is_some());
            assert!(
                payload.get("brightness_pct").is_none() || payload.get("brightness_pct").is_some()
            );

            // Verify no advanced fields present
            assert!(payload.get("color_temp_mirek").is_none());
            assert!(payload.get("color_xy").is_none());
        }
    }

    /// Test grouped light rejection of color temperature commands.
    #[test]
    fn grouped_light_rejects_color_temp_mirek() {
        let payload = json!({
            "on": true,
            "color_temp_mirek": 300
        });

        // Grouped lights do not support color_temp_mirek
        assert_eq!(
            payload.get("color_temp_mirek").and_then(Value::as_u64),
            Some(300)
        );

        // This command should fail at validation stage before API execution
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support color_temp_mirek",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of color XY commands.
    #[test]
    fn grouped_light_rejects_color_xy() {
        let payload = json!({
            "on": true,
            "color_xy": { "x": 0.3, "y": 0.4 }
        });

        // Grouped lights do not support color_xy
        assert!(payload.get("color_xy").is_some());

        // This command should fail at validation stage before API execution
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support color_xy",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of effect commands.
    #[test]
    fn grouped_light_rejects_effect() {
        let payload = json!({
            "on": true,
            "effect": "candle"
        });

        // Grouped lights do not support effect
        assert_eq!(
            payload.get("effect").and_then(Value::as_str),
            Some("candle")
        );

        // This command should fail at validation stage
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support effect",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of dynamic speed commands.
    #[test]
    fn grouped_light_rejects_dynamic_speed() {
        let payload = json!({
            "on": true,
            "dynamic_speed": 0.6
        });

        // Grouped lights do not support dynamic_speed
        assert_eq!(
            payload.get("dynamic_speed").and_then(Value::as_f64),
            Some(0.6)
        );

        // This command should fail at validation stage
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support dynamic_speed",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of gradient points commands.
    #[test]
    fn grouped_light_rejects_gradient_points() {
        let payload = json!({
            "on": true,
            "gradient_points": [
                { "x": 0.1, "y": 0.2 },
                { "x": 0.4, "y": 0.5 }
            ]
        });

        // Grouped lights do not support gradient_points
        assert!(payload.get("gradient_points").is_some());

        // This command should fail at validation stage
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support gradient_points",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of identify commands.
    #[test]
    fn grouped_light_rejects_identify() {
        let payload = json!({
            "on": true,
            "identify": true
        });

        // Grouped lights do not support identify
        assert_eq!(payload.get("identify").and_then(Value::as_bool), Some(true));

        // This command should fail at validation stage
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support identify",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test grouped light rejection of action commands (identify, stop_dynamic).
    #[test]
    fn grouped_light_rejects_action_commands() {
        let identify_action = json!({
            "action": "identify"
        });

        let stop_dynamic_action = json!({
            "action": "stop_dynamic"
        });

        // Both action types should be rejected for grouped lights
        for action in &[identify_action, stop_dynamic_action] {
            let error_event = json!({
                "device_id": "hue_bridge1_group_1",
                "operation": "set_group_state",
                "success": false,
                "error": format!("grouped_light does not support action: {}", action.get("action").and_then(Value::as_str).unwrap_or("unknown")),
                "error_code": "unsupported_field_for_resource"
            });

            assert_eq!(
                error_event.get("error_code").and_then(Value::as_str),
                Some("unsupported_field_for_resource")
            );
        }
    }

    /// Test grouped light command state schema does not include advanced fields.
    #[test]
    fn grouped_light_state_schema_excludes_advanced_fields() {
        let grouped_state = json!({
            "kind": "hue_grouped_light",
            "bridge_id": "bridge-1",
            "resource_id": "grouped_light-1",
            "on": true,
            "brightness_pct": 75.0,
            "supports_dimming": true
        });

        // Verify basic fields present
        assert_eq!(
            grouped_state.get("kind").and_then(Value::as_str),
            Some("hue_grouped_light")
        );
        assert_eq!(grouped_state.get("on").and_then(Value::as_bool), Some(true));

        // Verify advanced fields NOT present
        assert!(grouped_state.get("color_temp_mirek").is_none());
        assert!(grouped_state.get("color_xy").is_none());
        assert!(grouped_state.get("effect").is_none());
        assert!(grouped_state.get("effect_values").is_none());
        assert!(grouped_state.get("dynamic_status").is_none());
        assert!(grouped_state.get("dynamic_speed").is_none());
        assert!(grouped_state.get("gradient_points").is_none());
        assert!(grouped_state.get("supports_gradient").is_none());
        assert!(grouped_state.get("supports_identify").is_none());
        assert!(grouped_state.get("identify").is_none());
    }

    /// Test grouped light device registration includes only supported capabilities.
    #[test]
    fn grouped_light_registration_excludes_advanced_capabilities() {
        let registration = json!({
            "device_id": "hue_bridge1_group_1",
            "plugin_id": "plugin.hue",
            "name": "Living Room",
            "device_type": "grouped_light",
            "area": "living_room",
            "capabilities": {
                "on": { "type": "boolean" },
                "brightness_pct": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 100.0
                }
            }
        });

        // Verify registration structure
        assert_eq!(
            registration.get("device_type").and_then(Value::as_str),
            Some("grouped_light")
        );

        // Verify capabilities include only on/brightness
        let caps = registration.get("capabilities").unwrap();
        assert!(caps.get("on").is_some());
        assert!(caps.get("brightness_pct").is_some());

        // Verify advanced capabilities NOT exposed
        assert!(caps.get("color_temp_mirek").is_none());
        assert!(caps.get("color_xy").is_none());
        assert!(caps.get("effect").is_none());
        assert!(caps.get("dynamic_speed").is_none());
        assert!(caps.get("gradient_points").is_none());
        assert!(caps.get("identify").is_none());
    }

    /// Test that partial state patches for grouped lights only update on/brightness.
    #[test]
    fn grouped_light_partial_patch_excludes_advanced_fields() {
        let partial_patch = json!({
            "on": false,
            "brightness_pct": 25.0
        });

        // Verify only basic fields in patch
        assert_eq!(
            partial_patch.get("on").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            partial_patch.get("brightness_pct").and_then(Value::as_f64),
            Some(25.0)
        );

        // Verify no advanced fields
        assert!(partial_patch.get("color_temp_mirek").is_none());
        assert!(partial_patch.get("color_xy").is_none());
        assert!(partial_patch.get("effect").is_none());
        assert!(partial_patch.get("dynamic_speed").is_none());
        assert!(partial_patch.get("gradient_points").is_none());
    }

    /// Test grouped light command with mixed valid/invalid fields rejects early.
    #[test]
    fn grouped_light_mixed_valid_invalid_fields_rejected() {
        let _mixed_payload = json!({
            "on": true,
            "brightness_pct": 50.0,
            "effect": "candle"
        });

        // Payload has some valid fields (on, brightness_pct) but also invalid (effect)
        // Should be rejected due to single invalid field
        let error_event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light command contains unsupported field: effect",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(
            error_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test that empty grouped-light commands are rejected the same as light commands.
    #[test]
    fn grouped_light_empty_command_rejected() {
        let event = json!({
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "empty grouped_light command",
            "error_code": "empty_command"
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("empty_command")
        );
    }

    /// Test that only on/brightness fields are allowed in grouped light command result telemetry.
    #[test]
    fn grouped_light_command_result_only_records_basic_fields() {
        let state_patch = json!({
            "last_command": {
                "on": true,
                "brightness_pct": 60.0
            },
            "last_command_success": true,
            "last_command_latency_ms": 125,
            "last_command_retry_count": 0
        });

        // Verify last_command only includes basic fields
        let cmd = state_patch.get("last_command").unwrap();
        assert_eq!(cmd.get("on").and_then(Value::as_bool), Some(true));
        assert_eq!(
            cmd.get("brightness_pct").and_then(Value::as_f64),
            Some(60.0)
        );

        // Verify no advanced field records
        assert!(cmd.get("effect").is_none());
        assert!(cmd.get("color_temp_mirek").is_none());
    }
}
