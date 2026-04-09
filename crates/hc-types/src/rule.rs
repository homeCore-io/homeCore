//! Automation rule types: triggers, conditions, and actions.
//!
//! Rules are pure data — created and modified through the REST API, stored as
//! RON files on disk, and evaluated at runtime without any Rust recompilation.
//! The REST API exchanges rules as JSON (externally-tagged serde enums).

use crate::device::DeviceChangeKind;
use chrono::{NaiveTime, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use uuid::Uuid;

/// Controls how a rule behaves when it is triggered while previous actions
/// from the same rule are still executing.
///
/// Mirrors Home Assistant's `mode` automation field.
///
/// ```toml
/// run_mode = "single"   # skip if already running
/// run_mode = "restart"  # cancel in-flight and restart
/// run_mode = { type = "queued", max_queue = 3 }
/// run_mode = "parallel" # default — concurrent firings
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum RunMode {
    /// No limit — concurrent firings run in parallel (default).
    #[default]
    Parallel,
    /// If the rule is already executing, skip this firing.
    Single,
    /// Cancel the in-flight execution (cancels pending delays) and restart.
    Restart,
    /// Queue up to `max_queue` concurrent firings; drop if the queue is full.
    Queued {
        #[serde(default = "default_max_queue")]
        max_queue: usize,
    },
}

fn default_max_queue() -> usize {
    10
}

/// A complete automation rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    // ── Trigger label ────────────────────────────────────────────────────────
    /// Optional human-readable label for this rule's trigger.
    ///
    /// Accessible in Rhai scripts as `trigger_label()`.  Useful for giving
    /// meaningful names to multi-device triggers or to make conditions more
    /// readable:
    ///
    /// ```toml
    /// trigger_label = "motion_hallway"
    /// ```
    /// ```rhai
    /// if trigger_label() == "motion_hallway" { ... }
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_label: Option<String>,

    // ── Run mode (HA-style concurrency control) ──────────────────────────────
    /// Controls what happens when the rule fires while its previous actions are
    /// still executing.  Defaults to `Parallel` (current behavior).
    #[serde(default, skip_serializing_if = "is_parallel")]
    pub run_mode: RunMode,
}

fn is_parallel(m: &RunMode) -> bool {
    *m == RunMode::Parallel
}

/// What causes a rule to be evaluated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    DeviceStateChanged {
        /// Primary device ID (used when `device_ids` is empty).
        #[serde(alias = "device")]
        device_id: String,
        /// Additional device IDs — trigger fires if *any* of these devices
        /// changes.  When non-empty, `device_id` is also included in the set.
        #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "devices")]
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
        /// Optional filter for the origin class of the triggering change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change_kind: Option<DeviceChangeKind>,
        /// Optional exact-match filter for the specific change source label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change_source: Option<String>,
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
        #[serde(alias = "device")]
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
        #[serde(alias = "device")]
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
        #[serde(alias = "device")]
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
    /// Fires when a hub variable is set via `Action::SetHubVariable`.
    ///
    /// If `name` is `Some`, only fires when that specific variable changes.
    /// If `name` is `None`, fires on any hub variable change.
    ///
    /// ```toml
    /// [trigger]
    /// type = "hub_variable_changed"
    /// name = "alarm_state"   # optional — omit to watch all hub vars
    /// ```
    HubVariableChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Fires when a hub mode turns on or off.
    ///
    /// Matches the `mode_changed` internal event emitted by `ModeManager`
    /// whenever any mode device's `on` attribute changes.
    ///
    /// ```toml
    /// [trigger]
    /// type    = "mode_changed"
    /// mode_id = "mode_night"  # optional — omit to fire on any mode change
    /// to      = true          # optional — only fire when turning on
    /// ```
    ModeChanged {
        /// Mode device ID (e.g. `"mode_night"`).  `None` matches any mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode_id: Option<String>,
        /// Only fire when the mode transitions to this on/off state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<bool>,
    },
    /// Fires when a calendar event from a loaded `.ics` file starts (with
    /// optional offset).
    ///
    /// Calendar files are loaded from the configured `calendars.dir` directory
    /// (default `config/calendars/`).  The scheduler checks once per minute;
    /// a rule fires when any event's start time falls in the current minute
    /// window after applying `offset_minutes`.
    ///
    /// ```toml
    /// [trigger]
    /// type = "calendar_event"
    /// calendar_id    = "us_holidays"   # optional — stem of .ics filename
    /// title_contains = "Holiday"       # optional — case-insensitive substring
    /// offset_minutes = -30             # optional — fire 30 min before start
    /// ```
    CalendarEvent {
        /// Stem of the `.ics` filename to match (e.g. `"us_holidays"` for
        /// `us_holidays.ics`).  `None` matches events from any loaded calendar.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calendar_id: Option<String>,
        /// Case-insensitive substring match against the event summary/title.
        /// `None` matches any event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title_contains: Option<String>,
        /// Fire this many minutes before (negative) or after (positive) the
        /// event start time.  Default `0` — fires at the event start minute.
        #[serde(default)]
        offset_minutes: i32,
    },
}

