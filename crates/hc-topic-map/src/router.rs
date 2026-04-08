//! Ecosystem router.
//!
//! Matches incoming MQTT topics against all loaded ecosystem profiles and
//! returns translated HomeCore events. Also handles the outbound path:
//! HomeCore cmd → native device command topic.

use crate::coerce;
use crate::pattern::{match_segments, parse_pattern, render_template, sanitize_id, Segment};
use crate::profile::{AvailabilityTopicConfig, CmdTopicConfig, EcosystemProfile, StateTopicConfig};
use anyhow::{anyhow, Result};
use rhai::{Engine, Scope, AST};
use serde_json::{Map, Value};
use std::collections::HashMap;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// What the router produces for an inbound MQTT message.
#[derive(Debug)]
pub enum InboundResult {
    /// A state update — write to `homecore/devices/{device_id}/state[/partial]`.
    State {
        device_id: String,
        payload: Value,
        /// True → publish to `.../state/partial` (merge-patch).
        /// False → publish to `.../state` (full replace).
        partial: bool,
    },
    /// An availability update — set `device.available`.
    Availability { device_id: String, available: bool },
}

/// What the router produces for an outbound HomeCore cmd.
#[derive(Debug)]
pub struct OutboundResult {
    pub target_topic: String,
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Compiled entries
// ---------------------------------------------------------------------------

struct CompiledState {
    segments: Vec<Segment>,
    profile_prefix: String,
    config: StateTopicConfig,
}

struct CompiledAvailability {
    segments: Vec<Segment>,
    profile_prefix: String,
    config: AvailabilityTopicConfig,
}

struct CompiledCmd {
    segments: Vec<Segment>,
    config: CmdTopicConfig,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub struct EcosystemRouter {
    state_entries: Vec<CompiledState>,
    availability_entries: Vec<CompiledAvailability>,
    cmd_entries: Vec<CompiledCmd>,
    rhai_engine: Engine,
    rhai_ast: Option<AST>,
}

impl EcosystemRouter {
    /// Build a router from a list of parsed ecosystem profiles.
    /// `rhai_source` may contain custom transform functions.
    pub fn new(profiles: Vec<EcosystemProfile>, rhai_source: Option<&str>) -> Result<Self> {
        let mut state_entries = Vec::new();
        let mut availability_entries = Vec::new();
        let mut cmd_entries = Vec::new();

        for profile in profiles {
            let prefix = profile.prefix.clone();

            for sc in profile.state_topics {
                state_entries.push(CompiledState {
                    segments: parse_pattern(&sc.pattern),
                    profile_prefix: prefix.clone(),
                    config: sc,
                });
            }
            for ac in profile.availability_topics {
                availability_entries.push(CompiledAvailability {
                    segments: parse_pattern(&ac.pattern),
                    profile_prefix: prefix.clone(),
                    config: ac,
                });
            }
            for cc in profile.cmd_topics {
                cmd_entries.push(CompiledCmd {
                    segments: parse_pattern(&cc.source),
                    config: cc,
                });
            }
        }

        let mut rhai_engine = Engine::new();
        rhai_engine.set_max_operations(100_000);
        rhai_engine.set_max_call_levels(16);

        let rhai_ast = match rhai_source {
            Some(src) => Some(
                rhai_engine
                    .compile(src)
                    .map_err(|e| anyhow!("Rhai compile error: {e}"))?,
            ),
            None => None,
        };

        Ok(Self {
            state_entries,
            availability_entries,
            cmd_entries,
            rhai_engine,
            rhai_ast,
        })
    }

    // -----------------------------------------------------------------------
    // Inbound routing
    // -----------------------------------------------------------------------

