//! `hc-scripting` — sandboxed Rhai script runtime.
//!
//! # Reading device state in scripts
//!
//! `device_state("id")` returns the device's current attribute map.
//!
//! ```rhai
//! let lock = device_state("yolink_abc");
//! lock["locked"] == true && lock["battery"] > 25
//! ```
//!
//! # Time helpers
//!
//! ```rhai
//! current_hour()    // 0-23, local time
//! current_minute()  // 0-59
//! current_weekday() // "Monday", "Tuesday", …
//! ```
//!
//! # Side-effect functions (RunScript actions only)
//!
//! Available after calling `.with_side_effects()` on the runtime.
//! Effects are collected synchronously and executed by the async executor
//! after the script returns.
//!
//! ```rhai
//! set_device_state("device_id", #{ on: true, brightness: 200 });
//! notify("pushover", "Motion detected!");
//! notify_titled("pushover", "Alert", "Motion in the back yard");
//! http_get("http://10.0.10.200:5005/Bathroom/stop");
//! http_post("http://api.example.com/hook", `{"key":"value"}`);
//! publish_mqtt("homecore/events/my_event", "payload");
//! ```

use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike};
use rhai::{Dynamic, Engine, Scope};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::debug;

// ---------------------------------------------------------------------------
// Side-effect collection — bridges sync Rhai → async executor
// ---------------------------------------------------------------------------

/// A command collected by a `RunScript` action and executed asynchronously
/// by the rule executor after the script returns.
#[derive(Debug)]
pub enum ScriptSideEffect {
    SetDeviceState { device_id: String, state: JsonValue },
    Notify         { channel: String, title: String, message: String },
    PublishMqtt    { topic: String, payload: String },
    CallService    { method: String, url: String, body: String },
}

pub type EffectsBuf = Arc<Mutex<Vec<ScriptSideEffect>>>;

// ---------------------------------------------------------------------------
// ScriptRuntime
// ---------------------------------------------------------------------------

/// A configured Rhai engine ready to evaluate HomeCore scripts.
pub struct ScriptRuntime {
    engine: Engine,
}

impl ScriptRuntime {
    /// Runtime without device state or side-effect support.
    pub fn new() -> Self {
        Self::new_with_devices(HashMap::new())
    }

    /// Runtime with a device-state snapshot injected as `device_state(id)`.
    pub fn new_with_devices(devices: HashMap<String, JsonValue>) -> Self {
        let mut engine = Engine::new();
        engine.set_max_operations(100_000);
        engine.set_max_call_levels(32);
        engine.set_max_string_size(1024 * 64);
        engine.set_max_array_size(4096);
        engine.set_max_map_size(1024);

        // Build Rhai-native device snapshot for zero-copy lookups.
        let rhai_devices: Arc<rhai::Map> = Arc::new(
            devices
                .into_iter()
                .map(|(id, attrs)| (id.into(), json_to_dynamic(attrs)))
                .collect(),
        );

        engine.register_fn("device_state", move |id: &str| -> rhai::Map {
            rhai_devices
                .get(id)
                .and_then(|d| d.clone().try_cast::<rhai::Map>())
                .unwrap_or_default()
        });

        // Time helpers — always available, no side effects.
        engine.register_fn("current_hour", || -> i64 {
            chrono::Local::now().hour() as i64
        });
        engine.register_fn("current_minute", || -> i64 {
            chrono::Local::now().minute() as i64
        });
        engine.register_fn("current_weekday", || -> String {
            format!("{:?}", chrono::Local::now().weekday())
        });

        Self { engine }
    }

    /// Register side-effect functions on this runtime.
    ///
    /// Must be called before [`run_action`].  Collected effects are returned
    /// from [`take_effects`] and executed by the async executor.
    ///
    /// Available Rhai functions after this call:
    /// - `set_device_state(id, map)`
    /// - `notify(channel, message)`
    /// - `notify_titled(channel, title, message)`
    /// - `http_get(url)`
    /// - `http_post(url, json_body_string)`
    /// - `publish_mqtt(topic, payload)`
    pub fn with_side_effects(mut self, buf: EffectsBuf) -> Self {
        // set_device_state("id", #{ on: true, brightness: 200 })
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn(
                "set_device_state",
                move |id: &str, state: rhai::Map| {
                    let json = rhai_map_to_json(state);
                    b.lock().unwrap().push(ScriptSideEffect::SetDeviceState {
                        device_id: id.to_string(),
                        state: json,
                    });
                },
            );
        }

