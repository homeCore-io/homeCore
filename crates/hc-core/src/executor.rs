//! Action executor — runs rule actions sequentially or concurrently.
//!
//! # ExecutorContext
//!
//! All shared state (publish handle, delay registry, pause state, variables,
//! trigger context, etc.) is bundled into `Arc<ExecutorContext>`.  This avoids
//! a growing parameter list as new features are added and lets parallel
//! branches share the same state safely.
//!
//! # Cancellable Delays
//!
//! When `Delay { cancelable: true, cancel_key }` fires, an `Arc<Notify>` is
//! registered in `ctx.delay_registry` under `"{rule_id}/{key}"`.  A subsequent
//! `CancelDelays` or `CancelRuleTimers` action notifies the handle so the
//! `tokio::select!` inside the delay wakes early.  The entry is removed
//! regardless of how the delay exits.
//!
//! # ExitRule propagation
//!
//! `Action::ExitRule` sets `ctx.exit_flag`.  Every iteration of
//! `execute_actions_inner` checks the flag before dispatching the next action.
//! This propagates through all nested loops and recursive calls.

use anyhow::{anyhow, Result};
use chrono::Utc;
use dashmap::DashMap;
use hc_mqtt_client::PublishHandle;
use hc_notify::NotificationService;
use hc_scripting::{EffectsBuf, ScriptRuntime, ScriptSideEffect};
use hc_state::StateStore;
use hc_types::device::{with_command_change_metadata, DeviceChange};
use hc_types::event::Event;
use hc_types::rule::{Action, LogLevel, RuleAction, TriggerContext, VariableOp};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Action trace types
// ---------------------------------------------------------------------------

/// Outcome of a single action execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ActionOutcome {
    Ok,
    Error {
        message: String,
    },
    /// Action was present but disabled via `enabled = false`.
    Skipped,
}

/// Trace record for one top-level action within a rule firing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActionTrace {
    /// Zero-based index in the top-level action list.
    pub index: usize,
    /// Action variant name (e.g. "SetDeviceState", "Delay").
    pub action_type: String,
    /// Human-readable description of what the action targeted or contained.
    pub description: String,
    pub outcome: ActionOutcome,
    /// Elapsed milliseconds for this action (including any nested work).
    pub duration_ms: u64,
}

use crate::EventBus;

/// Boxed future alias used for mutually-recursive async functions.
/// `run_single_action` and `execute_actions_inner` call each other, so their
/// return types must be concrete (boxed) rather than opaque `impl Future` to
/// allow the compiler to verify `Send` bounds without infinite type expansion.
type BoxFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>>;

