//! Action executor — runs rule actions sequentially or concurrently.

use anyhow::{anyhow, Result};
use hc_mqtt_client::PublishHandle;
use hc_notify::NotificationService;
use hc_scripting::{EffectsBuf, ScriptRuntime, ScriptSideEffect};
use hc_types::rule::Action;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::EventBus;

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

/// Execute a list of actions sequentially, honouring `Parallel` and `Delay`.
///
/// `device_snapshot` is a pre-built in-memory snapshot of all device attributes
/// (built from the DashMap cache in the engine). It is passed through to all
/// script-based actions (RunScript, Conditional, RepeatUntil) so every script
/// within a single rule firing sees the same consistent state without any I/O.
///
/// `event_bus` is optional; when present, `FireEvent` actions publish directly
/// to the internal bus in addition to MQTT, enabling `Trigger::CustomEvent`
/// chaining within the same process.
pub async fn execute_actions(
    actions: Vec<Action>,
    publish: Option<PublishHandle>,
    notify: Option<Arc<NotificationService>>,
    device_snapshot: HashMap<String, JsonValue>,
    event_bus: Option<EventBus>,
) -> Result<()> {
    let total = actions.len();
    for (idx, action) in actions.into_iter().enumerate() {
        execute_one(action, idx, total, publish.clone(), notify.clone(), device_snapshot.clone(), event_bus.clone()).await?;
    }
    Ok(())
}

async fn execute_one(
    action: Action,
    idx: usize,
    total: usize,
    publish: Option<PublishHandle>,
    notify: Option<Arc<NotificationService>>,
    device_snapshot: HashMap<String, JsonValue>,
    event_bus: Option<EventBus>,
) -> Result<()> {
    let label = format!("action[{}/{}]", idx + 1, total);
    match action {
        Action::Delay { duration_ms } => {
            debug!(label, duration_ms, "action: Delay");
            tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms)).await;
            debug!(label, "action: Delay — done");
        }

        Action::Parallel { actions } => {
            let count = actions.len();
            debug!(label, parallel_count = count, "action: Parallel — spawning {} concurrent actions", count);
            let handles: Vec<_> = actions
                .into_iter()
                .enumerate()
                .map(|(i, a)| {
                    let p = publish.clone();
                    let n = notify.clone();
                    let snap = device_snapshot.clone();
                    let bus = event_bus.clone();
                    debug!("action: Parallel[{}/{}] — {:?}", i + 1, count, action_type_name(&a));
                    tokio::spawn(run_single_action(a, p, n, snap, bus))
                })
                .collect();
            for h in handles {
                h.await??;
            }
            debug!(label, "action: Parallel — all done");
        }

        other => run_single_action(other, publish, notify, device_snapshot, event_bus).await?,
    }
    Ok(())
}

