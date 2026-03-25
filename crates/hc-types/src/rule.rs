//! Automation rule types: triggers, conditions, and actions.
//!
//! Rules are pure data — created and modified through the REST API, stored as
//! JSON/TOML, and evaluated at runtime without any Rust recompilation.

use chrono::{NaiveTime, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// A complete automation rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    /// Higher priority rules are evaluated first (descending sort).
    pub priority: i32,
    pub trigger: Trigger,
    /// All conditions must pass (short-circuit AND logic).
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Set by the loader when the rule file fails to parse, or by the API when a
    /// referenced device is deleted.  Rules with an error are never executed.
    /// The value is a human-readable description of the problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What causes a rule to be evaluated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    DeviceStateChanged {
        device_id: String,
        /// When `None`, any attribute change fires the trigger.
        attribute: Option<String>,
        /// When set, the trigger only fires when the attribute changes **to** this
        /// exact value.  Has no effect when `attribute` is `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<JsonValue>,
    },
    MqttMessage {
        topic_pattern: String,
    },
    TimeOfDay {
        time: NaiveTime,
        days: Vec<Weekday>,
    },
    SunEvent {
        event: SunEventType,
        offset_minutes: i32,
    },
    WebhookReceived {
        path: String,
    },
    ManualTrigger,
}

/// Solar event types for time-based triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SunEventType {
    Sunrise,
    Sunset,
    SolarNoon,
    CivilDawn,
    CivilDusk,
}

/// A side-effect-free predicate evaluated before actions are executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    DeviceState {
        device_id: String,
        attribute: String,
        op: CompareOp,
        value: JsonValue,
    },
    TimeWindow {
        start: NaiveTime,
        end: NaiveTime,
    },
    /// A Rhai expression that must evaluate to `true`.
    ScriptExpression {
        script: String,
    },
}

/// Comparison operators for `Condition::DeviceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A single step in a rule's action sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SetDeviceState {
        device_id: String,
        state: JsonValue,
    },
    PublishMqtt {
        topic: String,
        payload: String,
        retain: bool,
    },
    CallService {
        url: String,
        method: String,
        #[serde(default)]
        body: JsonValue,
        /// Request timeout in milliseconds. Defaults to 10 000 ms (10 s).
        #[serde(default)]
        timeout_ms: Option<u64>,
        /// Number of retries on network error or 5xx response. Defaults to 0.
        /// Backoff: 500 ms, 1 000 ms, 2 000 ms, … (capped at 4 000 ms).
        #[serde(default)]
        retries: Option<u32>,
        /// If set, the response body (parsed as JSON) is fired as a `Custom`
        /// event with this `event_type` on the internal bus after a successful
        /// call. Other rules can subscribe to it via `Trigger::MqttMessage`.
        #[serde(default)]
        response_event: Option<String>,
    },
    FireEvent {
        event_type: String,
        payload: JsonValue,
    },
    /// A Rhai script executed in the sandboxed runtime.
    RunScript {
        script: String,
    },
    Notify {
        channel: String,
        message: String,
        /// Optional title / subject line.  Defaults to `"HomeCore Alert"` if omitted.
        #[serde(default)]
        title: Option<String>,
    },
    /// Suspend the action sequence without blocking the async runtime.
    Delay {
        duration_ms: u64,
    },
    /// A group of actions executed concurrently via `tokio::join!`.
    Parallel {
        actions: Vec<Action>,
    },
    /// Repeat `actions` until `condition` (Rhai expression → bool) is true,
    /// with an optional cap on iterations to prevent infinite loops.
    RepeatUntil {
        condition: String,
        actions: Vec<Action>,
        /// Maximum number of iterations (default: 100 if omitted).
        max_iterations: Option<u32>,
        /// Delay between iterations in milliseconds (default: 0).
        interval_ms: Option<u64>,
    },
    /// Evaluate a Rhai boolean expression and execute one of two action branches.
    /// `else_actions` is optional — omit or leave empty to do nothing on false.
    Conditional {
        /// Rhai boolean expression.  Has access to `device_state("id")`,
        /// `current_hour()`, `current_minute()`, and `current_weekday()`.
        condition: String,
        then_actions: Vec<Action>,
        #[serde(default)]
        else_actions: Vec<Action>,
    },
}

/// A named snapshot of device states that can be activated as a unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub id: Uuid,
    pub name: String,
    /// Map of device_id → desired attribute values.
    pub states: std::collections::HashMap<String, JsonValue>,
}
