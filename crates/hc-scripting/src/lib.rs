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
use hc_types::rule::TriggerContext;
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
    SetDeviceState {
        device_id: String,
        state: JsonValue,
    },
    Notify {
        channel: String,
        title: String,
        message: String,
    },
    PublishMqtt {
        topic: String,
        payload: String,
    },
    CallService {
        method: String,
        url: String,
        body: String,
    },
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
            self.engine
                .register_fn("set_device_state", move |id: &str, state: rhai::Map| {
                    let json = rhai_map_to_json(state);
                    b.lock().unwrap().push(ScriptSideEffect::SetDeviceState {
                        device_id: id.to_string(),
                        state: json,
                    });
                });
        }

        // notify("channel", "message")
        {
            let b = Arc::clone(&buf);
            self.engine
                .register_fn("notify", move |channel: &str, message: &str| {
                    b.lock().unwrap().push(ScriptSideEffect::Notify {
                        channel: channel.to_string(),
                        title: "HomeCore Alert".to_string(),
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
                        title: title.to_string(),
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
                    url: url.to_string(),
                    body: String::new(),
                });
            });
        }

        // http_post("url", "{\"key\":\"value\"}")
        {
            let b = Arc::clone(&buf);
            self.engine
                .register_fn("http_post", move |url: &str, body: &str| {
                    b.lock().unwrap().push(ScriptSideEffect::CallService {
                        method: "POST".to_string(),
                        url: url.to_string(),
                        body: body.to_string(),
                    });
                });
        }

        // publish_mqtt("topic", "payload")
        {
            let b = Arc::clone(&buf);
            self.engine
                .register_fn("publish_mqtt", move |topic: &str, payload: &str| {
                    b.lock().unwrap().push(ScriptSideEffect::PublishMqtt {
                        topic: topic.to_string(),
                        payload: payload.to_string(),
                    });
                });
        }

        self
    }

    /// Register hub variable accessor function on this runtime.
    ///
    /// Available Rhai function after this call:
    /// - `hub_var("name")` — current value of the named hub variable, or `()` if unset
    pub fn with_hub_vars(mut self, hub_vars: HashMap<String, JsonValue>) -> Self {
        let vars: Arc<HashMap<String, rhai::Dynamic>> = Arc::new(
            hub_vars
                .into_iter()
                .map(|(k, v)| (k, json_to_dynamic(v)))
                .collect(),
        );
        self.engine
            .register_fn("hub_var", move |name: &str| -> rhai::Dynamic {
                vars.get(name).cloned().unwrap_or(rhai::Dynamic::UNIT)
            });
        self
    }

    /// Register trigger context accessor functions on this runtime.
    ///
    /// Available Rhai functions after this call:
    /// - `trigger_device()` — `device_id` that fired the trigger, or `""`
    /// - `trigger_attribute()` — attribute name that changed, or `""`
    /// - `trigger_value()` — new attribute value (or unit if unavailable)
    /// - `trigger_prev_value()` — previous attribute value (or unit)
    /// - `trigger_event_type()` — event type string
    /// - `trigger_change_kind()` — origin class for device-state triggers, or `""`
    /// - `trigger_change_source()` — specific change source label, or `""`
    /// - `trigger_change_actor_id()` — actor id when known, or `""`
    /// - `trigger_change_actor_name()` — actor label when known, or `""`
    /// - `trigger_correlation_id()` — correlation id when present, or `""`
    /// - `trigger_extra()` — auxiliary context; for webhook triggers this is a map of
    ///   query-string parameters (e.g. `trigger_extra()["token"]`); unit otherwise
    /// - `trigger_label()` — user-defined label from `rule.trigger_label`, or `""`
    pub fn with_trigger_context(mut self, ctx: &TriggerContext) -> Self {
        let device_id = ctx.device_id.clone().unwrap_or_default();
        let attribute = ctx.attribute.clone().unwrap_or_default();
        let value = ctx
            .value
            .clone()
            .map(json_to_dynamic)
            .unwrap_or(Dynamic::UNIT);
        let prev_value = ctx
            .prev_value
            .clone()
            .map(json_to_dynamic)
            .unwrap_or(Dynamic::UNIT);
        let event_type = ctx.event_type.clone().unwrap_or_default();
        let change_kind = ctx
            .change_kind
            .as_ref()
            .map(|v| {
                serde_json::to_string(v)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string()
            })
            .unwrap_or_default();
        let change_source = ctx.change_source.clone().unwrap_or_default();
        let change_actor_id = ctx.change_actor_id.clone().unwrap_or_default();
        let change_actor_name = ctx.change_actor_name.clone().unwrap_or_default();
        let correlation_id = ctx.correlation_id.clone().unwrap_or_default();
        let extra = ctx
            .extra
            .clone()
            .map(json_to_dynamic)
            .unwrap_or(Dynamic::UNIT);
        let trigger_label = ctx.trigger_label.clone().unwrap_or_default();

        self.engine
            .register_fn("trigger_device", move || -> String { device_id.clone() });
        self.engine
            .register_fn("trigger_attribute", move || -> String { attribute.clone() });
        self.engine
            .register_fn("trigger_value", move || -> Dynamic { value.clone() });
        self.engine
            .register_fn("trigger_prev_value", move || -> Dynamic {
                prev_value.clone()
            });
        self.engine
            .register_fn("trigger_event_type", move || -> String {
                event_type.clone()
            });
        self.engine
            .register_fn("trigger_change_kind", move || -> String {
                change_kind.clone()
            });
        self.engine
            .register_fn("trigger_change_source", move || -> String {
                change_source.clone()
            });
        self.engine
            .register_fn("trigger_change_actor_id", move || -> String {
                change_actor_id.clone()
            });
        self.engine
            .register_fn("trigger_change_actor_name", move || -> String {
                change_actor_name.clone()
            });
        self.engine
            .register_fn("trigger_correlation_id", move || -> String {
                correlation_id.clone()
            });
        self.engine
            .register_fn("trigger_extra", move || -> Dynamic { extra.clone() });
        self.engine
            .register_fn("trigger_label", move || -> String { trigger_label.clone() });
        self
    }

    /// Register rule-local variable read access on this runtime.
    ///
    /// Available Rhai functions after this call:
    /// - `rule_var("name")` — returns the current value of a rule-local variable, or unit
    pub fn with_rule_vars(mut self, vars: HashMap<String, JsonValue>) -> Self {
        let rhai_vars: Arc<rhai::Map> = Arc::new(
            vars.into_iter()
                .map(|(k, v)| (k.into(), json_to_dynamic(v)))
                .collect(),
        );
        self.engine
            .register_fn("rule_var", move |name: &str| -> Dynamic {
                rhai_vars.get(name).cloned().unwrap_or(Dynamic::UNIT)
            });
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
        JsonValue::Null => Dynamic::UNIT,
        JsonValue::Bool(b) => Dynamic::from(b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(s) => Dynamic::from(s),
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
        devices.insert(
            "yolink_abc".to_string(),
            json!({ "locked": true, "battery": 75 }),
        );
        let rt = ScriptRuntime::new_with_devices(devices);
        assert!(rt
            .eval_condition(r#"device_state("yolink_abc")["locked"] == true"#)
            .unwrap());
        assert!(rt
            .eval_condition(r#"device_state("yolink_abc")["battery"] > 50"#)
            .unwrap());
    }

    #[test]
    fn device_state_unknown_returns_empty_map() {
        let rt = ScriptRuntime::new_with_devices(HashMap::new());
        assert!(rt
            .eval_condition(r#"device_state("no_such_device").is_empty()"#)
            .unwrap());
    }

    #[test]
    fn run_action_executes_script() {
        let _ = ScriptRuntime::new().run_action("let x = 42;").unwrap();
    }

    #[test]
    fn current_hour_returns_valid_range() {
        let rt = ScriptRuntime::new();
        let hour: bool = rt
            .eval_condition("current_hour() >= 0 && current_hour() <= 23")
            .unwrap();
        assert!(hour);
    }

    #[test]
    fn side_effects_collected() {
        let buf: EffectsBuf = Arc::new(Mutex::new(Vec::new()));
        let rt =
            ScriptRuntime::new_with_devices(HashMap::new()).with_side_effects(Arc::clone(&buf));
        let _ = rt
            .run_action(
                r#"
            set_device_state("plug_1", #{ on: true });
            notify("pushover", "hello");
            http_get("http://localhost/ping");
        "#,
            )
            .unwrap();
        let effects = buf.lock().unwrap();
        assert_eq!(effects.len(), 3);
        assert!(matches!(
            effects[0],
            ScriptSideEffect::SetDeviceState { .. }
        ));
        assert!(matches!(effects[1], ScriptSideEffect::Notify { .. }));
        assert!(
            matches!(effects[2], ScriptSideEffect::CallService { method: ref m, .. } if m == "GET")
        );
    }

    #[test]
    fn trigger_context_functions() {
        let ctx = TriggerContext {
            device_id: Some("light_1".into()),
            attribute: Some("on".into()),
            value: Some(json!(true)),
            prev_value: Some(json!(false)),
            event_type: Some("device_state_changed".into()),
            trigger_label: None,
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
        };
        let rt = ScriptRuntime::new().with_trigger_context(&ctx);
        assert!(rt
            .eval_condition(r#"trigger_device() == "light_1""#)
            .unwrap());
        assert!(rt.eval_condition(r#"trigger_attribute() == "on""#).unwrap());
        assert!(rt.eval_condition("trigger_value() == true").unwrap());
        assert!(rt.eval_condition("trigger_prev_value() == false").unwrap());
        assert!(rt
            .eval_condition(r#"trigger_event_type() == "device_state_changed""#)
            .unwrap());
    }

    #[test]
    fn rule_vars_accessible() {
        let mut vars = HashMap::new();
        vars.insert("counter".into(), json!(5_i64));
        vars.insert("mode".into(), json!("away"));
        let rt = ScriptRuntime::new().with_rule_vars(vars);
        assert!(rt.eval_condition("rule_var(\"counter\") == 5").unwrap());
        assert!(rt.eval_condition(r#"rule_var("mode") == "away""#).unwrap());
        assert!(rt.eval_condition("rule_var(\"missing\") == ()").unwrap());
    }

    #[test]
    fn time_based_branching_script() {
        // Verifies that current_hour() is usable in branch logic.
        let rt = ScriptRuntime::new();
        // Always-true OR always-false — just confirms syntax parses correctly.
        let result = rt
            .eval_condition(r#"current_hour() >= 0 || current_hour() < 0"#)
            .unwrap();
        assert!(result);
    }
}