async fn run_single_action(
    action: Action,
    publish: Option<PublishHandle>,
    notify: Option<Arc<NotificationService>>,
    device_snapshot: HashMap<String, JsonValue>,
    event_bus: Option<EventBus>,
) -> Result<()> {
    match action {
        Action::Delay { duration_ms } => {
            debug!(duration_ms, "action: Delay");
            tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms)).await;
        }

        Action::Parallel { actions } => {
            let count = actions.len();
            debug!(parallel_count = count, "action: Parallel (nested)");
            for a in actions {
                Box::pin(run_single_action(a, publish.clone(), notify.clone(), device_snapshot.clone(), event_bus.clone())).await?;
            }
        }

        Action::RepeatUntil { condition, actions, max_iterations, interval_ms } => {
            let limit = max_iterations.unwrap_or(100);
            let delay = interval_ms.unwrap_or(0);
            let snippet = if condition.len() > 60 { &condition[..60] } else { &condition };
            debug!(condition = %snippet, limit, delay_ms = delay, "action: RepeatUntil — starting");
            for i in 0..limit {
                let cond_script = condition.clone();
                let snap = device_snapshot.clone();
                let done = tokio::task::spawn_blocking(move || {
                    ScriptRuntime::new_with_devices(snap).eval_condition(&cond_script)
                })
                .await??;
                debug!(iteration = i + 1, done, "action: RepeatUntil — condition check");
                if done {
                    debug!(iterations = i + 1, "action: RepeatUntil — condition met, exiting loop");
                    break;
                }
                if i == limit - 1 {
                    warn!(max = limit, "action: RepeatUntil — hit max_iterations without condition becoming true");
                    break;
                }
                for a in &actions {
                    Box::pin(run_single_action(a.clone(), publish.clone(), notify.clone(), device_snapshot.clone(), event_bus.clone())).await?;
                }
                if delay > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Action::SetDeviceState { device_id, state: desired } => {
            let topic = format!("homecore/devices/{device_id}/cmd");
            debug!(device_id, payload = %desired, "action: SetDeviceState");
            let payload = desired.to_string().into_bytes();
            match publish {
                Some(ph) => {
                    ph.publish(&topic, payload).await?;
                    debug!(device_id, topic, "action: SetDeviceState — published");
                }
                None => {
                    warn!(device_id, "action: SetDeviceState — no publish handle, command dropped");
                }
            }
        }

        Action::PublishMqtt { topic, payload, retain } => {
            debug!(topic, retain, payload_len = payload.len(), "action: PublishMqtt");
            match publish {
                Some(ph) => {
                    if retain {
                        ph.publish_retained(&topic, payload.into_bytes()).await?;
                    } else {
                        ph.publish(&topic, payload.into_bytes()).await?;
                    }
                    debug!(topic, retain, "action: PublishMqtt — published");
                }
                None => {
                    warn!(topic, "action: PublishMqtt — no publish handle, message dropped");
                }
            }
        }

        Action::FireEvent { event_type, payload } => {
            debug!(event_type, "action: FireEvent");
            if let Some(ref ph) = publish {
                let topic = format!("homecore/events/{event_type}");
                ph.publish_json(&topic, &payload, false).await?;
                debug!(event_type, "action: FireEvent — published to MQTT");
            }
            // Also emit directly to the internal event bus so rules with
            // `Trigger::CustomEvent { event_type }` can react within the same process.
            if let Some(ref bus) = event_bus {
                let ev = hc_types::event::Event::Custom {
                    timestamp: chrono::Utc::now(),
                    event_type: event_type.clone(),
                    payload: payload.clone(),
                };
                let _ = bus.publish(ev);
                debug!(event_type, "action: FireEvent — emitted to internal bus");
            }
        }

        Action::CallService { url, method, body, timeout_ms, retries, response_event } => {
            let method_upper = method.to_uppercase();
            let timeout = tokio::time::Duration::from_millis(timeout_ms.unwrap_or(10_000));
            let max_attempts = retries.unwrap_or(0) + 1;
            let client = http_client();
            debug!(
                url,
                method    = %method_upper,
                retries   = retries.unwrap_or(0),
                timeout_ms = timeout_ms.unwrap_or(10_000),
                "action: CallService"
            );

            let mut last_err: anyhow::Error = anyhow!("no attempts made");
            let call_start = Instant::now();

            'retry: for attempt in 0..max_attempts {
                if attempt > 0 {
                    let backoff_ms = 500u64 * (1u64 << (attempt - 1).min(3));
                    info!(url, attempt, backoff_ms, "action: CallService — retrying");
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                }

                let req = match method_upper.as_str() {
                    "GET"    => client.get(&url),
                    "POST"   => client.post(&url).json(&body),
                    "PUT"    => client.put(&url).json(&body),
                    "PATCH"  => client.patch(&url).json(&body),
                    "DELETE" => client.delete(&url),
                    other    => return Err(anyhow!("Unsupported HTTP method: {other}")),
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
                            if let Some(ref ph) = publish {
                                let resp_body: JsonValue =
                                    resp.json().await.unwrap_or(JsonValue::Null);
                                let topic = format!("homecore/events/{event_type}");
                                ph.publish_json(&topic, &resp_body, false).await?;
                                debug!(event_type, "action: CallService — response published as event");
                            }
                        }
                        return Ok(());
                    }
                }
            }
            return Err(last_err);
        }

        Action::RunScript { script } => {
            let snippet = if script.len() > 80 { &script[..80] } else { &script };
            debug!(script = %snippet, "action: RunScript — starting");
            let snapshot = device_snapshot.clone();
            let script_clone = script.clone();
            // Collect side effects synchronously inside spawn_blocking, then
            // execute them asynchronously here after the script returns.
            let buf: EffectsBuf = Arc::new(Mutex::new(Vec::new()));
            let buf_clone = Arc::clone(&buf);
            tokio::task::spawn_blocking(move || {
                ScriptRuntime::new_with_devices(snapshot)
                    .with_side_effects(buf_clone)
                    .run_action(&script_clone)
                    .map(|_| ())
            })
            .await??;
            let effects = std::mem::take(&mut *buf.lock().unwrap());
            if !effects.is_empty() {
                debug!(script = %snippet, count = effects.len(), "action: RunScript — executing side effects");
            }
            for effect in effects {
                execute_script_effect(effect, publish.clone(), notify.clone()).await?;
            }
            debug!(script = %snippet, "action: RunScript — completed");
        }

        Action::Conditional { condition, then_actions, else_actions } => {
            let snippet = if condition.len() > 80 { &condition[..80] } else { &condition };
            debug!(condition = %snippet, "action: Conditional — evaluating");
            let snap = device_snapshot.clone();
            let cond = condition.clone();
            let passed = tokio::task::spawn_blocking(move || {
                ScriptRuntime::new_with_devices(snap).eval_condition(&cond)
            })
            .await??;
            let branch_name = if passed { "then" } else { "else" };
            let branch = if passed { then_actions } else { else_actions };
            debug!(passed, branch = branch_name, actions = branch.len(), "action: Conditional — branch selected");
            for a in branch {
                Box::pin(run_single_action(a, publish.clone(), notify.clone(), device_snapshot.clone(), event_bus.clone())).await?;
            }
        }

        Action::Notify { channel, message, title } => {
            let title_str = title.as_deref().unwrap_or("HomeCore Alert");
            debug!(channel, title = title_str, message = %message, "action: Notify");
            match &notify {
                Some(svc) => {
                    if let Err(e) = svc.notify(&channel, title_str, &message).await {
                        warn!(channel, error = %e, "action: Notify — failed");
                    } else {
                        info!(channel, "action: Notify — sent");
                    }
                }
                None => {
                    warn!(channel, "action: Notify — no NotificationService configured");
                }
            }
        }

        Action::StopRuleChain => {
            // No-op in the executor — this action is consumed by the engine
            // layer to break the rule evaluation loop before the task is spawned.
            debug!("action: StopRuleChain (no-op in executor)");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn action_type_name(action: &Action) -> &'static str {
    match action {
        Action::SetDeviceState { .. } => "SetDeviceState",
        Action::PublishMqtt { .. }    => "PublishMqtt",
        Action::CallService { .. }    => "CallService",
        Action::FireEvent { .. }      => "FireEvent",
        Action::RunScript { .. }      => "RunScript",
        Action::Notify { .. }         => "Notify",
        Action::Delay { .. }          => "Delay",
        Action::Parallel { .. }       => "Parallel",
        Action::RepeatUntil { .. }    => "RepeatUntil",
        Action::Conditional { .. }    => "Conditional",
        Action::StopRuleChain         => "StopRuleChain",
    }
}

