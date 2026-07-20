use serde_json::{json, Value};

use crate::hue::models::{BridgeTarget, HueAuxDevice, HueGroupedLight, HueLight, HueScene};

pub fn bridge_state(target: &BridgeTarget, online: bool, summary: Value) -> Value {
    let integration_state = summary
        .get("integration_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let pairing_status = match integration_state {
        "connected" => "paired",
        "auth_required" => "unpaired",
        "unreachable" => "unreachable",
        _ => "unknown",
    };
    json!({
        "bridge_id": target.bridge_id,
        "host": target.host,
        "name": target.name,
        "online": online,
        "integration_state": integration_state,
        "pairing_status": pairing_status,
        "kind": "hue_bridge",
        "summary": summary,
    })
}

pub fn command_result_patch(
    operation: &str,
    success: bool,
    error: Option<&str>,
    error_code: Option<&str>,
) -> Value {
    let mut patch = json!({
        "last_command": operation,
        "last_command_success": success,
    });

    if let Some(err) = error {
        patch["last_command_error"] = json!(err);
    } else {
        patch["last_command_error"] = Value::Null;
    }

    if let Some(code) = error_code {
        patch["last_command_error_code"] = json!(code);
    } else {
        patch["last_command_error_code"] = Value::Null;
    }

    patch
}

pub fn command_result_patch_with_metrics(
    operation: &str,
    success: bool,
    error: Option<&str>,
    error_code: Option<&str>,
    latency_ms: u128,
    retry_count: u32,
) -> Value {
    let mut patch = command_result_patch(operation, success, error, error_code);
    patch["last_command_latency_ms"] = json!(latency_ms);
    patch["last_command_retry_count"] = json!(retry_count);
    patch
}

/// All fields needed to build a `plugin_command_result` event payload.
pub struct CommandResult<'a> {
    pub plugin_id: &'a str,
    pub device_id: &'a str,
    pub operation: &'a str,
    pub success: bool,
    pub error: Option<&'a str>,
    pub error_code: Option<&'a str>,
    pub latency_ms: u128,
    pub retry_count: u32,
}

pub fn command_result_event(r: CommandResult) -> Value {
    let mut payload = json!({
        "plugin_id": r.plugin_id,
        "device_id": r.device_id,
        "operation": r.operation,
        "success": r.success,
        "latency_ms": r.latency_ms,
        "retry_count": r.retry_count,
    });
    if let Some(err) = r.error {
        payload["error"] = json!(err);
    }
    if let Some(code) = r.error_code {
        payload["error_code"] = json!(code);
    }
    payload
}

pub fn bridge_pairing_event(
    plugin_id: &str,
    device_id: &str,
    bridge_id: &str,
    phase: &str,
    success: Option<bool>,
    error: Option<&str>,
) -> Value {
    let mut payload = json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "bridge_id": bridge_id,
        "phase": phase,
    });

    if let Some(v) = success {
        payload["success"] = json!(v);
    }
    if let Some(v) = error {
        payload["error"] = json!(v);
    }

    payload
}

pub fn scene_activated_event(
    plugin_id: &str,
    device_id: &str,
    scene_id: &str,
    source: &str,
) -> Value {
    json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "scene_id": scene_id,
        "source": source,
    })
}

pub fn group_action_event(plugin_id: &str, device_id: &str, group_id: &str, source: &str) -> Value {
    json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "group_id": group_id,
        "source": source,
    })
}

pub fn entertainment_action_event(
    plugin_id: &str,
    device_id: &str,
    config_id: &str,
    active: bool,
    source: &str,
) -> Value {
    json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "config_id": config_id,
        "active": active,
        "action": if active { "start" } else { "stop" },
        "source": source,
    })
}

pub fn entertainment_status_changed_event(
    plugin_id: &str,
    device_id: &str,
    config_id: &str,
    active: Option<bool>,
    status: Option<&str>,
    config_type: Option<&str>,
    source: &str,
) -> Value {
    let mut payload = json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "config_id": config_id,
        "source": source,
    });

    if let Some(v) = active {
        payload["active"] = json!(v);
    }
    if let Some(v) = status {
        payload["status"] = json!(v);
    }
    if let Some(v) = config_type {
        payload["entertainment_type"] = json!(v);
    }

    payload
}

pub fn button_event(
    plugin_id: &str,
    device_id: &str,
    bridge_id: &str,
    resource_id: &str,
    event: &str,
) -> Value {
    json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "bridge_id": bridge_id,
        "resource_id": resource_id,
        "event": event,
    })
}

