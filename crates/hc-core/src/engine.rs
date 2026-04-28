//! Rule engine — listens on the event bus, evaluates rules, dispatches actions.
//!
//! # In-memory device cache
//! A `DashMap<device_id, attributes>` is populated at startup from the state
//! store and updated synchronously on every `DeviceStateChanged` event *before*
//! rule evaluation begins.  Condition checks never call `spawn_blocking` or
//! touch redb — they read directly from the DashMap.
//!
//! # Early RwLock release
//! The rules `RwLock` is held only long enough to clone the current `Vec<Rule>`
//! into a local snapshot.  All trigger matching and condition evaluation run
//! against the snapshot after the lock is released, so hot-reload is never
//! blocked while rules are being evaluated.
//!
//! # Advanced rule features
//! - `required_expression` — Rhai gate evaluated before trigger fires
//! - `trigger_condition` — per-rule Rhai gate evaluated after trigger fires
//! - `from` / `not_from` / `not_to` / `device_ids` on DeviceStateChanged
//! - `ButtonEvent` and `NumericThreshold` trigger variants
//! - `Periodic` trigger (handled by scheduler, see scheduler.rs)
//! - `And` / `Or` / `Xor` / `PrivateBooleanIs` conditions
//! - `for_duration_secs` on state triggers — deferred fire via synthetic event
//! - Per-rule logging levels (`log_events`, `log_triggers`, `log_actions`)
//! - Pause state check before action dispatch

use crate::calendar_store::CalendarHandle;
use crate::executor::{execute_actions, ActionTrace, ExecutorContext};
use crate::EventBus;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use hc_notify::NotificationService;
use hc_scripting::{ScriptRuntime, AST};
use hc_state::StateStore;
use hc_types::event::Event;
use hc_types::rule::{
    ButtonEventType, CompareOp, Condition, Rule, RunMode, ThresholdOp, Trigger, TriggerContext,
};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub const HISTORY_RING_SIZE: usize = 20;

async fn device_log_name(state: &StateStore, device_id: &str) -> String {
    match state.get_device(device_id).await {
        Ok(Some(device)) => device
            .canonical_name
            .or({
                if device.name.is_empty() {
                    None
                } else {
                    Some(device.name)
                }
            })
            .unwrap_or_else(|| device_id.to_string()),
        _ => device_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Trace types
// ---------------------------------------------------------------------------

/// Overall outcome of a single rule evaluation attempt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum FireOutcome {
    /// All conditions passed and actions were dispatched.
    Fired,
    /// A condition failed — evaluation stopped at `at_index`.
    ConditionFailed { at_index: usize, reason: String },
    /// Rule skipped because it fired within `cooldown_secs`.
    Cooldown { remaining_secs: u64 },
    /// Rule is currently paused via `PauseRule` action.
    Paused,
    /// `required_expression` evaluated to `false`.
    RequiredExpressionFailed,
    /// `trigger_condition` evaluated to `false`.
    TriggerGateFailed,
    /// Rule was skipped due to `run_mode` concurrency policy (Single or Queued).
    Skipped { reason: String },
}

/// Per-condition evaluation result within a rule firing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConditionTrace {
    /// Condition variant name (e.g. "DeviceState", "TimeWindow").
    pub condition_type: String,
    pub passed: bool,
    /// Actual value read at evaluation time (device attribute, elapsed seconds, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<JsonValue>,
    /// Expected value or constraint from the rule definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<JsonValue>,
    /// Human-readable one-line summary, e.g. `"open == false (actual: true) → FAIL"`.
    pub reason: String,
}

/// A single recorded rule evaluation attempt (conditions + actions).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleFiring {
    pub timestamp: chrono::DateTime<Utc>,
    /// Trigger variant that produced this firing attempt.
    pub trigger_type: String,
    /// Context captured from the triggering event.
    pub trigger_context: hc_types::rule::TriggerContext,
    /// Overall outcome of this attempt.
    pub outcome: FireOutcome,
    /// Per-condition results (in order; stops at first failure).
    pub conditions: Vec<ConditionTrace>,
    /// Per-action results (top-level actions only).
    pub actions: Vec<ActionTrace>,
    /// Milliseconds spent evaluating conditions.
    pub eval_ms: u64,
}

/// Per-rule ring buffer of recent firings; keyed by rule `Uuid`.
pub type FireHistoryHandle = Arc<DashMap<Uuid, VecDeque<RuleFiring>>>;

pub struct RuleEngine {
    internal_bus: EventBus,
    pub_bus: EventBus,
    rules: Arc<RwLock<Vec<Rule>>>,
    state: StateStore,
    publish: Option<hc_mqtt_client::PublishHandle>,
    notify: Option<Arc<NotificationService>>,
    /// In-memory device attribute cache.
    device_cache: Arc<DashMap<String, HashMap<String, JsonValue>>>,
    /// Per-attribute last-changed timestamps for `Condition::TimeElapsed`.
    attr_changed_at: Arc<DashMap<String, HashMap<String, DateTime<Utc>>>>,
    /// Per-rule ring buffer of the last `HISTORY_RING_SIZE` evaluation attempts.
    fire_history: FireHistoryHandle,
    /// Count of rule action tasks currently executing.
    in_flight: Arc<AtomicUsize>,
    /// Per-rule last-fire timestamps for cooldown enforcement.
    cooldown_map: Arc<DashMap<Uuid, Instant>>,
    /// Active cancellable delays shared with executor contexts.
    delay_registry: Arc<DashMap<String, Arc<tokio::sync::Notify>>>,
    /// Pause state per rule — checked before dispatching actions.
    pause_state: Arc<DashMap<Uuid, bool>>,
    /// Rule-local variable store; key = (rule_id, variable_name).
    rule_vars: Arc<DashMap<(Uuid, String), JsonValue>>,
    /// Private boolean store; key = (rule_id, boolean_name).
    priv_bools: Arc<DashMap<(Uuid, String), bool>>,
    /// Per-rule device state capture store for `CaptureDeviceState` /
    /// `RestoreDeviceState`.  Key: `(rule_id, capture_key)`.
    capture_store: crate::executor::CaptureStore,
    /// Cross-rule hub variable store.  Key: variable name.
    hub_vars: Arc<DashMap<String, JsonValue>>,
    /// Per-rule in-flight action task counter, used by run_mode: single/queued.
    rule_in_flight: Arc<DashMap<Uuid, Arc<AtomicUsize>>>,
    /// Seconds to wait for in-flight tasks during graceful shutdown.
    drain_timeout_secs: u64,
    /// Shared calendar store for `Condition::CalendarActive`.
    calendars: Option<CalendarHandle>,
    /// Per-script compiled Rhai ASTs, keyed by script text. Populated on first
    /// use and retained for the lifetime of the engine. Stale entries are
    /// harmless — they're simply never referenced again after a rule reload.
    script_cache: Arc<DashMap<String, Arc<AST>>>,
}

impl RuleEngine {
    pub fn new(
        internal_bus: EventBus,
        pub_bus: EventBus,
        rules: Vec<Rule>,
        state: StateStore,
        publish: Option<hc_mqtt_client::PublishHandle>,
        notify: Option<Arc<NotificationService>>,
    ) -> Self {
        // Initialise rule-local variables from each rule's `variables` map.
        let rule_vars: Arc<DashMap<(Uuid, String), JsonValue>> = Arc::new(DashMap::new());
        for rule in &rules {
            for (name, initial) in &rule.variables {
                rule_vars.insert((rule.id, name.clone()), initial.clone());
            }
        }

        Self {
            internal_bus,
            pub_bus,
            rules: Arc::new(RwLock::new(rules)),
            state,
            publish,
            notify,
            device_cache: Arc::new(DashMap::new()),
            attr_changed_at: Arc::new(DashMap::new()),
            fire_history: Arc::new(DashMap::new()),
            in_flight: Arc::new(AtomicUsize::new(0)),
            cooldown_map: Arc::new(DashMap::new()),
            delay_registry: Arc::new(DashMap::new()),
            pause_state: Arc::new(DashMap::new()),
            rule_vars,
            priv_bools: Arc::new(DashMap::new()),
            capture_store: Arc::new(DashMap::new()),
            hub_vars: Arc::new(DashMap::new()),
            rule_in_flight: Arc::new(DashMap::new()),
            drain_timeout_secs: 10,
            calendars: None,
            script_cache: Arc::new(DashMap::new()),
        }
    }

    /// Get-or-compile a script AST, caching the result for later reuse.
    ///
    /// `as_expression = true` uses Rhai's stricter expression parser (no
    /// statements, no trailing semicolon); used for condition gates.
    fn get_or_compile_ast(&self, script: &str, as_expression: bool) -> Result<Arc<AST>> {
        if let Some(entry) = self.script_cache.get(script) {
            let arc: &Arc<AST> = entry.value();
            return Ok(Arc::clone(arc));
        }
        let runtime = ScriptRuntime::new();
        let ast = if as_expression {
            runtime.compile_expression(script)?
        } else {
            runtime.compile(script)?
        };
        let arc: Arc<AST> = Arc::new(ast);
        self.script_cache
            .insert(script.to_string(), Arc::clone(&arc));
        Ok(arc)
    }

    /// Attach a calendar handle for `Condition::CalendarActive` evaluation.
    pub fn with_calendars(mut self, handle: CalendarHandle) -> Self {
        self.calendars = Some(handle);
        self
    }

    /// Override the graceful shutdown drain timeout (default: 10 s).
    pub fn with_drain_timeout(mut self, secs: u64) -> Self {
        self.drain_timeout_secs = secs;
        self
    }

    /// Returns a handle to update the live rule set without restart.
    pub fn rules_handle(&self) -> Arc<RwLock<Vec<Rule>>> {
        Arc::clone(&self.rules)
    }

    /// Returns a handle to the rule fire history ring buffers.
    pub fn fire_history_handle(&self) -> FireHistoryHandle {
        Arc::clone(&self.fire_history)
    }