        // notify("channel", "message")
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn("notify", move |channel: &str, message: &str| {
                b.lock().unwrap().push(ScriptSideEffect::Notify {
                    channel: channel.to_string(),
                    title:   "HomeCore Alert".to_string(),
                    message: message.to_string(),
                });
            });
        }

        // notify_titled("channel", "title", "message")
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn(
                "notify_titled",
                move |channel: &str, title: &str, message: &str| {
                    b.lock().unwrap().push(ScriptSideEffect::Notify {
                        channel: channel.to_string(),
                        title:   title.to_string(),
                        message: message.to_string(),
                    });
                },
            );
        }

        // http_get("url")
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn("http_get", move |url: &str| {
                b.lock().unwrap().push(ScriptSideEffect::CallService {
                    method: "GET".to_string(),
                    url:    url.to_string(),
                    body:   String::new(),
                });
            });
        }

        // http_post("url", "{\"key\":\"value\"}")
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn("http_post", move |url: &str, body: &str| {
                b.lock().unwrap().push(ScriptSideEffect::CallService {
                    method: "POST".to_string(),
                    url:    url.to_string(),
                    body:   body.to_string(),
                });
            });
        }

        // publish_mqtt("topic", "payload")
        {
            let b = Arc::clone(&buf);
            self.engine.register_fn("publish_mqtt", move |topic: &str, payload: &str| {
                b.lock().unwrap().push(ScriptSideEffect::PublishMqtt {
                    topic:   topic.to_string(),
                    payload: payload.to_string(),
                });
            });
        }

        self
    }

    // -----------------------------------------------------------------------
    // Evaluation methods
    // -----------------------------------------------------------------------

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
// JSON ↔ Rhai Dynamic conversion
// ---------------------------------------------------------------------------

fn json_to_dynamic(v: JsonValue) -> Dynamic {
    match v {
        JsonValue::Null      => Dynamic::UNIT,
        JsonValue::Bool(b)   => Dynamic::from(b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(s)  => Dynamic::from(s),
        JsonValue::Array(arr) => {
            let v: rhai::Array = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(v)
        }
        JsonValue::Object(map) => {
            let m: rhai::Map = map
                .into_iter()
                .map(|(k, v)| (k.into(), json_to_dynamic(v)))
                .collect();
            Dynamic::from(m)
        }
    }
}

fn dynamic_to_json(d: Dynamic) -> JsonValue {
    if d.is_unit() {
        JsonValue::Null
    } else if d.is::<bool>() {
        JsonValue::Bool(d.cast::<bool>())
    } else if d.is::<i64>() {
        JsonValue::Number(d.cast::<i64>().into())
    } else if d.is::<f64>() {
        serde_json::Number::from_f64(d.cast::<f64>())
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null)
    } else if d.is::<rhai::ImmutableString>() {
        JsonValue::String(d.cast::<rhai::ImmutableString>().to_string())
    } else if d.is::<rhai::Array>() {
        JsonValue::Array(
            d.cast::<rhai::Array>()
                .into_iter()
                .map(dynamic_to_json)
                .collect(),
        )
    } else if d.is::<rhai::Map>() {
        rhai_map_to_json(d.cast::<rhai::Map>())
    } else {
        JsonValue::String(d.to_string())
    }
}

fn rhai_map_to_json(map: rhai::Map) -> JsonValue {
    JsonValue::Object(
        map.into_iter()
            .map(|(k, v)| (k.to_string(), dynamic_to_json(v)))
            .collect(),
    )
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
        devices.insert("yolink_abc".to_string(), json!({ "locked": true, "battery": 75 }));
        let rt = ScriptRuntime::new_with_devices(devices);
        assert!(rt.eval_condition(r#"device_state("yolink_abc")["locked"] == true"#).unwrap());
        assert!(rt.eval_condition(r#"device_state("yolink_abc")["battery"] > 50"#).unwrap());
    }

    #[test]
    fn device_state_unknown_returns_empty_map() {
        let rt = ScriptRuntime::new_with_devices(HashMap::new());
        assert!(rt.eval_condition(r#"device_state("no_such_device").is_empty()"#).unwrap());
    }

    #[test]
    fn run_action_executes_script() {
        ScriptRuntime::new().run_action("let x = 42;").unwrap();
    }

    #[test]
    fn current_hour_returns_valid_range() {
        let rt = ScriptRuntime::new();
        let hour: bool = rt.eval_condition("current_hour() >= 0 && current_hour() <= 23").unwrap();
        assert!(hour);
    }

    #[test]
    fn side_effects_collected() {
        let buf: EffectsBuf = Arc::new(Mutex::new(Vec::new()));
        let rt = ScriptRuntime::new_with_devices(HashMap::new())
            .with_side_effects(Arc::clone(&buf));
        rt.run_action(r#"
            set_device_state("plug_1", #{ on: true });
            notify("pushover", "hello");
            http_get("http://localhost/ping");
        "#).unwrap();
        let effects = buf.lock().unwrap();
        assert_eq!(effects.len(), 3);
        assert!(matches!(effects[0], ScriptSideEffect::SetDeviceState { .. }));
        assert!(matches!(effects[1], ScriptSideEffect::Notify { .. }));
        assert!(matches!(effects[2], ScriptSideEffect::CallService { method: ref m, .. } if m == "GET"));
    }

    #[test]
    fn time_based_branching_script() {
        // Verifies that current_hour() is usable in branch logic.
        let rt = ScriptRuntime::new();
        // Always-true OR always-false — just confirms syntax parses correctly.
        let result = rt.eval_condition(
            r#"current_hour() >= 0 || current_hour() < 0"#
        ).unwrap();
        assert!(result);
    }
}
