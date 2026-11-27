//! Ecosystem router.
//!
//! Matches incoming MQTT topics against all loaded ecosystem profiles and
//! returns translated homeCore events. Also handles the outbound path:
//! homeCore cmd → native device command topic.

use crate::coerce;
use crate::pattern::{match_segments, parse_pattern, render_template, sanitize_id, Segment};
use crate::profile::{
    AvailabilityTopicConfig, CmdTopicConfig, DeviceConfig, EcosystemProfile, StateTopicConfig,
};
use anyhow::{anyhow, Result};
use rhai::{Engine, Scope, AST};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// What the router produces for an inbound MQTT message.
#[derive(Debug)]
pub enum InboundResult {
    /// A state update — write to `homecore/devices/{device_id}/state[/partial]`.
    State {
        /// The profile (its `name`) that matched.
        profile: Arc<str>,
        device_id: String,
        payload: Value,
        /// True → publish to `.../state/partial` (merge-patch).
        /// False → publish to `.../state` (full replace).
        partial: bool,
        /// What the device is, when the profile says: its type and name.
        /// `None` when the profile says neither.
        device: Option<DeviceConfig>,
    },
    /// An availability update — set `device.available`.
    Availability {
        /// The profile (its `name`) that matched.
        profile: Arc<str>,
        device_id: String,
        available: bool,
        /// The ids of the devices it speaks for: `device_id`, and the profile's
        /// `device_id` templates filled in from the topic (a Shelly's
        /// `shellies/sh1/online` is for `shelly_sh1_relay0` and the rest).
        /// A `{…}` left in one stands for any text; see
        /// [`crate::pattern::id_matches`].
        covers: Vec<String>,
    },
}

/// What the router produces for an outbound homeCore cmd.
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
    profile_name: Arc<str>,
    /// The profile's per-device table, shared by its state topics.
    devices: Arc<HashMap<String, DeviceConfig>>,
    config: StateTopicConfig,
}

struct CompiledAvailability {
    segments: Vec<Segment>,
    profile_prefix: String,
    profile_name: Arc<str>,
    /// The profile's state topics' `device_id` templates.
    id_templates: Arc<Vec<String>>,
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
    /// A device's topic captures as they were, where its id had to change
    /// them (`zigbee2mqtt/Living Room` is `zigbee_Living_Room`): a command
    /// goes to the device's own topic, not one made from its id.
    raw_vars: std::sync::Mutex<HashMap<String, HashMap<String, String>>>,
}

impl EcosystemRouter {
    /// Build a router from a list of parsed ecosystem profiles.
    /// `rhai_source` may contain custom transform functions; a compile error
    /// in it fails the build.
    pub fn new(profiles: Vec<EcosystemProfile>, rhai_source: Option<&str>) -> Result<Self> {
        let mut router = Self::build(profiles)?;
        if let Some(src) = rhai_source {
            router.rhai_ast = Some(
                router
                    .rhai_engine
                    .compile(src)
                    .map_err(|e| anyhow!("Rhai compile error: {e}"))?,
            );
        }
        Ok(router)
    }

    /// Build a router with the transform scripts from a profiles directory
    /// (see [`crate::loader::load_transforms_from_dir`]), as `(file name, source)`.
    ///
    /// Each script compiles on its own. One that doesn't is logged and left
    /// out, as a profile that doesn't parse is, so a typo in one script costs
    /// only the transforms it holds. A function defined in two scripts is
    /// taken from the later one, with a warning. A topic whose `transform`
    /// names no loaded function is warned about here, once, rather than on
    /// every message it fails.
    pub fn with_scripts(profiles: Vec<EcosystemProfile>, scripts: &[(String, String)]) -> Self {
        let mut router = Self::build(profiles).expect("building a router without Rhai cannot fail");

        let mut merged: Option<AST> = None;
        let mut defined_in: HashMap<String, String> = HashMap::new();
        for (file, src) in scripts {
            let ast = match router.rhai_engine.compile(src) {
                Ok(ast) => ast,
                Err(e) => {
                    warn!(file = %file, error = %e, "Rhai transform script does not compile; skipping");
                    continue;
                }
            };
            for f in ast.iter_functions() {
                if let Some(earlier) = defined_in.insert(f.name.to_string(), file.clone()) {
                    if earlier != *file {
                        warn!(
                            function = f.name,
                            earlier = %earlier,
                            file = %file,
                            "Rhai function defined in two scripts; using the later one"
                        );
                    }
                }
            }
            info!(file = %file, functions = ast.iter_functions().count(), "Loaded Rhai transforms");
            merged = Some(match merged {
                Some(m) => m.merge(&ast),
                None => ast,
            });
        }
        router.rhai_ast = merged;

        for name in router.missing_transforms() {
            warn!(
                transform = %name,
                "A profile's transform names no function in the loaded .rhai scripts; \
                 its messages will not be translated"
            );
        }
        router
    }

