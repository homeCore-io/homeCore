//! Action executor — runs rule actions sequentially or concurrently.

use anyhow::{anyhow, Result};
use hc_mqtt_client::PublishHandle;
use hc_scripting::ScriptRuntime;
use hc_state::StateStore;
use hc_types::rule::Action;
use tracing::{info, warn};

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

        Action::CallService { url, method, body } => {
            let method_upper = method.to_uppercase();
            let client = reqwest::Client::new();
            let req = match method_upper.as_str() {
                "GET" => client.get(&url),
                "POST" => client.post(&url).json(&body),
                "PUT" => client.put(&url).json(&body),
                "PATCH" => client.patch(&url).json(&body),
                "DELETE" => client.delete(&url),
                other => return Err(anyhow!("Unsupported HTTP method: {other}")),
            };
            let resp = req.send().await?;
            let status = resp.status();
            if !status.is_success() {
                warn!(url, %status, "CallService HTTP error");
            } else {
                info!(url, %status, "CallService OK");
            }
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
}
