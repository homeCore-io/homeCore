//! `hc-topic-map` — config-driven MQTT topic translation and payload transforms.
//!
//! Translates non-standard device topic schemas (Tasmota, Shelly, ESPHome)
//! into the canonical `homecore/devices/{id}/state` schema without requiring
//! a dedicated plugin.
//!
//! Each mapping entry specifies:
//! - `source_pattern`     — MQTT topic with `{var}` captures, e.g. `stat/{device}/POWER`
//! - `target_template`   — output topic with `{var}` substitution
//! - `transform`         — optional Rhai function name for payload reshaping
//!
//! Reverse (command) direction:
//! - `cmd_source_pattern` — HomeCore cmd topic to match, e.g. `homecore/devices/shelly_{device}/cmd`
//! - `cmd_target_template`— native device command topic, e.g. `shellies/{device}/relay/0/command`
//! - `cmd_transform`     — optional Rhai function to reshape the HomeCore JSON cmd payload

use anyhow::{anyhow, Result};
use rhai::{Engine, Scope, AST};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

/// A single topic mapping rule from the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMapEntry {
    pub source_pattern: String,
    pub target_template: String,
    /// Name of a Rhai function that transforms the inbound payload string.
    pub transform: Option<String>,

    /// HomeCore cmd topic pattern for the reverse direction.
    /// e.g. `homecore/devices/shelly_{device}/cmd`
    #[serde(default)]
    pub cmd_source_pattern: Option<String>,
    /// Native device command topic template.
    /// e.g. `shellies/{device}/relay/0/command`
    #[serde(default)]
    pub cmd_target_template: Option<String>,
    /// Name of a Rhai function that transforms the outbound HomeCore JSON cmd payload.
    #[serde(default)]
    pub cmd_transform: Option<String>,
}

/// Result of a successful topic translation.
#[derive(Debug)]
pub struct TranslationResult {
    pub target_topic: String,
    pub payload: Vec<u8>,
}

struct CompiledEntry {
    segments: Vec<Segment>,
    target_template: String,
    transform: Option<String>,
    cmd_segments: Option<Vec<Segment>>,
    cmd_target_template: Option<String>,
    cmd_transform: Option<String>,
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    /// Whole-segment capture: `{name}`
    Capture(String),
    /// Partial capture with literal prefix and/or suffix: e.g. `shelly_{device}` or `{device}_v2`
    PartialCapture { prefix: String, name: String, suffix: String },
}

/// Holds all configured mappings and pre-compiled Rhai scripts.
pub struct TopicMapper {
    entries: Vec<CompiledEntry>,
    engine: Engine,
    ast: Option<AST>,
}

impl TopicMapper {
    /// Construct a mapper from config entries and an optional Rhai script source.
    pub fn new(entries: Vec<TopicMapEntry>, script_source: Option<&str>) -> Result<Self> {
        let mut engine = Engine::new();
        engine.set_max_operations(50_000);
        engine.set_max_call_levels(16);

        let ast = match script_source {
            Some(src) => Some(
                engine
                    .compile(src)
                    .map_err(|e| anyhow!("Transform script compile error: {e}"))?,
            ),
            None => None,
        };

        let compiled = entries
            .into_iter()
            .map(|e| CompiledEntry {
                segments: parse_pattern(&e.source_pattern),
                target_template: e.target_template,
                transform: e.transform,
                cmd_segments: e.cmd_source_pattern.as_deref().map(parse_pattern),
                cmd_target_template: e.cmd_target_template,
                cmd_transform: e.cmd_transform,
            })
            .collect();

        Ok(Self { entries: compiled, engine, ast })
    }

    /// Attempt to translate an incoming device topic + payload into the HomeCore schema.
    /// Returns `None` if no mapping matches.
    pub fn translate(&self, topic: &str, payload: &[u8]) -> Result<Option<TranslationResult>> {
        self.run_translate(topic, payload, false)
    }

    /// Attempt to translate an outgoing HomeCore cmd topic + payload to the native device topic.
    /// Returns `None` if no reverse mapping matches.
    pub fn translate_cmd(&self, topic: &str, payload: &[u8]) -> Result<Option<TranslationResult>> {
        self.run_translate(topic, payload, true)
    }