impl Default for Trigger {
    fn default() -> Self {
        Trigger::ManualTrigger
    }
}

/// Button event types for `Trigger::ButtonEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonEventType {
    Pushed,
    Held,
    DoubleTapped,
    Released,
}

/// Threshold direction operators for `Trigger::NumericThreshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
pub enum PeriodicUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
}

/// Solar event types for time-based triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SunEventType {
    Sunrise,
    Sunset,
    SolarNoon,
    CivilDawn,
    CivilDusk,
}

/// A side-effect-free predicate evaluated before actions are executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    DeviceState {
        #[serde(alias = "device")]
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
        #[serde(alias = "device")]
        device_id: String,
        attribute: String,
        duration_secs: u64,
    },
    /// Passes when the device's last change provenance matches the supplied filters.
    DeviceLastChange {
        #[serde(alias = "device")]
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<DeviceChangeKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_name: Option<String>,
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
    /// Passes when a hub variable satisfies the comparison.
    ///
    /// ```toml
    /// [[conditions]]
    /// type  = "hub_variable"
    /// name  = "alarm_state"
    /// op    = "eq"
    /// value = "armed"
    /// ```
    HubVariable {
        name: String,
        op: CompareOp,
        value: JsonValue,
    },
    /// Passes when the named hub mode is in the specified on/off state.
    ///
    /// Reads the mode device's `on` attribute from the device cache.
    ///
    /// ```toml
    /// [[conditions]]
    /// type    = "mode_is"
    /// mode_id = "mode_night"
    /// on      = true
    /// ```
    ModeIs {
        /// Mode device ID (e.g. `"mode_night"`).
        mode_id: String,
        /// Expected on/off state.
        on: bool,
    },
    /// Passes when a calendar event is currently active (start ≤ now < end).
    ///
    /// ```toml
    /// [[conditions]]
    /// type           = "calendar_active"
    /// calendar_id    = "us_holidays"   # optional — stem of .ics filename
    /// title_contains = "Holiday"       # optional — case-insensitive substring
    /// ```
    CalendarActive {
        /// Stem of the `.ics` filename to match.  `None` matches any calendar.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        calendar_id: Option<String>,
        /// Case-insensitive substring match against the event summary/title.
        /// `None` matches any event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title_contains: Option<String>,
    },
}

/// Comparison operators for `Condition::DeviceState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A branch in a multi-arm `Conditional` action (`ELSE-IF`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConditionalBranch {
    /// Rhai boolean expression for this branch.
    pub condition: String,
    pub actions: Vec<Action>,
}

/// Variable operation for `SetVariable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Command for `Action::SetMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModeCommand {
    On,
    Off,
    Toggle,
}

/// Log level for `LogMessage` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

fn default_true() -> bool {
    true
}

/// A wrapper that pairs an `Action` with a per-action enable flag.
///
/// When `enabled` is `false` the executor skips the action and records a
/// `Skipped` trace entry.  Defaults to `true` so existing rule files that
/// omit the field continue to work unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleAction {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub action: Action,
}

