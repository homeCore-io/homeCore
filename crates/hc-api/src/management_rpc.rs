//! MQTT-based request-response RPC for plugin management.
//!
//! Publishes commands to `homecore/plugins/{id}/manage/cmd` and awaits
//! responses on `homecore/plugins/{id}/manage/response` with a matching
//! `request_id`.  Waits [`DEFAULT_TIMEOUT`] unless the action declares its own
//! `timeout_ms` on the capability manifest.
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

/// Default response window for management commands that don't declare their
/// own `timeout_ms`. Kept short because most management ops (config, log
/// level, heartbeat) answer in milliseconds; slow plugin actions (network
/// discovery, reboots) should declare a longer `timeout_ms` on the manifest.
const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
                        Ok(Event::Custom {
                            payload,
                            event_type,
                            ..
                        }) if event_type == "plugin_management_response" => {
                            let request_id =
                                payload["request_id"].as_str().unwrap_or("").to_string();
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

    /// Send a management command and wait up to [`DEFAULT_TIMEOUT`] for a
    /// response.
    pub async fn request(
        &self,
        plugin_id: &str,
        action: &str,
        extra: Option<Value>,
    ) -> Result<Value, String> {
        let request_id = Uuid::new_v4().to_string();
        self.request_with_id(plugin_id, action, &request_id, DEFAULT_TIMEOUT, extra)
            .await
    }

    /// Like [`ManagementRpc::request`] but uses a caller-supplied
    /// `request_id`. Lets concurrency-tracked streaming handlers reserve
    /// the slot before forwarding the command so a second invocation
    /// racing in can observe the reservation.
    pub async fn request_with_id(
        &self,
        plugin_id: &str,
        action: &str,
        request_id: &str,
        timeout: std::time::Duration,
        extra: Option<Value>,
    ) -> Result<Value, String> {
        let response = self
            .dispatch(plugin_id, action, request_id, timeout, extra)
            .await?;
        // Management convention: a `status:"error"` reply is a failure the
        // caller should observe as an `Err` (rejected config write, bad log
        // level, …). The HTTP action layer that must NOT conflate this with a
        // transport timeout uses `dispatch` / `send_command_raw_with_id`.
        if response["status"].as_str() == Some("error") {
            Err(response["error"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string())
        } else {
            Ok(response)
        }
    }

    /// Publish a command and await the plugin's reply, returning the response
    /// VERBATIM for any reply — including `status:"error"` business errors.
    /// `Err` is reserved for a genuine no-response timeout or a closed channel.
    /// This is what lets callers tell "the plugin answered with an error" apart
    /// from "the plugin never answered": the former is a 200 with an error
    /// payload, the latter a 504 gateway timeout — collapsing both into `Err`
    /// (as `request_with_id` does) is why a plugin's own error used to surface
    /// in the UI as a bogus gateway timeout.
    async fn dispatch(
        &self,
        plugin_id: &str,
        action: &str,
        request_id: &str,
        timeout: std::time::Duration,
        extra: Option<Value>,
    ) -> Result<Value, String> {
        let request_id = request_id.to_string();
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
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                // oneshot sender dropped (shouldn't happen)
                Err("internal error: response channel closed".into())
            }
            Err(_) => {
                // Timeout — clean up pending entry.
                let mut map = self.pending.lock().await;
                map.remove(&request_id);
                Err(format!(
                    "plugin {plugin_id} did not respond within {} seconds",
                    timeout.as_secs()
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

    /// Send an arbitrary plugin-specific command.  `params` is merged into the
    /// command envelope (alongside `action` + `request_id`).  Used by the
    /// generic `POST /plugins/{id}/command` endpoint.
    pub async fn send_command(
        &self,
        plugin_id: &str,
        action: &str,
        params: Value,
    ) -> Result<Value, String> {
        let extra = if params.is_object() {
            Some(params)
        } else {
            None
        };
        self.request(plugin_id, action, extra).await
    }

    /// Like [`ManagementRpc::send_command`] with a caller-supplied
    /// `request_id`. Used by the streaming handler so the tracker slot is
    /// reserved before the command leaves core.
    pub async fn send_command_with_id(
        &self,
        plugin_id: &str,
        action: &str,
        request_id: &str,
        timeout_ms: Option<u64>,
        params: Value,
    ) -> Result<Value, String> {
        let extra = if params.is_object() {
            Some(params)
        } else {
            None
        };
        let timeout = timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);
        self.request_with_id(plugin_id, action, request_id, timeout, extra)
            .await
    }

    /// Like [`send_command_with_id`] but returns the plugin's reply verbatim
    /// (including `status:"error"` payloads); only a genuine timeout / closed
    /// channel yields `Err`. The HTTP action endpoint uses this so a plugin's
    /// business error is surfaced as its payload (HTTP 200) rather than a
    /// misleading 504 gateway timeout.
    pub async fn send_command_raw_with_id(
        &self,
        plugin_id: &str,
        action: &str,
        request_id: &str,
        timeout_ms: Option<u64>,
        params: Value,
    ) -> Result<Value, String> {
        let extra = if params.is_object() {
            Some(params)
        } else {
            None
        };
        let timeout = timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);
        self.dispatch(plugin_id, action, request_id, timeout, extra)
            .await
    }
}
