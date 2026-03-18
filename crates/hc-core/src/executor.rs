//! Action executor — runs rule actions sequentially or concurrently.

use anyhow::{anyhow, Result};
use hc_mqtt_client::PublishHandle;
use hc_scripting::ScriptRuntime;
use hc_state::StateStore;
use hc_types::rule::Action;
use serde_json::Value as JsonValue;
use std::sync::OnceLock;
use tracing::{info, warn};

/// Shared HTTP client — initialised once, reused for every `CallService` action.
/// `reqwest::Client` is cheaply cloneable (Arc-backed) and safe to share across tasks.
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
pub async fn execute_actions(
    actions: Vec<Action>,
    publish: Option<PublishHandle>,
    state: StateStore,
) -> Result<()> {
    for action in actions {
        execute_one(action, publish.clone(), state.clone()).await?;
    }
    Ok(())
}

async fn execute_one(
    action: Action,
    publish: Option<PublishHandle>,
    state: StateStore,
) -> Result<()> {
    match action {
        Action::Delay { duration_ms } => {
            tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms)).await;
        }

        Action::Parallel { actions } => {
            let handles: Vec<_> = actions
                .into_iter()
                .map(|a| {
                    let p = publish.clone();
                    let s = state.clone();
                    tokio::spawn(run_single_action(a, p, s))
                })
                .collect();
            for h in handles {
                h.await??;
            }
        }

        other => run_single_action(other, publish, state).await?,
    }
    Ok(())
}