/// A single step in a rule's action sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    SetDeviceState {
        #[serde(alias = "device")]
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
    RunScript { script: String },
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
    Parallel { actions: Vec<Action> },
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
    Comment { text: String },
    /// Pause the action sequence until a matching event arrives on the bus,
    /// with an optional timeout.
    WaitForEvent {
        /// Custom event type to wait for (matched against `CustomEvent.event_type`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_type: Option<String>,
        /// Device ID whose state-changed event to wait for.
        #[serde(default, skip_serializing_if = "Option::is_none", alias = "device")]
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
    RunRuleActions { rule_id: Uuid },
    /// Pause another rule (prevent its actions from running on trigger events
    /// while paused).
    PauseRule { rule_id: Uuid },
    /// Resume a previously paused rule.
    ResumeRule { rule_id: Uuid },
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
    SetPrivateBoolean { name: String, value: bool },
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
        #[serde(alias = "device")]
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
    /// Snapshot the current state of one or more devices under a named key.
    ///
    /// The snapshot is stored per-rule and persists across firings (until
    /// replaced or the engine restarts).  Use `RestoreDeviceState` to replay
    /// the saved state back to the devices.
    ///
    /// ```toml
    /// [[actions]]
    /// type       = "capture_device_state"
    /// key        = "pre_movie"
    /// device_ids = ["light_living", "light_hall"]
    /// ```
    CaptureDeviceState {
        /// Rule-local name for this snapshot.
        key: String,
        /// Device IDs to capture.
        #[serde(alias = "devices")]
        device_ids: Vec<String>,
    },
    /// Publish the device states previously saved by `CaptureDeviceState`.
    ///
    /// ```toml
    /// [[actions]]
    /// type = "restore_device_state"
    /// key  = "pre_movie"
    /// ```
    RestoreDeviceState { key: String },
    /// Delay for a duration that depends on the currently active mode.
    ///
    /// The first matching mode entry wins; if no mode matches and `default_secs`
    /// is set, that duration is used instead.  A duration of `0` skips the
    /// delay entirely (useful for "in Away mode, don't wait").
    ///
    /// ```toml
    /// [[actions]]
    /// type         = "delay_per_mode"
    /// default_secs = 60
    ///
    /// [[actions.modes]]
    /// mode         = "mode_night"
    /// duration_secs = 300
    ///
    /// [[actions.modes]]
    /// mode         = "mode_away"
    /// duration_secs = 0   # skip delay when away
    /// ```
    DelayPerMode {
        modes: Vec<ModeDelayEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_secs: Option<u64>,
    },
    /// Set or modify a cross-rule hub variable.
    ///
    /// Hub variables are global key-value pairs shared across all rules.
    /// Setting a variable fires a `hub_variable_changed` event that rules
    /// with `Trigger::HubVariableChanged` can react to.  Variables are
    /// session-only (reset on engine restart).
    ///
    /// ```toml
    /// [[actions]]
    /// type  = "set_hub_variable"
    /// name  = "alarm_state"
    /// value = "armed"
    /// ```
    SetHubVariable {
        name: String,
        value: JsonValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<VariableOp>,
    },
    /// Activate a different scene depending on which mode is currently active.
    ///
    /// The first matching mode entry wins; if no mode matches and
    /// `default_scene_id` is set, that scene is activated.
    ///
    /// ```toml
    /// [[actions]]
    /// type = "activate_scene_per_mode"
    ///
    /// [[actions.modes]]
    /// mode     = "mode_night"
    /// scene_id = "11111111-0000-0000-0000-000000000001"
    ///
    /// [actions.default_scene_id]
    /// scene_id = "22222222-0000-0000-0000-000000000002"
    /// ```
    ActivateScenePerMode {
        modes: Vec<ModeSceneEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_scene_id: Option<Uuid>,
    },
    /// Gradually transition numeric device attributes to target values.
    ///
    /// The executor reads the current value from the device cache and
    /// publishes `steps` intermediate states at equal intervals over
    /// `duration_secs` seconds.  Non-numeric target fields (e.g. `on = true`)
    /// are included unchanged on every step.
    ///
    /// ```toml
    /// [[actions]]
    /// type          = "fade_device"
    /// device_id     = "light_living"
    /// duration_secs = 30
    /// steps         = 30          # optional — default = duration_secs (1/sec)
    ///
    /// [actions.target]
    /// on         = true
    /// brightness = 255
    /// ```
    FadeDevice {
        #[serde(alias = "device")]
        device_id: String,
        /// Target state.  Numeric fields are interpolated; non-numeric fields
        /// are applied as-is on every step.
        target: JsonValue,
        /// Total fade time in seconds.
        duration_secs: u64,
        /// Number of intermediate state publishes (default: 1 per second;
        /// clamped to 2–100).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<u32>,
    },
    /// Turn a hub mode on, off, or toggle it.
    ///
    /// Publishes `{ "command": "on|off|toggle" }` to the mode device's cmd
    /// topic.  The `ModeManager` handles the command and fires a
    /// `mode_changed` event that other rules can react to.
    ///
    /// ```toml
    /// [[actions]]
    /// type    = "set_mode"
    /// mode_id = "mode_away"
    /// command = "on"
    /// ```
    SetMode {
        /// Mode device ID (e.g. `"mode_away"`).
        mode_id: String,
        /// Whether to turn the mode on, off, or toggle its current state.
        command: ModeCommand,
    },
}

/// Context captured from the event that triggered a rule firing.
/// Injected into Rhai scripts as `trigger_device()`, `trigger_value()`, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerContext {
    pub device_id: Option<String>,
    pub attribute: Option<String>,
    pub value: Option<JsonValue>,
    pub prev_value: Option<JsonValue>,
    pub event_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_kind: Option<DeviceChangeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_actor_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Auxiliary context data — for webhook triggers this holds the query
    /// parameter map (`trigger_extra()` in Rhai scripts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<JsonValue>,
    /// User-defined label from `rule.trigger_label` (accessible as `trigger_label()` in Rhai).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_label: Option<String>,
}