    /// Route an inbound MQTT message. Returns `None` if no profile matches.
    pub fn route_inbound(&self, topic: &str, payload: &[u8]) -> Result<Option<InboundResult>> {
        // Try state topics first.
        for entry in &self.state_entries {
            if let Some(vars) = match_segments(&entry.segments, topic) {
                let result = self.apply_state(entry, &vars, payload)?;
                debug!(%topic, device_id = %result_device_id(&result), "State topic matched");
                return Ok(Some(result));
            }
        }
        // Try availability topics.
        for entry in &self.availability_entries {
            if let Some(vars) = match_segments(&entry.segments, topic) {
                if let Some(result) =
                    self.apply_availability(&entry.config, &entry.profile_prefix, &vars, payload)?
                {
                    debug!(%topic, "Availability topic matched");
                    return Ok(Some(result));
                }
            }
        }
        Ok(None)
    }

    fn apply_state(
        &self,
        entry: &CompiledState,
        vars: &HashMap<String, String>,
        raw_payload: &[u8],
    ) -> Result<InboundResult> {
        let config = &entry.config;
        let prefix = &entry.profile_prefix;

        // Resolve device ID.
        let device_id = resolve_device_id(config, prefix, vars);

        // Apply optional Rhai transform on the raw payload first.
        let payload_bytes = if let Some(fn_name) = &config.transform {
            self.run_rhai(fn_name, raw_payload)?
        } else {
            raw_payload.to_vec()
        };

        // Parse payload.
        let raw_str = std::str::from_utf8(&payload_bytes)
            .map_err(|_| anyhow!("Non-UTF-8 payload on {}", config.pattern))?
            .trim();

        let mut json_value: Value = if let Ok(v) = serde_json::from_str(raw_str) {
            v
        } else {
            // Non-JSON scalar — wrap as string for now.
            Value::String(raw_str.to_string())
        };

        // If `attribute` is set, wrap scalar in a JSON object.
        if let Some(attr_template) = &config.attribute {
            let attr_name = render_template(attr_template, vars);

            if config.coerce_scalar {
                json_value = coerce::coerce_scalar_auto(json_value);
            }
            if let Some(coercion) = config.coerce.get(&attr_name) {
                json_value = coerce::apply(coercion, json_value)?;
            }
            // value_map: translate scalar values to canonical representations.
            if !config.value_map.is_empty() {
                let key = match &json_value {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    _ => json_value.to_string(),
                };
                if let Some(mapped) = config.value_map.get(&key) {
                    json_value = mapped.clone();
                }
            }
            let mut obj = Map::new();
            obj.insert(attr_name, json_value);
            return Ok(InboundResult::State {
                device_id,
                payload: Value::Object(obj),
                partial: true, // scalar topics are always partial updates
            });
        }

        // Full JSON object — apply field_map and coercions.
        let mapped = apply_field_map(&json_value, &config.field_map, &config.coerce)?;

        // Determine partial flag:
        //   - explicit profile override wins if set
        //   - scalar attribute wraps (handled above) are always partial
        //   - field_map topics without explicit partial default to full replace
        let partial = config.partial.unwrap_or(config.attribute.is_some());

        Ok(InboundResult::State {
            device_id,
            payload: mapped,
            partial,
        })
    }

    fn apply_availability(
        &self,
        config: &AvailabilityTopicConfig,
        prefix: &str,
        vars: &HashMap<String, String>,
        raw_payload: &[u8],
    ) -> Result<Option<InboundResult>> {
        let device_var = vars
            .get("device")
            .or_else(|| vars.get("nodeId"))
            .or_else(|| vars.values().next());

        let device_id = match device_var {
            Some(d) => format!("{}{}", prefix, sanitize_id(d)),
            None => return Ok(None),
        };

        let raw_str = std::str::from_utf8(raw_payload)
            .unwrap_or("")
            .trim()
            .to_string();

        let available = match config.payload.as_deref() {
            Some("raw_bool") => raw_str.eq_ignore_ascii_case("true") || raw_str == "1",
            _ => {
                // Try json_field extraction first.
                let lookup_key = if let Some(field) = &config.json_field {
                    if let Ok(obj) = serde_json::from_str::<Value>(&raw_str) {
                        obj.get(field)
                            .and_then(|v| v.as_str())
                            .unwrap_or(&raw_str)
                            .to_string()
                    } else {
                        raw_str.clone()
                    }
                } else {
                    raw_str.clone()
                };

                // Look up in value_map.
                config.value_map.get(&lookup_key).copied().unwrap_or(false)
            }
        };

        Ok(Some(InboundResult::Availability {
            device_id,
            available,
        }))
    }

