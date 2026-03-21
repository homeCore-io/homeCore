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
        payload:   Value,
        /// True → publish to `.../state/partial` (merge-patch).
        /// False → publish to `.../state` (full replace).
        partial: bool,
        /// Non-None for profiles that require time-windowed aggregation (e.g. Z-Wave).
        /// The caller should buffer partial updates for this device for `aggregate_ms`
        /// milliseconds (debounced) before committing as a single state event.
        aggregate_ms: Option<u64>,
    },
    /// An availability update — set `device.available`.
    Availability {
        device_id: String,
        available: bool,
    },
}

/// What the router produces for an outbound HomeCore cmd.
#[derive(Debug)]
pub struct OutboundResult {
    pub target_topic: String,
    pub payload:      Vec<u8>,
}

// ---------------------------------------------------------------------------
// Compiled entries
// ---------------------------------------------------------------------------

struct CompiledState {
    segments:          Vec<Segment>,
    profile_prefix:    String,
    config:            StateTopicConfig,
    /// Propagated from `EcosystemProfile::aggregate_ms`.
    aggregate_ms:      Option<u64>,
    /// Propagated from `EcosystemProfile::attribute_aliases` (CC alias table).
    attribute_aliases: HashMap<String, String>,
}

struct CompiledAvailability {
    segments: Vec<Segment>,
    profile_prefix: String,
    config:   AvailabilityTopicConfig,
}