/// A mode → state mapping entry for `Action::SetDeviceStatePerMode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeStateEntry {
    /// Device ID of the mode (e.g. `"mode_night"`).
    pub mode: String,
    /// State to apply when this mode is active (`on == true`).
    pub state: JsonValue,
}

/// A mode → delay mapping entry for `Action::DelayPerMode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeDelayEntry {
    pub mode: String,
    /// Seconds to delay when this mode is active.  `0` skips the delay entirely.
    pub duration_secs: u64,
}

/// A mode → scene mapping entry for `Action::ActivateScenePerMode`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeSceneEntry {
    pub mode: String,
    pub scene_id: Uuid,
}

/// A named snapshot of device states that can be activated as a unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub id: Uuid,
    pub name: String,
    /// Map of device_id → desired attribute values.
    pub states: std::collections::HashMap<String, JsonValue>,
}

#[cfg(test)]
mod tests {
    use super::{Action, Condition, Rule, RuleAction, Trigger};
    use serde_json::json;

    #[test]
    fn json_externally_tagged_trigger() {
        let trigger: Trigger = serde_json::from_value(json!({
            "DeviceStateChanged": {
                "device_id": "living_room.floor_lamp",
                "device_ids": ["bedroom.floor_lamp"],
                "attribute": "on",
                "to": true
            }
        }))
        .unwrap();

        match trigger {
            Trigger::DeviceStateChanged {
                device_id,
                device_ids,
                ..
            } => {
                assert_eq!(device_id, "living_room.floor_lamp");
                assert_eq!(device_ids, vec!["bedroom.floor_lamp"]);
            }
            _ => panic!("unexpected trigger variant"),
        }
    }

    #[test]
    fn json_externally_tagged_condition_and_actions() {
        let condition: Condition = serde_json::from_value(json!({
            "DeviceState": {
                "device_id": "living_room.floor_lamp",
                "attribute": "on",
                "op": "Eq",
                "value": true
            }
        }))
        .unwrap();
        match condition {
            Condition::DeviceState { device_id, .. } => {
                assert_eq!(device_id, "living_room.floor_lamp");
            }
            _ => panic!("unexpected condition variant"),
        }

        let action: Action = serde_json::from_value(json!({
            "SetDeviceState": {
                "device_id": "living_room.floor_lamp",
                "state": { "on": true }
            }
        }))
        .unwrap();
        match action {
            Action::SetDeviceState { device_id, .. } => {
                assert_eq!(device_id, "living_room.floor_lamp");
            }
            _ => panic!("unexpected action variant"),
        }
    }

    #[test]
    fn ron_round_trip() {
        let rule = Rule {
            id: uuid::Uuid::nil(),
            name: "Test Rule".to_string(),
            enabled: true,
            priority: 10,
            tags: vec![],
            trigger: Trigger::DeviceStateChanged {
                device_id: "light_1".to_string(),
                device_ids: vec![],
                attribute: Some("on".to_string()),
                to: Some(json!(true)),
                from: None,
                not_from: None,
                not_to: None,
                for_duration_secs: None,
                change_kind: None,
                change_source: None,
            },
            conditions: vec![Condition::ModeIs {
                mode_id: "mode_night".to_string(),
                on: true,
            }],
            actions: vec![RuleAction {
                enabled: true,
                action: Action::SetDeviceState {
                    device_id: "light_1".to_string(),
                    state: json!({"on": true}),
                    track_event_value: false,
                },
            }],
            error: None,
            cooldown_secs: None,
            log_events: false,
            log_triggers: false,
            log_actions: false,
            required_expression: None,
            cancel_on_false: false,
            trigger_condition: None,
            variables: Default::default(),
            trigger_label: None,
            run_mode: Default::default(),
        };

        let ron_str =
            ron::ser::to_string_pretty(&rule, ron::ser::PrettyConfig::default()).unwrap();
        let parsed: Rule = ron::from_str(&ron_str).unwrap();
        assert_eq!(parsed.name, "Test Rule");
        assert_eq!(parsed.priority, 10);
        assert!(parsed.enabled);
        assert_eq!(parsed.actions.len(), 1);
        assert!(parsed.actions[0].enabled);
    }

    #[test]
    fn rule_action_named_field() {
        // RuleAction now uses a named `action` field, not flatten
        let ra = RuleAction {
            enabled: false,
            action: Action::Delay {
                duration_secs: 5,
                cancelable: false,
                cancel_key: None,
            },
        };
        let json = serde_json::to_value(&ra).unwrap();
        assert_eq!(json["enabled"], false);
        assert!(json["action"]["Delay"].is_object());

        let parsed: RuleAction = serde_json::from_value(json).unwrap();
        assert!(!parsed.enabled);
    }
}
