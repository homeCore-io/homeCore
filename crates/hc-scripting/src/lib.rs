//! `hc-scripting` — sandboxed Rhai script runtime.
//!
//! # `device_state()` in scripts
//!
//! When constructed with [`ScriptRuntime::new_with_devices`], scripts can
//! read the current state of any device:
//!
//! ```rhai
//! let lock = device_state("yolink_d88b4c01000d3478");
//! lock["locked"] == true && lock["battery"] > 25
//! ```
//!
//! The function returns an empty map when the device is unknown.

use anyhow::{anyhow, Result};
use rhai::{Dynamic, Engine, Scope};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

/// A configured Rhai engine ready to evaluate HomeCore scripts.
pub struct ScriptRuntime {
    engine: Engine,
}

impl ScriptRuntime {
    /// Create a runtime without device state access.
    /// Use [`new_with_devices`] when scripts need to call `device_state()`.
    pub fn new() -> Self {
        Self::new_with_devices(HashMap::new())
    }

    /// Create a runtime with a snapshot of device attributes injected as the
    /// `device_state(id)` Rhai function.
    ///
    /// `devices` maps device_id → full attributes JSON object.
    /// Typically produced by listing all devices from `StateStore`.
    pub fn new_with_devices(devices: HashMap<String, serde_json::Value>) -> Self {
        let mut engine = Engine::new();
        engine.set_max_operations(100_000);
        engine.set_max_call_levels(32);
        engine.set_max_string_size(1024 * 64);
        engine.set_max_array_size(4096);
        engine.set_max_map_size(1024);

        // Convert the snapshot to a Rhai-native map so lookups are fast
        // and the closure is 'static + Send + Sync (rhai "sync" feature).
        let rhai_devices: Arc<rhai::Map> = Arc::new(
            devices
                .into_iter()
                .map(|(id, attrs)| {
                    let rhai_attrs = json_to_dynamic(attrs);
                    (id.into(), rhai_attrs)
                })
                .collect(),
        );

        engine.register_fn("device_state", move |id: &str| -> rhai::Map {
            rhai_devices
                .get(id)
                .and_then(|d| d.clone().try_cast::<rhai::Map>())
                .unwrap_or_default()
        });

        Self { engine }
    }

    /// Evaluate a boolean expression (`Condition::ScriptExpression`).
    pub fn eval_condition(&self, script: &str) -> Result<bool> {
        let result: bool = self
            .engine
            .eval_expression(script)
            .map_err(|e| anyhow!("Condition script error: {e}"))?;
        debug!(%script, %result, "Condition script evaluated");
        Ok(result)
    }

    /// Execute a script (`Action::RunScript`).
    pub fn run_action(&self, script: &str) -> Result<Dynamic> {
        let mut scope = Scope::new();
        let result = self
            .engine
            .eval_with_scope::<Dynamic>(&mut scope, script)
            .map_err(|e| anyhow!("Action script error: {e}"))?;
        debug!(%script, "Action script executed");
        Ok(result)
    }

    /// Call a named transform function (used by `hc-topic-map`).
    pub fn call_transform(&self, ast: &rhai::AST, fn_name: &str, payload: &str) -> Result<String> {
        let result: Dynamic = self
            .engine
            .call_fn(&mut Scope::new(), ast, fn_name, (payload.to_string(),))
            .map_err(|e| anyhow!("Transform '{fn_name}' error: {e}"))?;
        Ok(result.to_string())
    }

    /// Compile a script to an AST for repeated invocation.
    pub fn compile(&self, script: &str) -> Result<rhai::AST> {
        self.engine
            .compile(script)
            .map_err(|e| anyhow!("Script compilation error: {e}"))
    }
}

impl Default for ScriptRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// JSON → Rhai Dynamic conversion
// ---------------------------------------------------------------------------

fn json_to_dynamic(v: serde_json::Value) -> Dynamic {
    match v {
        serde_json::Value::Null    => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s),
        serde_json::Value::Array(arr) => {
            let v: rhai::Array = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(v)
        }
        serde_json::Value::Object(map) => {
            let m: rhai::Map = map
                .into_iter()
                .map(|(k, v)| (k.into(), json_to_dynamic(v)))
                .collect();
            Dynamic::from(m)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eval_condition_true() {
        assert!(ScriptRuntime::new().eval_condition("1 + 1 == 2").unwrap());
    }

    #[test]
    fn eval_condition_false() {
        assert!(!ScriptRuntime::new().eval_condition("1 + 1 == 3").unwrap());
    }

    #[test]
    fn device_state_returns_attributes() {
        let mut devices = HashMap::new();
        devices.insert(
            "yolink_abc".to_string(),
            json!({ "locked": true, "battery": 75 }),
        );
        let rt = ScriptRuntime::new_with_devices(devices);
        assert!(rt.eval_condition(r#"device_state("yolink_abc")["locked"] == true"#).unwrap());
        assert!(rt.eval_condition(r#"device_state("yolink_abc")["battery"] > 50"#).unwrap());
    }

    #[test]
    fn device_state_unknown_device_returns_empty_map() {
        let rt = ScriptRuntime::new_with_devices(HashMap::new());
        // An unknown device returns an empty map; keys_of should be empty.
        assert!(rt.eval_condition(r#"device_state("no_such_device").is_empty()"#).unwrap());
    }

    #[test]
    fn run_action_executes_script() {
        let rt = ScriptRuntime::new();
        rt.run_action("let x = 42;").unwrap();
    }
}
