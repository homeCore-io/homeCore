//! Automation rule types: triggers, conditions, and actions.
//!
//! Rules are pure data — created and modified through the REST API, stored as
//! JSON/TOML, and evaluated at runtime without any Rust recompilation.

use chrono::{NaiveTime, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
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
    pub actions: Vec<RuleAction>,
    /// Set by the loader when the rule file fails to parse, or by the API when a
    /// referenced device is deleted.  Rules with an error are never executed.
    /// The value is a human-readable description of the problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional per-rule cooldown period.  After this rule fires, it will not
    /// fire again until at least this many seconds have elapsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<u64>,

    // ── Per-rule logging controls ────────────────────────────────────────────
    /// Log each trigger event that reaches this rule (at info level).
    #[serde(default)]
    pub log_events: bool,
    /// Log when this rule is triggered (at info level).
    #[serde(default)]
    pub log_triggers: bool,
    /// Log each action as it executes, including skipped conditional branches.
    #[serde(default)]
    pub log_actions: bool,

    // ── Required Expression (pre-trigger gate) ───────────────────────────────
    /// A Rhai boolean expression evaluated *before* the trigger event is
    /// processed.  If false the rule will not fire regardless of conditions.
    /// Useful for transition-specific rules: "only fire when transitioning from
    /// Away to Home mode".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_expression: Option<String>,
    /// When `true`, any in-flight cancellable delays for this rule are also
    /// cancelled when `required_expression` evaluates to false.
    #[serde(default)]
    pub cancel_on_false: bool,

    // ── Conditional trigger gate ─────────────────────────────────────────────
    /// An optional per-trigger condition expression (Rhai bool) evaluated
    /// *after* the trigger event fires but *before* the main conditions list.
    /// If false the rule is skipped for that specific event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_condition: Option<String>,

    // ── Rule-local variables ─────────────────────────────────────────────────
    /// Initial values for rule-local variables.  These are initialised at
    /// engine start and reset only when the rule is reloaded/restarted.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub variables: HashMap<String, JsonValue>,
}

/// What causes a rule to be evaluated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    DeviceStateChanged {
        /// Primary device ID (used when `device_ids` is empty).
        device_id: String,
        /// Additional device IDs — trigger fires if *any* of these devices
        /// changes.  When non-empty, `device_id` is also included in the set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        device_ids: Vec<String>,
        /// When `None`, any attribute change fires the trigger.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
        /// Only fire when the attribute changes **to** this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<JsonValue>,
        /// Only fire when the previous value was this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<JsonValue>,
        /// Only fire when the previous value was NOT this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_from: Option<JsonValue>,
        /// Only fire when the new value is NOT this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_to: Option<JsonValue>,
        /// Only fire if the attribute has held its new value for at least this
        /// many seconds (sticky / "and stays" trigger).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_duration_secs: Option<u64>,
    },
    MqttMessage {
        topic_pattern: String,
        /// If set, only fire when the raw payload exactly equals this string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
        /// JSON pointer (e.g. `"/temperature"`) to extract a value from the
        /// payload for comparison before firing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_path: Option<String>,
        /// Comparison operator for `value_path` extraction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_op: Option<CompareOp>,
        /// Expected value for the `value_path` comparison.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_cmp: Option<JsonValue>,
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
    /// internal event bus.
    CustomEvent {
        event_type: String,
    },
    /// Fires once immediately after the rule engine has finished pre-populating
    /// its device cache on startup.
    SystemStarted,
    /// Fires on a cron schedule using a 6-field expression:
    /// `{second} {minute} {hour} {day-of-month} {month} {day-of-week}`
    Cron {
        expression: String,
    },
    /// Fires when a device's availability (online/offline) changes.
    DeviceAvailabilityChanged {
        device_id: String,
        #[serde(default)]
        to: Option<bool>,
        /// Only fire if the new availability state has held for at least this
        /// many seconds (sticky trigger guard).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_duration_secs: Option<u64>,
    },
    /// Fires on a physical button push/hold/double-tap/release event.
    ///
    /// Button events arrive as `DeviceStateChanged` with an attribute named
    /// after the event type (`"pushed"`, `"held"`, `"double_tapped"`, or
    /// `"released"`) carrying the button number as its value.
    ButtonEvent {
        device_id: String,
        /// If `None`, fires for any button number.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button_number: Option<u32>,
        event: ButtonEventType,
    },
    /// Fires when a numeric device attribute crosses a threshold.
    ///
    /// Unlike `DeviceStateChanged` + `to`, this trigger only fires on the
    /// crossing edge (e.g. when temperature goes from ≤80 to >80), not on
    /// every change.
    NumericThreshold {
        device_id: String,
        attribute: String,
        op: ThresholdOp,
        value: f64,
        /// Only fire if the threshold condition has held for at least this many
        /// seconds (debounce).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_duration_secs: Option<u64>,
    },
    /// Fires on a recurring schedule without requiring cron syntax.
    ///
    /// Example: `every_n = 15, unit = Minutes` fires every 15 minutes.
    Periodic {
        every_n: u32,
        unit: PeriodicUnit,
    },
}

