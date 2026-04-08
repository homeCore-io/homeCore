//! MQTT-based request-response RPC for plugin management.
//!
//! Publishes commands to `homecore/plugins/{id}/manage/cmd` and awaits
//! responses on `homecore/plugins/{id}/manage/response` with a matching
//! `request_id`.  5-second timeout.
//!
//! This enables config read/write and log level changes for remote plugins
//! that implement the management protocol.

use hc_core::EventBus;
use hc_mqtt_client::PublishHandle;
use hc_types::event::Event;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tracing::debug;
use uuid::Uuid;

/// Pending RPC request waiting for a matching response.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// MQTT management RPC helper.
///
/// Clone-friendly — all internal state is behind Arc.
#[derive(Clone)]
pub struct ManagementRpc {
    publish: PublishHandle,
    pending: PendingMap,
}

impl ManagementRpc {
    /// Create a new RPC helper and spawn a background task that resolves
    /// pending requests from `PluginManagementResponse` events on the bus.
    pub fn new(publish: PublishHandle, event_bus: &EventBus) -> Self {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Background task: listen for management response events from state_bridge.
        {
            let mut rx = event_bus.subscribe();
            let pending_clone = Arc::clone(&pending);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(Event::Custom { payload, event_type, .. })
                            if event_type == "plugin_management_response" =>
                        {
                            let request_id = payload["request_id"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            if request_id.is_empty() {
                                continue;
                            }
                            let mut map = pending_clone.lock().await;
                            if let Some(tx) = map.remove(&request_id) {
                                let _ = tx.send(payload);
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!("ManagementRpc: lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        Self { publish, pending }
    }

    /// Send a management command and wait up to 5 seconds for a response.
    pub async fn request(
        &self,
        plugin_id: &str,
        action: &str,
        extra: Option<Value>,
    ) -> Result<Value, String> {
        let request_id = Uuid::new_v4().to_string();
        let topic = format!("homecore/plugins/{plugin_id}/manage/cmd");

        let mut payload = json!({
            "action": action,
            "request_id": request_id,
        });
        if let Some(extra) = extra {
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    payload[k.clone()] = v.clone();
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(request_id.clone(), tx);
        }

        // Publish the command.
        let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
        self.publish
            .publish(&topic, payload_bytes)
            .await
            .map_err(|e| format!("failed to publish management command: {e}"))?;

        // Await response with timeout.
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok(response)) => {
                if response["status"].as_str() == Some("error") {
                    Err(response["error"]
                        .as_str()
                        .unwrap_or("unknown error")
                        .to_string())
                } else {
                    Ok(response)
                }
            }
            Ok(Err(_)) => {
                // oneshot sender dropped (shouldn't happen)
                Err("internal error: response channel closed".into())
            }
            Err(_) => {
                // Timeout — clean up pending entry.
                let mut map = self.pending.lock().await;
                map.remove(&request_id);
                Err(format!(
                    "plugin {plugin_id} did not respond within 5 seconds"
                ))
            }
        }
    }

    /// Convenience: request config from a remote plugin.
    pub async fn get_config(&self, plugin_id: &str) -> Result<Value, String> {
        self.request(plugin_id, "get_config", None).await
    }

    /// Convenience: push config to a remote plugin.
    pub async fn set_config(&self, plugin_id: &str, config: Value) -> Result<Value, String> {
        self.request(plugin_id, "set_config", Some(json!({ "config": config })))
            .await
    }

    /// Convenience: change log level on a remote plugin.
    pub async fn set_log_level(&self, plugin_id: &str, level: &str) -> Result<Value, String> {
        self.request(plugin_id, "set_log_level", Some(json!({ "level": level })))
            .await
    }
}