    /// Every device a loaded profile describes by ID in its
    /// `[ecosystem.devices]` table, with the profile it is in, once each.
    pub fn configured_devices(&self) -> Vec<(Arc<str>, String, DeviceConfig)> {
        let mut out: Vec<(Arc<str>, String, DeviceConfig)> = Vec::new();
        let mut seen: Vec<*const HashMap<String, DeviceConfig>> = Vec::new();
        for e in &self.state_entries {
            let table = Arc::as_ptr(&e.devices);
            if seen.contains(&table) {
                continue;
            }
            seen.push(table);
            for (id, d) in e.devices.iter() {
                out.push((Arc::clone(&e.profile_name), id.clone(), d.clone()));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// The first topic level of every loaded profile's topics, where it is a
    /// fixed word (`zigbee2mqtt`, `stat`, `shellies`): the roots of the
    /// ecosystems that are on.
    pub fn topic_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .state_entries
            .iter()
            .map(|e| &e.segments)
            .chain(self.availability_entries.iter().map(|e| &e.segments))
            .filter_map(|s| match s.first() {
                Some(Segment::Literal(l)) => Some(l.clone()),
                _ => None,
            })
            .collect();
        // Where its commands go too: homeCore hears what it relays (`cmnd/plug/POWER`)
        // back from the broker, and that is not a device it is missing.
        roots.extend(self.cmd_entries.iter().filter_map(|e| {
            let first = e.config.target.as_deref()?.split('/').next()?;
            (!first.is_empty() && !first.contains('{')).then(|| first.to_string())
        }));
        roots.sort();
        roots.dedup();
        roots
    }

    /// Every device type a loaded profile names, on a topic or for one device,
    /// sorted and once each.
    pub fn device_types_named(&self) -> Vec<String> {
        let mut named: Vec<String> = self
            .state_entries
            .iter()
            .filter_map(|e| e.config.device_type.clone())
            .chain(
                self.state_entries
                    .iter()
                    .flat_map(|e| e.devices.values().filter_map(|d| d.device_type.clone())),
            )
            .collect();
        named.sort();
        named.dedup();
        named
    }

    /// Transform names used by a topic that no loaded script defines with
    /// one parameter, sorted.
    pub fn missing_transforms(&self) -> Vec<String> {
        let defined: std::collections::HashSet<&str> = self
            .rhai_ast
            .iter()
            .flat_map(|ast| ast.iter_functions())
            .filter(|f| f.params.len() == 1)
            .map(|f| f.name)
            .collect();
        let mut missing: Vec<String> = self
            .state_entries
            .iter()
            .filter_map(|e| e.config.transform.as_deref())
            .chain(
                self.cmd_entries
                    .iter()
                    .filter_map(|e| e.config.transform.as_deref()),
            )
            .filter(|name| !defined.contains(name))
            .map(str::to_string)
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }

    fn build(profiles: Vec<EcosystemProfile>) -> Result<Self> {
        let mut state_entries = Vec::new();
        let mut availability_entries = Vec::new();
        let mut cmd_entries = Vec::new();

        for profile in profiles {
            let prefix = profile.prefix.clone();
            let devices = Arc::new(profile.devices);
            let profile_name: Arc<str> = Arc::from(profile.name.as_str());
            let mut templates: Vec<String> = profile
                .state_topics
                .iter()
                .filter_map(|s| s.device_id.clone())
                .collect();
            templates.sort();
            templates.dedup();
            let id_templates = Arc::new(templates);

            for sc in profile.state_topics {
                state_entries.push(CompiledState {
                    segments: parse_pattern(&sc.pattern),
                    profile_prefix: prefix.clone(),
                    devices: Arc::clone(&devices),
                    profile_name: Arc::clone(&profile_name),
                    config: sc,
                });
            }
            for ac in profile.availability_topics {
                availability_entries.push(CompiledAvailability {
                    segments: parse_pattern(&ac.pattern),
                    profile_prefix: prefix.clone(),
                    profile_name: Arc::clone(&profile_name),
                    id_templates: Arc::clone(&id_templates),
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
        // A script's print() and debug() go to the log, not to stdout.
        rhai_engine.on_print(|s| info!(target: "hc_topic_map::rhai", "{s}"));
        rhai_engine.on_debug(|s, _src, pos| debug!(target: "hc_topic_map::rhai", %pos, "{s}"));

        Ok(Self {
            raw_vars: std::sync::Mutex::new(HashMap::new()),
            state_entries,
            availability_entries,
            cmd_entries,
            rhai_engine,
            rhai_ast: None,
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
                if let Some(result) = self.apply_availability(entry, &vars, payload)? {
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
        if vars.values().any(|v| sanitize_id(v) != *v) {
            self.raw_vars
                .lock()
                .unwrap()
                .insert(device_id.clone(), vars.clone());
        }
        let device = describe_device(entry, &device_id, vars);

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

        // Nothing in it (a retained message cleared, firmware with nothing to
        // say): no reading. As a scalar it would read as "" — off, for a
        // relay — and as a full state it would wipe the device's.
        if raw_str.is_empty() {
            return Ok(InboundResult::State {
                profile: Arc::clone(&entry.profile_name),
                device_id,
                payload: Value::Object(Map::new()),
                partial: true,
                device,
            });
        }

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
                profile: Arc::clone(&entry.profile_name),
                device_id,
                payload: Value::Object(obj),
                partial: true, // scalar topics are always partial updates
                device,
            });
        }

        // Full JSON object — apply field_map and coercions.
        let mapped = apply_field_map(&json_value, &config.field_map, &config.coerce)?;

        // Merged into what the device already reports, unless the profile says
        // `partial = false`: a device behind a profile is usually fed by several
        // topics (Tasmota's POWER and SENSOR, a Shelly's two components), and
        // a full replace from one would wipe what the others said. Nothing
        // mapped (a RESULT for a command it doesn't map) changes nothing.
        let nothing = mapped.as_object().is_some_and(Map::is_empty);
        let partial = nothing || config.partial.unwrap_or(true);

        Ok(InboundResult::State {
            profile: Arc::clone(&entry.profile_name),
            device_id,
            payload: mapped,
            partial,
            device,
        })
    }

    fn apply_availability(
        &self,
        entry: &CompiledAvailability,
        vars: &HashMap<String, String>,
        raw_payload: &[u8],
    ) -> Result<Option<InboundResult>> {
        let config = &entry.config;
        let prefix = entry.profile_prefix.as_str();
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

        // The templates that use one of this topic's captures, filled in.
        let mut covers = vec![device_id.clone()];
        for t in entry.id_templates.iter() {
            if vars.keys().any(|k| t.contains(&format!("{{{k}}}"))) {
                let id = sanitize_id(&render_template(t, vars));
                if !covers.contains(&id) {
                    covers.push(id);
                }
            }
        }
        Ok(Some(InboundResult::Availability {
            profile: Arc::clone(&entry.profile_name),
            device_id,
            available,
            covers,
        }))
    }

    // -----------------------------------------------------------------------
    // Outbound routing
    // -----------------------------------------------------------------------

    /// Route an outbound homeCore cmd. Returns `None` if no profile matches,
    /// or `Some(vec)` with one result per native publish (alias_reverse may
    /// produce one publish per attribute in the cmd payload).
    pub fn route_outbound(
        &self,
        topic: &str,
        payload: &[u8],
    ) -> Result<Option<Vec<OutboundResult>>> {
        for entry in &self.cmd_entries {
            if let Some(mut vars) = match_segments(&entry.segments, topic) {
                self.restore_raw_vars(topic, &mut vars);
                let results = self.apply_cmd(entry, &vars, payload)?;
                debug!(%topic, count = results.len(), "Cmd topic matched");
                return Ok(Some(results));
            }
        }
        Ok(None)
    }

    /// Put back what a device's id changed in its captures: a capture from
    /// the id (`Living_Room`) is the topic's (`Living Room`) when that is
    /// what the id was made from.
    fn restore_raw_vars(&self, topic: &str, vars: &mut HashMap<String, String>) {
        let Some(id) = topic
            .strip_prefix("homecore/devices/")
            .and_then(|t| t.strip_suffix("/cmd"))
        else {
            return;
        };
        let raw = self.raw_vars.lock().unwrap();
        let Some(was) = raw.get(id) else {
            return;
        };
        for (k, v) in vars.iter_mut() {
            if let Some(r) = was.get(k).filter(|r| sanitize_id(r) == *v) {
                *v = r.clone();
            }
        }
    }

    /// Carry over what an older router learned of devices' own topic names,
    /// so a reload doesn't send commands astray until each reports again.
    pub fn learn_from(&self, older: &EcosystemRouter) {
        let was = older.raw_vars.lock().unwrap().clone();
        self.raw_vars.lock().unwrap().extend(was);
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

        // Parse homeCore cmd payload as JSON.
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
            // A command that doesn't set this topic's attribute (brightness
            // to a relay's POWER topic) publishes nothing here, not `null`.
            let Some(attr_val) = cmd_value.get(attr_name).cloned() else {
                return Ok(Vec::new());
            };
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
        transform_output(fn_name, result)
    }
}

/// The payload a transform's return value stands for.
///
/// A string is the payload as it is. A map is JSON, which is what a state
/// topic parses next and what most devices take; its own display form
/// (`#{"a": 1}`) is neither. A number or bool is its text. Anything else,
/// nothing (`()`) included, is a mistake in the script, named here rather than
/// published.
fn transform_output(fn_name: &str, result: rhai::Dynamic) -> Result<Vec<u8>> {
    if result.is_string() {
        return Ok(result.into_string().unwrap_or_default().into_bytes());
    }
    if result.is_map() {
        let map = result.cast::<rhai::Map>();
        return Ok(rhai::format_map_as_json(&map).into_bytes());
    }
    if result.is_int() || result.is_float() || result.is_bool() {
        return Ok(result.to_string().into_bytes());
    }
    Err(anyhow!(
        "Rhai '{fn_name}' returned {}; return a string or a map",
        result.type_name()
    ))
}

// ---------------------------------------------------------------------------
// Field mapping helpers
// ---------------------------------------------------------------------------

/// Apply field_map (with dot-notation source keys) and coercions to an inbound
/// JSON payload, returning a new JSON object with homeCore canonical attribute names.
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
                    // A value the coercion can't read is left out, as an
                    // attribute topic's would be: a lock's "LOCK" is not `on`.
                    match coerce::apply(coercion, raw_val) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!(coercion, error = %e, "Coercion failed; value left out");
                            continue;
                        }
                    }
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
/// field_map here renames homeCore attribute names to ecosystem keys.
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
        // A coercion is looked up by homeCore's name first, so two attributes
        // sent to the same key (`on` and `locked` both become Z2M's `state`)
        // can be converted differently; then by the ecosystem's key.
        let coercion = coerce_map.get(hc_key).or_else(|| coerce_map.get(&eco_key));
        let coerced = if let Some(coercion) = coercion {
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
    // Rename homeCore keys to ecosystem keys for params.
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

/// Resolve the homeCore device ID for a state topic match.
/// What a state topic and the profile's device table say a device is. The
/// device's own entry wins, field by field, over the topic's.
fn describe_device(
    entry: &CompiledState,
    device_id: &str,
    vars: &HashMap<String, String>,
) -> Option<DeviceConfig> {
    let own = entry.devices.get(device_id);
    let device_type = own
        .and_then(|d| d.device_type.clone())
        .or_else(|| entry.config.device_type.clone())
        .filter(|t| !t.trim().is_empty());
    let name = own
        .and_then(|d| d.name.clone())
        .or_else(|| entry.config.name.as_ref().map(|t| render_template(t, vars)))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    (device_type.is_some() || name.is_some()).then_some(DeviceConfig { device_type, name })
}

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
  brightness = "brightness_pct"
  [ecosystem.state_topics.coerce]
  on             = "onoff_to_bool"
  brightness_pct = "pct254_to_100"

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
                assert_eq!(payload["brightness_pct"], 50.4);
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
                ..
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

    // --- The example profiles speak the device vocabulary ---

    fn example(name: &str) -> EcosystemRouter {
        let path = format!(
            "{}/../../config/profiles/examples/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        make_router(&std::fs::read_to_string(path).unwrap())
    }

    fn inbound(router: &EcosystemRouter, topic: &str, payload: &[u8]) -> Value {
        match router.route_inbound(topic, payload).unwrap().unwrap() {
            InboundResult::State { payload, .. } => payload,
            _ => panic!("Expected State"),
        }
    }

    fn outbound(router: &EcosystemRouter, topic: &str, payload: &[u8]) -> Value {
        let out = router.route_outbound(topic, payload).unwrap().unwrap();
        serde_json::from_slice(&out[0].payload).unwrap()
    }

    #[test]
    fn every_example_profile_loads() {
        for name in ["zigbee2mqtt", "shelly-gen1", "shelly-gen2", "tasmota"] {
            example(name);
        }
    }

    #[test]
    fn z2m_example_reports_shared_names() {
        let z2m = example("zigbee2mqtt");
        let light = inbound(
            &z2m,
            "zigbee2mqtt/lamp",
            br#"{"state":"ON","brightness":254,"color_temp":370,"linkquality":80}"#,
        );
        assert_eq!(light["on"], true);
        assert_eq!(light["brightness_pct"], 100);
        assert_eq!(light["color_temp"], 2703);

        let door = inbound(
            &z2m,
            "zigbee2mqtt/door",
            br#"{"contact":true,"battery":91,"tamper":false}"#,
        );
        assert_eq!(door["open"], false);
        assert_eq!(door["battery_pct"], 91);
        assert_eq!(door["tampered"], false);

        let stat = inbound(
            &z2m,
            "zigbee2mqtt/stat",
            br#"{"local_temperature":20.5,"occupied_heating_setpoint":21,
                "system_mode":"heat","running_state":"heat","occupancy":true}"#,
        );
        assert_eq!(stat["temperature"], 20.5);
        assert_eq!(stat["heat_setpoint"], 21);
        assert_eq!(stat["hvac_mode"], "heat");
        assert_eq!(stat["hvac_action"], "heating");
        assert_eq!(stat["occupied"], true);

        let lock = inbound(&z2m, "zigbee2mqtt/lock", br#"{"lock_state":"locked"}"#);
        assert_eq!(lock["locked"], true);
    }

    #[test]
    fn z2m_example_takes_shared_names() {
        let z2m = example("zigbee2mqtt");
        let cmd = "homecore/devices/zigbee_lamp/cmd";
        let out = outbound(
            &z2m,
            cmd,
            br#"{"on":true,"brightness_pct":50,"color_temp":2700,"transition_ms":1500}"#,
        );
        assert_eq!(out["state"], "ON");
        assert_eq!(out["brightness"], 127);
        assert_eq!(out["color_temp"], 370);
        assert_eq!(out["transition"], 1.5);

        assert_eq!(
            outbound(&z2m, cmd, br#"{"action":"toggle"}"#)["state"],
            "TOGGLE"
        );
        assert_eq!(outbound(&z2m, cmd, br#"{"locked":true}"#)["state"], "LOCK");
        let rgb = outbound(&z2m, cmd, br#"{"color_rgb":{"r":255,"g":0,"b":0}}"#);
        assert_eq!(rgb["color"]["r"], 255);
        let heat = outbound(&z2m, cmd, br#"{"heat_setpoint":21,"hvac_mode":"heat"}"#);
        assert_eq!(heat["occupied_heating_setpoint"], 21);
        assert_eq!(heat["system_mode"], "heat");
    }

    #[test]
    fn shelly_example_reports_open_the_right_way_round() {
        let shelly = example("shelly-gen1");
        let door = inbound(&shelly, "shellies/dw1/sensor/state", b"open");
        assert_eq!(door["open"], true);
        let door = inbound(&shelly, "shellies/dw1/sensor/state", b"closed");
        assert_eq!(door["open"], false);
    }

    // ── Rhai transforms ────────────────────────────────────────────────────

    const TRANSFORM_PROFILE: &str = r#"
[ecosystem]
name = "rhai-test"
prefix = "rt"

[[ecosystem.state_topics]]
pattern = "rt/{device}/raw"
transform = "raw_to_state"

[[ecosystem.cmd_topics]]
source = "homecore/devices/rt_{device}/cmd"
target = "rt/{device}/set"
transform = "cmd_to_native"
"#;

    fn scripts(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, s)| (n.to_string(), s.to_string()))
            .collect()
    }

    fn transform_router(list: &[(&str, &str)]) -> EcosystemRouter {
        EcosystemRouter::with_scripts(
            vec![load_profile_str(TRANSFORM_PROFILE).unwrap()],
            &scripts(list),
        )
    }

    fn state_of(router: &EcosystemRouter, topic: &str, payload: &[u8]) -> Value {
        match router.route_inbound(topic, payload).unwrap() {
            Some(InboundResult::State { payload, .. }) => payload,
            other => panic!("expected State, got {other:?}"),
        }
    }

    #[test]
    fn a_transform_returning_a_map_publishes_json() {
        let r = transform_router(&[(
            "a.rhai",
            r#"fn raw_to_state(p) { #{ on: p == "ON", note: "x" } }"#,
        )]);
        let s = state_of(&r, "rt/lamp/raw", b"ON");
        assert_eq!(s["on"], true);
        assert_eq!(s["note"], "x");
    }

    #[test]
    fn a_transform_returning_a_string_publishes_it_as_it_is() {
        let r = transform_router(&[(
            "a.rhai",
            r#"fn cmd_to_native(p) { "LEVEL " + parse_json(p).level }"#,
        )]);
        let out = r
            .route_outbound("homecore/devices/rt_lamp/cmd", br#"{"level":7}"#)
            .unwrap()
            .unwrap();
        assert_eq!(out[0].target_topic, "rt/lamp/set");
        assert_eq!(out[0].payload, b"LEVEL 7");
    }

    #[test]
    fn a_transform_returning_nothing_is_an_error_not_a_payload() {
        let r = transform_router(&[("a.rhai", "fn cmd_to_native(p) { }")]);
        let err = r
            .route_outbound("homecore/devices/rt_lamp/cmd", b"{}")
            .unwrap_err();
        assert!(
            err.to_string().contains("return a string or a map"),
            "{err}"
        );
    }

    #[test]
    fn a_script_that_does_not_compile_costs_only_its_own_functions() {
        let r = transform_router(&[
            ("a.rhai", r#"fn raw_to_state(p) { #{ on: true } }"#),
            ("b.rhai", "fn cmd_to_native(p) { this is not rhai"),
        ]);
        assert_eq!(state_of(&r, "rt/lamp/raw", b"x")["on"], true);
        assert_eq!(r.missing_transforms(), vec!["cmd_to_native".to_string()]);
    }

    #[test]
    fn the_later_script_wins_a_function_both_define() {
        let r = transform_router(&[
            ("a.rhai", r#"fn raw_to_state(p) { #{ from: "a" } }"#),
            ("b.rhai", r#"fn raw_to_state(p) { #{ from: "b" } }"#),
        ]);
        assert_eq!(state_of(&r, "rt/lamp/raw", b"x")["from"], "b");
    }

    #[test]
    fn a_transform_with_no_script_is_named_as_missing() {
        let r = transform_router(&[]);
        assert_eq!(
            r.missing_transforms(),
            vec!["cmd_to_native".to_string(), "raw_to_state".to_string()]
        );
        assert!(r.route_inbound("rt/lamp/raw", b"x").is_err());
    }

    // ── What a device is ─────────────────────────────────────────────────────

    fn described(r: &EcosystemRouter, topic: &str, payload: &[u8]) -> Option<DeviceConfig> {
        match r.route_inbound(topic, payload).unwrap() {
            Some(InboundResult::State { device, .. }) => device,
            other => panic!("expected state, got {other:?}"),
        }
    }

    fn described_router() -> EcosystemRouter {
        let profile = crate::loader::load_profile_str(
            r#"
            [ecosystem]
            name   = "demo"
            prefix = "demo_"

            [[ecosystem.state_topics]]
            pattern     = "demo/{device}/power"
            attribute   = "on"
            device_type = "switch"
            name        = "Plug {device}"

            [[ecosystem.state_topics]]
            pattern = "demo/{device}/sensor"

            [ecosystem.devices.demo_hall]
            name = "Hall lamp"

            [ecosystem.devices.demo_porch]
            device_type = "light"
            "#,
        )
        .unwrap();
        EcosystemRouter::new(vec![profile], None).unwrap()
    }

    #[test]
    fn a_topic_says_what_its_devices_are() {
        let d = described(&described_router(), "demo/kitchen/power", b"1").unwrap();
        assert_eq!(d.device_type.as_deref(), Some("switch"));
        assert_eq!(d.name.as_deref(), Some("Plug kitchen"));
    }

    #[test]
    fn a_device_s_own_entry_wins_field_by_field() {
        let r = described_router();
        let hall = described(&r, "demo/hall/power", b"1").unwrap();
        assert_eq!(hall.name.as_deref(), Some("Hall lamp"));
        assert_eq!(hall.device_type.as_deref(), Some("switch"));
        // On a topic that says nothing, the device's entry alone.
        let porch = described(&r, "demo/porch/sensor", br#"{"lux":3}"#).unwrap();
        assert_eq!(porch.device_type.as_deref(), Some("light"));
        assert_eq!(porch.name, None);
    }

    #[test]
    fn a_topic_that_says_nothing_describes_nothing() {
        assert_eq!(
            described(&described_router(), "demo/garage/sensor", br#"{"lux":3}"#),
            None
        );
    }
}