/// Button event types for `Trigger::ButtonEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonEventType {
    Pushed,
    Held,
    DoubleTapped,
    Released,
}

/// Threshold direction operators for `Trigger::NumericThreshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdOp {
    /// Attribute value is currently above the threshold (fires on every change while true).
    Above,
    /// Attribute value is currently below the threshold (fires on every change while true).
    Below,
    /// Attribute crossed upward: previous ≤ threshold and current > threshold.
    CrossesAbove,
    /// Attribute crossed downward: previous ≥ threshold and current < threshold.
    CrossesBelow,
}

/// Time unit for `Trigger::Periodic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
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
    /// True when a device attribute has not changed for at least `duration_secs` seconds.
    TimeElapsed {
        device_id: String,
        attribute: String,
        duration_secs: u64,
    },
    /// Inverts the result of the wrapped condition.
    Not {
        condition: Box<Condition>,
    },
    /// Passes only if **all** nested conditions pass (explicit AND grouping).
    ///
    /// Useful for building `(A AND B) OR (C AND D)` expressions when combined
    /// with the `Or` variant at the top level.
    And {
        conditions: Vec<Condition>,
    },
    /// Passes if **any** nested condition passes.
    Or {
        conditions: Vec<Condition>,
    },
    /// Passes if **exactly one** nested condition passes.
    Xor {
        conditions: Vec<Condition>,
    },
    /// Passes if this rule's named Private Boolean matches `value`.
    PrivateBooleanIs {
        name: String,
        value: bool,
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

/// A branch in a multi-arm `Conditional` action (`ELSE-IF`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalBranch {
    /// Rhai boolean expression for this branch.
    pub condition: String,
    pub actions: Vec<Action>,
}

/// Variable operation for `SetVariable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableOp {
    /// Replace with the given value (default).
    Set,
    Add,
    Subtract,
    Multiply,
    Divide,
    /// Toggle a boolean variable (ignores value field).
    Toggle,
}

/// Log level for `LogMessage` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

fn default_true() -> bool { true }

/// A wrapper that pairs an `Action` with a per-action enable flag.
///
/// When `enabled` is `false` the executor skips the action and records a
/// `Skipped` trace entry.  Defaults to `true` so existing rule files that
/// omit the field continue to work unchanged.
///
/// TOML representation (the `type` field and action-specific fields are
/// flattened to the same level as `enabled`):
///
/// ```toml
/// [[actions]]
/// type      = "set_device_state"
/// device_id = "light_1"
/// state     = { on = true }
/// enabled   = false           # optional — default true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleAction {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub action: Action,
}