struct CompiledCmd {
    segments:      Vec<Segment>,
    config:        CmdTopicConfig,
    /// For `alias_reverse` routing: maps HomeCore attribute name →
    /// (commandClass, endpoint, property) derived by inverting the profile's
    /// `attribute_aliases` table. Later entries in the alias table win on
    /// collision (e.g. `targetValue` overrides `currentValue` for the same attr).
    alias_reverse: HashMap<String, (String, String, String)>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub struct EcosystemRouter {
    state_entries:        Vec<CompiledState>,
    availability_entries: Vec<CompiledAvailability>,
    cmd_entries:          Vec<CompiledCmd>,
    rhai_engine:          Engine,
    rhai_ast:             Option<AST>,
}

impl EcosystemRouter {
    /// Build a router from a list of parsed ecosystem profiles.
    /// `rhai_source` may contain custom transform functions.
    pub fn new(profiles: Vec<EcosystemProfile>, rhai_source: Option<&str>) -> Result<Self> {
        let mut state_entries        = Vec::new();
        let mut availability_entries = Vec::new();
        let mut cmd_entries          = Vec::new();

        for profile in profiles {
            let prefix            = profile.prefix.clone();
            let aggregate_ms      = profile.aggregate_ms;
            let attribute_aliases = profile.attribute_aliases.clone();

            for sc in profile.state_topics {
                state_entries.push(CompiledState {
                    segments:          parse_pattern(&sc.pattern),
                    profile_prefix:    prefix.clone(),
                    aggregate_ms,
                    attribute_aliases: attribute_aliases.clone(),
                    config:            sc,
                });
            }
            for ac in profile.availability_topics {
                availability_entries.push(CompiledAvailability {
                    segments:       parse_pattern(&ac.pattern),
                    profile_prefix: prefix.clone(),
                    config:         ac,
                });
            }
            // Build alias reverse map: hc_attr → (commandClass, endpoint, property).
            // Sort keys before inverting so that for the same HC attribute,
            // alphabetically later properties win (e.g. "targetValue" > "currentValue",
            // so targetValue is the write target that ends up in the reverse map).
            let mut alias_reverse: HashMap<String, (String, String, String)> = HashMap::new();
            let mut sorted_aliases: Vec<(&String, &String)> = attribute_aliases.iter().collect();
            sorted_aliases.sort_by_key(|(cc_key, _)| cc_key.as_str());
            for (cc_key, hc_attr) in sorted_aliases {
                let parts: Vec<&str> = cc_key.splitn(3, '/').collect();
                if parts.len() == 3 {
                    alias_reverse.insert(
                        hc_attr.clone(),
                        (parts[0].to_string(), parts[1].to_string(), parts[2].to_string()),
                    );
                }
            }

            for cc in profile.cmd_topics {
                cmd_entries.push(CompiledCmd {
                    segments:      parse_pattern(&cc.source),
                    alias_reverse: alias_reverse.clone(),
                    config:        cc,
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
                if let Some(result) = self.apply_availability(&entry.config, &entry.profile_prefix, &vars, payload)? {
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
        let config            = &entry.config;
        let prefix            = &entry.profile_prefix;
        let aggregate_ms      = entry.aggregate_ms;
        let attribute_aliases = &entry.attribute_aliases;

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
            // Render the template (e.g. "{property}" → actual captured value).
            let rendered = render_template(attr_template, vars);

            // For Z-Wave: look up "{commandClass}/{endpoint}/{property}" (or
            // "{commandClass}/{endpoint}/{property}/{propertyKey}" for CCs like
            // Meter that publish a propertyKey segment) in the CC alias table.
            // Falls back to the rendered template for profiles without aliases.
            let cc  = vars.get("commandClass").map(String::as_str).unwrap_or("");
            let ep  = vars.get("endpoint").map(String::as_str).unwrap_or("");
            let prop = vars.get("property").map(String::as_str).unwrap_or("");
            let alias_key = match vars.get("propertyKey") {
                Some(pk) => format!("{cc}/{ep}/{prop}/{pk}"),
                None     => format!("{cc}/{ep}/{prop}"),
            };
            let attr_name = attribute_aliases
                .get(&alias_key)
                .cloned()
                .unwrap_or(rendered);

            if config.coerce_scalar {
                json_value = coerce::coerce_scalar_auto(json_value);
            }
            if let Some(coercion) = config.coerce.get(&attr_name) {
                json_value = coerce::apply(coercion, json_value)?;
            }
            // value_map: translate scalar values to canonical representations.
            // e.g. Z-Wave thermostat mode integer 1 → "heat".
            if !config.value_map.is_empty() {
                let key = match &json_value {
                    Value::String(s) => s.clone(),
                    Value::Bool(b)   => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    _                => json_value.to_string(),
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
                partial: true,    // scalar topics are always partial updates
                aggregate_ms,
            });
        }

        // Full JSON object — apply field_map and coercions.
        let mapped = apply_field_map(&json_value, &config.field_map, &config.coerce)?;

        // Determine partial flag:
        //   - explicit profile override wins if set
        //   - scalar attribute wraps (handled above) are always partial
        //   - field_map topics without explicit partial default to full replace
        let partial = config.partial.unwrap_or(config.attribute.is_some());

        Ok(InboundResult::State { device_id, payload: mapped, partial, aggregate_ms })
    }

    fn apply_availability(
        &self,
        config: &AvailabilityTopicConfig,
        prefix: &str,
        vars: &HashMap<String, String>,
        raw_payload: &[u8],
    ) -> Result<Option<InboundResult>> {
        let device_var = vars.get("device")
            .or_else(|| vars.get("nodeId"))
            .or_else(|| vars.values().next());

        let device_id = match device_var {
            Some(d) => format!("{}{}", prefix, sanitize_id(d)),
            None    => return Ok(None),
        };

        let raw_str = std::str::from_utf8(raw_payload)
            .unwrap_or("")
            .trim()
            .to_string();

        let available = match config.payload.as_deref() {
            Some("raw_bool") => {
                raw_str.eq_ignore_ascii_case("true") || raw_str == "1"
            }
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

        Ok(Some(InboundResult::Availability { device_id, available }))
    }

    // -----------------------------------------------------------------------
    // Outbound routing
    // -----------------------------------------------------------------------

    /// Route an outbound HomeCore cmd. Returns `None` if no profile matches,
    /// or `Some(vec)` with one result per native publish (alias_reverse may
    /// produce one publish per attribute in the cmd payload).
    pub fn route_outbound(&self, topic: &str, payload: &[u8]) -> Result<Option<Vec<OutboundResult>>> {
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

        // --- alias_reverse: Z-Wave CC-based multi-topic command routing ---
        if config.routing.as_deref() == Some("alias_reverse") {
            let pattern = config.target_pattern.as_deref()
                .ok_or_else(|| anyhow!("alias_reverse cmd entry requires target_pattern"))?;

            let cmd_value: Value = serde_json::from_slice(raw_payload)
                .map_err(|e| anyhow!("cmd payload is not valid JSON: {e}"))?;
            let obj = cmd_value.as_object()
                .ok_or_else(|| anyhow!("alias_reverse cmd payload must be a JSON object"))?;

            let mut results = Vec::new();
            for (hc_attr, val) in obj {
                let (cc, ep, prop) = entry.alias_reverse.get(hc_attr)
                    .ok_or_else(|| anyhow!("alias_reverse: no Z-Wave mapping for attribute '{hc_attr}'"))?;

                let mut topic_vars = vars.clone();
                topic_vars.insert("commandClass".into(), cc.clone());
                topic_vars.insert("endpoint".into(), ep.clone());
                topic_vars.insert("property".into(), prop.clone());

                let target_topic = render_template(pattern, &topic_vars);

                // cmd_value_map: translate canonical HC values to native device values.
                // e.g. locked: true → 255  (Z-Wave door lock secured mode).
                let effective_val = if let Some(attr_map) = config.cmd_value_map.get(hc_attr) {
                    let key = match val {
                        Value::String(s) => s.clone(),
                        Value::Bool(b)   => b.to_string(),
                        Value::Number(n) => n.to_string(),
                        other            => other.to_string(),
                    };
                    attr_map.get(&key).cloned().unwrap_or_else(|| val.clone())
                } else {
                    val.clone()
                };

                results.push(OutboundResult { target_topic, payload: value_to_bytes(&effective_val) });
            }
            return Ok(results);
        }

        // --- Standard single-target routing ---

        let target_topic = config
            .target
            .as_deref()
            .map(|t| render_template(t, vars))
            .ok_or_else(|| anyhow!("cmd_topics entry has no target"))?;

        // Apply optional Rhai transform on the full payload.
        if let Some(fn_name) = &config.transform {
            let out = self.run_rhai(fn_name, raw_payload)?;
            return Ok(vec![OutboundResult { target_topic, payload: out }]);
        }

        // Parse HomeCore cmd payload as JSON.
        let cmd_value: Value = serde_json::from_slice(raw_payload)
            .map_err(|e| anyhow!("cmd payload is not valid JSON: {e}"))?;

        // For Shelly Gen2 RPC commands.
        if let Some(method) = &config.rpc_method {
            let out_payload = build_rpc_payload(method, config.rpc_id, &cmd_value, &config.field_map)?;
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
            return Ok(vec![OutboundResult { target_topic, payload: value_to_bytes(&coerced) }]);
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
        let ast = self.rhai_ast.as_ref()
            .ok_or_else(|| anyhow!("Rhai transform '{fn_name}' called but no script loaded"))?;
        let payload_str = std::str::from_utf8(payload)
            .map_err(|_| anyhow!("Non-UTF-8 payload for Rhai transform '{fn_name}'"))?
            .to_string();
        let result: rhai::Dynamic = self.rhai_engine
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
        None    => return Ok(payload.clone()),
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
                    coerce::apply(coercion, raw_val)
                        .unwrap_or_else(|e| {
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
        None    => return Ok(payload.clone()),
    };

    let mut result = Map::new();

    for (hc_key, v) in obj {
        // Rename to ecosystem key if a mapping exists.
        let eco_key = field_map.get(hc_key).cloned().unwrap_or_else(|| hc_key.clone());
        let coerced = if let Some(coercion) = coerce_map.get(&eco_key) {
            coerce::apply(coercion, v.clone())
                .unwrap_or_else(|e| {
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
            let eco_key = field_map.get(hc_key).cloned().unwrap_or_else(|| hc_key.clone());
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
        other            => other.to_string().into_bytes(),
    }
}

/// Helper to extract device_id from an InboundResult for logging.
fn result_device_id(r: &InboundResult) -> &str {
    match r {
        InboundResult::State { device_id, .. }        => device_id,
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
        let var_val = vars.get("device")
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
        let result = router.route_inbound("zigbee2mqtt/living_room_light", payload).unwrap().unwrap();
        match result {
            InboundResult::State { device_id, payload, .. } => {
                assert_eq!(device_id, "zigbee_living_room_light");
                assert_eq!(payload["on"],         true);
                assert_eq!(payload["brightness"], 128);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn z2m_state_off() {
        let router = make_router(Z2M_PROFILE);
        let payload = br#"{"state":"OFF"}"#;
        let result = router.route_inbound("zigbee2mqtt/bedroom_switch", payload).unwrap().unwrap();
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
        let result = router.route_inbound(
            "zigbee2mqtt/living_room_light/availability",
            br#"{"state":"online"}"#,
        ).unwrap().unwrap();
        match result {
            InboundResult::Availability { device_id, available } => {
                assert_eq!(device_id, "zigbee_living_room_light");
                assert!(available);
            }
            _ => panic!("Expected Availability"),
        }
    }

    #[test]
    fn z2m_availability_offline() {
        let router = make_router(Z2M_PROFILE);
        let result = router.route_inbound(
            "zigbee2mqtt/living_room_light/availability",
            br#"{"state":"offline"}"#,
        ).unwrap().unwrap();
        match result {
            InboundResult::Availability { available, .. } => assert!(!available),
            _ => panic!("Expected Availability"),
        }
    }

    // --- Z2M outbound cmd ---

    #[test]
    fn z2m_cmd_on() {
        let router = make_router(Z2M_PROFILE);
        let results = router.route_outbound(
            "homecore/devices/zigbee_living_room_light/cmd",
            br#"{"on":true}"#,
        ).unwrap().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_topic, "zigbee2mqtt/living_room_light/set");
        let body: Value = serde_json::from_slice(&results[0].payload).unwrap();
        assert_eq!(body["state"], "ON");
    }

    #[test]
    fn z2m_cmd_off() {
        let router = make_router(Z2M_PROFILE);
        let results = router.route_outbound(
            "homecore/devices/zigbee_living_room_light/cmd",
            br#"{"on":false}"#,
        ).unwrap().unwrap();
        let body: Value = serde_json::from_slice(&results[0].payload).unwrap();
        assert_eq!(body["state"], "OFF");
    }

    // --- Shelly Gen1 scalar state ---

    #[test]
    fn shelly_scalar_on() {
        let router = make_router(SHELLY_PROFILE);
        let result = router.route_inbound("shellies/myplug/relay/0", b"1").unwrap().unwrap();
        match result {
            InboundResult::State { device_id, payload, partial, .. } => {
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
        let result = router.route_inbound("shellies/myplug/relay/0", b"0").unwrap().unwrap();
        match result {
            InboundResult::State { payload, .. } => assert_eq!(payload["on"], false),
            _ => panic!("Expected State"),
        }
    }

    // --- Shelly Gen1 availability ---

    #[test]
    fn shelly_availability_raw_bool() {
        let router = make_router(SHELLY_PROFILE);
        let result = router.route_inbound("shellies/myplug/online", b"true").unwrap().unwrap();
        match result {
            InboundResult::Availability { available, .. } => assert!(available),
            _ => panic!("Expected Availability"),
        }
    }

    // --- Shelly Gen1 outbound cmd ---

    #[test]
    fn shelly_cmd_on() {
        let router = make_router(SHELLY_PROFILE);
        let results = router.route_outbound(
            "homecore/devices/shelly_myplug/cmd",
            br#"{"on":true}"#,
        ).unwrap().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_topic, "shellies/myplug/relay/0/command");
        assert_eq!(results[0].payload, b"1");
    }

    #[test]
    fn shelly_cmd_off() {
        let router = make_router(SHELLY_PROFILE);
        let results = router.route_outbound(
            "homecore/devices/shelly_myplug/cmd",
            br#"{"on":false}"#,
        ).unwrap().unwrap();
        assert_eq!(results[0].payload, b"0");
    }

    // --- No match ---

    #[test]
    fn no_match_returns_none() {
        let router = make_router(Z2M_PROFILE);
        assert!(router.route_inbound("unknown/topic/here", b"{}").unwrap().is_none());
        assert!(router.route_outbound("homecore/devices/unknown/cmd", b"{}").unwrap().is_none());
    }

    // --- Z-Wave inbound (alias lookup + aggregation metadata) ---

    const ZWAVE_PROFILE: &str = r#"
[ecosystem]
name         = "zwave"
prefix       = "zwave_"
aggregate_ms = 100

[ecosystem.attribute_aliases]
"37/0/currentValue"   = "on"
"38/0/currentValue"   = "brightness"
"49/0/Air temperature" = "temperature"
"128/0/level"         = "battery"
"50/0/value/65537"    = "power_w"

[[ecosystem.state_topics]]
pattern       = "zwave/{nodeId}/{commandClass}/{endpoint}/{property}"
attribute     = "{property}"
coerce_scalar = true

[[ecosystem.state_topics]]
pattern       = "zwave/{nodeId}/{commandClass}/{endpoint}/{property}/{propertyKey}"
attribute     = "{property}/{propertyKey}"
coerce_scalar = true
"#;

    #[test]
    fn zwave_binary_switch_resolves_alias() {
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/5/37/0/currentValue", b"true").unwrap().unwrap();
        match result {
            InboundResult::State { device_id, payload, partial, aggregate_ms } => {
                assert_eq!(device_id, "zwave_5");
                assert_eq!(payload["on"], true);
                assert!(partial);
                assert_eq!(aggregate_ms, Some(100));
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_dimmer_brightness_alias() {
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/3/38/0/currentValue", b"128").unwrap().unwrap();
        match result {
            InboundResult::State { payload, aggregate_ms, .. } => {
                assert_eq!(payload["brightness"], 128);
                assert_eq!(aggregate_ms, Some(100));
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_sensor_temperature_alias() {
        // ZwaveJS UI uses spaces in property names per the Z-Wave spec.
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/7/49/0/Air temperature", b"21.5").unwrap().unwrap();
        match result {
            InboundResult::State { payload, .. } => {
                let temp = payload["temperature"].as_f64().unwrap();
                assert!((temp - 21.5).abs() < 0.01);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_meter_six_segment_propertykey() {
        // Meter CC publishes 6-segment topics: zwave/{node}/50/{ep}/value/{propertyKey}
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/5/50/0/value/65537", b"127.4").unwrap().unwrap();
        match result {
            InboundResult::State { device_id, payload, aggregate_ms, .. } => {
                assert_eq!(device_id, "zwave_5");
                // Alias "50/0/value/65537" → "power_w"
                let pw = payload["power_w"].as_f64().unwrap();
                assert!((pw - 127.4).abs() < 0.01);
                assert_eq!(aggregate_ms, Some(100));
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_meter_six_segment_no_alias_falls_back() {
        // 6-segment topic with no alias entry → attr name is "{property}/{propertyKey}"
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/5/50/0/value/99999", b"42").unwrap().unwrap();
        match result {
            InboundResult::State { payload, .. } => {
                assert_eq!(payload["value/99999"], 42);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_unknown_property_falls_back_to_raw_name() {
        // A property not in the alias table should use the raw property name.
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/9/99/0/someNewProp", b"42").unwrap().unwrap();
        match result {
            InboundResult::State { payload, .. } => {
                // Falls back to rendered template value: "someNewProp"
                assert_eq!(payload["someNewProp"], 42);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn zwave_battery_level_alias() {
        let router = make_router(ZWAVE_PROFILE);
        let result = router.route_inbound("zwave/4/128/0/level", b"85").unwrap().unwrap();
        match result {
            InboundResult::State { payload, .. } => {
                assert_eq!(payload["battery"], 85);
            }
            _ => panic!("Expected State"),
        }
    }

    #[test]
    fn non_zwave_profile_has_no_aggregate_ms() {
        let router = make_router(Z2M_PROFILE);
        let payload = br#"{"state":"ON","brightness":128}"#;
        let result = router.route_inbound("zigbee2mqtt/living_room_light", payload).unwrap().unwrap();
        match result {
            InboundResult::State { aggregate_ms, .. } => {
                assert_eq!(aggregate_ms, None);
            }
            _ => panic!("Expected State"),
        }
    }

    // --- Z-Wave outbound cmd (alias_reverse routing) ---

    const ZWAVE_PROFILE_WITH_CMD: &str = r#"
[ecosystem]
name         = "zwave"
prefix       = "zwave_"
aggregate_ms = 100

[ecosystem.attribute_aliases]
"37/0/currentValue" = "on"
"38/0/currentValue" = "brightness"
"38/0/targetValue"  = "brightness"
"67/1/value"        = "target_temp"

[[ecosystem.state_topics]]
pattern       = "zwave/{nodeId}/{commandClass}/{endpoint}/{property}"
attribute     = "{property}"
coerce_scalar = true

[[ecosystem.cmd_topics]]
source         = "homecore/devices/zwave_{nodeId}/cmd"
target_pattern = "zwave/{nodeId}/{commandClass}/{endpoint}/{property}/set"
routing        = "alias_reverse"
"#;

    #[test]
    fn zwave_cmd_single_attr_routes_to_set_topic() {
        let router = make_router(ZWAVE_PROFILE_WITH_CMD);
        let results = router.route_outbound(
            "homecore/devices/zwave_5/cmd",
            br#"{"on":true}"#,
        ).unwrap().unwrap();
        assert_eq!(results.len(), 1);
        // "on" → CC 37, ep 0, property currentValue (only alias for "on")
        assert_eq!(results[0].target_topic, "zwave/5/37/0/currentValue/set");
        assert_eq!(results[0].payload, b"true");
    }

    #[test]
    fn zwave_cmd_brightness_uses_last_alias_entry() {
        // "brightness" has two alias entries; targetValue comes later and should win.
        let router = make_router(ZWAVE_PROFILE_WITH_CMD);
        let results = router.route_outbound(
            "homecore/devices/zwave_3/cmd",
            br#"{"brightness":128}"#,
        ).unwrap().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target_topic, "zwave/3/38/0/targetValue/set");
        assert_eq!(results[0].payload, b"128");
    }

    #[test]
    fn zwave_cmd_multi_attr_produces_multiple_publishes() {
        let router = make_router(ZWAVE_PROFILE_WITH_CMD);
        let mut results = router.route_outbound(
            "homecore/devices/zwave_5/cmd",
            br#"{"on":false,"brightness":50}"#,
        ).unwrap().unwrap();
        assert_eq!(results.len(), 2);
        // Sort by topic for deterministic assertion order.
        results.sort_by(|a, b| a.target_topic.cmp(&b.target_topic));
        assert_eq!(results[0].target_topic, "zwave/5/37/0/currentValue/set");
        assert_eq!(results[0].payload, b"false");
        assert_eq!(results[1].target_topic, "zwave/5/38/0/targetValue/set");
        assert_eq!(results[1].payload, b"50");
    }

    #[test]
    fn zwave_cmd_unknown_attr_returns_error() {
        let router = make_router(ZWAVE_PROFILE_WITH_CMD);
        let err = router.route_outbound(
            "homecore/devices/zwave_5/cmd",
            br#"{"color_xy":{"x":0.3,"y":0.6}}"#,
        ).unwrap_err();
        assert!(err.to_string().contains("color_xy"), "{err}");
    }
}