    /// Returns a handle to the in-flight task counter, used for graceful shutdown.
    pub fn in_flight_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.in_flight)
    }

    /// Returns a closure that purges state for rule IDs no longer in the live set.
    /// Pass this to `RuleWatcher` as the post-reload callback.
    pub fn purge_callback(&self) -> impl Fn(&[Rule]) + Send + Sync + 'static {
        let cooldown_map = Arc::clone(&self.cooldown_map);
        let pause_state = Arc::clone(&self.pause_state);
        let rule_vars = Arc::clone(&self.rule_vars);
        let priv_bools = Arc::clone(&self.priv_bools);
        let capture_store = Arc::clone(&self.capture_store);
        let rule_in_flight = Arc::clone(&self.rule_in_flight);
        let fire_history = Arc::clone(&self.fire_history);

        move |live_rules: &[Rule]| {
            let live_ids: std::collections::HashSet<Uuid> =
                live_rules.iter().map(|r| r.id).collect();

            let before =
                cooldown_map.len() + pause_state.len() + rule_in_flight.len() + fire_history.len();

            cooldown_map.retain(|id, _| live_ids.contains(id));
            pause_state.retain(|id, _| live_ids.contains(id));
            rule_in_flight.retain(|id, _| live_ids.contains(id));
            fire_history.retain(|id, _| live_ids.contains(id));
            rule_vars.retain(|(id, _), _| live_ids.contains(id));
            priv_bools.retain(|(id, _), _| live_ids.contains(id));
            capture_store.retain(|(id, _), _| live_ids.contains(id));

            let after =
                cooldown_map.len() + pause_state.len() + rule_in_flight.len() + fire_history.len();
            let purged = before.saturating_sub(after);
            if purged > 0 {
                tracing::debug!(purged, "Purged stale rule state entries after hot-reload");
            }
        }
    }

    /// Drive the rule engine until the bus is dropped or `shutdown` fires.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        // Pre-populate device cache from current state store.
        match self.state.list_devices().await {
            Ok(devices) => {
                let count = devices.len();
                for d in devices {
                    let baseline = d.last_seen;
                    let ts_map: HashMap<String, DateTime<Utc>> =
                        d.attributes.keys().map(|k| (k.clone(), baseline)).collect();
                    self.attr_changed_at.insert(d.device_id.clone(), ts_map);
                    self.device_cache
                        .insert(d.device_id, d.attributes.into_iter().collect());
                }
                info!(count, "Rule engine: device cache pre-populated");
            }
            Err(e) => warn!(error = %e, "Rule engine: failed to pre-populate device cache"),
        }

        let mut internal_rx = self.internal_bus.subscribe();
        let mut pub_rx = self.pub_bus.subscribe();
        let _ = self.pub_bus.publish(Event::Custom {
            timestamp: Utc::now(),
            event_type: "system_started".to_string(),
            payload: serde_json::json!({}),
        });
        info!("Rule engine started");

        // When no external shutdown sender was provided, `lib.rs` creates a
        // watch channel but immediately drops the sender.  A dropped sender
        // causes `shutdown.changed()` to return `Err(Closed)` on every poll,
        // which — combined with `biased` — would starve the recv branches.
        // Track whether the shutdown channel is still active so we can fall back
        // to an event-only loop once it's closed.
        let mut shutdown_active = true;

        loop {
            if shutdown_active {
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        match changed {
                            Ok(()) => {
                                if *shutdown.borrow() {
                                    info!("Rule engine: shutdown signal received — stopping event loop");
                                    break;
                                }
                            }
                            Err(_) => {
                                // Sender dropped — no more shutdown signals possible.
                                // Switch to the event-only path below.
                                shutdown_active = false;
                            }
                        }
                    }
                    result = internal_rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let Err(e) = self.handle_event(&event).await {
                                    warn!(error = %e, "Rule engine error handling event (internal bus)");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Rule engine lagged by {n} events (internal bus)");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    result = pub_rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let Err(e) = self.handle_event(&event).await {
                                    warn!(error = %e, "Rule engine error handling event (public bus)");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Rule engine lagged by {n} events (public bus)");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            } else {
                // Shutdown channel is gone — just wait for bus events.
                tokio::select! {
                    result = internal_rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let Err(e) = self.handle_event(&event).await {
                                    warn!(error = %e, "Rule engine error handling event (internal bus)");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Rule engine lagged by {n} events (internal bus)");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    result = pub_rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let Err(e) = self.handle_event(&event).await {
                                    warn!(error = %e, "Rule engine error handling event (public bus)");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Rule engine lagged by {n} events (public bus)");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }

        // Drain in-flight tasks (up to drain_timeout_secs).
        let deadline = Instant::now() + Duration::from_secs(self.drain_timeout_secs);
        loop {
            let n = self.in_flight.load(Ordering::SeqCst);
            if n == 0 {
                break;
            }
            if Instant::now() >= deadline {
                warn!(
                    in_flight = n,
                    "Rule engine: shutdown drain timed out — forcing stop"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        info!("Rule engine stopped");
    }

    async fn handle_event(&self, event: &Event) -> Result<()> {
        // ── 1. Update device cache ─────────────────────────────────────────────
        if let Event::DeviceStateChanged {
            device_id, current, ..
        } = event
        {
            let now = Utc::now();
            {
                let prev_attrs = self.device_cache.get(device_id.as_str());
                let mut ts_entry = self.attr_changed_at.entry(device_id.clone()).or_default();
                for (k, new_v) in current {
                    let changed =
                        prev_attrs.as_ref().and_then(|p| p.get(k.as_str())) != Some(new_v);
                    if changed {
                        ts_entry.insert(k.clone(), now);
                    }
                }
            }
            self.device_cache
                .entry(device_id.clone())
                .and_modify(|attrs| {
                    for (k, v) in current {
                        attrs.insert(k.clone(), v.clone());
                    }
                })
                .or_insert_with(|| current.clone());
        }

        // ── 2. Snapshot rules ─────────────────────────────────────────────────
        let rules_snapshot: Vec<Rule> = {
            let guard = self.rules.read().await;
            guard.clone()
        };

        // ── 3. Scheduler tick (pass-through to fire_rule) ─────────────────────
        if let Event::Custom {
            event_type,
            payload,
            ..
        } = event
        {
            if event_type == "scheduler_tick" {
                if let Some(rule_id_str) = payload.get("rule_id").and_then(|v| v.as_str()) {
                    if let Ok(rule_id) = Uuid::parse_str(rule_id_str) {
                        if let Some(rule) =
                            rules_snapshot.iter().find(|r| r.id == rule_id && r.enabled)
                        {
                            debug!(rule_name = %rule.name, rule_id = %rule.id, "rule.trigger: scheduler_tick matched");
                            let _ = self.fire_rule(rule, TriggerContext::default()).await?;
                        }
                    }
                }
                return Ok(());
            }

            // Deferred for_duration_secs re-check
            if event_type == "deferred_rule_fire" {
                if let Some(rule_id_str) = payload.get("rule_id").and_then(|v| v.as_str()) {
                    if let Ok(rule_id) = Uuid::parse_str(rule_id_str) {
                        if let Some(rule) =
                            rules_snapshot.iter().find(|r| r.id == rule_id && r.enabled)
                        {
                            debug!(rule_name = %rule.name, rule_id = %rule.id, "rule.trigger: deferred_rule_fire — re-checking");
                            if still_matches_for_duration(&rule.trigger, &self.device_cache) {
                                let tctx = TriggerContext::default();
                                let _ = self.fire_rule(rule, tctx).await?;
                            } else {
                                debug!(rule_name = %rule.name, "deferred_rule_fire: device state changed, not firing");
                            }
                        }
                    }
                }
                return Ok(());
            }
        }

        log_incoming_event(&self.state, event).await;

        // ── 4. Trigger matching ────────────────────────────────────────────────
        let mut matching: Vec<(&Rule, TriggerContext, bool /* deferred */)> = Vec::new();
        for rule in rules_snapshot.iter() {
            if !rule.enabled {
                debug!(rule_name = %rule.name, "rule.trigger: SKIP (disabled)");
                continue;
            }
            if !trigger_kind_can_match_event(&rule.trigger, event) {
                continue;
            }
            match trigger_check(&rule.trigger, event) {
                TriggerResult::Matched => {
                    if rule.log_events {
                        info!(
                            rule_name = %rule.name, rule_id = %rule.id,
                            trigger   = trigger_type(&rule.trigger),
                            "rule.event: trigger matched"
                        );
                    } else {
                        debug!(
                            rule_name = %rule.name, rule_id = %rule.id,
                            trigger   = trigger_type(&rule.trigger),
                            matched   = true, "rule.trigger"
                        );
                    }
                    let tctx = extract_trigger_ctx(event, rule);
                    // Check for_duration_secs — if set, defer the fire
                    let for_dur = trigger_for_duration(&rule.trigger);
                    matching.push((rule, tctx, for_dur.is_some()));

                    if let Some(dur_secs) = for_dur {
                        let rule_id = rule.id;
                        let rule_name = rule.name.clone();
                        let trigger = rule.trigger.clone();
                        let bus = self.pub_bus.clone();
                        let cache = Arc::clone(&self.device_cache);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(dur_secs)).await;
                            if still_matches_for_duration(&trigger, &cache) {
                                let _ = bus.publish(Event::Custom {
                                    timestamp: Utc::now(),
                                    event_type: "deferred_rule_fire".into(),
                                    payload: serde_json::json!({ "rule_id": rule_id }),
                                });
                            } else {
                                debug!(rule_name = %rule_name, "for_duration_secs: state changed, not firing");
                            }
                        });
                    }
                }
                TriggerResult::NoMatch(reason) => {
                    debug!(
                        rule_name = %rule.name, rule_id = %rule.id,
                        trigger   = trigger_type(&rule.trigger),
                        matched   = false, reason, "rule.trigger"
                    );
                }
            }
        }

        if matching.is_empty() {
            debug!("rule.trigger: no rules matched this event");
            return Ok(());
        }

        debug!(
            count = matching.len(),
            "rule.trigger: {} rule(s) matched, evaluating",
            matching.len()
        );

        for (rule, tctx, deferred) in matching {
            if deferred {
                // Deferred rules are handled by the spawned task above
                continue;
            }
            let stop = self.fire_rule(rule, tctx).await?;
            if stop {
                debug!(
                    rule_name = %rule.name, rule_id = %rule.id,
                    "rule.trigger: StopRuleChain — halting further evaluation"
                );
                break;
            }
        }
        Ok(())
    }

    /// Push a `RuleFiring` into the in-memory ring buffer and persist to DB.
    fn push_history(&self, rule_id: Uuid, firing: RuleFiring) {
        {
            let mut buf = self.fire_history.entry(rule_id).or_default();
            if buf.len() >= HISTORY_RING_SIZE {
                buf.pop_front();
            }
            buf.push_back(firing.clone());
        }
        Self::persist_firing(self.state.clone(), rule_id, firing);
    }

    /// Spawn a background task to write `firing` to the SQLite history table.
    fn persist_firing(store: StateStore, rule_id: Uuid, firing: RuleFiring) {
        tokio::spawn(async move {
            match serde_json::to_string(&firing) {
                Ok(json) => {
                    let rid = rule_id.to_string();
                    let ts = firing.timestamp.to_rfc3339();
                    if let Err(e) = store.append_rule_firing(rid, ts, json).await {
                        warn!(rule_id = %rule_id, error = %e, "failed to persist rule firing to DB");
                    }
                }
                Err(e) => warn!(rule_id = %rule_id, error = %e, "failed to serialize rule firing"),
            }
        });
    }

    /// Pre-populate the in-memory ring buffer from DB records loaded at startup.
    ///
    /// `records` maps rule_id strings to ordered (oldest-first) JSON strings.
    pub fn populate_fire_history(&self, records: HashMap<String, Vec<String>>) {
        let mut loaded = 0usize;
        for (rule_id_str, jsons) in records {
            if let Ok(rule_id) = Uuid::parse_str(&rule_id_str) {
                let mut buf = self.fire_history.entry(rule_id).or_default();
                for json in jsons {
                    match serde_json::from_str::<RuleFiring>(&json) {
                        Ok(firing) => {
                            if buf.len() >= HISTORY_RING_SIZE {
                                buf.pop_front();
                            }
                            buf.push_back(firing);
                            loaded += 1;
                        }
                        Err(e) => {
                            warn!(rule_id = %rule_id_str, error = %e, "skipping malformed fire history record");
                        }
                    }
                }
            }
        }
        info!(entries = loaded, "pre-populated rule fire history from DB");
    }

    /// Build a minimal `RuleFiring` for an early-exit (pre-condition) case.
    fn early_firing(
        &self,
        rule: &Rule,
        trigger_ctx: &TriggerContext,
        outcome: FireOutcome,
    ) -> RuleFiring {
        RuleFiring {
            timestamp: Utc::now(),
            trigger_type: trigger_type(&rule.trigger).to_string(),
            trigger_context: trigger_ctx.clone(),
            outcome,
            conditions: vec![],
            actions: vec![],
            eval_ms: 0,
        }
    }

    /// Fire a rule, returning `Ok(true)` if the rule chain should stop.
    async fn fire_rule(&self, rule: &Rule, trigger_ctx: TriggerContext) -> Result<bool> {
        // ── Pause check ───────────────────────────────────────────────────────
        if self.pause_state.get(&rule.id).map(|v| *v).unwrap_or(false) {
            debug!(rule_name = %rule.name, rule_id = %rule.id, "rule.fire: skipping — rule is paused");
            let firing = self.early_firing(rule, &trigger_ctx, FireOutcome::Paused);
            self.push_history(rule.id, firing);
            return Ok(false);
        }

        // ── Cooldown check ────────────────────────────────────────────────────
        if let Some(cooldown_secs) = rule.cooldown_secs {
            if let Some(last_fire) = self.cooldown_map.get(&rule.id) {
                let elapsed = last_fire.elapsed().as_secs();
                if elapsed < cooldown_secs {
                    let remaining_secs = cooldown_secs - elapsed;
                    debug!(
                        rule_name = %rule.name, rule_id = %rule.id,
                        elapsed_secs = elapsed, cooldown_secs,
                        "rule.cooldown: skipping — within cooldown window"
                    );
                    let firing = self.early_firing(
                        rule,
                        &trigger_ctx,
                        FireOutcome::Cooldown { remaining_secs },
                    );
                    self.push_history(rule.id, firing);
                    return Ok(false);
                }
            }
        }

        // Build the device snapshot once for this entire rule evaluation.
        let snapshot = self.snapshot_from_cache();

        // ── Required Expression gate ──────────────────────────────────────────
        if let Some(ref expr) = rule.required_expression {
            let snap = snapshot.clone();
            let tctx = trigger_ctx.clone();
            let ast = self.get_or_compile_ast(expr, true)?;
            let passes = tokio::task::spawn_blocking(move || {
                ScriptRuntime::new_with_devices(snap)
                    .with_trigger_context(&tctx)
                    .eval_condition_ast(ast.as_ref())
            })
            .await??;
            if !passes {
                if rule.cancel_on_false {
                    let prefix = format!("{}/", rule.id);
                    let keys: Vec<String> = self
                        .delay_registry
                        .iter()
                        .filter(|e| e.key().starts_with(&prefix))
                        .map(|e| e.key().clone())
                        .collect();
                    for k in keys {
                        if let Some((_, n)) = self.delay_registry.remove(&k) {
                            n.notify_one();
                        }
                    }
                }
                debug!(rule_name = %rule.name, rule_id = %rule.id, "rule.required_expression: false — skipping");
                let firing =
                    self.early_firing(rule, &trigger_ctx, FireOutcome::RequiredExpressionFailed);
                self.push_history(rule.id, firing);
                return Ok(false);
            }
        }

        // ── Trigger Condition gate ────────────────────────────────────────────
        if let Some(ref expr) = rule.trigger_condition {
            let snap = snapshot.clone();
            let tctx = trigger_ctx.clone();
            let ast = self.get_or_compile_ast(expr, true)?;
            let passes = tokio::task::spawn_blocking(move || {
                ScriptRuntime::new_with_devices(snap)
                    .with_trigger_context(&tctx)
                    .eval_condition_ast(ast.as_ref())
            })
            .await??;
            if !passes {
                debug!(rule_name = %rule.name, rule_id = %rule.id, "rule.trigger_condition: false — skipping");
                let firing = self.early_firing(rule, &trigger_ctx, FireOutcome::TriggerGateFailed);
                self.push_history(rule.id, firing);
                return Ok(false);
            }
        }

        // ── Run mode concurrency check ────────────────────────────────────────
        let rule_counter = self
            .rule_in_flight
            .entry(rule.id)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();

        match &rule.run_mode {
            RunMode::Parallel => {} // always proceed
            RunMode::Single => {
                if rule_counter.load(Ordering::SeqCst) > 0 {
                    debug!(
                        rule_name = %rule.name, rule_id = %rule.id,
                        "run_mode=single: already in-flight, skipping"
                    );
                    let firing = self.early_firing(
                        rule,
                        &trigger_ctx,
                        FireOutcome::Skipped {
                            reason: "single: already in-flight".into(),
                        },
                    );
                    self.push_history(rule.id, firing);
                    return Ok(false);
                }
            }
            RunMode::Queued { max_queue } => {
                if rule_counter.load(Ordering::SeqCst) >= *max_queue {
                    debug!(
                        rule_name = %rule.name, rule_id = %rule.id,
                        max_queue, "run_mode=queued: queue full, skipping"
                    );
                    let firing = self.early_firing(
                        rule,
                        &trigger_ctx,
                        FireOutcome::Skipped {
                            reason: format!("queued: queue full (max {})", max_queue),
                        },
                    );
                    self.push_history(rule.id, firing);
                    return Ok(false);
                }
            }
            RunMode::Restart => {
                // Cancel all pending cancellable delays for this rule so the
                // in-flight execution winds down quickly, then proceed.
                let prefix = format!("{}/", rule.id);
                let keys: Vec<String> = self
                    .delay_registry
                    .iter()
                    .filter(|e| e.key().starts_with(&prefix))
                    .map(|e| e.key().clone())
                    .collect();
                if !keys.is_empty() {
                    debug!(
                        rule_name = %rule.name, rule_id = %rule.id,
                        cancelled = keys.len(),
                        "run_mode=restart: cancelling in-flight delays"
                    );
                    for k in keys {
                        if let Some((_, n)) = self.delay_registry.remove(&k) {
                            n.notify_one();
                        }
                    }
                }
            }
        }

        let eval_start = Instant::now();
        let condition_traces = self.evaluate_conditions(rule, &snapshot).await?;
        let conditions_passed = rule.conditions.is_empty()
            || (condition_traces.len() == rule.conditions.len()
                && condition_traces.iter().all(|t| t.passed));
        let eval_ms = eval_start.elapsed().as_millis() as u64;

        if !conditions_passed {
            let failed_idx = condition_traces.iter().position(|t| !t.passed).unwrap_or(0);
            let reason = condition_traces
                .get(failed_idx)
                .map(|t| t.reason.clone())
                .unwrap_or_default();
            if rule.log_triggers {
                info!(
                    rule_name = %rule.name, rule_id = %rule.id, fired = false,
                    reason = "condition_failed", failed_cond = failed_idx, eval_ms,
                    "rule.eval: conditions not met"
                );
            } else {
                debug!(
                    rule_name = %rule.name, rule_id = %rule.id, fired = false,
                    reason = "condition_failed", failed_cond = failed_idx, eval_ms,
                    "rule.eval"
                );
            }
            let firing = RuleFiring {
                timestamp: Utc::now(),
                trigger_type: trigger_type(&rule.trigger).to_string(),
                trigger_context: trigger_ctx,
                outcome: FireOutcome::ConditionFailed {
                    at_index: failed_idx,
                    reason: reason.clone(),
                },
                conditions: condition_traces,
                actions: vec![],
                eval_ms,
            };
            self.push_history(rule.id, firing);

            // Normal condition mismatches are expected — already logged at
            // debug level and recorded in per-rule fire history.  No need to
            // emit a bus event; real failures surface via ActionFailed.

            return Ok(false);
        }

        if rule.log_triggers {
            info!(
                rule_name  = %rule.name, rule_id = %rule.id, fired = true,
                conditions = rule.conditions.len(), actions = rule.actions.len(), eval_ms,
                "rule.eval: fired"
            );
        } else {
            debug!(
                rule_name  = %rule.name, rule_id = %rule.id, fired = true,
                conditions = rule.conditions.len(), actions = rule.actions.len(), eval_ms,
                "rule.eval"
            );
        }

        let stop_chain = rule
            .actions
            .iter()
            .any(|ra| matches!(ra.action, hc_types::rule::Action::StopRuleChain));

        // Update cooldown map immediately after confirming the rule fires.
        if rule.cooldown_secs.is_some() {
            self.cooldown_map.insert(rule.id, Instant::now());
        }

        let actions = rule.actions.clone();
        let action_count = actions.len();
        let trigger_type_str = trigger_type(&rule.trigger).to_string();
        let bus = self.pub_bus.clone();
        let rule_id = rule.id;
        let rule_name = rule.name.clone();
        // Per-firing correlation ID threaded through actions → device commands → events.
        let firing_correlation_id = Uuid::new_v4().to_string();
        let in_flight = Arc::clone(&self.in_flight);
        let rule_counter_spawn = Arc::clone(&rule_counter);
        let fire_history = Arc::clone(&self.fire_history);
        let state_for_persist = self.state.clone();

        // Trace accumulator shared between ExecutorContext and the history entry
        // pushed after actions complete.
        let action_trace_buf = Arc::new(std::sync::Mutex::new(Vec::<ActionTrace>::new()));

        // Build the executor context for this firing.
        let ctx = Arc::new(ExecutorContext {
            publish: self.publish.clone(),
            notify: self.notify.clone(),
            event_bus: Some(self.pub_bus.clone()),
            device_cache: Arc::clone(&self.device_cache),
            delay_registry: Arc::clone(&self.delay_registry),
            pause_state: Arc::clone(&self.pause_state),
            rule_vars: Arc::clone(&self.rule_vars),
            priv_bools: Arc::clone(&self.priv_bools),
            capture_store: Arc::clone(&self.capture_store),
            hub_vars: Arc::clone(&self.hub_vars),
            state: Some(self.state.clone()),
            rules_handle: Arc::clone(&self.rules),
            trigger_ctx: trigger_ctx.clone(),
            rule_id: rule.id,
            rule_name: rule.name.clone(),
            log_actions: rule.log_actions,
            exit_flag: Arc::new(AtomicBool::new(false)),
            trace: Some(Arc::clone(&action_trace_buf)),
            correlation_id: Some(firing_correlation_id.clone()),
        });

        in_flight.fetch_add(1, Ordering::SeqCst);
        rule_counter_spawn.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let action_start = Instant::now();
            debug!(rule_name = %rule_name, rule_id = %rule_id, count = action_count, "rule.actions: starting");
            match execute_actions(actions, ctx, snapshot).await {
                Ok(()) => {
                    let action_ms = action_start.elapsed().as_millis();
                    debug!(rule_name = %rule_name, rule_id = %rule_id, action_ms, "rule.actions: completed");
                }
                Err(e) => {
                    warn!(rule_name = %rule_name, rule_id = %rule_id, error = %e, "rule.actions: failed");
                }
            }

            // Record completed firing with full condition + action traces.
            let action_traces = action_trace_buf.lock().unwrap().clone();
            let firing = RuleFiring {
                timestamp: Utc::now(),
                trigger_type: trigger_type_str.clone(),
                trigger_context: trigger_ctx,
                outcome: FireOutcome::Fired,
                conditions: condition_traces,
                actions: action_traces,
                eval_ms,
            };
            {
                let mut buf = fire_history.entry(rule_id).or_default();
                if buf.len() >= HISTORY_RING_SIZE {
                    buf.pop_front();
                }
                buf.push_back(firing.clone());
            }
            Self::persist_firing(state_for_persist, rule_id, firing);

            let total_ms = eval_ms + action_start.elapsed().as_millis() as u64;
            let _ = bus.publish(Event::RuleFired {
                timestamp: chrono::Utc::now(),
                rule_id: rule_id.to_string(),
                rule_name,
                trigger_type: trigger_type_str,
                action_count,
                elapsed_ms: Some(total_ms),
                correlation_id: Some(firing_correlation_id),
            });
            rule_counter_spawn.fetch_sub(1, Ordering::SeqCst);
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(stop_chain)
    }

    /// Evaluate all conditions for a rule.
    ///
    /// Returns a `Vec<ConditionTrace>` in evaluation order, stopping at the
    /// first failure (short-circuit AND).  The caller determines pass/fail by
    /// checking whether all conditions are present and all have `passed = true`.
    async fn evaluate_conditions(
        &self,
        rule: &Rule,
        snapshot: &Arc<HashMap<String, JsonValue>>,
    ) -> Result<Vec<ConditionTrace>> {
        if rule.conditions.is_empty() {
            debug!(rule_name = %rule.name, "rule.conditions: none — auto-pass");
            return Ok(vec![]);
        }

        debug!(rule_name = %rule.name, count = rule.conditions.len(), "rule.conditions: evaluating");

        let mut traces = Vec::with_capacity(rule.conditions.len());
        for (i, cond) in rule.conditions.iter().enumerate() {
            let (passed, trace) = self
                .evaluate_one(rule, i, rule.conditions.len(), cond, snapshot)
                .await?;
            traces.push(trace);
            if !passed {
                break; // short-circuit; remaining conditions are "not evaluated"
            }
        }

        if traces.iter().all(|t| t.passed) {
            debug!(rule_name = %rule.name, "rule.conditions: all passed");
        }
        Ok(traces)
    }

    async fn evaluate_one(
        &self,
        rule: &Rule,
        idx: usize,
        total: usize,
        condition: &Condition,
        snapshot: &Arc<HashMap<String, JsonValue>>,
    ) -> Result<(bool, ConditionTrace)> {
        let cond_label = format!("{}/{}", idx + 1, total);

        match condition {
            Condition::DeviceState {
                device_id,
                attribute,
                op,
                value,
            } => {
                let device = device_log_name(&self.state, device_id).await;
                let entry = self.device_cache.get(device_id.as_str());
                let Some(attrs) = entry else {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, "rule.condition: FAIL — device not found in cache"
                    );
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "device_state".into(),
                            passed: false,
                            actual: None,
                            expected: Some(value.clone()),
                            reason: format!("device '{}' not found in cache", device),
                        },
                    ));
                };
                let Some(actual) = attrs.get(attribute.as_str()) else {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute, "rule.condition: FAIL — attribute not present"
                    );
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "device_state".into(),
                            passed: false,
                            actual: None,
                            expected: Some(value.clone()),
                            reason: format!("device '{}' has no attribute '{}'", device, attribute),
                        },
                    ));
                };
                let result = compare(actual, op, value);
                let op_sym = compare_op_symbol(op);
                if result {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute, op = ?op, expected = %value, actual = %actual,
                        "rule.condition: pass"
                    );
                } else {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute, op = ?op, expected = %value, actual = %actual,
                        "rule.condition: FAIL"
                    );
                }
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "device_state".into(),
                        passed: result,
                        actual: Some(actual.clone()),
                        expected: Some(value.clone()),
                        reason: format!(
                            "{}.{} {} {} (actual: {}) → {}",
                            device,
                            attribute,
                            op_sym,
                            value,
                            actual,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::TimeWindow { start, end } => {
                let now = chrono::Local::now().time();
                let result = if start <= end {
                    now >= *start && now <= *end
                } else {
                    now >= *start || now <= *end
                };
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    %start, %end, now = %now, result, "rule.condition: TimeWindow"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "time_window".into(),
                        passed: result,
                        actual: Some(JsonValue::String(now.format("%H:%M:%S").to_string())),
                        expected: Some(JsonValue::String(format!("{}-{}", start, end))),
                        reason: format!(
                            "now {} within [{}, {}] → {}",
                            now.format("%H:%M:%S"),
                            start,
                            end,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::ScriptExpression { script } => {
                let snippet = if script.len() > 80 {
                    &script[..80]
                } else {
                    script
                };
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    script = %snippet, "rule.condition: ScriptExpression — evaluating"
                );
                let snap = snapshot.clone();
                let ast = self.get_or_compile_ast(script, true)?;
                let hub_snap: HashMap<String, JsonValue> = self
                    .hub_vars
                    .iter()
                    .map(|e| (e.key().clone(), e.value().clone()))
                    .collect();
                let tctx = TriggerContext::default(); // conditions don't use trigger ctx
                let result = tokio::task::spawn_blocking(move || {
                    ScriptRuntime::new_with_devices(snap)
                        .with_hub_vars(hub_snap)
                        .with_trigger_context(&tctx)
                        .eval_condition_ast(ast.as_ref())
                })
                .await??;
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    result, "rule.condition: ScriptExpression"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "script_expression".into(),
                        passed: result,
                        actual: None,
                        expected: Some(JsonValue::String(snippet.to_string())),
                        reason: format!(
                            "script → {} → {}",
                            result,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::Not { condition } => {
                let (inner, _) =
                    Box::pin(self.evaluate_one(rule, idx, total, condition, snapshot)).await?;
                let result = !inner;
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    inner, result, "rule.condition: Not"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "not".into(),
                        passed: result,
                        actual: None,
                        expected: None,
                        reason: format!(
                            "not({}) → {}",
                            inner,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::And { conditions } => {
                let mut all_passed = true;
                for (i, c) in conditions.iter().enumerate() {
                    let (passed, _) =
                        Box::pin(self.evaluate_one(rule, i, conditions.len(), c, snapshot)).await?;
                    if !passed {
                        all_passed = false;
                        debug!(rule_name = %rule.name, "rule.condition: And — short-circuit false at {}", i);
                        break;
                    }
                }
                debug!(rule_name = %rule.name, all_passed, "rule.condition: And");
                Ok((
                    all_passed,
                    ConditionTrace {
                        condition_type: "and".into(),
                        passed: all_passed,
                        actual: None,
                        expected: None,
                        reason: format!(
                            "and({} conditions) → {}",
                            conditions.len(),
                            if all_passed { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::Or { conditions } => {
                let mut any_passed = false;
                for (i, c) in conditions.iter().enumerate() {
                    let (passed, _) =
                        Box::pin(self.evaluate_one(rule, i, conditions.len(), c, snapshot)).await?;
                    if passed {
                        any_passed = true;
                        debug!(rule_name = %rule.name, "rule.condition: Or — short-circuit true at {}", i);
                        break;
                    }
                }
                debug!(rule_name = %rule.name, any_passed, "rule.condition: Or");
                Ok((
                    any_passed,
                    ConditionTrace {
                        condition_type: "or".into(),
                        passed: any_passed,
                        actual: None,
                        expected: None,
                        reason: format!(
                            "or({} conditions) → {}",
                            conditions.len(),
                            if any_passed { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::Xor { conditions } => {
                let mut count = 0usize;
                for (i, c) in conditions.iter().enumerate() {
                    let (passed, _) =
                        Box::pin(self.evaluate_one(rule, i, conditions.len(), c, snapshot)).await?;
                    if passed {
                        count += 1;
                    }
                }
                let result = count == 1;
                debug!(rule_name = %rule.name, count, result, "rule.condition: Xor");
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "xor".into(),
                        passed: result,
                        actual: Some(JsonValue::Number(count.into())),
                        expected: Some(JsonValue::Number(1u64.into())),
                        reason: format!(
                            "xor({} conditions): {} passed → {}",
                            conditions.len(),
                            count,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::PrivateBooleanIs { name, value } => {
                let actual = self
                    .priv_bools
                    .get(&(rule.id, name.clone()))
                    .map(|v| *v)
                    .unwrap_or(false);
                let result = actual == *value;
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    boolean = name, expected = value, actual, result,
                    "rule.condition: PrivateBooleanIs"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "private_boolean_is".into(),
                        passed: result,
                        actual: Some(JsonValue::Bool(actual)),
                        expected: Some(JsonValue::Bool(*value)),
                        reason: format!(
                            "priv_bool.{} == {} (actual: {}) → {}",
                            name,
                            value,
                            actual,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::HubVariable { name, op, value } => {
                let actual = self.hub_vars.get(name.as_str()).map(|v| v.clone());
                let (result, actual_display) = match &actual {
                    Some(a) => (compare(a, op, value), a.clone()),
                    None => (false, JsonValue::Null),
                };
                let op_sym = compare_op_symbol(op);
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    name, op = ?op, expected = %value, actual = %actual_display, result,
                    "rule.condition: HubVariable"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "hub_variable".into(),
                        passed: result,
                        actual: Some(actual_display),
                        expected: Some(value.clone()),
                        reason: format!(
                            "hub.{} {} {} (actual: {}) → {}",
                            name,
                            op_sym,
                            value,
                            actual.as_ref().unwrap_or(&JsonValue::Null),
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::ModeIs {
                mode_id,
                on: expected_on,
            } => {
                let actual_on = self
                    .device_cache
                    .get(mode_id.as_str())
                    .and_then(|attrs| attrs.get("on").and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                let result = actual_on == *expected_on;
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    mode_id, expected = expected_on, actual = actual_on, result,
                    "rule.condition: ModeIs"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "mode_is".into(),
                        passed: result,
                        actual: Some(JsonValue::Bool(actual_on)),
                        expected: Some(JsonValue::Bool(*expected_on)),
                        reason: format!(
                            "mode.{}.on == {} (actual: {}) → {}",
                            mode_id,
                            expected_on,
                            actual_on,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::TimeElapsed {
                device_id,
                attribute,
                duration_secs,
            } => {
                let device = device_log_name(&self.state, device_id).await;
                let changed_at = self
                    .attr_changed_at
                    .get(device_id.as_str())
                    .and_then(|ts| ts.get(attribute.as_str()).copied());

                let Some(changed_at) = changed_at else {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute,
                        "rule.condition: TimeElapsed FAIL — attribute not tracked"
                    );
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "time_elapsed".into(),
                            passed: false,
                            actual: None,
                            expected: Some(JsonValue::Number((*duration_secs).into())),
                            reason: format!(
                                "{}.{} not tracked — no change recorded since startup",
                                device, attribute
                            ),
                        },
                    ));
                };

                let elapsed_secs = (Utc::now() - changed_at).num_seconds().max(0) as u64;
                let result = elapsed_secs >= *duration_secs;
                if result {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute, elapsed_secs, duration_secs,
                        "rule.condition: TimeElapsed pass"
                    );
                } else {
                    debug!(
                        rule_name = %rule.name, cond = %cond_label,
                        device = %device, attribute, elapsed_secs, duration_secs,
                        "rule.condition: TimeElapsed FAIL"
                    );
                }
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "time_elapsed".into(),
                        passed: result,
                        actual: Some(JsonValue::Number(elapsed_secs.into())),
                        expected: Some(JsonValue::Number((*duration_secs).into())),
                        reason: format!(
                            "{}.{}: elapsed {}s {} {}s → {}",
                            device,
                            attribute,
                            elapsed_secs,
                            if result { ">=" } else { "<" },
                            duration_secs,
                            if result { "pass" } else { "FAIL" }
                        ),
                    },
                ))
            }

            Condition::DeviceLastChange {
                device_id,
                kind,
                source,
                actor_id,
                actor_name,
            } => {
                let device = device_log_name(&self.state, device_id).await;
                let Ok(Some(device_state)) = self.state.get_device(device_id).await else {
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "device_last_change".into(),
                            passed: false,
                            actual: None,
                            expected: None,
                            reason: format!("device '{}' not found", device),
                        },
                    ));
                };

                let Some(change) = device_state.last_change else {
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "device_last_change".into(),
                            passed: false,
                            actual: None,
                            expected: None,
                            reason: format!("device '{}' has no last_change metadata", device),
                        },
                    ));
                };

                let passed = kind.as_ref().map(|v| *v == change.kind).unwrap_or(true)
                    && source
                        .as_deref()
                        .map(|v| change.source.as_deref() == Some(v))
                        .unwrap_or(true)
                    && actor_id
                        .as_deref()
                        .map(|v| change.actor_id.as_deref() == Some(v))
                        .unwrap_or(true)
                    && actor_name
                        .as_deref()
                        .map(|v| change.actor_name.as_deref() == Some(v))
                        .unwrap_or(true);

                Ok((
                    passed,
                    ConditionTrace {
                        condition_type: "device_last_change".into(),
                        passed,
                        actual: serde_json::to_value(&change).ok(),
                        expected: Some(serde_json::json!({
                            "kind": kind,
                            "source": source,
                            "actor_id": actor_id,
                            "actor_name": actor_name,
                        })),
                        reason: if passed {
                            format!("{}.last_change matched requested provenance", device)
                        } else {
                            format!("{}.last_change did not match requested provenance", device)
                        },
                    },
                ))
            }

            Condition::CalendarActive {
                calendar_id,
                title_contains,
            } => {
                let Some(ref cal_handle) = self.calendars else {
                    return Ok((
                        false,
                        ConditionTrace {
                            condition_type: "calendar_active".into(),
                            passed: false,
                            actual: None,
                            expected: None,
                            reason: "no calendar store configured".into(),
                        },
                    ));
                };
                let now = Utc::now();
                let entries = cal_handle.read().await;
                let mut matched_summary: Option<String> = None;

                'outer: for entry in entries.iter() {
                    if let Some(ref cid) = calendar_id {
                        if entry.id != *cid {
                            continue;
                        }
                    }
                    for ev in &entry.events {
                        if ev.start > now {
                            // Events are sorted by start; no later ones can be active.
                            break;
                        }
                        if ev.end <= now {
                            continue;
                        }
                        // Event is active: start <= now < end
                        if let Some(ref substr) = title_contains {
                            if !ev.summary.to_lowercase().contains(&substr.to_lowercase()) {
                                continue;
                            }
                        }
                        matched_summary = Some(ev.summary.clone());
                        break 'outer;
                    }
                }

                let result = matched_summary.is_some();
                debug!(
                    rule_name = %rule.name, cond = %cond_label,
                    calendar_id = ?calendar_id, title_contains = ?title_contains,
                    matched = ?matched_summary, result,
                    "rule.condition: CalendarActive"
                );
                Ok((
                    result,
                    ConditionTrace {
                        condition_type: "calendar_active".into(),
                        passed: result,
                        actual: matched_summary.map(JsonValue::String),
                        expected: Some(serde_json::json!({
                            "calendar_id": calendar_id,
                            "title_contains": title_contains,
                        })),
                        reason: if result {
                            "calendar event currently active".into()
                        } else {
                            "no matching calendar event active right now".into()
                        },
                    },
                ))
            }
        }
    }

    /// Convert the DashMap cache to a `HashMap<device_id, {attrs}>` for Rhai scripts.
    ///
    /// Wrapped in `Arc` so multiple script evaluations within a single rule
    /// firing share the same allocation instead of each cloning the full map.
    fn snapshot_from_cache(&self) -> Arc<HashMap<String, JsonValue>> {
        Arc::new(
            self.device_cache
                .iter()
                .map(|entry| {
                    let attrs = JsonValue::Object(
                        entry
                            .value()
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    );
                    (entry.key().clone(), attrs)
                })
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// TriggerContext extraction
// ---------------------------------------------------------------------------

/// Build a `TriggerContext` from the matching event + rule trigger.
fn extract_trigger_ctx(event: &Event, rule: &Rule) -> TriggerContext {
    let label = rule.trigger_label.clone();
    match event {
        Event::DeviceStateChanged {
            device_id,
            current,
            previous,
            change,
            ..
        } => {
            // Find the attribute specified in this rule's trigger (if any).
            let attr = match &rule.trigger {
                Trigger::DeviceStateChanged { attribute, .. } => attribute.clone(),
                Trigger::ButtonEvent { event, .. } => Some(match event {
                    ButtonEventType::Pushed => "pushed".into(),
                    ButtonEventType::Held => "held".into(),
                    ButtonEventType::DoubleTapped => "double_tapped".into(),
                    ButtonEventType::Released => "released".into(),
                }),
                Trigger::NumericThreshold { attribute, .. } => Some(attribute.clone()),
                _ => None,
            };
            let (value, prev_value) = if let Some(ref a) = attr {
                (
                    current.get(a.as_str()).cloned(),
                    previous.get(a.as_str()).cloned(),
                )
            } else {
                // Pick first changed attribute value
                let first_changed = current.iter().find(|(k, v)| previous.get(*k) != Some(v));
                if let Some((_, v)) = first_changed {
                    (Some(v.clone()), None)
                } else {
                    (None, None)
                }
            };
            TriggerContext {
                device_id: Some(device_id.clone()),
                attribute: attr,
                value,
                prev_value,
                event_type: Some("device_state_changed".into()),
                change_kind: Some(change.kind.clone()),
                change_source: change.source.clone(),
                change_actor_id: change.actor_id.clone(),
                change_actor_name: change.actor_name.clone(),
                correlation_id: change.correlation_id.clone(),
                extra: None,
                trigger_label: label,
            }
        }
        Event::MqttMessage { topic, payload, .. } => TriggerContext {
            device_id: None,
            attribute: None,
            value: Some(JsonValue::String(
                String::from_utf8_lossy(payload).into_owned(),
            )),
            prev_value: None,
            event_type: Some(format!("mqtt:{topic}")),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
            trigger_label: label,
        },
        // Webhook events: expose body as trigger_value(), query params as trigger_extra().
        Event::Custom {
            event_type,
            payload,
            ..
        } if event_type == "webhook" => TriggerContext {
            device_id: None,
            attribute: None,
            value: payload.get("body").cloned(),
            prev_value: None,
            event_type: Some(event_type.clone()),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: payload.get("query").cloned(),
            trigger_label: label,
        },
        Event::Custom {
            event_type,
            payload,
            ..
        } => TriggerContext {
            device_id: None,
            attribute: None,
            value: Some(payload.clone()),
            prev_value: None,
            event_type: Some(event_type.clone()),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
            trigger_label: label,
        },
        Event::DeviceAvailabilityChanged {
            device_id,
            available,
            ..
        } => TriggerContext {
            device_id: Some(device_id.clone()),
            attribute: Some("available".into()),
            value: Some(JsonValue::Bool(*available)),
            prev_value: None,
            event_type: Some("device_availability_changed".into()),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
            trigger_label: label,
        },
        Event::DeviceBatteryLow {
            device_id,
            battery_pct,
            threshold_pct,
            ..
        } => TriggerContext {
            device_id: Some(device_id.clone()),
            attribute: Some("battery".into()),
            value: serde_json::Number::from_f64(*battery_pct).map(JsonValue::Number),
            prev_value: serde_json::Number::from_f64(*threshold_pct).map(JsonValue::Number),
            event_type: Some("device_battery_low".into()),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
            trigger_label: label,
        },
        Event::DeviceBatteryRecovered {
            device_id,
            battery_pct,
            ..
        } => TriggerContext {
            device_id: Some(device_id.clone()),
            attribute: Some("battery".into()),
            value: serde_json::Number::from_f64(*battery_pct).map(JsonValue::Number),
            prev_value: None,
            event_type: Some("device_battery_recovered".into()),
            change_kind: None,
            change_source: None,
            change_actor_id: None,
            change_actor_name: None,
            correlation_id: None,
            extra: None,
            trigger_label: label,
        },
        _ => TriggerContext::default(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the `for_duration_secs` value if the trigger has one.
fn trigger_for_duration(trigger: &Trigger) -> Option<u64> {
    match trigger {
        Trigger::DeviceStateChanged {
            for_duration_secs, ..
        } => *for_duration_secs,
        Trigger::DeviceAvailabilityChanged {
            for_duration_secs, ..
        } => *for_duration_secs,
        Trigger::NumericThreshold {
            for_duration_secs, ..
        } => *for_duration_secs,
        _ => None,
    }
}

/// Re-check whether the device state still satisfies the trigger after a
/// `for_duration_secs` delay has elapsed.
fn still_matches_for_duration(
    trigger: &Trigger,
    device_cache: &DashMap<String, HashMap<String, JsonValue>>,
) -> bool {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id,
            attribute,
            to,
            not_to,
            ..
        } => {
            let Some(attrs) = device_cache.get(device_id.as_str()) else {
                return false;
            };
            let Some(attr) = attribute else {
                return true;
            };
            let Some(curr) = attrs.get(attr.as_str()) else {
                return false;
            };
            if let Some(expected) = to {
                if curr != expected {
                    return false;
                }
            }
            if let Some(excluded) = not_to {
                if curr == excluded {
                    return false;
                }
            }
            true
        }
        Trigger::NumericThreshold {
            device_id,
            attribute,
            op,
            value,
            ..
        } => {
            let Some(attrs) = device_cache.get(device_id.as_str()) else {
                return false;
            };
            let Some(curr_f) = attrs.get(attribute.as_str()).and_then(|v| v.as_f64()) else {
                return false;
            };
            let threshold = *value;
            match op {
                ThresholdOp::Above | ThresholdOp::CrossesAbove => curr_f > threshold,
                ThresholdOp::Below | ThresholdOp::CrossesBelow => curr_f < threshold,
            }
        }
        // For DeviceAvailabilityChanged and others, optimistically assume still matches.
        _ => true,
    }
}

/// Short human-readable label for the trigger variant (for log fields).
fn trigger_type(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChanged { .. } => "DeviceStateChanged",
        Trigger::MqttMessage { .. } => "MqttMessage",
        Trigger::TimeOfDay { .. } => "TimeOfDay",
        Trigger::SunEvent { .. } => "SunEvent",
        Trigger::WebhookReceived { .. } => "WebhookReceived",
        Trigger::ManualTrigger => "ManualTrigger",
        Trigger::CustomEvent { .. } => "CustomEvent",
        Trigger::SystemStarted => "SystemStarted",
        Trigger::Cron { .. } => "Cron",
        Trigger::DeviceAvailabilityChanged { .. } => "DeviceAvailabilityChanged",
        Trigger::ButtonEvent { .. } => "ButtonEvent",
        Trigger::NumericThreshold { .. } => "NumericThreshold",
        Trigger::Periodic { .. } => "Periodic",
        Trigger::HubVariableChanged { .. } => "HubVariableChanged",
        Trigger::CalendarEvent { .. } => "CalendarEvent",
        Trigger::ModeChanged { .. } => "ModeChanged",
        Trigger::DeviceBatteryLow { .. } => "DeviceBatteryLow",
        Trigger::DeviceBatteryRecovered { .. } => "DeviceBatteryRecovered",
    }
}

/// Log key fields from the incoming event.
async fn log_incoming_event(state: &StateStore, event: &Event) {
    match event {
        Event::DeviceStateChanged {
            device_id,
            current,
            previous,
            ..
        } => {
            let changes: Vec<String> = current
                .keys()
                .filter(|k| previous.get(*k) != current.get(*k))
                .map(|k| {
                    let prev = previous
                        .get(k)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(none)".into());
                    let curr = current
                        .get(k)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(none)".into());
                    format!("{k}: {prev} → {curr}")
                })
                .collect();
            let device = device_log_name(state, device_id).await;
            debug!(device = %device, changes = %changes.join(", "), "rule.event: DeviceStateChanged");
        }
        Event::DeviceAvailabilityChanged {
            device_id,
            available,
            ..
        } => {
            let device = device_log_name(state, device_id).await;
            debug!(
                device = %device,
                available, "rule.event: DeviceAvailabilityChanged"
            );
        }
        Event::MqttMessage { topic, .. } => {
            debug!(topic, "rule.event: MqttMessage");
        }
        Event::Custom { event_type, .. } => {
            debug!(event_type, "rule.event: Custom");
        }
        _ => {}
    }
}

enum TriggerResult {
    Matched,
    NoMatch(&'static str),
}

/// Fast discriminant-level pre-filter for the dispatch loop.
///
/// Returns `false` when the event's variant (and, for `Custom`, its event_type)
/// cannot possibly match this trigger kind — lets the dispatcher skip
/// `trigger_check` entirely for rules whose trigger type is unrelated to the
/// incoming event. Should remain conservative: return `true` on any uncertainty
/// so rules that could match never get silently dropped.
fn trigger_kind_can_match_event(trigger: &Trigger, event: &Event) -> bool {
    // Scheduler-only triggers (TimeOfDay, SunEvent, Cron, Periodic) never
    // match a dispatched event — the scheduler fires them via `scheduler_tick`
    // which is short-circuited before this function is reached.
    // ManualTrigger is API-only and also never matches here.
    match trigger {
        Trigger::TimeOfDay { .. }
        | Trigger::SunEvent { .. }
        | Trigger::Cron { .. }
        | Trigger::Periodic { .. }
        | Trigger::ManualTrigger => return false,
        _ => {}
    }
    match event {
        Event::DeviceStateChanged { .. } => matches!(
            trigger,
            Trigger::DeviceStateChanged { .. }
                | Trigger::ButtonEvent { .. }
                | Trigger::NumericThreshold { .. }
        ),
        Event::DeviceAvailabilityChanged { .. } => {
            matches!(trigger, Trigger::DeviceAvailabilityChanged { .. })
        }
        Event::DeviceBatteryLow { .. } => {
            matches!(trigger, Trigger::DeviceBatteryLow { .. })
        }
        Event::DeviceBatteryRecovered { .. } => {
            matches!(trigger, Trigger::DeviceBatteryRecovered { .. })
        }
        Event::MqttMessage { .. } => matches!(trigger, Trigger::MqttMessage { .. }),
        Event::Custom { event_type, .. } => match event_type.as_str() {
            "webhook" => matches!(trigger, Trigger::WebhookReceived { .. }),
            "hub_variable_changed" => matches!(trigger, Trigger::HubVariableChanged { .. }),
            "calendar_event" => matches!(trigger, Trigger::CalendarEvent { .. }),
            "system_started" => matches!(trigger, Trigger::SystemStarted),
            "mode_changed" => matches!(trigger, Trigger::ModeChanged { .. }),
            // Unknown or user-emitted events can only match CustomEvent triggers.
            _ => matches!(trigger, Trigger::CustomEvent { .. }),
        },
        // Unknown event variants: don't filter — preserve existing behavior.
        _ => true,
    }
}

/// Check whether an event matches a trigger.
fn trigger_check(trigger: &Trigger, event: &Event) -> TriggerResult {
    use TriggerResult::*;
    match (trigger, event) {
        // ── DeviceStateChanged ──────────────────────────────────────────────
        (
            Trigger::DeviceStateChanged {
                device_id,
                device_ids,
                attribute,
                to,
                from,
                not_from,
                not_to,
                change_kind,
                change_source,
                ..
            },
            Event::DeviceStateChanged {
                device_id: eid,
                current,
                previous,
                change,
                ..
            },
        ) => {
            // Match if device_id equals OR any of device_ids equals
            let dev_matches = device_id == eid || device_ids.iter().any(|d| d == eid);
            if !dev_matches {
                return NoMatch("device_id mismatch");
            }
            match attribute {
                None => {}
                Some(attr) => {
                    let Some(curr_val) = current.get(attr.as_str()) else {
                        return NoMatch("attribute not in current state");
                    };
                    let prev_val = previous.get(attr.as_str());
                    // Must have changed
                    if prev_val == Some(curr_val) {
                        return NoMatch("attribute value unchanged");
                    }
                    if let Some(expected) = to {
                        if curr_val != expected {
                            return NoMatch("attribute did not change to expected value (to)");
                        }
                    }
                    if let Some(excluded) = not_to {
                        if curr_val == excluded {
                            return NoMatch("attribute changed to excluded value (not_to)");
                        }
                    }
                    if let Some(expected_from) = from {
                        if prev_val != Some(expected_from) {
                            return NoMatch("previous value does not match 'from' filter");
                        }
                    }
                    if let Some(excluded_from) = not_from {
                        if prev_val == Some(excluded_from) {
                            return NoMatch("previous value matches 'not_from' exclusion");
                        }
                    }
                }
            }
            if let Some(expected_kind) = change_kind {
                if change.kind != *expected_kind {
                    return NoMatch("change_kind mismatch");
                }
            }
            if let Some(expected_source) = change_source {
                if change.source.as_deref() != Some(expected_source.as_str()) {
                    return NoMatch("change_source mismatch");
                }
            }
            Matched
        }
        // ── ButtonEvent ───────────────────────────────────────────────────
        (
            Trigger::ButtonEvent {
                device_id,
                button_number,
                event: btn_event,
            },
            Event::DeviceStateChanged {
                device_id: eid,
                current,
                previous,
                ..
            },
        ) => {
            if device_id != eid {
                return NoMatch("device_id mismatch");
            }
            let attr = match btn_event {
                ButtonEventType::Pushed => "pushed",
                ButtonEventType::Held => "held",
                ButtonEventType::DoubleTapped => "double_tapped",
                ButtonEventType::Released => "released",
            };
            let Some(new_val) = current.get(attr) else {
                return NoMatch("button attribute not in current state");
            };
            // Must have changed (button events are momentary)
            if previous.get(attr) == Some(new_val) {
                return NoMatch("button attribute value unchanged");
            }
            if let Some(expected_btn) = button_number {
                let actual_btn = new_val.as_u64().unwrap_or(0) as u32;
                if actual_btn != *expected_btn {
                    return NoMatch("button number mismatch");
                }
            }
            Matched
        }

        // ── NumericThreshold ──────────────────────────────────────────────
        (
            Trigger::NumericThreshold {
                device_id,
                attribute,
                op,
                value,
                ..
            },
            Event::DeviceStateChanged {
                device_id: eid,
                current,
                previous,
                ..
            },
        ) => {
            if device_id != eid {
                return NoMatch("device_id mismatch");
            }
            let Some(curr_f) = current.get(attribute.as_str()).and_then(|v| v.as_f64()) else {
                return NoMatch("attribute not numeric in current state");
            };
            let prev_f = previous.get(attribute.as_str()).and_then(|v| v.as_f64());
            let threshold = *value;
            let fires = match op {
                ThresholdOp::Above => curr_f > threshold,
                ThresholdOp::Below => curr_f < threshold,
                ThresholdOp::CrossesAbove => {
                    prev_f.is_some_and(|p| p <= threshold) && curr_f > threshold
                }
                ThresholdOp::CrossesBelow => {
                    prev_f.is_some_and(|p| p >= threshold) && curr_f < threshold
                }
            };
            if fires {
                Matched
            } else {
                NoMatch("threshold condition not met")
            }
        }

        (_, Event::DeviceStateChanged { .. }) => {
            NoMatch("wrong trigger type for DeviceStateChanged")
        }

        // ── MqttMessage ────────────────────────────────────────────────────
        (
            Trigger::MqttMessage {
                topic_pattern,
                payload: expected_payload,
                value_path,
                value_op,
                value_cmp,
            },
            Event::MqttMessage {
                topic,
                payload: msg_payload,
                ..
            },
        ) => {
            if !mqtt_topic_matches(topic_pattern, topic) {
                return NoMatch("topic pattern mismatch");
            }
            let payload_str = String::from_utf8_lossy(msg_payload);
            if let Some(expected) = expected_payload {
                if payload_str.as_ref() != expected.as_str() {
                    return NoMatch("payload mismatch");
                }
            }
            if let (Some(path), Some(op), Some(cmp)) = (value_path, value_op, value_cmp) {
                let Ok(json) = serde_json::from_str::<JsonValue>(&payload_str) else {
                    return NoMatch("payload is not valid JSON for value_path match");
                };
                let Some(actual) = json.pointer(path.as_str()) else {
                    return NoMatch("value_path not found in payload");
                };
                if !compare(actual, op, cmp) {
                    return NoMatch("value_path comparison failed");
                }
            }
            Matched
        }

        // ── Time-based + calendar (handled by scheduler) ──────────────────
        (
            Trigger::TimeOfDay { .. }
            | Trigger::SunEvent { .. }
            | Trigger::Cron { .. }
            | Trigger::Periodic { .. }
            | Trigger::CalendarEvent { .. },
            _,
        ) => NoMatch("handled by scheduler"),

        // ── WebhookReceived ────────────────────────────────────────────────
        (
            Trigger::WebhookReceived { path: trigger_path },
            Event::Custom {
                event_type,
                payload,
                ..
            },
        ) => {
            if event_type != "webhook" {
                return NoMatch("not a webhook event");
            }
            if payload.get("path").and_then(|v| v.as_str()) == Some(trigger_path.as_str()) {
                Matched
            } else {
                NoMatch("webhook path mismatch")
            }
        }

        // ── CustomEvent ────────────────────────────────────────────────────
        (Trigger::CustomEvent { event_type }, Event::Custom { event_type: et, .. }) => {
            if event_type == et {
                Matched
            } else {
                NoMatch("event_type mismatch")
            }
        }

        // ── SystemStarted ──────────────────────────────────────────────────
        (Trigger::SystemStarted, Event::Custom { event_type, .. }) => {
            if event_type == "system_started" {
                Matched
            } else {
                NoMatch("not system_started")
            }
        }

        // ── HubVariableChanged ─────────────────────────────────────────────
        (
            Trigger::HubVariableChanged { name: filter },
            Event::Custom {
                event_type,
                payload,
                ..
            },
        ) if event_type == "hub_variable_changed" => match filter {
            None => Matched,
            Some(n) => {
                if payload.get("name").and_then(|v| v.as_str()) == Some(n.as_str()) {
                    Matched
                } else {
                    NoMatch("hub variable name mismatch")
                }
            }
        },

        // ── ModeChanged ────────────────────────────────────────────────────
        (
            Trigger::ModeChanged {
                mode_id: filter,
                to,
            },
            Event::Custom {
                event_type,
                payload,
                ..
            },
        ) if event_type == "mode_changed" => {
            if let Some(expected_id) = filter {
                if payload.get("mode_id").and_then(|v| v.as_str()) != Some(expected_id.as_str()) {
                    return NoMatch("mode_id mismatch");
                }
            }
            if let Some(expected_on) = to {
                let actual = payload.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
                if actual != *expected_on {
                    return NoMatch("mode 'to' state mismatch");
                }
            }
            Matched
        }

        // ── ManualTrigger ──────────────────────────────────────────────────
        (Trigger::ManualTrigger, _) => NoMatch("manual trigger only fires via API"),

        // ── DeviceAvailabilityChanged ──────────────────────────────────────
        (
            Trigger::DeviceAvailabilityChanged { device_id, to, .. },
            Event::DeviceAvailabilityChanged {
                device_id: ev_device,
                available,
                ..
            },
        ) => {
            if device_id != ev_device {
                return NoMatch("device_id mismatch");
            }
            if let Some(expected) = to {
                if expected != available {
                    return NoMatch("availability mismatch");
                }
            }
            Matched
        }
        (_, Event::DeviceAvailabilityChanged { .. }) => {
            NoMatch("wrong trigger type for DeviceAvailabilityChanged")
        }

        // ── DeviceBatteryLow ───────────────────────────────────────────────
        (
            Trigger::DeviceBatteryLow { device_id },
            Event::DeviceBatteryLow {
                device_id: ev_device,
                ..
            },
        ) => match device_id {
            Some(want) if want != ev_device => NoMatch("device_id mismatch"),
            _ => Matched,
        },
        (_, Event::DeviceBatteryLow { .. }) => NoMatch("wrong trigger type for DeviceBatteryLow"),

        // ── DeviceBatteryRecovered ─────────────────────────────────────────
        (
            Trigger::DeviceBatteryRecovered { device_id },
            Event::DeviceBatteryRecovered {
                device_id: ev_device,
                ..
            },
        ) => match device_id {
            Some(want) if want != ev_device => NoMatch("device_id mismatch"),
            _ => Matched,
        },
        (_, Event::DeviceBatteryRecovered { .. }) => {
            NoMatch("wrong trigger type for DeviceBatteryRecovered")
        }

        _ => NoMatch("event type does not match trigger type"),
    }
}

/// MQTT wildcard topic matching (`+` = single level, `#` = multi-level).
fn mqtt_topic_matches(pattern: &str, topic: &str) -> bool {
    let mut pparts = pattern.split('/');
    let mut tparts = topic.split('/');
    loop {
        match (pparts.next(), tparts.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => continue,
            (Some(p), Some(t)) if p == t => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn compare_op_symbol(op: &CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "==",
        CompareOp::Ne => "!=",
        CompareOp::Gt => ">",
        CompareOp::Gte => ">=",
        CompareOp::Lt => "<",
        CompareOp::Lte => "<=",
    }
}

fn compare(actual: &JsonValue, op: &CompareOp, expected: &JsonValue) -> bool {
    match op {
        CompareOp::Eq => actual == expected,
        CompareOp::Ne => actual != expected,
        CompareOp::Gt => num_cmp(actual, expected) == Some(std::cmp::Ordering::Greater),
        CompareOp::Gte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CompareOp::Lt => num_cmp(actual, expected) == Some(std::cmp::Ordering::Less),
        CompareOp::Lte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}

fn num_cmp(a: &JsonValue, b: &JsonValue) -> Option<std::cmp::Ordering> {
    let af = a.as_f64()?;
    let bf = b.as_f64()?;
    af.partial_cmp(&bf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use hc_types::device::{DeviceChange, DeviceChangeKind};
    use serde_json::json;

    fn dsc_event(device_id: &str, attr: &str, prev: JsonValue, curr: JsonValue) -> Event {
        let mut previous = HashMap::new();
        previous.insert(attr.into(), prev);
        let mut current = HashMap::new();
        current.insert(attr.into(), curr);
        Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.into(),
            device_name: None,
            previous,
            current,
            changed: vec![attr.into()],
            change: DeviceChange::unknown(),
        }
    }

    #[test]
    fn webhook_trigger_matches_correct_path() {
        let trigger = Trigger::WebhookReceived {
            path: "doorbell".into(),
        };
        let event = Event::Custom {
            timestamp: Utc::now(),
            event_type: "webhook".into(),
            payload: json!({ "path": "doorbell", "body": {} }),
        };
        assert!(matches!(
            trigger_check(&trigger, &event),
            TriggerResult::Matched
        ));
    }

    #[test]
    fn webhook_trigger_does_not_match_wrong_path() {
        let trigger = Trigger::WebhookReceived {
            path: "doorbell".into(),
        };
        let event = Event::Custom {
            timestamp: Utc::now(),
            event_type: "webhook".into(),
            payload: json!({ "path": "motion", "body": {} }),
        };
        assert!(matches!(
            trigger_check(&trigger, &event),
            TriggerResult::NoMatch(_)
        ));
    }

    #[test]
    fn mqtt_wildcard_hash_matches_any_suffix() {
        assert!(mqtt_topic_matches(
            "homecore/#",
            "homecore/devices/light/state"
        ));
        assert!(mqtt_topic_matches(
            "homecore/#",
            "homecore/events/rule_fired"
        ));
        assert!(!mqtt_topic_matches("homecore/#", "other/topic"));
    }

    #[test]
    fn mqtt_wildcard_plus_matches_single_level() {
        assert!(mqtt_topic_matches(
            "homecore/devices/+/state",
            "homecore/devices/light/state"
        ));
        assert!(!mqtt_topic_matches(
            "homecore/devices/+/state",
            "homecore/devices/light/cmd"
        ));
        assert!(!mqtt_topic_matches(
            "homecore/devices/+/state",
            "homecore/devices/a/b/state"
        ));
    }

    #[test]
    fn device_state_trigger_from_filter() {
        let trigger = Trigger::DeviceStateChanged {
            device_id: "door_1".into(),
            device_ids: vec![],
            attribute: Some("locked".into()),
            to: None,
            from: Some(json!(true)),
            not_from: None,
            not_to: None,
            for_duration_secs: None,
            change_kind: None,
            change_source: None,
        };
        // from = true (was locked), now false (unlocked) — matches
        let ev = dsc_event("door_1", "locked", json!(true), json!(false));
        assert!(matches!(
            trigger_check(&trigger, &ev),
            TriggerResult::Matched
        ));
        // from = false (was unlocked), now true (locked) — does NOT match
        let ev2 = dsc_event("door_1", "locked", json!(false), json!(true));
        assert!(matches!(
            trigger_check(&trigger, &ev2),
            TriggerResult::NoMatch(_)
        ));
    }

    #[test]
    fn device_state_trigger_multi_device_ids() {
        let trigger = Trigger::DeviceStateChanged {
            device_id: "motion_1".into(),
            device_ids: vec!["motion_2".into(), "motion_3".into()],
            attribute: Some("motion".into()),
            to: Some(json!("active")),
            from: None,
            not_from: None,
            not_to: None,
            for_duration_secs: None,
            change_kind: None,
            change_source: None,
        };
        // Fires for motion_2 (in device_ids)
        let ev = dsc_event("motion_2", "motion", json!("inactive"), json!("active"));
        assert!(matches!(
            trigger_check(&trigger, &ev),
            TriggerResult::Matched
        ));
        // Does not fire for unknown device
        let ev2 = dsc_event("motion_99", "motion", json!("inactive"), json!("active"));
        assert!(matches!(
            trigger_check(&trigger, &ev2),
            TriggerResult::NoMatch(_)
        ));
    }

    #[test]
    fn device_state_trigger_matches_change_provenance_filters() {
        let trigger = Trigger::DeviceStateChanged {
            device_id: "hall_switch".into(),
            device_ids: vec![],
            attribute: Some("on".into()),
            to: Some(json!(true)),
            from: None,
            not_from: None,
            not_to: None,
            for_duration_secs: None,
            change_kind: Some(DeviceChangeKind::Physical),
            change_source: Some("physical".into()),
        };

        let mut event = dsc_event("hall_switch", "on", json!(false), json!(true));
        if let Event::DeviceStateChanged { change, .. } = &mut event {
            *change = DeviceChange::physical(Some("physical".into()));
        }

        assert!(matches!(
            trigger_check(&trigger, &event),
            TriggerResult::Matched
        ));
    }

    #[test]
    fn device_state_trigger_rejects_wrong_change_provenance() {
        let trigger = Trigger::DeviceStateChanged {
            device_id: "hall_switch".into(),
            device_ids: vec![],
            attribute: Some("on".into()),
            to: Some(json!(true)),
            from: None,
            not_from: None,
            not_to: None,
            for_duration_secs: None,
            change_kind: Some(DeviceChangeKind::Homecore),
            change_source: Some("rule".into()),
        };

        let mut event = dsc_event("hall_switch", "on", json!(false), json!(true));
        if let Event::DeviceStateChanged { change, .. } = &mut event {
            *change = DeviceChange::physical(Some("physical".into()));
        }

        assert!(matches!(
            trigger_check(&trigger, &event),
            TriggerResult::NoMatch(_)
        ));
    }

    #[test]
    fn numeric_threshold_crosses_above() {
        let trigger = Trigger::NumericThreshold {
            device_id: "temperature_sensor".into(),
            attribute: "temperature".into(),
            op: ThresholdOp::CrossesAbove,
            value: 80.0,
            for_duration_secs: None,
        };
        // Crossing upward: 75 → 82
        let ev = dsc_event(
            "temperature_sensor",
            "temperature",
            json!(75.0),
            json!(82.0),
        );
        assert!(matches!(
            trigger_check(&trigger, &ev),
            TriggerResult::Matched
        ));
        // Already above, no crossing: 81 → 85
        let ev2 = dsc_event(
            "temperature_sensor",
            "temperature",
            json!(81.0),
            json!(85.0),
        );
        assert!(matches!(
            trigger_check(&trigger, &ev2),
            TriggerResult::NoMatch(_)
        ));
        // Crossing downward: not a CrossesAbove
        let ev3 = dsc_event(
            "temperature_sensor",
            "temperature",
            json!(85.0),
            json!(75.0),
        );
        assert!(matches!(
            trigger_check(&trigger, &ev3),
            TriggerResult::NoMatch(_)
        ));
    }

    #[test]
    fn button_event_trigger_any_button() {
        let trigger = Trigger::ButtonEvent {
            device_id: "pico_remote".into(),
            button_number: None,
            event: ButtonEventType::Pushed,
        };
        let ev = dsc_event("pico_remote", "pushed", json!(0), json!(1));
        assert!(matches!(
            trigger_check(&trigger, &ev),
            TriggerResult::Matched
        ));
    }

    #[test]
    fn button_event_trigger_specific_button() {
        let trigger = Trigger::ButtonEvent {
            device_id: "pico_remote".into(),
            button_number: Some(2),
            event: ButtonEventType::Pushed,
        };
        // Button 2 pressed
        let ev = dsc_event("pico_remote", "pushed", json!(0), json!(2));
        assert!(matches!(
            trigger_check(&trigger, &ev),
            TriggerResult::Matched
        ));
        // Button 1 pressed (wrong number)
        let ev2 = dsc_event("pico_remote", "pushed", json!(0), json!(1));
        assert!(matches!(
            trigger_check(&trigger, &ev2),
            TriggerResult::NoMatch(_)
        ));
    }
}