pub fn rotary_event(
    plugin_id: &str,
    device_id: &str,
    bridge_id: &str,
    resource_id: &str,
    action: Option<&str>,
    direction: Option<&str>,
    steps: Option<i64>,
) -> Value {
    let mut payload = json!({
        "plugin_id": plugin_id,
        "device_id": device_id,
        "bridge_id": bridge_id,
        "resource_id": resource_id,
    });
    if let Some(v) = action {
        payload["action"] = json!(v);
    }
    if let Some(v) = direction {
        payload["direction"] = json!(v);
    }
    if let Some(v) = steps {
        payload["steps"] = json!(v);
    }
    payload
}

pub fn light_state(light: &HueLight) -> Value {
    let mut state = json!({
        "kind": "hue_light",
        "bridge_id": light.bridge_id,
        "resource_id": light.resource_id,
        "on": light.on,
        "supports_dimming": light.supports_dimming,
        "supports_color_xy": light.supports_color_xy,
        "supports_gradient": light.supports_gradient,
        "supports_identify": light.supports_identify,
    });

    if let Some(brightness) = light.brightness_pct {
        state["brightness_pct"] = json!(brightness);
    }
    if let Some(mirek) = light.color_temp_mirek {
        state["color_temp_mirek"] = json!(mirek);
    }
    if let Some((x, y)) = light.color_xy {
        state["color_xy"] = json!({ "x": x, "y": y });
    }
    if let Some(effect) = &light.effect {
        state["effect"] = json!(effect);
    }
    if !light.effect_values.is_empty() {
        state["effect_values"] = json!(light.effect_values);
    }
    if let Some(status) = &light.dynamic_status {
        state["dynamic_status"] = json!(status);
    }
    if let Some(speed) = light.dynamic_speed {
        state["dynamic_speed"] = json!(speed);
    }
    if !light.gradient_points.is_empty() {
        let points = light
            .gradient_points
            .iter()
            .map(|(x, y)| json!({ "x": x, "y": y }))
            .collect::<Vec<_>>();
        state["gradient_points"] = Value::Array(points);
    }
    if let Some(min) = light.mirek_min {
        state["color_temp_mirek_min"] = json!(min);
    }
    if let Some(max) = light.mirek_max {
        state["color_temp_mirek_max"] = json!(max);
    }

    state
}

pub fn light_capabilities(light: &HueLight) -> Value {
    let mut caps = serde_json::Map::new();
    caps.insert("on".to_string(), json!({ "type": "boolean" }));

    if light.supports_dimming {
        caps.insert(
            "brightness_pct".to_string(),
            json!({ "type": "number", "minimum": 0.0, "maximum": 100.0 }),
        );
    }

    if let (Some(min), Some(max)) = (light.mirek_min, light.mirek_max) {
        caps.insert(
            "color_temp_mirek".to_string(),
            json!({ "type": "integer", "minimum": min, "maximum": max }),
        );
    } else if light.color_temp_mirek.is_some() {
        caps.insert(
            "color_temp_mirek".to_string(),
            json!({ "type": "integer", "minimum": 153, "maximum": 500 }),
        );
    }

    if light.supports_color_xy {
        caps.insert(
            "color_xy".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "x": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "y": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                }
            }),
        );
    }

    if !light.effect_values.is_empty() {
        caps.insert(
            "effect".to_string(),
            json!({ "type": "string", "enum": light.effect_values }),
        );
    }

    if light.dynamic_status.is_some() || light.dynamic_speed.is_some() {
        caps.insert(
            "dynamic_speed".to_string(),
            json!({ "type": "number", "minimum": 0.0, "maximum": 1.0 }),
        );
    }

    if light.supports_gradient {
        caps.insert(
            "gradient_points".to_string(),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                        "y": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                    }
                }
            }),
        );
    }

    if light.supports_identify {
        caps.insert("identify".to_string(), json!({ "type": "boolean" }));
    }

    Value::Object(caps)
}

pub fn grouped_light_state(group: &HueGroupedLight) -> Value {
    let mut state = json!({
        "kind": "hue_grouped_light",
        "bridge_id": group.bridge_id,
        "resource_id": group.resource_id,
    });

    if let Some(area) = &group.area {
        state["area"] = json!(area);
    }
    if let Some(kind) = &group.group_kind {
        state["group_kind"] = json!(kind);
    }

    if let Some(on) = group.on {
        state["on"] = json!(on);
    }
    if let Some(brightness) = group.brightness_pct {
        state["brightness_pct"] = json!(brightness);
    }

    state
}

pub fn grouped_light_capabilities() -> Value {
    json!({
        "on": { "type": "boolean" },
        "brightness_pct": { "type": "number", "minimum": 0.0, "maximum": 100.0 }
    })
}

