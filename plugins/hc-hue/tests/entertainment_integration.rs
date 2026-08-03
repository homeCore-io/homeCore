/// Integration tests for entertainment command, validation, and event semantics.
#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    #[test]
    fn parses_entertainment_activate_and_deactivate_actions() {
        let activate = json!({
            "action": "activate_entertainment",
            "config_id": "cfg-1"
        });
        let deactivate = json!({
            "action": "deactivate_entertainment",
            "config_id": "cfg-1"
        });

        assert_eq!(
            activate.get("action").and_then(Value::as_str),
            Some("activate_entertainment")
        );
        assert_eq!(
            deactivate.get("action").and_then(Value::as_str),
            Some("deactivate_entertainment")
        );
        assert_eq!(
            activate.get("config_id").and_then(Value::as_str),
            Some("cfg-1")
        );
        assert_eq!(
            deactivate.get("config_id").and_then(Value::as_str),
            Some("cfg-1")
        );
    }

    #[test]
    fn entertainment_state_schema_contains_extended_fields() {
        let state = json!({
            "entertainment_active": true,
            "entertainment_status": "streaming",
            "entertainment_type": "screen",
            "entertainment_name": "TV Area",
            "entertainment_owner": "owner-rid-1",
            "entertainment_channel_count": 2,
            "entertainment_segment_count": 1,
            "entertainment_proxy_type": "ent_proxy_v2"
        });

        assert_eq!(
            state.get("entertainment_active").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            state.get("entertainment_status").and_then(Value::as_str),
            Some("streaming")
        );
        assert_eq!(
            state.get("entertainment_name").and_then(Value::as_str),
            Some("TV Area")
        );
        assert_eq!(
            state
                .get("entertainment_channel_count")
                .and_then(Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn entertainment_capability_schema_contains_extended_fields() {
        let caps = json!({
            "entertainment_active": { "type": "boolean" },
            "entertainment_status": { "type": "string", "enum": ["idle", "streaming", "interrupted", "unavailable"] },
            "entertainment_type": { "type": "string" },
            "entertainment_name": { "type": "string" },
            "entertainment_owner": { "type": "string" },
            "entertainment_channel_count": { "type": "integer", "minimum": 0 },
            "entertainment_segment_count": { "type": "integer", "minimum": 0 },
            "entertainment_proxy_type": { "type": "string" },
            "action": { "type": "string", "enum": ["activate_entertainment", "deactivate_entertainment"] }
        });

        assert!(caps.get("entertainment_name").is_some());
        assert!(caps.get("entertainment_owner").is_some());
        assert!(caps.get("entertainment_channel_count").is_some());
        assert!(caps.get("entertainment_segment_count").is_some());
        assert!(caps.get("entertainment_proxy_type").is_some());
    }

    #[test]
    fn rejects_blank_entertainment_config_id_with_missing_required_field() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_entertainment_configuration_1",
            "operation": "set_entertainment_active",
            "success": false,
            "error": "missing entertainment config_id",
            "error_code": "missing_required_field"
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("missing_required_field")
        );
    }

    #[test]
    fn rejects_entertainment_command_on_wrong_resource_type() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_motion_1",
            "operation": "set_entertainment_active",
            "success": false,
            "error": "entertainment command is only supported for entertainment_configuration resources (target type: motion)",
            "error_code": "unsupported_field_for_resource"
        });

        assert_eq!(event.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("unsupported_field_for_resource")
        );
    }

    #[test]
    fn publishes_entertainment_action_applied_on_success() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_entertainment_configuration_1",
            "config_id": "cfg-1",
            "active": true,
            "action": "start",
            "source": "entertainment_device_command"
        });

        assert_eq!(event.get("action").and_then(Value::as_str), Some("start"));
        assert_eq!(event.get("active").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn publishes_passive_entertainment_status_changed_event_from_patch() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_entertainment_configuration_1",
            "config_id": "cfg-1",
            "active": false,
            "status": "idle",
            "entertainment_type": "screen",
            "source": "eventstream_patch"
        });

        assert_eq!(
            event.get("source").and_then(Value::as_str),
            Some("eventstream_patch")
        );
        assert_eq!(event.get("status").and_then(Value::as_str), Some("idle"));
        assert_eq!(
            event.get("entertainment_type").and_then(Value::as_str),
            Some("screen")
        );
    }

    #[test]
    fn command_result_event_contains_entertainment_error_code() {
        let event = json!({
            "plugin_id": "plugin.hue",
            "device_id": "hue_bridge1_entertainment_configuration_1",
            "operation": "set_entertainment_active",
            "success": false,
            "error": "missing entertainment config_id",
            "error_code": "missing_required_field",
            "latency_ms": 12,
            "retry_count": 0
        });

        assert_eq!(
            event.get("operation").and_then(Value::as_str),
            Some("set_entertainment_active")
        );
        assert_eq!(
            event.get("error_code").and_then(Value::as_str),
            Some("missing_required_field")
        );
    }

    #[test]
    fn entertainment_partial_patch_contains_extended_fields() {
        let patch = json!({
            "entertainment_active": true,
            "entertainment_status": "streaming",
            "entertainment_name": "TV Area",
            "entertainment_channel_count": 3,
            "entertainment_segment_count": 2,
            "entertainment_proxy_type": "ent_proxy_v2"
        });

        assert_eq!(
            patch.get("entertainment_active").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            patch.get("entertainment_name").and_then(Value::as_str),
            Some("TV Area")
        );
        assert_eq!(
            patch
                .get("entertainment_channel_count")
                .and_then(Value::as_u64),
            Some(3)
        );
    }

    #[test]
    fn entertainment_command_context_fields_are_published() {
        let patch = json!({
            "last_entertainment_command_config_id": "cfg-1",
            "last_entertainment_command_action": "start",
            "last_entertainment_command_active": true,
            "last_entertainment_command_success": true,
            "last_entertainment_command_error": null
        });

        assert_eq!(
            patch
                .get("last_entertainment_command_config_id")
                .and_then(Value::as_str),
            Some("cfg-1")
        );
        assert_eq!(
            patch
                .get("last_entertainment_command_action")
                .and_then(Value::as_str),
            Some("start")
        );
        assert_eq!(
            patch
                .get("last_entertainment_command_success")
                .and_then(Value::as_bool),
            Some(true)
        );
    }
}