// ---------------------------------------------------------------------------
// Script side-effect executor
// ---------------------------------------------------------------------------

async fn execute_script_effect(
    effect: ScriptSideEffect,
    publish: Option<PublishHandle>,
    notify: Option<Arc<NotificationService>>,
) -> Result<()> {
    match effect {
        ScriptSideEffect::SetDeviceState { device_id, state } => {
            let topic = format!("homecore/devices/{device_id}/cmd");
            debug!(device_id, payload = %state, "RunScript: set_device_state");
            match publish {
                Some(ph) => ph.publish(&topic, state.to_string().into_bytes()).await?,
                None => warn!(device_id, "RunScript: set_device_state — no publish handle, dropped"),
            }
        }

        ScriptSideEffect::Notify { channel, title, message } => {
            debug!(channel, title, message, "RunScript: notify");
            match notify {
                Some(svc) => {
                    if let Err(e) = svc.notify(&channel, &title, &message).await {
                        warn!(channel, error = %e, "RunScript: notify failed");
                    }
                }
                None => warn!(channel, "RunScript: notify — no NotificationService configured"),
            }
        }

        ScriptSideEffect::PublishMqtt { topic, payload } => {
            debug!(topic, "RunScript: publish_mqtt");
            match publish {
                Some(ph) => ph.publish(&topic, payload.into_bytes()).await?,
                None => warn!(topic, "RunScript: publish_mqtt — no publish handle, dropped"),
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
            match req
                .timeout(tokio::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!(method, url, status = %resp.status(), "RunScript: call_service — OK");
                }
                Ok(resp) => {
                    warn!(method, url, status = %resp.status(), "RunScript: call_service — HTTP error");
                }
                Err(e) => {
                    warn!(method, url, error = %e, "RunScript: call_service — request failed");
                }
            }
        }
    }
    Ok(())
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

    #[tokio::test]
    async fn repeat_until_exits_when_condition_true_immediately() {
        let action = Action::RepeatUntil {
            condition: "true".into(),
            actions: vec![Action::Delay { duration_ms: 0 }],
            max_iterations: Some(10),
            interval_ms: None,
        };
        execute_actions(vec![action], None, None, empty_snapshot(), None).await.unwrap();
    }

    #[tokio::test]
    async fn repeat_until_respects_max_iterations() {
        let action = Action::RepeatUntil {
            condition: "false".into(),
            actions: vec![Action::Delay { duration_ms: 0 }],
            max_iterations: Some(3),
            interval_ms: None,
        };
        execute_actions(vec![action], None, None, empty_snapshot(), None).await.unwrap();
    }

    #[tokio::test]
    async fn delay_action_completes() {
        execute_actions(vec![Action::Delay { duration_ms: 1 }], None, None, empty_snapshot(), None).await.unwrap();
    }

    fn call_service(url: &str, method: &str) -> Action {
        Action::CallService {
            url: url.to_string(),
            method: method.to_string(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: None,
            response_event: None,
        }
    }

    #[tokio::test]
    async fn call_service_success() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("POST", "/hook").with_status(200).create_async().await;

        let action = Action::CallService {
            url: format!("{}/hook", server.url()),
            method: "POST".into(),
            body: serde_json::json!({"key": "val"}),
            timeout_ms: None,
            retries: None,
            response_event: None,
        };
        execute_actions(vec![action], None, None, empty_snapshot(), None).await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn call_service_4xx_returns_error() {
        let mut server = mockito::Server::new_async().await;
        server.mock("GET", "/gone").with_status(404).create_async().await;

        let result = execute_actions(
            vec![call_service(&format!("{}/gone", server.url()), "GET")],
            None, None, empty_snapshot(), None,
        ).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_retries_on_5xx_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server.mock("POST", "/retry").with_status(500).create_async().await;
        let _m2 = server.mock("POST", "/retry").with_status(200).create_async().await;

        let action = Action::CallService {
            url: format!("{}/retry", server.url()),
            method: "POST".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        };
        execute_actions(vec![action], None, None, empty_snapshot(), None).await.unwrap();
    }

    #[tokio::test]
    async fn call_service_exhausts_retries_returns_error() {
        let mut server = mockito::Server::new_async().await;
        let _m1 = server.mock("GET", "/fail").with_status(500).create_async().await;
        let _m2 = server.mock("GET", "/fail").with_status(500).create_async().await;

        let action = Action::CallService {
            url: format!("{}/fail", server.url()),
            method: "GET".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        };
        let result = execute_actions(vec![action], None, None, empty_snapshot(), None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_unsupported_method_returns_error() {
        let result = execute_actions(
            vec![call_service("http://localhost/x", "CONNECT")],
            None, None, empty_snapshot(), None,
        ).await;
        assert!(result.is_err());
    }
}
