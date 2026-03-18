//! Single-device polling loop.
//!
//! Each device runs in its own tokio task.  The loop:
//! 1. Waits for the interval tick.
//! 2. Fetches the configured URL.
//! 3. Parses the response JSON.
//! 4. Applies the configured mapping (Rhai transform → field_map → raw passthrough).
//! 5. Publishes the resulting state and updates device availability.

use crate::config::PollerConfig;
use anyhow::{Context, Result};
use plugin_sdk_rs::DevicePublisher;
use rhai::{Dynamic, Engine, Map as RhaiMap, Scope, AST};
use serde_json::Value;
use std::collections::HashMap;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run one device's poll loop forever.  Marks the device online/offline based
/// on whether each poll succeeds.
pub async fn run_poller(cfg: PollerConfig, publisher: DevicePublisher) {
    // Build a dedicated HTTP client with per-poller timeout.
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
        .user_agent(concat!("HomeCore/http-poller-", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(device_id = %cfg.device_id, error = %e, "Failed to build HTTP client");
            return;
        }
    };

    // Pre-compile the Rhai script once; any syntax errors surface immediately
    // (startup validation catches them earlier, but this is the safety net).
    let engine = build_engine();
    let compiled: Option<AST> = cfg.transform.as_deref().and_then(|src| {
        engine.compile(src)
            .map_err(|e| warn!(device_id = %cfg.device_id, error = %e, "Transform compile error"))
            .ok()
    });

    let mut interval = tokio::time::interval(
        std::time::Duration::from_secs(cfg.interval_secs),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!(
        device_id = %cfg.device_id,
        url = %cfg.url,
        interval_secs = cfg.interval_secs,
        mapping = if compiled.is_some() { "rhai" } else if cfg.field_map.is_empty() { "raw" } else { "field_map" },
        "Poller started",
    );

    loop {
        interval.tick().await;

        match poll_once(&cfg, &http, &engine, compiled.as_ref()).await {
            Ok(state) => {
                let _ = publisher.set_available(&cfg.device_id, true).await;
                if let Err(e) = publisher.publish_state(&cfg.device_id, &state).await {
                    warn!(device_id = %cfg.device_id, error = %e, "Failed to publish state");
                } else {
                    info!(device_id = %cfg.device_id, "Poll OK — state published");
                }
            }
            Err(e) => {
                warn!(device_id = %cfg.device_id, error = %e, "Poll failed — marking device offline");
                let _ = publisher.set_available(&cfg.device_id, false).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP fetch + mapping pipeline
// ---------------------------------------------------------------------------

async fn poll_once(
    cfg: &PollerConfig,
    http: &reqwest::Client,
    engine: &Engine,
    ast: Option<&AST>,
) -> Result<Value> {
    let mut req = http.get(&cfg.url);
    for (name, value) in &cfg.headers {
        req = req.header(name, value);
    }

    let resp = req.send().await.context("HTTP request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }

    let raw: Value = resp.json().await.context("Response is not valid JSON")?;

    if let Some(ast) = ast {
        apply_transform(&raw, engine, ast).context("Rhai transform failed")
    } else if !cfg.field_map.is_empty() {
        Ok(apply_field_map(&raw, &cfg.field_map))
    } else {
        Ok(raw)
    }
}

// ---------------------------------------------------------------------------
// field_map extraction
// ---------------------------------------------------------------------------

fn apply_field_map(response: &Value, field_map: &HashMap<String, String>) -> Value {
    let mut state = serde_json::Map::new();
    for (attr, path) in field_map {
        match extract_path(response, path) {
            Some(val) => {
                state.insert(attr.clone(), val);
            }
            None => {
                warn!(attr, path, "Field path not found in response — attribute omitted");
            }
        }
    }
    Value::Object(state)
}

/// Walk a dot-notation + bracket-index path through a JSON value.
///
/// Syntax examples:
/// - `"temperature"`               → `response["temperature"]`
/// - `"main.temp"`                 → `response["main"]["temp"]`
/// - `"weather[0].description"`    → `response["weather"][0]["description"]`
/// - `"sensors[1].readings[0].v"`  → nested array + object traversal
///
/// Returns `None` if any segment is missing; the caller logs a warning.
fn extract_path(root: &Value, path: &str) -> Option<Value> {
    let mut current = root;
    let mut remaining = path;

    while !remaining.is_empty() {
        if remaining.starts_with('[') {
            // Array index: [n]
            let close = remaining.find(']')?;
            let idx: usize = remaining[1..close].trim().parse().ok()?;
            current = current.get(idx)?;
            remaining = remaining[close + 1..].trim_start_matches('.');
        } else {
            // Object key: everything up to the next '.' or '['
            let end = remaining
                .find(|c| c == '.' || c == '[')
                .unwrap_or(remaining.len());
            let key = &remaining[..end];
            current = current.get(key)?;
            remaining = if end < remaining.len() {
                remaining[end..].trim_start_matches('.')
            } else {
                ""
            };
        }
    }
    Some(current.clone())
}

// ---------------------------------------------------------------------------
// Rhai transform
// ---------------------------------------------------------------------------

/// Evaluate the pre-compiled AST with `response` bound to the JSON value.
/// The script must evaluate to a Rhai map (`#{ ... }`).
fn apply_transform(response: &Value, engine: &Engine, ast: &AST) -> Result<Value> {
    let mut scope = Scope::new();
    scope.push("response", json_to_dynamic(response));

    let result: Dynamic = engine
        .eval_ast_with_scope(&mut scope, ast)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Device state must be a JSON object — enforce that the script returned a map.
    if !result.is::<RhaiMap>() {
        anyhow::bail!(
            "Transform must return a map (`#{{ ... }}`), got: {}",
            result.type_name()
        );
    }
    dynamic_to_json(result)
}

fn build_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(100_000);
    engine.set_max_string_size(65_536);
    engine.set_max_array_size(4_096);
    engine.set_max_map_size(1_024);
    engine.set_max_call_levels(32);
    engine
}

// ---------------------------------------------------------------------------
// serde_json::Value <-> rhai::Dynamic bridge
// ---------------------------------------------------------------------------

pub fn json_to_dynamic(value: &Value) -> Dynamic {
    match value {
        Value::Null       => Dynamic::UNIT,
        Value::Bool(b)    => Dynamic::from(*b),
        Value::String(s)  => Dynamic::from(s.clone()),
        Value::Number(n)  => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::Array(arr) => {
            let v: rhai::Array = arr.iter().map(json_to_dynamic).collect();
            Dynamic::from(v)
        }
        Value::Object(map) => {
            let m: RhaiMap = map
                .iter()
                .map(|(k, v)| (k.as_str().into(), json_to_dynamic(v)))
                .collect();
            Dynamic::from(m)
        }
    }
}

fn dynamic_to_json(value: Dynamic) -> Result<Value> {
    if value.is_unit() {
        return Ok(Value::Null);
    }
    if let Ok(b) = value.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Ok(i) = value.as_int() {
        return Ok(Value::Number(i.into()));
    }
    if let Ok(f) = value.as_float() {
        let n = serde_json::Number::from_f64(f)
            .ok_or_else(|| anyhow::anyhow!("Non-finite float in transform result"))?;
        return Ok(Value::Number(n));
    }
    // String
    if let Ok(s) = value.clone().into_string() {
        return Ok(Value::String(s));
    }
    // Array
    if let Some(arr) = value.clone().try_cast::<rhai::Array>() {
        let out: Result<Vec<_>> = arr.into_iter().map(dynamic_to_json).collect();
        return Ok(Value::Array(out?));
    }
    // Map — the expected return type for a well-written transform
    if let Some(map) = value.try_cast::<RhaiMap>() {
        let mut obj = serde_json::Map::new();
        for (k, v) in map {
            obj.insert(k.to_string(), dynamic_to_json(v)?);
        }
        return Ok(Value::Object(obj));
    }
    Err(anyhow::anyhow!(
        "Transform returned an unsupported Rhai type — script must return a map (#{{ ... }})"
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- field_map / extract_path ---

    #[test]
    fn extract_simple_key() {
        let v = json!({ "temp": 21.5 });
        assert_eq!(extract_path(&v, "temp"), Some(json!(21.5)));
    }

    #[test]
    fn extract_nested_key() {
        let v = json!({ "main": { "temp": 18.0, "humidity": 60 } });
        assert_eq!(extract_path(&v, "main.temp"),     Some(json!(18.0)));
        assert_eq!(extract_path(&v, "main.humidity"), Some(json!(60)));
    }

    #[test]
    fn extract_array_index() {
        let v = json!({ "weather": [{ "description": "clear sky" }, { "description": "clouds" }] });
        assert_eq!(extract_path(&v, "weather[0].description"), Some(json!("clear sky")));
        assert_eq!(extract_path(&v, "weather[1].description"), Some(json!("clouds")));
    }

    #[test]
    fn extract_deeply_nested() {
        let v = json!({ "a": { "b": [{ "c": 42 }] } });
        assert_eq!(extract_path(&v, "a.b[0].c"), Some(json!(42)));
    }

    #[test]
    fn extract_missing_returns_none() {
        let v = json!({ "main": { "temp": 5 } });
        assert_eq!(extract_path(&v, "main.humidity"), None);
        assert_eq!(extract_path(&v, "missing"),       None);
    }

    #[test]
    fn extract_out_of_bounds_returns_none() {
        let v = json!({ "items": [1, 2] });
        assert_eq!(extract_path(&v, "items[5]"), None);
    }

    #[test]
    fn field_map_builds_state_object() {
        let response = json!({
            "main": { "temp": 22.0, "humidity": 55 },
            "weather": [{ "description": "sunny" }]
        });
        let mut field_map = HashMap::new();
        field_map.insert("temperature".into(), "main.temp".into());
        field_map.insert("humidity".into(),    "main.humidity".into());
        field_map.insert("desc".into(),        "weather[0].description".into());

        let state = apply_field_map(&response, &field_map);
        assert_eq!(state["temperature"], json!(22.0));
        assert_eq!(state["humidity"],    json!(55));
        assert_eq!(state["desc"],        json!("sunny"));
    }

    #[test]
    fn field_map_omits_missing_paths() {
        let response = json!({ "temp": 20.0 });
        let mut field_map = HashMap::new();
        field_map.insert("temperature".into(), "temp".into());
        field_map.insert("humidity".into(),    "missing.path".into());

        let state = apply_field_map(&response, &field_map);
        assert!(state.get("temperature").is_some());
        assert!(state.get("humidity").is_none());
    }

    // --- JSON <-> Dynamic bridge ---

    #[test]
    fn json_null_roundtrip() {
        let v = Value::Null;
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    #[test]
    fn json_bool_roundtrip() {
        for b in [true, false] {
            let v = Value::Bool(b);
            let d = json_to_dynamic(&v);
            assert_eq!(dynamic_to_json(d).unwrap(), v);
        }
    }

    #[test]
    fn json_integer_roundtrip() {
        let v = json!(42i64);
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    #[test]
    fn json_float_roundtrip() {
        let v = json!(3.14f64);
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    #[test]
    fn json_string_roundtrip() {
        let v = json!("hello world");
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    #[test]
    fn json_nested_object_roundtrip() {
        let v = json!({ "a": 1, "b": { "c": true } });
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    #[test]
    fn json_array_roundtrip() {
        let v = json!([1, "two", false, null]);
        let d = json_to_dynamic(&v);
        assert_eq!(dynamic_to_json(d).unwrap(), v);
    }

    // --- Rhai transform ---

    #[test]
    fn transform_extracts_fields() {
        let engine = build_engine();
        let script = r#"
            let t = response["main"]["temp"];
            let h = response["main"]["humidity"];
            #{ "temperature": t, "humidity": h }
        "#;
        let ast = engine.compile(script).unwrap();
        let response = json!({ "main": { "temp": 21.0, "humidity": 65 } });

        let state = apply_transform(&response, &engine, &ast).unwrap();
        assert_eq!(state["temperature"], json!(21.0));
        assert_eq!(state["humidity"],    json!(65));
    }

    #[test]
    fn transform_can_do_arithmetic() {
        let engine = build_engine();
        // Convert Kelvin to Celsius
        let script = r#"
            let kelvin = response["temp_k"].to_float();
            #{ "temp_c": kelvin - 273.15 }
        "#;
        let ast = engine.compile(script).unwrap();
        let response = json!({ "temp_k": 300.0 });

        let state = apply_transform(&response, &engine, &ast).unwrap();
        let celsius = state["temp_c"].as_f64().unwrap();
        assert!((celsius - 26.85).abs() < 0.01);
    }

    #[test]
    fn transform_bad_script_returns_error() {
        let engine = build_engine();
        let ast = engine.compile("1 + ").unwrap_err();
        // Compile error is expected
        let _ = ast; // just proving it errored
    }

    #[test]
    fn transform_non_map_result_returns_error() {
        let engine = build_engine();
        let script = "42"; // returns an integer, not a map
        let ast = engine.compile(script).unwrap();
        let response = json!({});

        let result = apply_transform(&response, &engine, &ast);
        assert!(result.is_err());
    }
}