async fn device_log_name(state: Option<&StateStore>, device_id: &str) -> String {
    let Some(store) = state else {
        return device_id.to_string();
    };

    match store.get_device(device_id).await {
        Ok(Some(device)) => device
            .canonical_name
            .or_else(|| {
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

fn rule_change(ctx: &ExecutorContext, source: &str) -> DeviceChange {
    DeviceChange::homecore(source.to_string())
        .with_actor(Some(ctx.rule_id.to_string()), Some(ctx.rule_name.clone()))
        .with_correlation_id(ctx.correlation_id.clone())
}

async fn publish_device_command(
    ctx: &ExecutorContext,
    device_id: &str,
    payload: JsonValue,
    change: DeviceChange,
) -> Result<()> {
    let topic = format!("homecore/devices/{device_id}/cmd");
    let payload = with_command_change_metadata(payload, &change);
    match &ctx.publish {
        Some(ph) => ph.publish(&topic, serde_json::to_vec(&payload)?).await?,
        None => warn!(device_id, "device command dropped — no publish handle"),
    }

    // Emit DeviceCommandSent event for troubleshooting visibility.
    if let Some(ref bus) = ctx.event_bus {
        let _ = bus.publish(Event::DeviceCommandSent {
            timestamp: chrono::Utc::now(),
            device_id: device_id.to_string(),
            command: payload,
            source: "rule".to_string(),
            source_id: Some(ctx.rule_id.to_string()),
            correlation_id: ctx.correlation_id.clone(),
        });
    }

    Ok(())
}

/// Shared HTTP client — initialised once, reused for every `CallService` action.
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("HomeCore/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build shared HTTP client")
    })
}

/// Maximum recursive depth for `RunRuleActions` calls.
const MAX_CALL_DEPTH: u32 = 10;

// ---------------------------------------------------------------------------
// ExecutorContext
// ---------------------------------------------------------------------------

/// Shared state threaded through every action handler in a single rule firing.
///
/// Create one per rule firing, wrap in `Arc`, then pass to `execute_actions`.
/// All fields are either immutable or wrapped in concurrent containers so
/// `Parallel` branches sharing the same context are safe.
pub struct ExecutorContext {
    pub publish: Option<PublishHandle>,
    pub notify: Option<Arc<NotificationService>>,
    pub event_bus: Option<EventBus>,
    /// Live device attribute cache — used by `WaitForExpression` for fresh reads
    /// and by `RunRuleActions` to build a current snapshot for the called rule.
    pub device_cache: Arc<DashMap<String, HashMap<String, JsonValue>>>,
    /// Registry of active cancellable delays.
    /// Key format: `"{rule_id}/{cancel_key}"`.
    pub delay_registry: Arc<DashMap<String, Arc<tokio::sync::Notify>>>,
    /// Pause state per rule (`true` = paused).
    pub pause_state: Arc<DashMap<Uuid, bool>>,
    /// Rule-local variable store.  Key: `(rule_id, variable_name)`.
    pub rule_vars: Arc<DashMap<(Uuid, String), JsonValue>>,
    /// Private boolean store.  Key: `(rule_id, boolean_name)`.
    pub priv_bools: Arc<DashMap<(Uuid, String), bool>>,
    /// Live rule set — used by `RunRuleActions` to locate target rules.
    pub rules_handle: Arc<RwLock<Vec<hc_types::rule::Rule>>>,
    /// Context extracted from the triggering event.
    pub trigger_ctx: TriggerContext,
    /// ID of the rule currently executing.
    pub rule_id: Uuid,
    pub rule_name: String,
    /// When `true`, each action is logged at `info` level.
    pub log_actions: bool,
    /// Set to `true` by `Action::ExitRule` to stop further action execution.
    pub exit_flag: Arc<AtomicBool>,
    /// Per-rule device state capture store.  Key: `(rule_id, capture_key)`.
    /// Shared across all firings of the same rule so that Capture in one
    /// firing can be Restored in a later firing.
    pub capture_store: Arc<DashMap<(Uuid, String), HashMap<String, HashMap<String, JsonValue>>>>,
    /// Cross-rule hub variable store.
    pub hub_vars: Arc<DashMap<String, JsonValue>>,
    /// State store — used by `ActivateScenePerMode` to look up scene contents.
    pub state: Option<StateStore>,
    /// When `Some`, each top-level action appends an `ActionTrace` after it
    /// completes.  `None` disables tracing (e.g. for `RunRuleActions` calls).
    pub trace: Option<Arc<Mutex<Vec<ActionTrace>>>>,
    /// Per-firing correlation ID threaded through device commands and events.
    pub correlation_id: Option<String>,
}

impl ExecutorContext {
    /// Minimal context for unit tests.
    #[cfg(test)]
    pub fn for_test() -> Arc<Self> {
        Arc::new(Self {
            publish: None,
            notify: None,
            event_bus: None,
            device_cache: Arc::new(DashMap::new()),
            delay_registry: Arc::new(DashMap::new()),
            pause_state: Arc::new(DashMap::new()),
            rule_vars: Arc::new(DashMap::new()),
            priv_bools: Arc::new(DashMap::new()),
            capture_store: Arc::new(DashMap::new()),
            hub_vars: Arc::new(DashMap::new()),
            state: None,
            rules_handle: Arc::new(RwLock::new(vec![])),
            trigger_ctx: TriggerContext::default(),
            rule_id: Uuid::nil(),
            rule_name: "test".into(),
            log_actions: false,
            exit_flag: Arc::new(AtomicBool::new(false)),
            trace: None,
            correlation_id: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a script-ready device snapshot from the live DashMap cache.
///
/// Called by `WaitForExpression` and `RunRuleActions` when they need a fresh
/// view of device state rather than the snapshot built at rule-fire time.
fn snapshot_from_cache(
    cache: &DashMap<String, HashMap<String, JsonValue>>,
) -> HashMap<String, JsonValue> {
    cache
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
        .collect()
}

/// Promote an f64 to the most compact JSON number type.
fn to_json_number(f: f64) -> JsonValue {
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        JsonValue::Number((f as i64).into())
    } else {
        serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute a list of rule actions against the provided context and device snapshot.
///
/// `snapshot` is a frozen-at-fire-time view of device state used by Rhai
/// scripts.  It stays consistent across all script evaluations within one
/// rule firing.  `ctx.device_cache` (the live DashMap) is used only by
/// `WaitForExpression` and `RunRuleActions`.
///
/// Actions with `enabled = false` are skipped and recorded as `Skipped` in the
/// action trace.
pub async fn execute_actions(
    actions: Vec<RuleAction>,
    ctx: Arc<ExecutorContext>,
    snapshot: HashMap<String, JsonValue>,
) -> Result<()> {
    let total = actions.len();
    for (idx, ra) in actions.into_iter().enumerate() {
        if ctx.exit_flag.load(Ordering::SeqCst) {
            debug!(rule = %ctx.rule_name, "execute_actions: ExitRule — stopping");
            break;
        }
        if !ra.enabled {
            if let Some(ref trace_buf) = ctx.trace {
                let action_type = action_type_name(&ra.action).to_string();
                let description = action_description(Arc::clone(&ctx), &ra.action).await;
                trace_buf.lock().unwrap().push(ActionTrace {
                    index: idx,
                    action_type,
                    description,
                    outcome: ActionOutcome::Skipped,
                    duration_ms: 0,
                });
            }
            if ctx.log_actions {
                info!(
                    rule  = %ctx.rule_name,
                    label = %format!("action[{}/{}]", idx + 1, total),
                    "action: disabled — skipping"
                );
            }
            continue;
        }
        execute_one(ra.action, idx, total, Arc::clone(&ctx), snapshot.clone(), 0).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal execution engine
// ---------------------------------------------------------------------------

fn execute_actions_inner(
    actions: Vec<Action>,
    ctx: Arc<ExecutorContext>,
    snapshot: HashMap<String, JsonValue>,
    call_depth: u32,
) -> BoxFut {
    Box::pin(async move {
        let total = actions.len();
        for (idx, action) in actions.into_iter().enumerate() {
            if ctx.exit_flag.load(Ordering::SeqCst) {
                debug!(rule = %ctx.rule_name, "execute_actions: ExitRule — stopping");
                break;
            }
            execute_one(
                action,
                idx,
                total,
                Arc::clone(&ctx),
                snapshot.clone(),
                call_depth,
            )
            .await?;
        }
        Ok(())
    })
}

async fn execute_one(
    action: Action,
    idx: usize,
    total: usize,
    ctx: Arc<ExecutorContext>,
    snapshot: HashMap<String, JsonValue>,
    call_depth: u32,
) -> Result<()> {
    let label = format!("action[{}/{}]", idx + 1, total);
    let action_type = action_type_name(&action).to_string();
    let description = action_description(Arc::clone(&ctx), &action).await;
    let is_comment = matches!(action, Action::Comment { .. });
    let trace_start = Instant::now();

    if ctx.log_actions && !is_comment {
        info!(rule = %ctx.rule_name, label, action = %action_type, "action: executing");
    }

    // Run action in an inner async block so `?` propagates to `result` rather
    // than returning early from `execute_one` before we can record the trace.
    let result: Result<()> = async {
        match action {
            Action::Parallel { actions } => {
                let count = actions.len();
                debug!(
                    label,
                    parallel_count = count,
                    "action: Parallel — spawning {} concurrent actions",
                    count
                );
                let handles: Vec<_> = actions
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let c = Arc::clone(&ctx);
                        let snap = snapshot.clone();
                        debug!(
                            "action: Parallel[{}/{}] — {}",
                            i + 1,
                            count,
                            action_type_name(&a)
                        );
                        tokio::spawn(run_single_action(a, c, snap, call_depth))
                    })
                    .collect();
                for h in handles {
                    h.await??;
                }
                debug!(label, "action: Parallel — all done");
            }
            other => run_single_action(other, Arc::clone(&ctx), snapshot, call_depth).await?,
        }
        Ok(())
    }
    .await;

    // Record trace for top-level actions (call_depth 0, excluding RunRuleActions
    // sub-calls which set call_depth > 0 and pass a ctx with trace = None).
    if let Some(ref trace_buf) = ctx.trace {
        let outcome = match &result {
            Ok(()) => ActionOutcome::Ok,
            Err(e) => ActionOutcome::Error {
                message: e.to_string(),
            },
        };

        // Emit ActionFailed event for errors so the activity stream shows them.
        if let ActionOutcome::Error { ref message } = outcome {
            if let Some(ref bus) = ctx.event_bus {
                let _ = bus.publish(Event::ActionFailed {
                    timestamp: chrono::Utc::now(),
                    rule_id: ctx.rule_id.to_string(),
                    rule_name: ctx.rule_name.clone(),
                    action_index: idx,
                    action_type: action_type.clone(),
                    error: message.clone(),
                    correlation_id: ctx.correlation_id.clone(),
                });
            }
        }

        trace_buf.lock().unwrap().push(ActionTrace {
            index: idx,
            action_type,
            description,
            outcome,
            duration_ms: trace_start.elapsed().as_millis() as u64,
        });
    }

    result
}

fn run_single_action(
    action: Action,
    ctx: Arc<ExecutorContext>,
    snapshot: HashMap<String, JsonValue>,
    call_depth: u32,
) -> BoxFut {
    Box::pin(async move {
        match action {
            // ── Parallel (nested inside another action body) ─────────────────────
            Action::Parallel { actions } => {
                let count = actions.len();
                debug!(parallel_count = count, "action: Parallel (nested)");
                let handles: Vec<_> = actions
                    .into_iter()
                    .map(|a| {
                        let c = Arc::clone(&ctx);
                        let snap = snapshot.clone();
                        tokio::spawn(run_single_action(a, c, snap, call_depth))
                    })
                    .collect();
                for h in handles {
                    h.await??;
                }
            }

            // ── Delay ─────────────────────────────────────────────────────────────
            Action::Delay {
                duration_secs,
                cancelable,
                cancel_key,
            } => {
                debug!(duration_secs, cancelable, "action: Delay");
                if cancelable {
                    let key = cancel_key
                        .map(|k| format!("{}/{k}", ctx.rule_id))
                        .unwrap_or_else(|| format!("{}/auto_{}", ctx.rule_id, Uuid::new_v4()));
                    let notify_handle = Arc::new(tokio::sync::Notify::new());
                    ctx.delay_registry
                        .insert(key.clone(), Arc::clone(&notify_handle));
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(duration_secs)) => {}
                        _ = notify_handle.notified() => {
                            debug!(key, "action: Delay — cancelled early");
                        }
                    }
                    ctx.delay_registry.remove(&key);
                } else {
                    tokio::time::sleep(Duration::from_secs(duration_secs)).await;
                }
                debug!(duration_secs, "action: Delay — done");
            }

            // ── RepeatUntil (post-condition loop) ─────────────────────────────────
            Action::RepeatUntil {
                condition,
                actions,
                max_iterations,
                interval_ms,
            } => {
                let limit = max_iterations.unwrap_or(100);
                let delay = interval_ms.unwrap_or(0);
                let snippet = if condition.len() > 60 {
                    &condition[..60]
                } else {
                    &condition
                };
                debug!(condition = %snippet, limit, delay_ms = delay, "action: RepeatUntil — starting");
                for i in 0..limit {
                    if ctx.exit_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let cond_script = condition.clone();
                    let snap = snapshot.clone();
                    let done = tokio::task::spawn_blocking(move || {
                        ScriptRuntime::new_with_devices(snap).eval_condition(&cond_script)
                    })
                    .await??;
                    debug!(
                        iteration = i + 1,
                        done, "action: RepeatUntil — condition check"
                    );
                    if done {
                        debug!(
                            iterations = i + 1,
                            "action: RepeatUntil — condition met, exiting loop"
                        );
                        break;
                    }
                    if i == limit - 1 {
                        warn!(max = limit, "action: RepeatUntil — hit max_iterations without condition becoming true");
                        break;
                    }
                    execute_actions_inner(
                        actions.clone(),
                        Arc::clone(&ctx),
                        snapshot.clone(),
                        call_depth,
                    )
                    .await?;
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
            }

            // ── RepeatWhile (pre-condition loop) ──────────────────────────────────
            Action::RepeatWhile {
                condition,
                actions,
                max_iterations,
                interval_ms,
            } => {
                let limit = max_iterations.unwrap_or(100);
                let delay = interval_ms.unwrap_or(0);
                let snippet = if condition.len() > 60 {
                    &condition[..60]
                } else {
                    &condition
                };
                debug!(condition = %snippet, limit, "action: RepeatWhile — starting");
                let mut i = 0u32;
                loop {
                    if ctx.exit_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if i >= limit {
                        warn!(max = limit, "action: RepeatWhile — hit max_iterations");
                        break;
                    }
                    let snap = snapshot.clone();
                    let cond = condition.clone();
                    let passes = tokio::task::spawn_blocking(move || {
                        ScriptRuntime::new_with_devices(snap).eval_condition(&cond)
                    })
                    .await??;
                    if !passes {
                        debug!(
                            iterations = i,
                            "action: RepeatWhile — condition false, exiting loop"
                        );
                        break;
                    }
                    execute_actions_inner(
                        actions.clone(),
                        Arc::clone(&ctx),
                        snapshot.clone(),
                        call_depth,
                    )
                    .await?;
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    i += 1;
                }
            }

            // ── RepeatCount (counted loop) ────────────────────────────────────────
            Action::RepeatCount {
                count,
                actions,
                interval_ms,
            } => {
                let delay = interval_ms.unwrap_or(0);
                debug!(count, "action: RepeatCount — starting");
                for i in 0..count {
                    if ctx.exit_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    execute_actions_inner(
                        actions.clone(),
                        Arc::clone(&ctx),
                        snapshot.clone(),
                        call_depth,
                    )
                    .await?;
                    if delay > 0 && i < count - 1 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
            }

            // ── SetDeviceState ─────────────────────────────────────────────────────
            Action::SetDeviceState {
                device_id,
                state,
                track_event_value,
            } => {
                let device = device_log_name(ctx.state.as_ref(), &device_id).await;
                let actual_state = if track_event_value {
                    ctx.trigger_ctx.value.clone().unwrap_or(state)
                } else {
                    state
                };
                debug!(device = %device, payload = %actual_state, track_event_value, "action: SetDeviceState");
                publish_device_command(
                    &ctx,
                    &device_id,
                    actual_state,
                    rule_change(&ctx, "rule"),
                )
                .await?;
                debug!(device = %device, "action: SetDeviceState — published");
            }

            // ── PublishMqtt ────────────────────────────────────────────────────────
            Action::PublishMqtt {
                topic,
                payload,
                retain,
            } => {
                debug!(
                    topic,
                    retain,
                    payload_len = payload.len(),
                    "action: PublishMqtt"
                );
                match &ctx.publish {
                    Some(ph) => {
                        if retain {
                            ph.publish_retained(&topic, payload.into_bytes()).await?;
                        } else {
                            ph.publish(&topic, payload.into_bytes()).await?;
                        }
                        debug!(topic, retain, "action: PublishMqtt — published");
                    }
                    None => warn!(topic, "action: PublishMqtt — no publish handle, dropped"),
                }
            }

            // ── FireEvent ──────────────────────────────────────────────────────────
            Action::FireEvent {
                event_type,
                payload,
            } => {
                debug!(event_type, "action: FireEvent");
                if let Some(ref ph) = ctx.publish {
                    let topic = format!("homecore/events/{event_type}");
                    ph.publish_json(&topic, &payload, false).await?;
                    debug!(event_type, "action: FireEvent — published to MQTT");
                }
                if let Some(ref bus) = ctx.event_bus {
                    let ev = Event::Custom {
                        timestamp: chrono::Utc::now(),
                        event_type: event_type.clone(),
                        payload: payload.clone(),
                    };
                    let _ = bus.publish(ev);
                    debug!(event_type, "action: FireEvent — emitted to internal bus");
                }
            }

            // ── CallService ────────────────────────────────────────────────────────
            Action::CallService {
                url,
                method,
                body,
                timeout_ms,
                retries,
                response_event,
            } => {
                let method_upper = method.to_uppercase();
                let timeout = Duration::from_millis(timeout_ms.unwrap_or(10_000));
                let max_attempts = retries.unwrap_or(0) + 1;
                let client = http_client();
                debug!(url, method = %method_upper, retries = retries.unwrap_or(0), timeout_ms = timeout_ms.unwrap_or(10_000), "action: CallService");
                let mut last_err: anyhow::Error = anyhow!("no attempts made");
                let call_start = Instant::now();
                'retry: for attempt in 0..max_attempts {
                    if attempt > 0 {
                        let backoff_ms = 500u64 * (1u64 << (attempt - 1).min(3));
                        info!(url, attempt, backoff_ms, "action: CallService — retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    }
                    let req = match method_upper.as_str() {
                        "GET" => client.get(&url),
                        "POST" => client.post(&url).json(&body),
                        "PUT" => client.put(&url).json(&body),
                        "PATCH" => client.patch(&url).json(&body),
                        "DELETE" => client.delete(&url),
                        other => return Err(anyhow!("Unsupported HTTP method: {other}")),
                    };
                    match req.timeout(timeout).send().await {
                        Err(e) => {
                            warn!(url, attempt, error = %e, "action: CallService — request failed");
                            last_err = e.into();
                        }
                        Ok(resp) => {
                            let status = resp.status();
                            if status.is_server_error() {
                                warn!(url, %status, attempt, "action: CallService — 5xx, will retry");
                                last_err = anyhow!("HTTP {status}");
                                continue 'retry;
                            }
                            if !status.is_success() {
                                warn!(url, %status, "action: CallService — HTTP error (not retrying)");
                                return Err(anyhow!("HTTP {status}"));
                            }
                            let elapsed_ms = call_start.elapsed().as_millis();
                            info!(url, %status, elapsed_ms, "action: CallService — OK");
                            if let Some(ref event_type) = response_event {
                                if let Some(ref ph) = ctx.publish {
                                    let resp_body: JsonValue =
                                        resp.json().await.unwrap_or(JsonValue::Null);
                                    let topic = format!("homecore/events/{event_type}");
                                    ph.publish_json(&topic, &resp_body, false).await?;
                                    debug!(
                                        event_type,
                                        "action: CallService — response published as event"
                                    );
                                }
                            }
                            return Ok(());
                        }
                    }
                }
                return Err(last_err);
            }

            // ── RunScript ─────────────────────────────────────────────────────────
            Action::RunScript { script } => {
                let snippet = if script.len() > 80 {
                    &script[..80]
                } else {
                    &script
                };
                debug!(script = %snippet, "action: RunScript — starting");
                let buf: EffectsBuf = Arc::new(Mutex::new(Vec::new()));
                let buf_clone = Arc::clone(&buf);
                let snap = snapshot.clone();
                let script_clone = script.clone();
                let trigger_ctx = ctx.trigger_ctx.clone();
                let vars_snapshot: HashMap<String, JsonValue> = ctx
                    .rule_vars
                    .iter()
                    .filter(|e| e.key().0 == ctx.rule_id)
                    .map(|e| (e.key().1.clone(), e.value().clone()))
                    .collect();
                let hub_snap: HashMap<String, JsonValue> = ctx
                    .hub_vars
                    .iter()
                    .map(|e| (e.key().clone(), e.value().clone()))
                    .collect();
                tokio::task::spawn_blocking(move || {
                    ScriptRuntime::new_with_devices(snap)
                        .with_side_effects(buf_clone)
                        .with_trigger_context(&trigger_ctx)
                        .with_rule_vars(vars_snapshot)
                        .with_hub_vars(hub_snap)
                        .run_action(&script_clone)
                        .map(|_| ())
                })
                .await??;
                let effects = std::mem::take(&mut *buf.lock().unwrap());
                if !effects.is_empty() {
                    debug!(script = %snippet, count = effects.len(), "action: RunScript — executing side effects");
                }
                for effect in effects {
                    execute_script_effect(
                        effect,
                        ctx.publish.clone(),
                        ctx.notify.clone(),
                        ctx.state.clone(),
                        ctx.correlation_id.clone(),
                    )
                    .await?;
                }
                debug!(script = %snippet, "action: RunScript — completed");
            }

            // ── Conditional (IF / ELSE-IF / ELSE) ─────────────────────────────────
            Action::Conditional {
                condition,
                then_actions,
                else_if,
                else_actions,
            } => {
                let snippet = if condition.len() > 80 {
                    &condition[..80]
                } else {
                    &condition
                };
                debug!(condition = %snippet, else_if_branches = else_if.len(), "action: Conditional — evaluating");
                let snap = snapshot.clone();
                let cond = condition.clone();
                let passed = tokio::task::spawn_blocking(move || {
                    ScriptRuntime::new_with_devices(snap).eval_condition(&cond)
                })
                .await??;

                if passed {
                    debug!(branch = "then", "action: Conditional — selected");
                    execute_actions_inner(then_actions, Arc::clone(&ctx), snapshot, call_depth)
                        .await?;
                } else {
                    let mut matched_else_if = false;
                    for branch in else_if {
                        if ctx.exit_flag.load(Ordering::SeqCst) {
                            break;
                        }
                        let snap = snapshot.clone();
                        let cond = branch.condition.clone();
                        let branch_passed = tokio::task::spawn_blocking(move || {
                            ScriptRuntime::new_with_devices(snap).eval_condition(&cond)
                        })
                        .await??;
                        if branch_passed {
                            debug!(branch = "else_if", "action: Conditional — selected");
                            execute_actions_inner(
                                branch.actions,
                                Arc::clone(&ctx),
                                snapshot.clone(),
                                call_depth,
                            )
                            .await?;
                            matched_else_if = true;
                            break;
                        }
                    }
                    if !matched_else_if {
                        debug!(branch = "else", "action: Conditional — selected");
                        execute_actions_inner(else_actions, Arc::clone(&ctx), snapshot, call_depth)
                            .await?;
                    }
                }
            }

            // ── Notify ─────────────────────────────────────────────────────────────
            Action::Notify {
                channel,
                message,
                title,
            } => {
                let title_str = title.as_deref().unwrap_or("HomeCore Alert");
                debug!(channel, title = title_str, message = %message, "action: Notify");
                match &ctx.notify {
                    Some(svc) => {
                        if let Err(e) = svc.notify(&channel, title_str, &message).await {
                            warn!(channel, error = %e, "action: Notify — failed");
                        } else {
                            info!(channel, "action: Notify — sent");
                        }
                    }
                    None => warn!(
                        channel,
                        "action: Notify — no NotificationService configured"
                    ),
                }
            }

            // ── StopRuleChain ──────────────────────────────────────────────────────
            Action::StopRuleChain => {
                // Consumed by the engine layer before the task spawns.
                debug!("action: StopRuleChain (no-op in executor)");
            }

            // ── ExitRule ───────────────────────────────────────────────────────────
            Action::ExitRule => {
                info!(rule = %ctx.rule_name, "action: ExitRule — setting exit flag");
                ctx.exit_flag.store(true, Ordering::SeqCst);
            }

            // ── Comment ───────────────────────────────────────────────────────────
            Action::Comment { text } => {
                if ctx.log_actions {
                    info!(rule = %ctx.rule_name, comment = %text, "action: Comment");
                } else {
                    debug!(rule = %ctx.rule_name, comment = %text, "action: Comment");
                }
            }

            // ── LogMessage ─────────────────────────────────────────────────────────
            Action::LogMessage { message, level } => {
                let rule = ctx.rule_name.as_str();
                match level.unwrap_or(LogLevel::Info) {
                    LogLevel::Trace => trace!(%rule, "{message}"),
                    LogLevel::Debug => debug!(%rule, "{message}"),
                    LogLevel::Info => info!(%rule,  "{message}"),
                    LogLevel::Warn => warn!(%rule,  "{message}"),
                    LogLevel::Error => error!(%rule, "{message}"),
                }
            }

            // ── SetPrivateBoolean ──────────────────────────────────────────────────
            Action::SetPrivateBoolean { name, value } => {
                debug!(rule = %ctx.rule_name, name, value, "action: SetPrivateBoolean");
                ctx.priv_bools.insert((ctx.rule_id, name), value);
            }

            // ── SetVariable ───────────────────────────────────────────────────────
            Action::SetVariable { name, value, op } => {
                let key = (ctx.rule_id, name.clone());
                let op = op.unwrap_or(VariableOp::Set);
                let new_val = match op {
                    VariableOp::Set => value,
                    VariableOp::Toggle => {
                        let current = ctx
                            .rule_vars
                            .get(&key)
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        JsonValue::Bool(!current)
                    }
                    VariableOp::Add => {
                        let current = ctx
                            .rule_vars
                            .get(&key)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        to_json_number(current + value.as_f64().unwrap_or(0.0))
                    }
                    VariableOp::Subtract => {
                        let current = ctx
                            .rule_vars
                            .get(&key)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        to_json_number(current - value.as_f64().unwrap_or(0.0))
                    }
                    VariableOp::Multiply => {
                        let current = ctx
                            .rule_vars
                            .get(&key)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        to_json_number(current * value.as_f64().unwrap_or(1.0))
                    }
                    VariableOp::Divide => {
                        let current = ctx
                            .rule_vars
                            .get(&key)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let divisor = value.as_f64().unwrap_or(1.0);
                        if divisor == 0.0 {
                            warn!(rule = %ctx.rule_name, variable = name, "action: SetVariable — divide by zero, setting null");
                            JsonValue::Null
                        } else {
                            to_json_number(current / divisor)
                        }
                    }
                };
                debug!(rule = %ctx.rule_name, variable = name, op = ?op, value = %new_val, "action: SetVariable");
                ctx.rule_vars.insert(key, new_val);
            }

            // ── PauseRule ──────────────────────────────────────────────────────────
            Action::PauseRule { rule_id } => {
                info!(rule = %ctx.rule_name, target = %rule_id, "action: PauseRule");
                ctx.pause_state.insert(rule_id, true);
            }

            // ── ResumeRule ─────────────────────────────────────────────────────────
            Action::ResumeRule { rule_id } => {
                info!(rule = %ctx.rule_name, target = %rule_id, "action: ResumeRule");
                ctx.pause_state.remove(&rule_id);
            }

            // ── CancelDelays ───────────────────────────────────────────────────────
            Action::CancelDelays { key } => match key {
                Some(k) => {
                    let full_key = format!("{}/{k}", ctx.rule_id);
                    debug!(rule = %ctx.rule_name, key = %full_key, "action: CancelDelays — specific key");
                    if let Some((_, n)) = ctx.delay_registry.remove(&full_key) {
                        n.notify_one();
                    }
                }
                None => {
                    let prefix = format!("{}/", ctx.rule_id);
                    debug!(rule = %ctx.rule_name, "action: CancelDelays — all delays for current rule");
                    let keys: Vec<String> = ctx
                        .delay_registry
                        .iter()
                        .filter(|e| e.key().starts_with(&prefix))
                        .map(|e| e.key().clone())
                        .collect();
                    for k in keys {
                        if let Some((_, n)) = ctx.delay_registry.remove(&k) {
                            n.notify_one();
                        }
                    }
                }
            },

            // ── CancelRuleTimers ───────────────────────────────────────────────────
            Action::CancelRuleTimers { rule_id } => {
                let target = rule_id.unwrap_or(ctx.rule_id);
                let prefix = format!("{target}/");
                debug!(rule = %ctx.rule_name, target = %target, "action: CancelRuleTimers");
                let keys: Vec<String> = ctx
                    .delay_registry
                    .iter()
                    .filter(|e| e.key().starts_with(&prefix))
                    .map(|e| e.key().clone())
                    .collect();
                for k in keys {
                    if let Some((_, n)) = ctx.delay_registry.remove(&k) {
                        n.notify_one();
                    }
                }
            }

            // ── RunRuleActions ─────────────────────────────────────────────────────
            Action::RunRuleActions { rule_id } => {
                if call_depth >= MAX_CALL_DEPTH {
                    warn!(
                        rule = %ctx.rule_name, target = %rule_id, depth = call_depth,
                        "action: RunRuleActions — max call depth reached, skipping"
                    );
                    return Ok(());
                }
                info!(rule = %ctx.rule_name, target = %rule_id, depth = call_depth, "action: RunRuleActions");
                let rules = ctx.rules_handle.read().await;
                let target = rules
                    .iter()
                    .find(|r| r.id == rule_id)
                    .map(|r| (r.actions.clone(), r.name.clone(), r.log_actions));
                drop(rules);

                match target {
                    None => {
                        warn!(target = %rule_id, "action: RunRuleActions — target rule not found")
                    }
                    Some((target_actions, target_name, target_log)) => {
                        let sub_ctx = Arc::new(ExecutorContext {
                            publish: ctx.publish.clone(),
                            notify: ctx.notify.clone(),
                            event_bus: ctx.event_bus.clone(),
                            device_cache: Arc::clone(&ctx.device_cache),
                            delay_registry: Arc::clone(&ctx.delay_registry),
                            pause_state: Arc::clone(&ctx.pause_state),
                            rule_vars: Arc::clone(&ctx.rule_vars),
                            priv_bools: Arc::clone(&ctx.priv_bools),
                            capture_store: Arc::clone(&ctx.capture_store),
                            hub_vars: Arc::clone(&ctx.hub_vars),
                            state: ctx.state.clone(),
                            rules_handle: Arc::clone(&ctx.rules_handle),
                            trigger_ctx: ctx.trigger_ctx.clone(),
                            rule_id,
                            rule_name: target_name,
                            log_actions: target_log,
                            exit_flag: Arc::new(AtomicBool::new(false)),
                            trace: None, // sub-calls not traced in parent history
                            correlation_id: ctx.correlation_id.clone(),
                        });
                        let sub_snapshot = snapshot_from_cache(&ctx.device_cache);
                        // Use execute_actions so per-action enabled flags are respected.
                        execute_actions(target_actions, sub_ctx, sub_snapshot).await?;
                    }
                }
            }

            // ── WaitForEvent ───────────────────────────────────────────────────────
            Action::WaitForEvent {
                event_type: et,
                device_id,
                attribute,
                timeout_ms,
            } => {
                let device = match device_id.as_deref() {
                    Some(id) => Some(device_log_name(ctx.state.as_ref(), id).await),
                    None => None,
                };
                let Some(bus) = ctx.event_bus.clone() else {
                    warn!(rule = %ctx.rule_name, "action: WaitForEvent — no event bus, skipping");
                    return Ok(());
                };
                debug!(
                    rule = %ctx.rule_name, event_type = ?et, device = ?device,
                    timeout_ms = ?timeout_ms, "action: WaitForEvent — waiting"
                );
                let mut rx = bus.subscribe();
                let wait_fut = async {
                    loop {
                        match rx.recv().await {
                            Ok(event) => {
                                let matched = match &event {
                                    Event::Custom { event_type, .. } => {
                                        et.as_deref().map_or(false, |e| e == event_type.as_str())
                                    }
                                    Event::DeviceStateChanged {
                                        device_id: eid,
                                        current,
                                        ..
                                    } => {
                                        device_id.as_deref().map_or(false, |d| d == eid.as_str())
                                            && attribute
                                                .as_ref()
                                                .map_or(true, |a| current.contains_key(a.as_str()))
                                    }
                                    _ => false,
                                };
                                if matched {
                                    debug!(rule = %ctx.rule_name, "action: WaitForEvent — matched, resuming");
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                };
                match timeout_ms {
                    Some(ms) => {
                        let _ = tokio::time::timeout(Duration::from_millis(ms), wait_fut).await;
                    }
                    None => wait_fut.await,
                }
            }

            // ── PingHost ──────────────────────────────────────────────────────────
            Action::PingHost {
                host,
                count,
                timeout_ms: ping_timeout_ms,
                then_actions,
                else_actions,
                response_event,
            } => {
                let n = count.unwrap_or(1);
                let wait_ms = ping_timeout_ms.unwrap_or(3000);
                // Convert ms → whole seconds, minimum 1 s, for the -W flag.
                let timeout_secs = ((wait_ms + 999) / 1000).max(1);

                debug!(rule = %ctx.rule_name, host, n, timeout_secs, "action: PingHost — running");

                let output = tokio::process::Command::new("ping")
                    .args(["-c", &n.to_string(), "-W", &timeout_secs.to_string(), &host])
                    .output()
                    .await;

                let (reachable, rtt_ms) = match output {
                    Err(e) => {
                        warn!(rule = %ctx.rule_name, host, error = %e, "action: PingHost — failed to spawn ping");
                        (false, None)
                    }
                    Ok(out) => {
                        let success = out.status.success();
                        // Try to parse avg RTT from the summary line:
                        // "rtt min/avg/max/mdev = 0.234/0.567/0.890/0.123 ms"
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let avg_rtt = stdout
                            .lines()
                            .find(|l| l.contains("rtt min/avg/max"))
                            .and_then(|l| l.split('=').nth(1))
                            .and_then(|vals| vals.trim().split('/').nth(1))
                            .and_then(|avg| avg.trim().parse::<f64>().ok());
                        debug!(
                            rule     = %ctx.rule_name,
                            host,
                            reachable = success,
                            rtt_ms   = ?avg_rtt,
                            "action: PingHost — result"
                        );
                        (success, avg_rtt)
                    }
                };

                // Fire optional response_event so other rules can react.
                if let Some(ref ev_type) = response_event {
                    let mut payload = serde_json::json!({ "host": host, "reachable": reachable });
                    if let Some(rtt) = rtt_ms {
                        payload["rtt_ms"] = serde_json::json!(rtt);
                    }
                    let event = hc_types::event::Event::Custom {
                        timestamp: Utc::now(),
                        event_type: ev_type.clone(),
                        payload,
                    };
                    if let Some(ref bus) = ctx.event_bus {
                        let _ = bus.publish(event.clone());
                    }
                    if let Some(ref ph) = ctx.publish {
                        let topic = format!("homecore/events/{ev_type}");
                        let _ = ph
                            .publish(&topic, serde_json::to_vec(&event).unwrap_or_default())
                            .await;
                    }
                }

                // Run then/else branch.
                let branch = if reachable {
                    then_actions
                } else {
                    else_actions
                };
                if !branch.is_empty() {
                    execute_actions_inner(branch, Arc::clone(&ctx), snapshot, call_depth).await?;
                }
            }

            // ── SetDeviceStatePerMode ─────────────────────────────────────────────
            Action::SetDeviceStatePerMode {
                device_id,
                modes,
                default_state,
            } => {
                let device = device_log_name(ctx.state.as_ref(), &device_id).await;
                debug!(rule = %ctx.rule_name, device = %device, "action: SetDeviceStatePerMode");
                // Check mode entries in order; apply the first one whose mode is active.
                let state_to_apply = modes
                    .iter()
                    .find_map(|entry| {
                        let mode_attrs = ctx.device_cache.get(&entry.mode)?;
                        let is_on = mode_attrs
                            .get("on")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if is_on {
                            Some(entry.state.clone())
                        } else {
                            None
                        }
                    })
                    .or(default_state);

                match state_to_apply {
                    None => {
                        debug!(rule = %ctx.rule_name, device = %device, "action: SetDeviceStatePerMode — no mode matched and no default, skipping");
                    }
                    Some(state) => {
                        info!(rule = %ctx.rule_name, device = %device, state = %state, "action: SetDeviceStatePerMode — applying");
                        publish_device_command(
                            &ctx,
                            &device_id,
                            state,
                            rule_change(&ctx, "rule"),
                        )
                        .await?;
                    }
                }
            }

            // ── DelayPerMode ──────────────────────────────────────────────────────
            Action::DelayPerMode {
                modes,
                default_secs,
            } => {
                let secs = modes
                    .iter()
                    .find_map(|entry| {
                        let attrs = ctx.device_cache.get(entry.mode.as_str())?;
                        let on = attrs.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
                        if on {
                            Some(entry.duration_secs)
                        } else {
                            None
                        }
                    })
                    .or(default_secs);

                match secs {
                    None => {
                        debug!(rule = %ctx.rule_name, "action: DelayPerMode — no mode matched and no default, skipping")
                    }
                    Some(0) => {
                        debug!(rule = %ctx.rule_name, "action: DelayPerMode — duration 0, skipping")
                    }
                    Some(d) => {
                        debug!(rule = %ctx.rule_name, duration_secs = d, "action: DelayPerMode — delaying");
                        tokio::time::sleep(Duration::from_secs(d)).await;
                    }
                }
            }

            // ── SetHubVariable ────────────────────────────────────────────────────
            Action::SetHubVariable { name, value, op } => {
                let prev = ctx.hub_vars.get(name.as_str()).map(|v| v.clone());
                let new_val = match op.as_ref().unwrap_or(&VariableOp::Set) {
                    VariableOp::Set => value.clone(),
                    VariableOp::Toggle => {
                        let current_bool = prev.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                        JsonValue::Bool(!current_bool)
                    }
                    VariableOp::Add => {
                        let a = prev.as_ref().and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let b = value.as_f64().unwrap_or(0.0);
                        to_json_number(a + b)
                    }
                    VariableOp::Subtract => {
                        let a = prev.as_ref().and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let b = value.as_f64().unwrap_or(0.0);
                        to_json_number(a - b)
                    }
                    VariableOp::Multiply => {
                        let a = prev.as_ref().and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let b = value.as_f64().unwrap_or(1.0);
                        to_json_number(a * b)
                    }
                    VariableOp::Divide => {
                        let a = prev.as_ref().and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let b = value.as_f64().unwrap_or(1.0);
                        if b == 0.0 {
                            warn!(rule = %ctx.rule_name, variable = name, "action: SetHubVariable — divide by zero, setting null");
                            JsonValue::Null
                        } else {
                            to_json_number(a / b)
                        }
                    }
                };
                debug!(rule = %ctx.rule_name, variable = name, value = %new_val, "action: SetHubVariable");
                ctx.hub_vars.insert(name.clone(), new_val.clone());
                // Fire hub_variable_changed event so HubVariableChanged triggers can react.
                let payload = serde_json::json!({
                    "name": name,
                    "value": new_val,
                    "prev_value": prev.unwrap_or(JsonValue::Null),
                });
                let event = hc_types::event::Event::Custom {
                    timestamp: Utc::now(),
                    event_type: "hub_variable_changed".into(),
                    payload,
                };
                if let Some(ref bus) = ctx.event_bus {
                    let _ = bus.publish(event.clone());
                }
                if let Some(ref ph) = ctx.publish {
                    let topic = "homecore/events/hub_variable_changed";
                    let _ = ph
                        .publish(topic, serde_json::to_vec(&event).unwrap_or_default())
                        .await;
                }
            }

            // ── SetMode ───────────────────────────────────────────────────────────
            Action::SetMode { mode_id, command } => {
                use hc_types::rule::ModeCommand;
                let payload = match command {
                    ModeCommand::On => serde_json::json!({ "command": "on" }),
                    ModeCommand::Off => serde_json::json!({ "command": "off" }),
                    ModeCommand::Toggle => serde_json::json!({ "command": "toggle" }),
                };
                info!(rule = %ctx.rule_name, mode_id, command = ?command, "action: SetMode");
                publish_device_command(
                    &ctx,
                    &mode_id,
                    payload,
                    rule_change(&ctx, "rule"),
                )
                .await?;
            }

            // ── ActivateScenePerMode ──────────────────────────────────────────────
            Action::ActivateScenePerMode {
                modes,
                default_scene_id,
            } => {
                let scene_id = modes
                    .iter()
                    .find_map(|entry| {
                        let attrs = ctx.device_cache.get(entry.mode.as_str())?;
                        let on = attrs.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
                        if on {
                            Some(entry.scene_id)
                        } else {
                            None
                        }
                    })
                    .or(default_scene_id);

                let Some(sid) = scene_id else {
                    debug!(rule = %ctx.rule_name, "action: ActivateScenePerMode — no mode matched and no default, skipping");
                    return Ok(());
                };

                let Some(ref state_store) = ctx.state else {
                    warn!(rule = %ctx.rule_name, "action: ActivateScenePerMode — no state store available");
                    return Ok(());
                };

                match state_store.get_scene(sid).await {
                    Ok(None) => {
                        warn!(rule = %ctx.rule_name, scene_id = %sid, "action: ActivateScenePerMode — scene not found")
                    }
                    Err(e) => {
                        warn!(rule = %ctx.rule_name, scene_id = %sid, error = %e, "action: ActivateScenePerMode — error loading scene")
                    }
                    Ok(Some(scene)) => {
                        info!(rule = %ctx.rule_name, scene_id = %sid, scene_name = %scene.name, "action: ActivateScenePerMode — activating");
                        let scene_change = DeviceChange::homecore("scene")
                            .with_actor(Some(sid.to_string()), Some(scene.name.clone()))
                            .with_correlation_id(ctx.correlation_id.clone());
                        for (device_id, desired) in &scene.states {
                            publish_device_command(
                                &ctx,
                                device_id,
                                desired.clone(),
                                scene_change.clone(),
                            )
                            .await?;
                        }
                        if let Some(ref bus) = ctx.event_bus {
                            let ev = hc_types::event::Event::SceneActivated {
                                timestamp: Utc::now(),
                                scene_id: sid.to_string(),
                                scene_name: scene.name.clone(),
                            };
                            let _ = bus.publish(ev);
                        }
                    }
                }
            }

            // ── CaptureDeviceState ────────────────────────────────────────────────
            Action::CaptureDeviceState { key, device_ids } => {
                let mut snapshot: HashMap<String, HashMap<String, JsonValue>> = HashMap::new();
                for device_id in &device_ids {
                    if let Some(attrs) = ctx.device_cache.get(device_id.as_str()) {
                        snapshot.insert(device_id.clone(), attrs.clone());
                    } else {
                        let device = device_log_name(ctx.state.as_ref(), device_id).await;
                        debug!(rule = %ctx.rule_name, key, device = %device, "action: CaptureDeviceState — device not in cache, skipping");
                    }
                }
                let captured_count = snapshot.len();
                debug!(rule = %ctx.rule_name, key, captured_count, "action: CaptureDeviceState — saved");
                ctx.capture_store.insert((ctx.rule_id, key), snapshot);
            }

            // ── RestoreDeviceState ────────────────────────────────────────────────
            Action::RestoreDeviceState { key } => {
                let snapshot = ctx
                    .capture_store
                    .get(&(ctx.rule_id, key.clone()))
                    .map(|e| e.clone());
                match snapshot {
                    None => {
                        warn!(rule = %ctx.rule_name, key, "action: RestoreDeviceState — no capture found for key");
                    }
                    Some(snapshot) => {
                        let device_count = snapshot.len();
                        debug!(rule = %ctx.rule_name, key, device_count, "action: RestoreDeviceState — restoring");
                        for (device_id, attrs) in snapshot {
                            let device = device_log_name(ctx.state.as_ref(), &device_id).await;
                            let state = JsonValue::Object(attrs.into_iter().collect());
                            if ctx.publish.is_none() {
                                warn!(rule = %ctx.rule_name, device = %device, "action: RestoreDeviceState — no publish handle");
                                break;
                            }
                            publish_device_command(
                                &ctx,
                                &device_id,
                                state,
                                rule_change(&ctx, "rule"),
                            )
                            .await?;
                        }
                    }
                }
            }

            // ── FadeDevice ────────────────────────────────────────────────────────
            Action::FadeDevice {
                device_id,
                target,
                duration_secs,
                steps,
            } => {
                let device = device_log_name(ctx.state.as_ref(), &device_id).await;
                let n_steps = steps
                    .map(|s| (s as u64).max(2).min(100))
                    .unwrap_or_else(|| duration_secs.max(2).min(100));
                let interval_ms = (duration_secs * 1000).saturating_div(n_steps);

                // Read current numeric values from the live device cache.
                let current_nums: HashMap<String, f64> = ctx
                    .device_cache
                    .get(device_id.as_str())
                    .map(|attrs| {
                        attrs
                            .iter()
                            .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                            .collect()
                    })
                    .unwrap_or_default();

                // Separate numeric target fields (interpolated) from non-numeric (pass-through).
                let target_nums: HashMap<String, f64> = target
                    .as_object()
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                            .collect()
                    })
                    .unwrap_or_default();

                debug!(rule = %ctx.rule_name, device = %device, n_steps, interval_ms, "action: FadeDevice — starting");

                for step in 1..=n_steps {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;

                    let t = step as f64 / n_steps as f64;
                    let mut state_obj = serde_json::Map::new();

                    // Non-numeric pass-through fields on every step.
                    if let Some(obj) = target.as_object() {
                        for (k, v) in obj {
                            if !target_nums.contains_key(k) {
                                state_obj.insert(k.clone(), v.clone());
                            }
                        }
                    }

                    // Interpolated numeric fields.
                    for (attr, &target_val) in &target_nums {
                        let start_val = current_nums.get(attr).copied().unwrap_or(target_val);
                        let interp = start_val + (target_val - start_val) * t;
                        state_obj.insert(attr.clone(), to_json_number(interp));
                    }

                    let state = JsonValue::Object(state_obj);
                    if ctx.publish.is_none() {
                        warn!(rule = %ctx.rule_name, device = %device, "action: FadeDevice — no publish handle");
                        break;
                    }
                    publish_device_command(
                        &ctx,
                        &device_id,
                        state,
                        rule_change(&ctx, "rule"),
                    )
                    .await?;
                }
                debug!(rule = %ctx.rule_name, device = %device, "action: FadeDevice — complete");
            }

            // ── WaitForExpression ─────────────────────────────────────────────────
            Action::WaitForExpression {
                expression,
                poll_interval_ms,
                timeout_ms,
                hold_duration_ms,
            } => {
                let poll_ms = poll_interval_ms.unwrap_or(500);
                debug!(
                    rule = %ctx.rule_name, poll_ms, timeout_ms = ?timeout_ms,
                    hold_ms = ?hold_duration_ms, "action: WaitForExpression — waiting"
                );
                let device_cache = Arc::clone(&ctx.device_cache);
                let wait_fut = async move {
                    let mut hold_start: Option<Instant> = None;
                    loop {
                        let snap = snapshot_from_cache(&device_cache);
                        let expr = expression.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            ScriptRuntime::new_with_devices(snap).eval_condition(&expr)
                        })
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or(false);

                        if result {
                            match hold_duration_ms {
                                None => break,
                                Some(hold_ms) => {
                                    let start = hold_start.get_or_insert_with(Instant::now);
                                    if start.elapsed().as_millis() as u64 >= hold_ms {
                                        break;
                                    }
                                }
                            }
                        } else {
                            hold_start = None;
                        }
                        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
                    }
                };
                match timeout_ms {
                    Some(ms) => {
                        let _ = tokio::time::timeout(Duration::from_millis(ms), wait_fut).await;
                    }
                    None => wait_fut.await,
                }
                debug!(rule = %ctx.rule_name, "action: WaitForExpression — done");
            }
        }
        Ok(())
    }) // end Box::pin(async move { match action { ... } })
}

// ---------------------------------------------------------------------------
// Script side-effect executor
// ---------------------------------------------------------------------------

async fn execute_script_effect(
    effect: ScriptSideEffect,
    publish: Option<PublishHandle>,
    notify: Option<Arc<NotificationService>>,
    state_store: Option<StateStore>,
    correlation_id: Option<String>,
) -> Result<()> {
    match effect {
        ScriptSideEffect::SetDeviceState { device_id, state } => {
            let device = device_log_name(state_store.as_ref(), &device_id).await;
            debug!(device = %device, payload = %state, "RunScript: set_device_state");
            let topic = format!("homecore/devices/{device_id}/cmd");
            let change = DeviceChange::homecore("script").with_correlation_id(correlation_id);
            let payload = with_command_change_metadata(state, &change);
            match publish {
                Some(ph) => ph.publish(&topic, serde_json::to_vec(&payload)?).await?,
                None => warn!(%device_id, "device command dropped — no publish handle"),
            }
        }

        ScriptSideEffect::Notify {
            channel,
            title,
            message,
        } => {
            debug!(channel, title, message, "RunScript: notify");
            match notify {
                Some(svc) => {
                    if let Err(e) = svc.notify(&channel, &title, &message).await {
                        warn!(channel, error = %e, "RunScript: notify failed");
                    }
                }
                None => warn!(
                    channel,
                    "RunScript: notify — no NotificationService configured"
                ),
            }
        }

        ScriptSideEffect::PublishMqtt { topic, payload } => {
            debug!(topic, "RunScript: publish_mqtt");
            match publish {
                Some(ph) => ph.publish(&topic, payload.into_bytes()).await?,
                None => warn!(
                    topic,
                    "RunScript: publish_mqtt — no publish handle, dropped"
                ),
            }
        }

        ScriptSideEffect::CallService { method, url, body } => {
            let client = http_client();
            debug!(method, url, "RunScript: call_service");
            let req = match method.to_uppercase().as_str() {
                "GET" => client.get(&url),
                "POST" => {
                    let body_json: JsonValue =
                        serde_json::from_str(&body).unwrap_or(JsonValue::Null);
                    client.post(&url).json(&body_json)
                }
                other => return Err(anyhow!("RunScript: unsupported HTTP method '{other}'")),
            };
            match req.timeout(Duration::from_secs(10)).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(method, url, status = %resp.status(), "RunScript: call_service — OK")
                }
                Ok(resp) => {
                    warn!(method, url, status = %resp.status(), "RunScript: call_service — HTTP error")
                }
                Err(e) => {
                    warn!(method, url, error = %e, "RunScript: call_service — request failed")
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Human-readable summary of an action's target / content for trace output.
async fn action_description(ctx: Arc<ExecutorContext>, action: &Action) -> String {
    match action {
        Action::SetDeviceState {
            device_id,
            state,
            track_event_value,
        } => {
            let device = device_log_name(ctx.state.as_ref(), device_id).await;
            if *track_event_value {
                format!("{} ← (trigger value)", device)
            } else {
                format!("{} ← {}", device, state)
            }
        }
        Action::PublishMqtt { topic, .. } => format!("topic: {}", topic),
        Action::CallService { url, method, .. } => format!("{} {}", method, url),
        Action::FireEvent { event_type, .. } => format!("event: {}", event_type),
        Action::RunScript { script } => {
            let s = if script.len() > 60 {
                &script[..60]
            } else {
                script
            };
            s.to_string()
        }
        Action::Notify {
            channel, message, ..
        } => {
            let m = if message.len() > 60 {
                &message[..60]
            } else {
                message
            };
            format!("[{}] {}", channel, m)
        }
        Action::Delay {
            duration_secs,
            cancelable,
            cancel_key,
        } => {
            if *cancelable {
                format!(
                    "{}s (cancelable: {})",
                    duration_secs,
                    cancel_key.as_deref().unwrap_or("auto")
                )
            } else {
                format!("{}s", duration_secs)
            }
        }
        Action::Parallel { actions } => format!("{} actions", actions.len()),
        Action::RepeatUntil { max_iterations, .. } => match max_iterations {
            Some(n) => format!("max {} iterations", n),
            None => "no limit".into(),
        },
        Action::RepeatWhile { max_iterations, .. } => match max_iterations {
            Some(n) => format!("max {} iterations", n),
            None => "no limit".into(),
        },
        Action::RepeatCount { count, .. } => format!("{} times", count),
        Action::Conditional { condition, .. } => {
            let c = if condition.len() > 60 {
                &condition[..60]
            } else {
                condition
            };
            format!("if {}", c)
        }
        Action::Comment { text } => text.clone(),
        Action::LogMessage { message, level } => match level {
            Some(l) => format!("[{:?}] {}", l, message),
            None => message.clone(),
        },
        Action::SetPrivateBoolean { name, value } => format!("{} = {}", name, value),
        Action::SetVariable { name, value, op } => match op {
            Some(VariableOp::Set) | None => format!("{} = {}", name, value),
            Some(o) => format!("{} {:?}= {}", name, o, value),
        },
        Action::PauseRule { rule_id } => rule_id.to_string(),
        Action::ResumeRule { rule_id } => rule_id.to_string(),
        Action::CancelDelays { key } => key.as_deref().unwrap_or("all").into(),
        Action::CancelRuleTimers { rule_id } => rule_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "self".into()),
        Action::RunRuleActions { rule_id } => rule_id.to_string(),
        Action::WaitForEvent {
            event_type,
            device_id,
            timeout_ms,
            ..
        } => {
            let what = if let Some(event_type) = event_type.as_deref() {
                event_type.to_string()
            } else if let Some(device_id) = device_id.as_deref() {
                device_log_name(ctx.state.as_ref(), device_id).await
            } else {
                "any".to_string()
            };
            match timeout_ms {
                Some(t) => format!("{} (timeout: {}ms)", what, t),
                None => what,
            }
        }
        Action::WaitForExpression {
            expression,
            timeout_ms,
            ..
        } => {
            let e = if expression.len() > 60 {
                &expression[..60]
            } else {
                expression
            };
            match timeout_ms {
                Some(t) => format!("{} (timeout: {}ms)", e, t),
                None => e.into(),
            }
        }
        Action::StopRuleChain | Action::ExitRule => String::new(),
        Action::PingHost {
            host,
            count,
            timeout_ms,
            ..
        } => {
            let n = count.unwrap_or(1);
            let tms = timeout_ms.unwrap_or(3000);
            format!("{host} ({n}× / {tms}ms)")
        }
        Action::SetDeviceStatePerMode {
            device_id,
            modes,
            default_state,
        } => {
            let device = device_log_name(ctx.state.as_ref(), device_id).await;
            let mode_names: Vec<&str> = modes.iter().map(|e| e.mode.as_str()).collect();
            match default_state {
                Some(_) => format!("{} (modes: [{}] + default)", device, mode_names.join(", ")),
                None => format!("{} (modes: [{}])", device, mode_names.join(", ")),
            }
        }
        Action::CaptureDeviceState { key, device_ids } => {
            let mut devices = Vec::with_capacity(device_ids.len());
            for device_id in device_ids {
                devices.push(device_log_name(ctx.state.as_ref(), device_id).await);
            }
            format!("key={} devices=[{}]", key, devices.join(", "))
        }
        Action::RestoreDeviceState { key } => format!("key={}", key),
        Action::FadeDevice {
            device_id,
            duration_secs,
            steps,
            ..
        } => {
            let device = device_log_name(ctx.state.as_ref(), device_id).await;
            let n = steps
                .map(|s| s.to_string())
                .unwrap_or_else(|| duration_secs.to_string());
            format!("{} over {}s ({} steps)", device, duration_secs, n)
        }
        Action::DelayPerMode {
            modes,
            default_secs,
        } => {
            let mode_names: Vec<&str> = modes.iter().map(|e| e.mode.as_str()).collect();
            match default_secs {
                Some(d) => format!("modes: [{}] default: {}s", mode_names.join(", "), d),
                None => format!("modes: [{}]", mode_names.join(", ")),
            }
        }
        Action::SetHubVariable { name, value, op } => match op {
            Some(VariableOp::Set) | None => format!("hub.{} = {}", name, value),
            Some(o) => format!("hub.{} {:?}= {}", name, o, value),
        },
        Action::ActivateScenePerMode {
            modes,
            default_scene_id,
        } => {
            let mode_names: Vec<&str> = modes.iter().map(|e| e.mode.as_str()).collect();
            match default_scene_id {
                Some(id) => format!("modes: [{}] default: {}", mode_names.join(", "), id),
                None => format!("modes: [{}]", mode_names.join(", ")),
            }
        }
        Action::SetMode { mode_id, command } => format!("{} {:?}", mode_id, command),
    }
}

fn action_type_name(action: &Action) -> &'static str {
    match action {
        Action::SetDeviceState { .. } => "SetDeviceState",
        Action::PublishMqtt { .. } => "PublishMqtt",
        Action::CallService { .. } => "CallService",
        Action::FireEvent { .. } => "FireEvent",
        Action::RunScript { .. } => "RunScript",
        Action::Notify { .. } => "Notify",
        Action::Delay { .. } => "Delay",
        Action::Parallel { .. } => "Parallel",
        Action::RepeatUntil { .. } => "RepeatUntil",
        Action::RepeatWhile { .. } => "RepeatWhile",
        Action::RepeatCount { .. } => "RepeatCount",
        Action::Conditional { .. } => "Conditional",
        Action::StopRuleChain => "StopRuleChain",
        Action::ExitRule => "ExitRule",
        Action::Comment { .. } => "Comment",
        Action::LogMessage { .. } => "LogMessage",
        Action::SetPrivateBoolean { .. } => "SetPrivateBoolean",
        Action::SetVariable { .. } => "SetVariable",
        Action::PauseRule { .. } => "PauseRule",
        Action::ResumeRule { .. } => "ResumeRule",
        Action::CancelDelays { .. } => "CancelDelays",
        Action::CancelRuleTimers { .. } => "CancelRuleTimers",
        Action::RunRuleActions { .. } => "RunRuleActions",
        Action::WaitForEvent { .. } => "WaitForEvent",
        Action::WaitForExpression { .. } => "WaitForExpression",
        Action::SetDeviceStatePerMode { .. } => "SetDeviceStatePerMode",
        Action::PingHost { .. } => "PingHost",
        Action::CaptureDeviceState { .. } => "CaptureDeviceState",
        Action::RestoreDeviceState { .. } => "RestoreDeviceState",
        Action::FadeDevice { .. } => "FadeDevice",
        Action::DelayPerMode { .. } => "DelayPerMode",
        Action::SetHubVariable { .. } => "SetHubVariable",
        Action::ActivateScenePerMode { .. } => "ActivateScenePerMode",
        Action::SetMode { .. } => "SetMode",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_snapshot() -> HashMap<String, JsonValue> {
        HashMap::new()
    }

    fn ra(action: Action) -> RuleAction {
        RuleAction {
            enabled: true,
            action,
        }
    }

    fn test_delay(secs: u64) -> RuleAction {
        ra(Action::Delay {
            duration_secs: secs,
            cancelable: false,
            cancel_key: None,
        })
    }

    #[tokio::test]
    async fn repeat_until_exits_when_condition_true_immediately() {
        let action = ra(Action::RepeatUntil {
            condition: "true".into(),
            actions: vec![test_delay(0).action],
            max_iterations: Some(10),
            interval_ms: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repeat_until_respects_max_iterations() {
        let action = ra(Action::RepeatUntil {
            condition: "false".into(),
            actions: vec![test_delay(0).action],
            max_iterations: Some(3),
            interval_ms: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repeat_while_skips_body_when_false() {
        let action = ra(Action::RepeatWhile {
            condition: "false".into(),
            actions: vec![test_delay(0).action],
            max_iterations: Some(5),
            interval_ms: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repeat_count_runs_exact_iterations() {
        // Verify it completes without error for count=3
        let action = ra(Action::RepeatCount {
            count: 3,
            actions: vec![test_delay(0).action],
            interval_ms: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exit_rule_stops_action_sequence() {
        // ExitRule should prevent the second Comment from executing (no panic / infinite loop).
        let actions = vec![
            ra(Action::ExitRule),
            ra(Action::Comment {
                text: "should not reach here".into(),
            }),
        ];
        execute_actions(actions, ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delay_action_completes() {
        execute_actions(
            vec![test_delay(1)],
            ExecutorContext::for_test(),
            empty_snapshot(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn set_variable_add() {
        let ctx = ExecutorContext::for_test();
        let actions = vec![
            ra(Action::SetVariable {
                name: "x".into(),
                value: serde_json::json!(5_i64),
                op: None,
            }),
            ra(Action::SetVariable {
                name: "x".into(),
                value: serde_json::json!(3_i64),
                op: Some(VariableOp::Add),
            }),
        ];
        execute_actions(actions, Arc::clone(&ctx), empty_snapshot())
            .await
            .unwrap();
        let val = ctx
            .rule_vars
            .get(&(Uuid::nil(), "x".into()))
            .map(|v| v.clone());
        assert_eq!(val, Some(serde_json::json!(8_i64)));
    }

    #[tokio::test]
    async fn set_private_boolean() {
        let ctx = ExecutorContext::for_test();
        let actions = vec![ra(Action::SetPrivateBoolean {
            name: "armed".into(),
            value: true,
        })];
        execute_actions(actions, Arc::clone(&ctx), empty_snapshot())
            .await
            .unwrap();
        let val = ctx
            .priv_bools
            .get(&(Uuid::nil(), "armed".into()))
            .map(|v| *v);
        assert_eq!(val, Some(true));
    }

    #[tokio::test]
    async fn conditional_else_if_branch() {
        let actions = vec![ra(Action::Conditional {
            condition: "false".into(),
            then_actions: vec![],
            else_if: vec![hc_types::rule::ConditionalBranch {
                condition: "true".into(),
                actions: vec![Action::LogMessage {
                    message: "else_if branch".into(),
                    level: None,
                }],
            }],
            else_actions: vec![],
        })];
        execute_actions(actions, ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    fn call_service_action(url: &str, method: &str) -> RuleAction {
        ra(Action::CallService {
            url: url.to_string(),
            method: method.to_string(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: None,
            response_event: None,
        })
    }

    #[tokio::test]
    async fn call_service_success() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/hook")
            .with_status(200)
            .create_async()
            .await;
        let action = ra(Action::CallService {
            url: format!("{}/hook", server.url()),
            method: "POST".into(),
            body: serde_json::json!({"key": "val"}),
            timeout_ms: None,
            retries: None,
            response_event: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn call_service_4xx_returns_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/gone")
            .with_status(404)
            .create_async()
            .await;
        let result = execute_actions(
            vec![call_service_action(
                &format!("{}/gone", server.url()),
                "GET",
            )],
            ExecutorContext::for_test(),
            empty_snapshot(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_retries_on_5xx_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server
            .mock("POST", "/retry")
            .with_status(500)
            .create_async()
            .await;
        let _m2 = server
            .mock("POST", "/retry")
            .with_status(200)
            .create_async()
            .await;
        let action = ra(Action::CallService {
            url: format!("{}/retry", server.url()),
            method: "POST".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        });
        execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn call_service_exhausts_retries_returns_error() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server
            .mock("GET", "/fail")
            .with_status(500)
            .create_async()
            .await;
        let _m2 = server
            .mock("GET", "/fail")
            .with_status(500)
            .create_async()
            .await;
        let action = ra(Action::CallService {
            url: format!("{}/fail", server.url()),
            method: "GET".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        });
        let result =
            execute_actions(vec![action], ExecutorContext::for_test(), empty_snapshot()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_unsupported_method_returns_error() {
        let result = execute_actions(
            vec![call_service_action("http://localhost/x", "CONNECT")],
            ExecutorContext::for_test(),
            empty_snapshot(),
        )
        .await;
        assert!(result.is_err());
    }
}