    fn run_translate(
        &self,
        topic: &str,
        payload: &[u8],
        cmd_direction: bool,
    ) -> Result<Option<TranslationResult>> {
        for entry in &self.entries {
            let (segments, target_template, transform) = if cmd_direction {
                match (&entry.cmd_segments, &entry.cmd_target_template) {
                    (Some(s), Some(t)) => (s, t, entry.cmd_transform.as_deref()),
                    _ => continue,
                }
            } else {
                (&entry.segments, &entry.target_template, entry.transform.as_deref())
            };

            if let Some(vars) = match_segments(segments, topic) {
                let target_topic = render_template(target_template, &vars);
                debug!(%topic, %target_topic, cmd = cmd_direction, "Topic mapped");

                let out_payload = if let (Some(fn_name), Some(ast)) = (transform, &self.ast) {
                    let payload_str = std::str::from_utf8(payload)
                        .map_err(|_| anyhow!("Non-UTF-8 payload for transform '{fn_name}'"))?
                        .to_string();
                    let result: rhai::Dynamic = self
                        .engine
                        .call_fn(&mut Scope::new(), ast, fn_name, (payload_str,))
                        .map_err(|e| anyhow!("Transform '{fn_name}' error: {e}"))?;
                    result.to_string().into_bytes()
                } else {
                    payload.to_vec()
                };

                return Ok(Some(TranslationResult { target_topic, payload: out_payload }));
            }
        }
        Ok(None)
    }
}

fn parse_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .split('/')
        .map(|seg| {
            if let (Some(open), Some(close)) = (seg.find('{'), seg.find('}')) {
                if open < close {
                    let prefix = seg[..open].to_string();
                    let name = seg[open + 1..close].to_string();
                    let suffix = seg[close + 1..].to_string();
                    if prefix.is_empty() && suffix.is_empty() {
                        return Segment::Capture(name);
                    } else {
                        return Segment::PartialCapture { prefix, name, suffix };
                    }
                }
            }
            Segment::Literal(seg.to_string())
        })
        .collect()
}

fn match_segments(segments: &[Segment], topic: &str) -> Option<HashMap<String, String>> {
    let topic_parts: Vec<&str> = topic.split('/').collect();
    if topic_parts.len() != segments.len() {
        return None;
    }
    let mut vars = HashMap::new();
    for (seg, part) in segments.iter().zip(topic_parts.iter()) {
        match seg {
            Segment::Literal(lit) if lit == part => {}
            Segment::Literal(_) => return None,
            Segment::Capture(name) => {
                vars.insert(name.clone(), (*part).to_string());
            }
            Segment::PartialCapture { prefix, name, suffix } => {
                if part.starts_with(prefix.as_str()) && part.ends_with(suffix.as_str()) {
                    let inner_end = part.len() - suffix.len();
                    let value = &part[prefix.len()..inner_end];
                    vars.insert(name.clone(), value.to_string());
                } else {
                    return None;
                }
            }
        }
    }
    Some(vars)
}

fn render_template(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (k, v) in vars {
        result = result.replace(&format!("{{{k}}}"), v);
    }
    result
}

// ---------------------------------------------------------------------------
// Built-in transform scripts for common device ecosystems.
// ---------------------------------------------------------------------------

/// Rhai source for Tasmota and Shelly built-in transforms (both directions).
pub const BUILTIN_TRANSFORMS: &str = r#"
// Note: Rhai's trim() mutates in place and returns (); compare directly instead.

// --- Inbound (device → HomeCore) ---

fn tasmota_power_to_state(payload) {
    if payload == "ON" || payload == "on" { "{\"on\":true}" } else { "{\"on\":false}" }
}

fn shelly_relay_to_state(payload) {
    if payload == "1" { "{\"on\":true}" } else { "{\"on\":false}" }
}

fn shelly_gen2_to_state(payload) {
    payload
}

fn esphome_binary_to_state(payload) {
    if payload == "ON" || payload == "on" { "{\"on\":true}" } else { "{\"on\":false}" }
}

// --- Outbound (HomeCore cmd → device) ---

// HomeCore sends {"on":true} or {"on":false}; Shelly Gen1 expects "on" or "off"
fn homecore_cmd_to_shelly_relay(payload) {
    if "true" in payload { "on" } else { "off" }
}