    // -----------------------------------------------------------------------
    // Outbound routing
    // -----------------------------------------------------------------------

    /// Route an outbound HomeCore cmd. Returns `None` if no profile matches,
    /// or `Some(vec)` with one result per native publish (alias_reverse may
    /// produce one publish per attribute in the cmd payload).
    pub fn route_outbound(
        &self,
        topic: &str,
        payload: &[u8],
    ) -> Result<Option<Vec<OutboundResult>>> {
        for entry in &self.cmd_entries {
            if let Some(vars) = match_segments(&entry.segments, topic) {
                let results = self.apply_cmd(entry, &vars, payload)?;
                debug!(%topic, count = results.len(), "Cmd topic matched");
                return Ok(Some(results));
            }
        }
        Ok(None)
    }

    fn apply_cmd(
        &self,
        entry: &CompiledCmd,
        vars: &HashMap<String, String>,
        raw_payload: &[u8],
    ) -> Result<Vec<OutboundResult>> {
        let config = &entry.config;

        // --- Standard single-target routing ---

        let target_topic = config
            .target
            .as_deref()
            .map(|t| render_template(t, vars))
            .ok_or_else(|| anyhow!("cmd_topics entry has no target"))?;

        // Apply optional Rhai transform on the full payload.
        if let Some(fn_name) = &config.transform {
            let out = self.run_rhai(fn_name, raw_payload)?;
            return Ok(vec![OutboundResult {
                target_topic,
                payload: out,
            }]);
        }

        // Parse HomeCore cmd payload as JSON.
        let cmd_value: Value = serde_json::from_slice(raw_payload)
            .map_err(|e| anyhow!("cmd payload is not valid JSON: {e}"))?;

        // For Shelly Gen2 RPC commands.
        if let Some(method) = &config.rpc_method {
            let out_payload =
                build_rpc_payload(method, config.rpc_id, &cmd_value, &config.field_map)?;
            return Ok(vec![OutboundResult {
                target_topic,
                payload: serde_json::to_vec(&out_payload)?,
            }]);
        }

        // If `attribute` is set, extract that single field and publish its scalar value.
        if let Some(attr_name) = &config.attribute {
            let attr_val = cmd_value.get(attr_name).cloned().unwrap_or(Value::Null);
            let coerced = if let Some(coercion) = config.coerce.get(attr_name) {
                coerce::apply(coercion, attr_val)?
            } else {
                attr_val
            };
            return Ok(vec![OutboundResult {
                target_topic,
                payload: value_to_bytes(&coerced),
            }]);
        }

        // Full JSON object: rename and coerce.
        let mapped = apply_field_map_cmd(&cmd_value, &config.field_map, &config.coerce)?;
        Ok(vec![OutboundResult {
            target_topic,
            payload: serde_json::to_vec(&mapped)?,
        }])
    }

    // -----------------------------------------------------------------------
    // Rhai helper
    // -----------------------------------------------------------------------