/// A single step in a rule's action sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    SetDeviceState {
        device_id: String,
        state: JsonValue,
        /// When `true`, use the trigger event's value instead of `state`.
        /// Mirrors Hubitat's "Track Event Switch/Dimmer" — useful for
        /// "when switch A changes, mirror the state to switch B".
        #[serde(default)]
        track_event_value: bool,
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
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        retries: Option<u32>,
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
        #[serde(default)]
        title: Option<String>,
    },
    /// Suspend the action sequence without blocking the async runtime.
    ///
    /// When `cancelable` is `true`, this delay can be interrupted by a
    /// `CancelDelays` or `CancelRuleTimers` action using the matching key.
    Delay {
        duration_secs: u64,
        /// Whether this delay can be cancelled externally.
        #[serde(default)]
        cancelable: bool,
        /// Optional label used to cancel a specific delay.  When `None` and
        /// `cancelable` is `true`, a rule-unique key is generated automatically.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_key: Option<String>,
    },
    /// A group of actions executed concurrently via `tokio::join!`.
    Parallel {
        actions: Vec<Action>,
    },
    /// Repeat `actions` until `condition` (Rhai expression → bool) is true.
    /// The condition is checked *after* each iteration (post-condition loop —
    /// body always runs at least once).
    RepeatUntil {
        condition: String,
        actions: Vec<Action>,
        max_iterations: Option<u32>,
        interval_ms: Option<u64>,
    },
    /// Repeat `actions` while `condition` (Rhai expression → bool) is true.
    /// The condition is checked *before* each iteration (pre-condition loop —
    /// body may not run at all).
    RepeatWhile {
        condition: String,
        actions: Vec<Action>,
        #[serde(default)]
        max_iterations: Option<u32>,
        #[serde(default)]
        interval_ms: Option<u64>,
    },
    /// Execute `actions` exactly `count` times.
    RepeatCount {
        count: u32,
        actions: Vec<Action>,
        #[serde(default)]
        interval_ms: Option<u64>,
    },
    /// Evaluate a Rhai boolean expression and execute one of the action branches.
    ///
    /// Supports full `IF / ELSE-IF / ELSE / END-IF` chaining via `else_if`.
    Conditional {
        condition: String,
        then_actions: Vec<Action>,
        /// Ordered list of `ELSE-IF` branches.  First matching branch wins.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_if: Vec<ConditionalBranch>,
        #[serde(default)]
        else_actions: Vec<Action>,
    },
    /// Stops further rules in the current event's evaluation chain.
    StopRuleChain,
    /// Halt execution of the remaining actions in this rule without affecting
    /// pending delays or lower-priority rules.
    ExitRule,
    /// Inline comment / documentation that is logged when action logging is enabled.
    Comment {
        text: String,
    },
    /// Pause the action sequence until a matching event arrives on the bus,
    /// with an optional timeout.
    WaitForEvent {
        /// Custom event type to wait for (matched against `CustomEvent.event_type`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_type: Option<String>,
        /// Device ID whose state-changed event to wait for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        /// Optional attribute filter for device-state events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attribute: Option<String>,
        /// Maximum milliseconds to wait.  When elapsed, execution continues.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Pause the action sequence until a Rhai expression becomes `true`,
    /// with optional timeout and hold-duration.
    WaitForExpression {
        /// Rhai boolean expression re-evaluated on each poll tick.
        expression: String,
        /// How often (ms) to re-evaluate the expression (default: 500 ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll_interval_ms: Option<u64>,
        /// Maximum milliseconds to wait before giving up.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// How long (ms) the expression must remain true before proceeding.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold_duration_ms: Option<u64>,
    },
    /// Set or modify a rule-local variable.
    SetVariable {
        name: String,
        value: JsonValue,
        /// Operation to perform.  Defaults to `Set` (replace).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<VariableOp>,
    },
    /// Directly run the actions of another rule (bypassing its trigger and
    /// required expression).  Equivalent to Hubitat's "Run Rule Actions".
    RunRuleActions {
        rule_id: Uuid,
    },
    /// Pause another rule (prevent its actions from running on trigger events
    /// while paused).
    PauseRule {
        rule_id: Uuid,
    },
    /// Resume a previously paused rule.
    ResumeRule {
        rule_id: Uuid,
    },
    /// Cancel pending cancellable delays.
    ///
    /// If `key` is `Some`, only cancels the delay with that key.
    /// If `key` is `None`, cancels all cancellable delays in the current rule.
    CancelDelays {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Cancel ALL in-flight timers (delays and waits) for a rule.
    ///
    /// If `rule_id` is `None`, cancels timers for the current rule.
    CancelRuleTimers {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule_id: Option<Uuid>,
    },
    /// Set this rule's Private Boolean to `value`.
    ///
    /// Other rules can read this boolean via `Condition::PrivateBooleanIs`.
    SetPrivateBoolean {
        name: String,
        value: bool,
    },
    /// Write a message to the structured log at the given level.
    LogMessage {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<LogLevel>,
    },
    /// Apply a different device state depending on which mode is currently active.
    ///
    /// The first entry whose mode device reports `on == true` wins.  If no mode
    /// matches, `default_state` is applied (when set).  Equivalent to Hubitat's
    /// "Set Switches/Dimmers Per Mode".
    ///
    /// ```toml
    /// [[actions]]
    /// type      = "set_device_state_per_mode"
    /// device_id = "light_desk"
    ///
    /// [[actions.modes]]
    /// mode  = "mode_night"
    /// state = { brightness = 30, on = true }
    ///
    /// [[actions.modes]]
    /// mode  = "mode_away"
    /// state = { on = false }
    ///
    /// [actions.default_state]
    /// brightness = 200
    /// on         = true
    /// ```
    SetDeviceStatePerMode {
        device_id: String,
        modes: Vec<ModeStateEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_state: Option<JsonValue>,
    },
    /// Send ICMP echo requests to a host and branch on reachability.
    ///
    /// Uses the system `ping` binary (`ping -c {count} -W {timeout_secs} {host}`).
    /// `then_actions` run when the host responds; `else_actions` when it does not.
    /// If `response_event` is set, a `Custom` event is fired with
    /// `{ "host": "…", "reachable": true/false, "rtt_ms": … }` so other rules
    /// can react.
    ///
    /// ```toml
    /// [[actions]]
    /// type           = "ping_host"
    /// host           = "192.168.1.1"
    /// count          = 3          # optional — default 1
    /// timeout_ms     = 3000       # optional — default 3000
    /// response_event = "router_ping"  # optional
    ///
    /// [[actions.then_actions]]
    /// type    = "log_message"
    /// message = "Router is up"
    ///
    /// [[actions.else_actions]]
    /// type    = "notify"
    /// channel = "telegram"
    /// message = "Router unreachable!"
    /// ```
    PingHost {
        host: String,
        /// Number of ICMP echo requests to send (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
        /// Total time to wait for all replies, in milliseconds (default 3000).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// Actions to run when the host responds.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        then_actions: Vec<Action>,
        /// Actions to run when the host does not respond.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        else_actions: Vec<Action>,
        /// If set, fires a `Custom` event with this type carrying the ping result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_event: Option<String>,
    },
}

/// Context captured from the event that triggered a rule firing.
/// Injected into Rhai scripts as `trigger_device()`, `trigger_value()`, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerContext {
    pub device_id:  Option<String>,
    pub attribute:  Option<String>,
    pub value:      Option<JsonValue>,
    pub prev_value: Option<JsonValue>,
    pub event_type: Option<String>,
    /// Auxiliary context data — for webhook triggers this holds the query
    /// parameter map (`trigger_extra()` in Rhai scripts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra:      Option<JsonValue>,
}

/// A mode → state mapping entry for `Action::SetDeviceStatePerMode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeStateEntry {
    /// Device ID of the mode (e.g. `"mode_night"`).
    pub mode: String,
    /// State to apply when this mode is active (`on == true`).
    pub state: JsonValue,
}

/// A named snapshot of device states that can be activated as a unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub id: Uuid,
    pub name: String,
    /// Map of device_id → desired attribute values.
    pub states: std::collections::HashMap<String, JsonValue>,
}