pub fn scene_state(scene: &HueScene) -> Value {
    let mut state = json!({
        "kind": "hue_scene",
        "bridge_id": scene.bridge_id,
        "resource_id": scene.resource_id,
        "name": scene.name,
    });

    if let Some(group_rid) = &scene.group_rid {
        state["group_rid"] = json!(group_rid);
    }
    if let Some(group_name) = &scene.group_name {
        state["group_name"] = json!(group_name);
    }
    if let Some(area) = &scene.area {
        state["area"] = json!(area);
    }
    if let Some(kind) = &scene.group_kind {
        state["group_kind"] = json!(kind);
    }
    if let Some(active) = scene.active {
        state["active"] = json!(active);
    }

    state
}

pub fn scene_capabilities() -> Value {
    json!({
        "action": {
            "type": "string",
            "enum": ["activate_scene"]
        }
    })
}

pub fn aux_state(aux: &HueAuxDevice) -> Value {
    let mut state = json!({
        "kind": format!("hue_{}", aux.resource_type),
        "bridge_id": aux.bridge_id,
        "resource_type": aux.resource_type,
        "resource_id": aux.resource_id,
    });

    if let Some(obj) = aux.attributes.as_object() {
        for (k, v) in obj {
            state[k] = v.clone();
        }
    }

    state
}