    fn run_rhai(&self, fn_name: &str, payload: &[u8]) -> Result<Vec<u8>> {
        let ast = self
            .rhai_ast
            .as_ref()
            .ok_or_else(|| anyhow!("Rhai transform '{fn_name}' called but no script loaded"))?;
        let payload_str = std::str::from_utf8(payload)
            .map_err(|_| anyhow!("Non-UTF-8 payload for Rhai transform '{fn_name}'"))?
            .to_string();
        let result: rhai::Dynamic = self
            .rhai_engine
            .call_fn(&mut Scope::new(), ast, fn_name, (payload_str,))
            .map_err(|e| anyhow!("Rhai '{fn_name}': {e}"))?;
        Ok(result.to_string().into_bytes())
    }
}

// ---------------------------------------------------------------------------
// Field mapping helpers
// ---------------------------------------------------------------------------

/// Apply field_map (with dot-notation source keys) and coercions to an inbound
/// JSON payload, returning a new JSON object with HomeCore canonical attribute names.
/// Keys not present in field_map are passed through unchanged.
fn apply_field_map(
    payload: &Value,
    field_map: &HashMap<String, String>,
    coerce_map: &HashMap<String, String>,
) -> Result<Value> {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => return Ok(payload.clone()),
    };

    let mut result = Map::new();

    if field_map.is_empty() {
        // No mapping — pass all keys through unchanged.
        for (k, v) in obj {
            result.insert(k.clone(), v.clone());
        }
    } else {
        // Map only the declared keys; discard everything else.
        for (src_key, dst_key) in field_map {
            if let Some(raw_val) = extract_path(payload, src_key) {
                let coerced = if let Some(coercion) = coerce_map.get(dst_key) {
                    coerce::apply(coercion, raw_val).unwrap_or_else(|e| {
                        warn!(coercion, error = %e, "Coercion failed; using raw value");
                        payload.clone()
                    })
                } else {
                    raw_val
                };
                result.insert(dst_key.clone(), coerced);
            }
        }
    }

    Ok(Value::Object(result))
}

/// Apply field_map and coercions for the outbound (cmd) direction.
/// field_map here renames HomeCore attribute names to ecosystem keys.
fn apply_field_map_cmd(
    payload: &Value,
    field_map: &HashMap<String, String>,
    coerce_map: &HashMap<String, String>,
) -> Result<Value> {
    let obj = match payload.as_object() {
        Some(o) => o,
        None => return Ok(payload.clone()),
    };

    let mut result = Map::new();

    for (hc_key, v) in obj {
        // Rename to ecosystem key if a mapping exists.
        let eco_key = field_map
            .get(hc_key)
            .cloned()
            .unwrap_or_else(|| hc_key.clone());
        let coerced = if let Some(coercion) = coerce_map.get(&eco_key) {
            coerce::apply(coercion, v.clone()).unwrap_or_else(|e| {
                warn!(coercion, error = %e, "Cmd coercion failed; using raw value");
                v.clone()
            })
        } else {
            v.clone()
        };
        result.insert(eco_key, coerced);
    }

    Ok(Value::Object(result))
}

/// Extract a value from a JSON object using dot-notation paths.
/// `"aenergy.total"` → `payload["aenergy"]["total"]`.
fn extract_path(value: &Value, path: &str) -> Option<Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// Build a Shelly Gen2 JSON-RPC payload.
fn build_rpc_payload(
    method: &str,
    rpc_id: Option<u32>,
    cmd: &Value,
    field_map: &HashMap<String, String>,
) -> Result<Value> {
    // Rename HomeCore keys to ecosystem keys for params.
    let mut params: Map<String, Value> = Map::new();
    if let Some(obj) = cmd.as_object() {
        for (hc_key, v) in obj {
            let eco_key = field_map
                .get(hc_key)
                .cloned()
                .unwrap_or_else(|| hc_key.clone());
            params.insert(eco_key, v.clone());
        }
    }
    if let Some(id) = rpc_id {
        params.insert("id".into(), Value::Number(id.into()));
    }
    Ok(serde_json::json!({
        "id":     1,
        "src":    "homecore",
        "method": method,
        "params": params,
    }))
}

/// Serialize a JSON value to bytes. Strings are emitted without quotes.
fn value_to_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        other => other.to_string().into_bytes(),
    }
}

