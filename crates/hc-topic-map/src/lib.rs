//! `hc-topic-map` — config-driven MQTT topic translation and payload transforms.
//!
//! Translates non-standard device topic schemas (Tasmota, Shelly, ESPHome)
//! into the canonical `homecore/devices/{id}/state` schema without requiring
//! a dedicated plugin.
//!
//! Each mapping entry specifies:
//! - `source_pattern`  — MQTT topic with `{var}` captures, e.g. `stat/{device}/POWER`
//! - `target_template` — output topic with `{var}` substitution
//! - `transform`       — optional Rhai function name for payload reshaping

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
    /// Name of a Rhai function that transforms the payload string.
    pub transform: Option<String>,
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
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(String),
    Capture(String),
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
            })
            .collect();

        Ok(Self { entries: compiled, engine, ast })
    }

    /// Attempt to translate an incoming topic + payload.
    /// Returns `None` if no mapping matches.
    pub fn translate(&self, topic: &str, payload: &[u8]) -> Result<Option<TranslationResult>> {
        for entry in &self.entries {
            if let Some(vars) = match_segments(&entry.segments, topic) {
                let target_topic = render_template(&entry.target_template, &vars);
                debug!(%topic, %target_topic, "Topic mapped");

                let out_payload = if let (Some(fn_name), Some(ast)) =
                    (entry.transform.as_deref(), &self.ast)
                {
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
            if seg.starts_with('{') && seg.ends_with('}') {
                Segment::Capture(seg[1..seg.len() - 1].to_string())
            } else {
                Segment::Literal(seg.to_string())
            }
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

/// Rhai source for Tasmota and Shelly built-in transforms.
pub const BUILTIN_TRANSFORMS: &str = r#"
// Note: Rhai's trim() mutates in place and returns (); compare directly instead.
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
"#;

/// Default topic map entries for Tasmota and Shelly devices.
pub fn default_entries() -> Vec<TopicMapEntry> {
    vec![
        TopicMapEntry {
            source_pattern: "stat/{device}/POWER".into(),
            target_template: "homecore/devices/tasmota_{device}/state".into(),
            transform: Some("tasmota_power_to_state".into()),
        },
        TopicMapEntry {
            source_pattern: "shellies/{device}/relay/0".into(),
            target_template: "homecore/devices/shelly_{device}/state".into(),
            transform: Some("shelly_relay_to_state".into()),
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
        }];
        let mapper = TopicMapper::new(entries, None).unwrap();
        let r = mapper.translate("sensors/bedroom/temperature", b"22").unwrap().unwrap();
        assert_eq!(r.target_topic, "homecore/devices/bedroom_temperature/state");
    }
}
