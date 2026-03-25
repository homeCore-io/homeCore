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
    /// Optional tags for grouping and bulk operations (e.g. "deck", "vacation").
    #[serde(default)]
    pub tags: Vec<String>,
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
    /// Fires when a `FireEvent` action emits the matching `event_type` on the
    /// internal event bus.  Enables clean rule chaining (fan-out) without
    /// duplicating action lists: one rule fires the event, others react to it.
    CustomEvent {
        event_type: String,
    },
    /// Fires once immediately after the rule engine has finished pre-populating
    /// its device cache on startup.  Use this to handle state that may have
    /// changed while homeCore was not running (e.g. a door left open across a
    /// restart).  Pair with `DeviceState` conditions to guard the action.
    SystemStarted,
    /// Fires on a cron schedule using a 6-field expression:
    /// `{second} {minute} {hour} {day-of-month} {month} {day-of-week}`
    ///
    /// Examples:
    /// - `"0 30 9 * * *"` — every day at 09:30:00
    /// - `"0 */15 * * * *"` — every 15 minutes
    /// - `"0 0 8 * * Mon"` — 08:00 on Mondays
    ///
    /// The schedule is evaluated in local wall-clock time.  Invalid expressions
    /// cause the rule to never fire (an error is logged at startup validation).
    Cron {
        expression: String,
    },
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
    /// True when a device attribute has not changed for at least `duration_secs`
    /// seconds.  Useful for "door open > 10 minutes" patterns without
    /// requiring a separate timer device.
    ///
    /// The elapsed time is measured from the last observed value change for
    /// the specific attribute, tracked by the rule engine's in-memory cache.
    /// On first evaluation after restart (before any change has been seen),
    /// the condition uses `DeviceState.last_seen` as a conservative baseline.
    TimeElapsed {
        device_id: String,
        attribute: String,
        /// Minimum elapsed seconds since the attribute last changed.
        duration_secs: u64,
    },
    /// Inverts the result of the wrapped condition.
    ///
    /// Useful for "device is NOT in state X" without needing a `ne` operator
    /// on a ScriptExpression, or for negating a `TimeWindow`/`TimeElapsed` check.
    ///
    /// Example: fire when a switch is NOT on:
    /// ```toml
    /// [[conditions]]
    /// type = "not"
    ///
    /// [conditions.condition]
    /// type = "device_state"
    /// device_id = "switch_vacation"
    /// attribute = "on"
    /// op = "eq"
    /// value = true
    /// ```
    Not {
        condition: Box<Condition>,
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
