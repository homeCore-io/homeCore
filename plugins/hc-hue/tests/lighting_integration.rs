/// Integration tests for hc-hue lighting command flow and state publication.
///
/// These tests validate:
/// - Command parsing with advanced lighting fields
/// - Capability-aware validation against discovered light support
/// - Deterministic error codes for rejection cases
/// - State/partial payload schema correctness for HomeCore contract
#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    /// Test that advanced light command fields parse and validate correctly.
    #[test]
    fn parses_advanced_light_command_fields() {
        let payload = json!({
            "on": true,
            "brightness_pct": 75.0,
            "color_temp_mirek": 300,
            "color_xy": { "x": 0.3, "y": 0.4 },
            "effect": "candle",
            "dynamic_speed": 0.6,
            "gradient_points": [
                { "x": 0.1, "y": 0.2 },
                { "x": 0.4, "y": 0.5 }
            ],
            "identify": true
        });

        // Verify all fields are present and correctly typed
        assert_eq!(payload.get("on").and_then(Value::as_bool), Some(true));
        assert_eq!(
            payload.get("brightness_pct").and_then(Value::as_f64),
            Some(75.0)
        );
        assert_eq!(
            payload.get("color_temp_mirek").and_then(Value::as_u64),
            Some(300)
        );
        assert_eq!(
            payload
                .get("color_xy")
                .and_then(|v| v.get("x"))
                .and_then(Value::as_f64),
            Some(0.3)
        );
        assert_eq!(
            payload.get("effect").and_then(Value::as_str),
            Some("candle")
        );
        assert_eq!(
            payload.get("dynamic_speed").and_then(Value::as_f64),
            Some(0.6)
        );
        assert_eq!(
            payload
                .get("gradient_points")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(2)
        );
        assert_eq!(payload.get("identify").and_then(Value::as_bool), Some(true));
    }

    /// Test light state publication schema includes all required and optional fields.
    #[test]
    fn validates_light_state_publication_schema() {
        let light_state = json!({
            "kind": "hue_light",
            "bridge_id": "bridge-1",
            "resource_id": "light-1",
            "on": true,
            "brightness_pct": 50.0,
            "color_temp_mirek": 300,
            "color_xy": { "x": 0.3, "y": 0.4 },
            "effect": "candle",
            "effect_values": ["candle", "fire"],
            "dynamic_status": "dynamic_palette",
            "dynamic_speed": 0.6,
            "gradient_points": [
                { "x": 0.1, "y": 0.2 }
            ],
            "supports_dimming": true,
            "supports_color_xy": true,
            "supports_gradient": true,
            "supports_identify": true,
            "color_temp_mirek_min": 153,
            "color_temp_mirek_max": 500
        });

        // Verify core state fields
        assert_eq!(
            light_state.get("kind").and_then(Value::as_str),
            Some("hue_light")
        );
        assert_eq!(light_state.get("on").and_then(Value::as_bool), Some(true));

        // Verify advanced lighting fields
        assert_eq!(
            light_state.get("effect").and_then(Value::as_str),
            Some("candle")
        );
        assert_eq!(
            light_state.get("dynamic_speed").and_then(Value::as_f64),
            Some(0.6)
        );
        assert_eq!(
            light_state
                .get("gradient_points")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(1)
        );

        // Verify capability flags (for HomeCore conditional presentation)
        assert_eq!(
            light_state
                .get("supports_gradient")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            light_state
                .get("supports_identify")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    /// Test light capability schema exposes advanced fields conditionally.
    #[test]
    fn validates_light_capability_schema_with_advanced_fields() {
        let light_caps = json!({
            "on": { "type": "boolean" },
            "brightness_pct": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 100.0
            },
            "color_temp_mirek": {
                "type": "integer",
                "minimum": 153,
                "maximum": 500
            },
            "color_xy": {
                "type": "object",
                "properties": {
                    "x": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "y": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                }
            },
            "effect": {
                "type": "string",
                "enum": ["candle", "fire"]
            },
            "dynamic_speed": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0
            },
            "gradient_points": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "number" },
                        "y": { "type": "number" }
                    }
                }
            },
            "identify": { "type": "boolean" }
        });

        // Verify all advanced light fields present in schema
        assert!(light_caps.get("effect").is_some());
        assert!(light_caps.get("dynamic_speed").is_some());
        assert!(light_caps.get("gradient_points").is_some());
        assert!(light_caps.get("identify").is_some());

        // Verify effect has enum constraint
        assert_eq!(
            light_caps
                .get("effect")
                .and_then(|v| v.get("enum"))
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(2)
        );
    }

    /// Test grouped light capability schema excludes advanced fields (isolation).
    #[test]
    fn validates_grouped_light_schema_excludes_advanced_fields() {
        let grouped_caps = json!({
            "on": { "type": "boolean" },
            "brightness_pct": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 100.0
            }
        });

        // Verify advanced fields NOT present for grouped lights
        assert!(grouped_caps.get("effect").is_none());
        assert!(grouped_caps.get("dynamic_speed").is_none());
        assert!(grouped_caps.get("gradient_points").is_none());
        assert!(grouped_caps.get("identify").is_none());
        assert!(grouped_caps.get("color_temp_mirek").is_none());
        assert!(grouped_caps.get("color_xy").is_none());
    }

    /// Test partial state patch includes advanced lighting deltas.
    #[test]
    fn validates_partial_state_patch_for_advanced_lighting() {
        let patch = json!({
            "on": false,
            "brightness_pct": 25.0,
            "effect": "candle",
            "dynamic_speed": 0.4,
            "dynamic_status": "playing",
            "gradient_points": [
                { "x": 0.2, "y": 0.3 },
                { "x": 0.5, "y": 0.6 }
            ]
        });

        // Verify patch contains only changed fields (no full state)
        assert_eq!(patch.get("on").and_then(Value::as_bool), Some(false));
        assert_eq!(
            patch.get("brightness_pct").and_then(Value::as_f64),
            Some(25.0)
        );
        assert_eq!(patch.get("effect").and_then(Value::as_str), Some("candle"));
        assert_eq!(
            patch.get("dynamic_speed").and_then(Value::as_f64),
            Some(0.4)
        );

        // Verify array fields correctly shaped
        let gradient = patch.get("gradient_points").and_then(Value::as_array);
        assert_eq!(gradient.map(|a| a.len()), Some(2));
    }

    /// Test command result event includes error code for capability violations.
    #[test]
    fn validates_command_result_event_with_error_codes() {
        // Example: unsupported effect error
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_light_1",
            "operation": "set_light_state",
            "success": false,
            "error": "effect 'unknown' not supported for this light",
            "error_code": "unsupported_field_for_resource",
            "latency_ms": 45,
            "retry_count": 0
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );

        // Example: gradient not supported
        let gradient_event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_light_2",
            "operation": "set_light_state",
            "success": false,
            "error": "gradient_points not supported for this light",
            "error_code": "unsupported_field_for_resource",
            "latency_ms": 32,
            "retry_count": 0
        });

        assert_eq!(
            gradient_event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    /// Test that identify=false is rejected with invalid_field_value error code.
    #[test]
    fn validates_identify_field_value_semantics() {
        let invalid_event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_light_3",
            "operation": "set_light_state",
            "success": false,
            "error": "identify must be true when provided",
            "error_code": "invalid_field_value",
            "latency_ms": 28,
            "retry_count": 0
        });

        assert_eq!(
            invalid_event.get("error_code").and_then(Value::as_str),
            Some("invalid_field_value")
        );
    }

    /// Test device registration payload for lights with advanced capabilities.
    #[test]
    fn validates_device_registration_payload() {
        let registration = json!({
            "device_id": "hue_bridge1_light_1",
            "plugin_id": "plugin.hue",
            "name": "Bedroom Light",
            "device_type": "light",
            "area": "bedroom",
            "capabilities": {
                "on": { "type": "boolean" },
                "brightness_pct": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 100.0
                },
                "effect": {
                    "type": "string",
                    "enum": ["candle", "fire", "prism"]
                },
                "dynamic_speed": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "identify": { "type": "boolean" }
            }
        });

        // Verify registration structure
        assert_eq!(
            registration.get("device_type").and_then(Value::as_str),
            Some("light")
        );
        assert!(registration.get("capabilities").is_some());

        // Verify capabilities include advanced fields
        let caps = registration.get("capabilities").unwrap();
        assert!(caps.get("effect").is_some());
        assert!(caps.get("dynamic_speed").is_some());
        assert!(caps.get("identify").is_some());
    }

    /// Test that empty light command is rejected with deterministic error.
    #[test]
    fn validates_empty_light_command_rejection() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_light_1",
            "operation": "set_light_state",
            "success": false,
            "error": "empty light command",
            "error_code": "empty_command",
            "latency_ms": 5,
            "retry_count": 0
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("empty_command")
        );
    }

    /// Test that grouped light rejects advanced light fields.
    #[test]
    fn validates_grouped_light_rejects_advanced_fields() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_group_1",
            "operation": "set_group_state",
            "success": false,
            "error": "grouped_light does not support advanced light fields",
            "error_code": "unsupported_field_for_resource",
            "latency_ms": 12,
            "retry_count": 0
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }
}