pub fn aux_capabilities(aux: &HueAuxDevice) -> Value {
    let specific = match aux.resource_type.as_str() {
        "motion" => Some(json!({
            "motion": { "type": "boolean" },
            "motion_valid": { "type": "boolean" },
            "enabled": { "type": "boolean" },
            "motion_sensitivity": { "type": "integer", "minimum": 0, "maximum": 2 }
        })),
        "grouped_motion" => Some(json!({
            "motion": { "type": "boolean" },
            "motion_valid": { "type": "boolean" },
            "enabled": { "type": "boolean" },
            "motion_sensitivity": { "type": "integer", "minimum": 0, "maximum": 2 },
            "temperature": { "type": "number" },
            "temperature_c": { "type": "number" },
            "temperature_f": { "type": "number" },
            "temperature_valid": { "type": "boolean" },
            "temperature_unit": { "type": "string", "enum": ["C", "F"] },
            "illuminance": { "type": "number", "minimum": 0.0 },
            "illuminance_raw": { "type": "number", "minimum": 0.0 },
            "illuminance_lux": { "type": "number", "minimum": 0.0 },
            "illuminance_valid": { "type": "boolean" },
            "illuminance_unit": { "type": "string", "enum": ["lux", "raw"] }
        })),
        "temperature" => Some(json!({
            "temperature": { "type": "number" },
            "temperature_c": { "type": "number" },
            "temperature_f": { "type": "number" },
            "temperature_valid": { "type": "boolean" },
            "enabled": { "type": "boolean" },
            "temperature_unit": { "type": "string", "enum": ["C", "F"] }
        })),
        "light_level" | "grouped_light_level" => Some(json!({
            "illuminance": { "type": "number", "minimum": 0.0 },
            "illuminance_raw": { "type": "number", "minimum": 0.0 },
            "illuminance_lux": { "type": "number", "minimum": 0.0 },
            "illuminance_valid": { "type": "boolean" },
            "enabled": { "type": "boolean" },
            "illuminance_unit": { "type": "string", "enum": ["lux", "raw"] }
        })),
        "contact" => Some(json!({
            "contact_state": { "type": "string" },
            "enabled": { "type": "boolean" },
            "tampered": { "type": "boolean" }
        })),
        "device_power" => Some(json!({
            "battery_pct": { "type": "number", "minimum": 0.0, "maximum": 100.0 },
            "battery_state": { "type": "string" }
        })),
        "zigbee_connectivity" => Some(json!({
            "connectivity_status": { "type": "string" }
        })),
        "button" => Some(json!({
            "button_event": { "type": "string" },
            "button_updated": { "type": "string" },
            "button_repeat_interval_ms": { "type": "integer", "minimum": 0 }
        })),
        "relative_rotary" => Some(json!({
            "rotary_action": { "type": "string" },
            "rotary_direction": { "type": "string" },
            "rotary_steps": { "type": "integer" },
            "rotary_updated": { "type": "string" }
        })),
        "entertainment_configuration" => Some(json!({
            "entertainment_active": { "type": "boolean" },
            "entertainment_status": { "type": "string", "enum": ["idle", "streaming", "interrupted", "unavailable"] },
            "entertainment_type": { "type": "string" },
            "entertainment_name": { "type": "string" },
            "entertainment_owner": { "type": "string" },
            "entertainment_channel_count": { "type": "integer", "minimum": 0 },
            "entertainment_segment_count": { "type": "integer", "minimum": 0 },
            "entertainment_proxy_type": { "type": "string" },
            "action": { "type": "string", "enum": ["activate_entertainment", "deactivate_entertainment"] }
        })),
        "bridge_home" => Some(json!({
            "child_count": { "type": "integer", "minimum": 0 },
            "id_v1": { "type": "string" }
        })),
        _ => None,
    };

    if let Some(v) = specific {
        return v;
    }

    let mut caps = serde_json::Map::new();
    if let Some(obj) = aux.attributes.as_object() {
        for (key, value) in obj {
            let t = match value {
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Object(_) => "object",
                Value::Array(_) => "array",
                Value::Null => "null",
            };
            caps.insert(key.clone(), json!({ "type": t }));
        }
    }
    Value::Object(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hue::models::HueLight;
    use serde_json::json;

    fn aux(resource_type: &str) -> HueAuxDevice {
        HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
            owner_rid: "owner-1".to_string(),
            resource_type: resource_type.to_string(),
            resource_id: "rid-1".to_string(),
            device_id: "device-1".to_string(),
            name: "test".to_string(),
            attributes: json!({}),
        }
    }

    #[test]
    fn button_and_rotary_capabilities_include_expected_fields() {
        let button_caps = aux_capabilities(&aux("button"));
        assert!(button_caps.get("button_event").is_some());
        assert!(button_caps.get("button_updated").is_some());
        assert!(button_caps.get("button_repeat_interval_ms").is_some());

        let rotary_caps = aux_capabilities(&aux("relative_rotary"));
        assert!(rotary_caps.get("rotary_action").is_some());
        assert!(rotary_caps.get("rotary_direction").is_some());
        assert!(rotary_caps.get("rotary_steps").is_some());
        assert!(rotary_caps.get("rotary_updated").is_some());
    }

    #[test]
    fn grouped_sensor_capabilities_include_compacted_fields() {
        let grouped_motion_caps = aux_capabilities(&aux("grouped_motion"));
        assert!(grouped_motion_caps.get("motion").is_some());
        assert!(grouped_motion_caps.get("temperature_c").is_some());
        assert!(grouped_motion_caps.get("illuminance_lux").is_some());

        let grouped_light_caps = aux_capabilities(&aux("grouped_light_level"));
        assert!(grouped_light_caps.get("illuminance_raw").is_some());
        assert!(grouped_light_caps.get("illuminance_unit").is_some());
    }

    #[test]
    fn entertainment_and_bridge_home_capabilities_include_expected_fields() {
        let entertainment_caps = aux_capabilities(&aux("entertainment_configuration"));
        assert!(entertainment_caps.get("entertainment_active").is_some());
        assert!(entertainment_caps.get("entertainment_status").is_some());
        assert!(entertainment_caps.get("entertainment_type").is_some());
        assert!(entertainment_caps.get("entertainment_name").is_some());
        assert!(entertainment_caps.get("entertainment_owner").is_some());
        assert!(entertainment_caps
            .get("entertainment_channel_count")
            .is_some());
        assert!(entertainment_caps
            .get("entertainment_segment_count")
            .is_some());
        assert!(entertainment_caps.get("entertainment_proxy_type").is_some());

        let bridge_home_caps = aux_capabilities(&aux("bridge_home"));
        assert!(bridge_home_caps.get("child_count").is_some());
        assert!(bridge_home_caps.get("id_v1").is_some());
    }

    #[test]
    fn light_capabilities_include_advanced_fields_when_supported() {
        let light = HueLight {
            area: None,
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_id: "rid-1".to_string(),
            device_id: "dev-1".to_string(),
            name: "Light".to_string(),
            on: true,
            brightness_pct: Some(50.0),
            supports_dimming: true,
            color_temp_mirek: Some(300),
            mirek_min: Some(153),
            mirek_max: Some(500),
            color_xy: Some((0.3, 0.4)),
            supports_color_xy: true,
            effect: Some("candle".to_string()),
            effect_values: vec!["candle".to_string(), "fire".to_string()],
            dynamic_status: Some("dynamic_palette".to_string()),
            dynamic_speed: Some(0.6),
            gradient_points: vec![(0.2, 0.3), (0.4, 0.5)],
            supports_gradient: true,
            supports_identify: true,
        };

        let caps = light_capabilities(&light);
        assert!(caps.get("effect").is_some());
        assert!(caps.get("dynamic_speed").is_some());
        assert!(caps.get("gradient_points").is_some());
        assert!(caps.get("identify").is_some());

        let state = light_state(&light);
        assert_eq!(state.get("effect").and_then(Value::as_str), Some("candle"));
        assert_eq!(
            state.get("dynamic_speed").and_then(Value::as_f64),
            Some(0.6)
        );
        assert_eq!(
            state
                .get("gradient_points")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            state.get("supports_identify").and_then(Value::as_bool),
            Some(true)
        );
    }
}