/// Helper to extract device_id from an InboundResult for logging.
fn result_device_id(r: &InboundResult) -> &str {
    match r {
        InboundResult::State { device_id, .. } => device_id,
        InboundResult::Availability { device_id, .. } => device_id,
    }
}

/// Resolve the HomeCore device ID for a state topic match.
fn resolve_device_id(
    config: &StateTopicConfig,
    prefix: &str,
    vars: &HashMap<String, String>,
) -> String {
    if let Some(tmpl) = &config.device_id {
        // Explicit override template from profile.
        sanitize_id(&render_template(tmpl, vars))
    } else {
        // Default: prefix + first capture variable (prefer "device" then "nodeId").
        let var_val = vars
            .get("device")
            .or_else(|| vars.get("nodeId"))
            .or_else(|| vars.values().next())
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        format!("{}{}", prefix, sanitize_id(var_val))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::load_profile_str;

    // Minimal Zigbee2MQTT profile for testing.
    const Z2M_PROFILE: &str = r#"
[ecosystem]
name   = "zigbee2mqtt"
prefix = "zigbee_"

[[ecosystem.state_topics]]
pattern = "zigbee2mqtt/{device}"
  [ecosystem.state_topics.field_map]
  state      = "on"
  brightness = "brightness"
  [ecosystem.state_topics.coerce]
  on = "onoff_to_bool"

[[ecosystem.availability_topics]]
pattern    = "zigbee2mqtt/{device}/availability"
json_field = "state"
  [ecosystem.availability_topics.value_map]
  online  = true
  offline = false

[[ecosystem.cmd_topics]]
source = "homecore/devices/zigbee_{device}/cmd"
target = "zigbee2mqtt/{device}/set"
  [ecosystem.cmd_topics.field_map]
  on = "state"
  [ecosystem.cmd_topics.coerce]
  state = "bool_to_onoff"
"#;

    // Minimal Shelly Gen1 profile for testing.
    const SHELLY_PROFILE: &str = r#"
[ecosystem]
name   = "shelly-gen1"
prefix = "shelly_"

[[ecosystem.state_topics]]
pattern   = "shellies/{device}/relay/0"
attribute = "on"
  [ecosystem.state_topics.coerce]
  on = "01_to_bool"

[[ecosystem.availability_topics]]
pattern = "shellies/{device}/online"
payload = "raw_bool"

[[ecosystem.cmd_topics]]
source    = "homecore/devices/shelly_{device}/cmd"
target    = "shellies/{device}/relay/0/command"
attribute = "on"
  [ecosystem.cmd_topics.coerce]
  on = "bool_to_01"
"#;

    fn make_router(toml_src: &str) -> EcosystemRouter {
        let profile = load_profile_str(toml_src).unwrap();
        EcosystemRouter::new(vec![profile], None).unwrap()
    }

    // --- Z2M inbound state ---

    #[test]
    fn z2m_state_on() {
        let router = make_router(Z2M_PROFILE);
        let payload = br#"{"state":"ON","brightness":128}"#;
        let result = router
            .route_inbound("zigbee2mqtt/living_room_light", payload)
            .unwrap()
            .unwrap();
        match result {
            InboundResult::State {
                device_id, payload, ..
            } => {
                assert_eq!(device_id, "zigbee_living_room_light");
                assert_eq!(payload["on"], true);
                assert_eq!(payload["brightness"], 128);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn z2m_state_off() {
        let router = make_router(Z2M_PROFILE);
        let payload = br#"{"state":"OFF"}"#;
        let result = router
            .route_inbound("zigbee2mqtt/bedroom_switch", payload)
            .unwrap()
            .unwrap();
        match result {
            InboundResult::State { payload, .. } => {
                assert_eq!(payload["on"], false);
            }
            _ => panic!("Expected State"),
        }
    }

    // --- Z2M availability ---

    #[test]
    fn z2m_availability_online() {
        let router = make_router(Z2M_PROFILE);
        let result = router
            .route_inbound(
                "zigbee2mqtt/living_room_light/availability",
                br#"{"state":"online"}"#,
            )
            .unwrap()
            .unwrap();
        match result {
            InboundResult::Availability {
                device_id,
                available,
            } => {
                assert_eq!(device_id, "zigbee_living_room_light");
                assert!(available);
            }
            _ => panic!("Expected Availability"),
        }
    }

    #[test]
    fn z2m_availability_offline() {
        let router = make_router(Z2M_PROFILE);
        let result = router
            .route_inbound(
                "zigbee2mqtt/living_room_light/availability",
                br#"{"state":"offline"}"#,
            )
            .unwrap()
            .unwrap();
        match result {
            InboundResult::Availability { available, .. } => assert!(!available),
            _ => panic!("Expected Availability"),
        }
    }

    // --- Z2M outbound cmd ---

    #[test]
    fn z2m_cmd_on() {
        let router = make_router(Z2M_PROFILE);
        let results = router
            .route_outbound(
                "homecore/devices/zigbee_living_room_light/cmd",
                br#"{"on":true}"#,
            )
            .unwrap()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_topic, "zigbee2mqtt/living_room_light/set");
        let body: Value = serde_json::from_slice(&results[0].payload).unwrap();
        assert_eq!(body["state"], "ON");
    }

    #[test]
    fn z2m_cmd_off() {
        let router = make_router(Z2M_PROFILE);
        let results = router
            .route_outbound(
                "homecore/devices/zigbee_living_room_light/cmd",
                br#"{"on":false}"#,
            )
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_slice(&results[0].payload).unwrap();
        assert_eq!(body["state"], "OFF");
    }

    // --- Shelly Gen1 scalar state ---

    #[test]
    fn shelly_scalar_on() {
        let router = make_router(SHELLY_PROFILE);
        let result = router
            .route_inbound("shellies/myplug/relay/0", b"1")
            .unwrap()
            .unwrap();
        match result {
            InboundResult::State {
                device_id,
                payload,
                partial,
                ..
            } => {
                assert_eq!(device_id, "shelly_myplug");
                assert_eq!(payload["on"], true);
                assert!(partial);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn shelly_scalar_off() {
        let router = make_router(SHELLY_PROFILE);
        let result = router
            .route_inbound("shellies/myplug/relay/0", b"0")
            .unwrap()
            .unwrap();
        match result {
            InboundResult::State { payload, .. } => assert_eq!(payload["on"], false),
            _ => panic!("Expected State"),
        }
    }

    // --- Shelly Gen1 availability ---

    #[test]
    fn shelly_availability_raw_bool() {
        let router = make_router(SHELLY_PROFILE);
        let result = router
            .route_inbound("shellies/myplug/online", b"true")
            .unwrap()
            .unwrap();
        match result {
            InboundResult::Availability { available, .. } => assert!(available),
            _ => panic!("Expected Availability"),
        }
    }

    // --- Shelly Gen1 outbound cmd ---

    #[test]
    fn shelly_cmd_on() {
        let router = make_router(SHELLY_PROFILE);
        let results = router
            .route_outbound("homecore/devices/shelly_myplug/cmd", br#"{"on":true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_topic, "shellies/myplug/relay/0/command");
        assert_eq!(results[0].payload, b"1");
    }

    #[test]
    fn shelly_cmd_off() {
        let router = make_router(SHELLY_PROFILE);
        let results = router
            .route_outbound("homecore/devices/shelly_myplug/cmd", br#"{"on":false}"#)
            .unwrap()
            .unwrap();
        assert_eq!(results[0].payload, b"0");
    }

    // --- No match ---

    #[test]
    fn no_match_returns_none() {
        let router = make_router(Z2M_PROFILE);
        assert!(router
            .route_inbound("unknown/topic/here", b"{}")
            .unwrap()
            .is_none());
        assert!(router
            .route_outbound("homecore/devices/unknown/cmd", b"{}")
            .unwrap()
            .is_none());
    }
}