// HomeCore sends {"on":true} or {"on":false}; Tasmota expects "ON" or "OFF"
fn homecore_cmd_to_tasmota(payload) {
    if "true" in payload { "ON" } else { "OFF" }
}
"#;

/// Default topic map entries for Tasmota and Shelly devices (bidirectional).
pub fn default_entries() -> Vec<TopicMapEntry> {
    vec![
        TopicMapEntry {
            source_pattern: "stat/{device}/POWER".into(),
            target_template: "homecore/devices/tasmota_{device}/state".into(),
            transform: Some("tasmota_power_to_state".into()),
            cmd_source_pattern: Some("homecore/devices/tasmota_{device}/cmd".into()),
            cmd_target_template: Some("cmnd/{device}/POWER".into()),
            cmd_transform: Some("homecore_cmd_to_tasmota".into()),
        },
        TopicMapEntry {
            source_pattern: "shellies/{device}/relay/0".into(),
            target_template: "homecore/devices/shelly_{device}/state".into(),
            transform: Some("shelly_relay_to_state".into()),
            cmd_source_pattern: Some("homecore/devices/shelly_{device}/cmd".into()),
            cmd_target_template: Some("shellies/{device}/relay/0/command".into()),
            cmd_transform: Some("homecore_cmd_to_shelly_relay".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_tasmota_topic() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        let result = mapper
            .translate("stat/kitchen_plug/POWER", b"ON")
            .unwrap()
            .unwrap();
        assert_eq!(result.target_topic, "homecore/devices/tasmota_kitchen_plug/state");
        let json: serde_json::Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(json["on"], serde_json::json!(true));
    }

    #[test]
    fn matches_shelly_topic() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        let result = mapper
            .translate("shellies/living_room/relay/0", b"1")
            .unwrap()
            .unwrap();
        assert_eq!(result.target_topic, "homecore/devices/shelly_living_room/state");
        let json: serde_json::Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(json["on"], serde_json::json!(true));
    }

    #[test]
    fn no_match_returns_none() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        assert!(mapper.translate("homecore/devices/foo/state", b"{}").unwrap().is_none());
    }

    #[test]
    fn variable_capture() {
        let entries = vec![TopicMapEntry {
            source_pattern: "sensors/{room}/{kind}".into(),
            target_template: "homecore/devices/{room}_{kind}/state".into(),
            transform: None,
            cmd_source_pattern: None,
            cmd_target_template: None,
            cmd_transform: None,
        }];
        let mapper = TopicMapper::new(entries, None).unwrap();
        let r = mapper.translate("sensors/bedroom/temperature", b"22").unwrap().unwrap();
        assert_eq!(r.target_topic, "homecore/devices/bedroom_temperature/state");
    }

    #[test]
    fn shelly_cmd_translated_on() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        let result = mapper
            .translate_cmd("homecore/devices/shelly_living_room/cmd", b"{\"on\":true}")
            .unwrap()
            .unwrap();
        assert_eq!(result.target_topic, "shellies/living_room/relay/0/command");
        assert_eq!(result.payload, b"on");
    }

    #[test]
    fn shelly_cmd_translated_off() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        let result = mapper
            .translate_cmd("homecore/devices/shelly_living_room/cmd", b"{\"on\":false}")
            .unwrap()
            .unwrap();
        assert_eq!(result.target_topic, "shellies/living_room/relay/0/command");
        assert_eq!(result.payload, b"off");
    }

    #[test]
    fn tasmota_cmd_translated() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        let result = mapper
            .translate_cmd("homecore/devices/tasmota_kitchen_plug/cmd", b"{\"on\":true}")
            .unwrap()
            .unwrap();
        assert_eq!(result.target_topic, "cmnd/kitchen_plug/POWER");
        assert_eq!(result.payload, b"ON");
    }

    #[test]
    fn cmd_no_match_returns_none() {
        let mapper = TopicMapper::new(default_entries(), Some(BUILTIN_TRANSFORMS)).unwrap();
        assert!(mapper
            .translate_cmd("homecore/devices/unknown_device/cmd", b"{\"on\":true}")
            .unwrap()
            .is_none());
    }
}