async fn run_single_action(
    action: Action,
    publish: Option<PublishHandle>,
    state: StateStore,
) -> Result<()> {
    match action {
        Action::Delay { duration_ms } => {
            tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms)).await;
        }

        Action::Parallel { actions } => {
            for a in actions {
                Box::pin(run_single_action(a, publish.clone(), state.clone())).await?;
            }
        }

        Action::RepeatUntil { condition, actions, max_iterations, interval_ms } => {
            let limit = max_iterations.unwrap_or(100);
            let delay = interval_ms.unwrap_or(0);
            for i in 0..limit {
                let cond_script = condition.clone();
                let done = tokio::task::spawn_blocking(move || {
                    ScriptRuntime::new().eval_condition(&cond_script)
                })
                .await??;
                if done {
                    break;
                }
                if i == limit - 1 {
                    warn!(max = limit, "RepeatUntil hit max_iterations without condition becoming true");
                    break;
                }
                for a in &actions {
                    Box::pin(run_single_action(a.clone(), publish.clone(), state.clone())).await?;
                }
                if delay > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Action::SetDeviceState { device_id, state: desired } => {
            let topic = format!("homecore/devices/{device_id}/cmd");
            let payload = desired.to_string().into_bytes();
            if let Some(ph) = publish {
                ph.publish(&topic, payload).await?;
            } else {
                warn!(device = %device_id, "No publish handle; SetDeviceState dropped");
            }
        }

        Action::PublishMqtt { topic, payload, retain } => {
            if let Some(ph) = publish {
                if retain {
                    ph.publish_retained(&topic, payload.into_bytes()).await?;
                } else {
                    ph.publish(&topic, payload.into_bytes()).await?;
                }
            } else {
                warn!(topic, "No publish handle; PublishMqtt dropped");
            }
        }

        Action::FireEvent { event_type, payload } => {
            if let Some(ph) = publish {
                let topic = format!("homecore/events/{event_type}");
                ph.publish_json(&topic, &payload, false).await?;
            }
        }

        Action::CallService { url, method, body, timeout_ms, retries, response_event } => {
            let method_upper = method.to_uppercase();
            let timeout = tokio::time::Duration::from_millis(timeout_ms.unwrap_or(10_000));
            let max_attempts = retries.unwrap_or(0) + 1;
            let client = http_client();

            let mut last_err: anyhow::Error = anyhow!("no attempts made");

            'retry: for attempt in 0..max_attempts {
                if attempt > 0 {
                    // Exponential backoff: 500 ms, 1 000 ms, 2 000 ms, capped at 4 000 ms.
                    let backoff_ms = 500u64 * (1u64 << (attempt - 1).min(3));
                    info!(url, attempt, backoff_ms, "CallService retrying");
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
                        warn!(url, attempt, error = %e, "CallService request failed");
                        last_err = e.into();
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        // Retry on 5xx; treat 4xx as a permanent failure.
                        if status.is_server_error() {
                            warn!(url, %status, attempt, "CallService 5xx — will retry");
                            last_err = anyhow!("HTTP {status}");
                            continue 'retry;
                        }
                        if !status.is_success() {
                            warn!(url, %status, "CallService HTTP error (not retrying)");
                            return Err(anyhow!("HTTP {status}"));
                        }
                        info!(url, %status, "CallService OK");

                        // Optionally forward the response body onto the event bus.
                        if let Some(ref event_type) = response_event {
                            if let Some(ref ph) = publish {
                                let resp_body: JsonValue =
                                    resp.json().await.unwrap_or(JsonValue::Null);
                                let topic = format!("homecore/events/{event_type}");
                                ph.publish_json(&topic, &resp_body, false).await?;
                            }
                        }
                        return Ok(());
                    }
                }
            }

            return Err(last_err);
        }

        Action::RunScript { script } => {
            // Rhai scripts run synchronously on a blocking thread.
            // `Dynamic` is not Send, so we map to () before crossing the thread boundary.
            tokio::task::spawn_blocking(move || {
                let runtime = ScriptRuntime::new();
                runtime.run_action(&script).map(|_| ())
            })
            .await??;
        }

        Action::Notify { channel, message } => {
            info!(channel, "NOTIFY: {message}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_state::StateStore;

    async fn dummy_store() -> StateStore {
        StateStore::open(
            &format!("/tmp/test-exec-{}.redb", uuid::Uuid::new_v4()),
            &format!("/tmp/test-exec-{}.db", uuid::Uuid::new_v4()),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn repeat_until_exits_when_condition_true_immediately() {
        let store = dummy_store().await;
        // Condition is always true → loop body should never run (0 iterations).
        let action = Action::RepeatUntil {
            condition: "true".into(),
            actions: vec![Action::Delay { duration_ms: 0 }],
            max_iterations: Some(10),
            interval_ms: None,
        };
        execute_actions(vec![action], None, store).await.unwrap();
    }

    #[tokio::test]
    async fn repeat_until_respects_max_iterations() {
        let store = dummy_store().await;
        // Condition is always false → must stop at max_iterations.
        let action = Action::RepeatUntil {
            condition: "false".into(),
            actions: vec![Action::Delay { duration_ms: 0 }],
            max_iterations: Some(3),
            interval_ms: None,
        };
        // Should complete without hanging.
        execute_actions(vec![action], None, store).await.unwrap();
    }

    #[tokio::test]
    async fn delay_action_completes() {
        let store = dummy_store().await;
        execute_actions(vec![Action::Delay { duration_ms: 1 }], None, store).await.unwrap();
    }

    // ---- CallService tests ----

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

        let store = dummy_store().await;
        let action = Action::CallService {
            url: format!("{}/hook", server.url()),
            method: "POST".into(),
            body: serde_json::json!({"key": "val"}),
            timeout_ms: None,
            retries: None,
            response_event: None,
        };
        execute_actions(vec![action], None, store).await.unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn call_service_4xx_returns_error() {
        let mut server = mockito::Server::new_async().await;
        server.mock("GET", "/gone").with_status(404).create_async().await;

        let store = dummy_store().await;
        let result = execute_actions(vec![call_service(&format!("{}/gone", server.url()), "GET")], None, store).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_retries_on_5xx_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First call returns 500, second returns 200.
        let _m1 = server.mock("POST", "/retry").with_status(500).create_async().await;
        let _m2 = server.mock("POST", "/retry").with_status(200).create_async().await;

        let store = dummy_store().await;
        let action = Action::CallService {
            url: format!("{}/retry", server.url()),
            method: "POST".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        };
        execute_actions(vec![action], None, store).await.unwrap();
    }

    #[tokio::test]
    async fn call_service_exhausts_retries_returns_error() {
        let mut server = mockito::Server::new_async().await;
        // Both attempts return 500.
        let _m1 = server.mock("GET", "/fail").with_status(500).create_async().await;
        let _m2 = server.mock("GET", "/fail").with_status(500).create_async().await;

        let store = dummy_store().await;
        let action = Action::CallService {
            url: format!("{}/fail", server.url()),
            method: "GET".into(),
            body: serde_json::Value::Null,
            timeout_ms: None,
            retries: Some(1),
            response_event: None,
        };
        let result = execute_actions(vec![action], None, store).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_service_unsupported_method_returns_error() {
        let store = dummy_store().await;
        let result = execute_actions(
            vec![call_service("http://localhost/x", "CONNECT")],
            None,
            store,
        )
        .await;
        assert!(result.is_err());
    }
}
