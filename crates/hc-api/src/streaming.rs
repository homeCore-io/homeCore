//! Core-side streaming plumbing (Phase 2a of `pluginCapabilitiesPlan.md`).
//!
//! Responsibilities:
//! - Track active streaming requests for `concurrency:"single"` enforcement.
//! - Listen to MQTT messages on the event bus and release tracker entries
//!   when a terminal stage (`complete`/`error`/`canceled`/`timeout`) is
//!   observed on the stream topic.
//! - Inject synthetic `timeout` terminals per request's `timeout_ms`.
//! - Inject synthetic `{stage:"error", data:{reason:"plugin_offline"}}`
//!   on every active stream when a plugin drops offline.
//!
//! The [`StreamingRegistry`] is cloneable (internal `Arc`) and stored on
//! [`crate::AppState`]. Handler code calls [`StreamingRegistry::reserve`]
//! and [`StreamingRegistry::release`]; background tasks drive the rest.

use chrono::Utc;
use hc_core::EventBus;
use hc_mqtt_client::PublishHandle;
use hc_types::event::Event;
use hc_types::Concurrency;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, warn};

/// Per-request tracker entry.
#[derive(Clone, Debug)]
pub struct StreamingEntry {
    pub plugin_id: String,
    pub action_id: String,
    pub concurrency: Concurrency,
}

/// Ordered cache of stream events per request_id. The plan deliberately
/// clears the retained MQTT message on terminal so late subscribers don't
/// see stale state, but that makes fast actions (which finish before the
/// HTTP client can open SSE) appear as empty drawers. This cache bridges
/// that window: state_bridge still lives on MQTT retained, but an
/// in-process cache lets the SSE handler replay events emitted before
/// the subscriber attached.
///
/// Entries are pruned `ENTRY_TTL_SECS` after the terminal event so the
/// cache doesn't grow without bound.
#[derive(Clone, Default)]
pub struct StreamCache {
    inner: Arc<Mutex<HashMap<String, CachedStream>>>,
}

const ENTRY_TTL_SECS: u64 = 60;
const MAX_EVENTS_PER_REQUEST: usize = 256;

#[derive(Clone)]
struct CachedStream {
    events: Vec<Value>,
    terminal_at: Option<std::time::Instant>,
}

impl StreamCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self, request_id: &str) -> Vec<Value> {
        let guard = self.inner.lock().await;
        guard
            .get(request_id)
            .map(|c| c.events.clone())
            .unwrap_or_default()
    }

    async fn append(&self, request_id: String, event: Value, is_terminal: bool) {
        let mut guard = self.inner.lock().await;
        let entry = guard.entry(request_id).or_insert_with(|| CachedStream {
            events: Vec::new(),
            terminal_at: None,
        });
        if entry.events.len() < MAX_EVENTS_PER_REQUEST {
            entry.events.push(event);
        }
        if is_terminal {
            entry.terminal_at = Some(std::time::Instant::now());
        }
    }

    async fn gc(&self) {
        let now = std::time::Instant::now();
        let mut guard = self.inner.lock().await;
        guard.retain(|_rid, c| match c.terminal_at {
            Some(t) => now.duration_since(t) < std::time::Duration::from_secs(ENTRY_TTL_SECS),
            None => true,
        });
    }
}

/// Subscribe to the raw bus, mirror stream events into a [`StreamCache`].
/// Call from main.rs (production) or the test harness, once per process.
pub fn spawn_stream_cache_populator(raw_bus: &EventBus, cache: StreamCache) {
    let mut rx = raw_bus.subscribe();
    tokio::spawn(async move {
        let mut gc_interval = tokio::time::interval(std::time::Duration::from_secs(ENTRY_TTL_SECS));
        // Skip the first immediate tick.
        gc_interval.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(Event::MqttMessage { topic, payload, .. }) => {
                            let Some((_pid, request_id)) = parse_stream_topic(&topic) else {
                                continue;
                            };
                            // Empty retained-clear — not a real event; skip.
                            if payload.is_empty() {
                                continue;
                            }
                            let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                                continue;
                            };
                            let is_term = val
                                .get("stage")
                                .and_then(Value::as_str)
                                .map(is_terminal)
                                .unwrap_or(false);
                            cache.append(request_id, val, is_term).await;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = gc_interval.tick() => {
                    cache.gc().await;
                }
            }
        }
    });
}

/// Cloneable registry of active streaming requests.
#[derive(Clone, Default)]
pub struct StreamingRegistry {
    inner: Arc<Mutex<HashMap<String, StreamingEntry>>>,
    timeout_cancel: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
}

impl StreamingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the `request_id` of an active `concurrency:"single"` stream
    /// for `(plugin_id, action_id)`, if any. Used to short-circuit a new
    /// invocation with `status:"busy"`.
    pub async fn active_single(&self, plugin_id: &str, action_id: &str) -> Option<String> {
        let map = self.inner.lock().await;
        map.iter()
            .find(|(_, e)| {
                e.plugin_id == plugin_id
                    && e.action_id == action_id
                    && matches!(e.concurrency, Concurrency::Single)
            })
            .map(|(rid, _)| rid.clone())
    }

    /// Reserve a slot. Called right after core mints the `request_id` and
    /// before forwarding the command to the plugin. Spawning a timeout
    /// task here (if `timeout_ms` set) is optional per-caller.
    pub async fn reserve(&self, request_id: String, entry: StreamingEntry) {
        let mut map = self.inner.lock().await;
        map.insert(request_id, entry);
    }

    /// Release a slot on observed terminal, synthetic timeout, or plugin
    /// offline. Idempotent.
    pub async fn release(&self, request_id: &str) -> Option<StreamingEntry> {
        // Cancel any pending timeout task.
        if let Some(tx) = self.timeout_cancel.lock().await.remove(request_id) {
            let _ = tx.send(());
        }
        self.inner.lock().await.remove(request_id)
    }

    pub async fn entries_for_plugin(&self, plugin_id: &str) -> Vec<(String, StreamingEntry)> {
        self.inner
            .lock()
            .await
            .iter()
            .filter(|(_, e)| e.plugin_id == plugin_id)
            .map(|(rid, e)| (rid.clone(), e.clone()))
            .collect()
    }

    /// Install a timeout cancel channel for `request_id` so the caller
    /// can race the sleep against a terminal observation.
    async fn install_timeout_cancel(
        &self,
        request_id: String,
        tx: tokio::sync::oneshot::Sender<()>,
    ) {
        self.timeout_cancel.lock().await.insert(request_id, tx);
    }
}

/// Background task: watch the event bus for stream events on
/// `homecore/plugins/+/commands/+/events`. On any terminal stage, release
/// the corresponding registry entry so the concurrency slot frees up.
pub fn spawn_terminal_observer(registry: StreamingRegistry, event_bus: &EventBus) {
    let mut rx = event_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::MqttMessage { topic, payload, .. }) => {
                    let Some((_plugin_id, request_id)) = parse_stream_topic(&topic) else {
                        continue;
                    };
                    // Empty retained-clear — ignore.
                    if payload.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                        continue;
                    };
                    let stage = val.get("stage").and_then(Value::as_str).unwrap_or("");
                    if is_terminal(stage) {
                        registry.release(&request_id).await;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Spawn a timeout task that publishes a synthetic terminal `timeout`
/// event (retained) + retained-clear after the given deadline if the
/// registry still holds the request.
pub async fn schedule_timeout(
    registry: &StreamingRegistry,
    publish: PublishHandle,
    plugin_id: String,
    request_id: String,
    timeout_ms: u64,
) {
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    registry
        .install_timeout_cancel(request_id.clone(), cancel_tx)
        .await;
    let registry_clone = registry.clone();
    tokio::spawn(async move {
        let sleep = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {
                // Only inject if the request is still outstanding. A race
                // with a terminal that just landed is OK — release() above
                // will have been called first.
                if registry_clone.release(&request_id).await.is_none() {
                    return;
                }
                let topic = format!(
                    "homecore/plugins/{plugin_id}/commands/{request_id}/events"
                );
                let ev = json!({
                    "stage": "timeout",
                    "request_id": request_id,
                    "ts": Utc::now().to_rfc3339(),
                    "message": "core-injected timeout",
                });
                if let Ok(bytes) = serde_json::to_vec(&ev) {
                    if let Err(e) = publish.publish(&topic, bytes).await {
                        warn!(error = %e, "timeout injection publish failed");
                    }
                }
                // Retained clear.
                let _ = publish.publish(&topic, Vec::new()).await;
            }
            _ = cancel_rx => {
                debug!(request_id, "timeout task canceled — terminal observed first");
            }
        }
    });
}

/// Called by the plugin-offline sweep when a plugin's status flips to
/// `offline`. Publishes a synthetic terminal error on every open stream
/// belonging to that plugin and releases each slot.
pub async fn inject_plugin_offline(
    registry: &StreamingRegistry,
    publish: &PublishHandle,
    plugin_id: &str,
) {
    for (request_id, _entry) in registry.entries_for_plugin(plugin_id).await {
        // Release first so the observer doesn't race with us.
        registry.release(&request_id).await;

        let topic = format!("homecore/plugins/{plugin_id}/commands/{request_id}/events");
        let ev = json!({
            "stage": "error",
            "request_id": request_id,
            "ts": Utc::now().to_rfc3339(),
            "message": "plugin went offline",
            "data": { "reason": "plugin_offline" },
        });
        if let Ok(bytes) = serde_json::to_vec(&ev) {
            if let Err(e) = publish.publish(&topic, bytes).await {
                warn!(error = %e, "plugin_offline injection publish failed");
            }
        }
        let _ = publish.publish(&topic, Vec::new()).await;
    }
}

/// Return `Some((plugin_id, request_id))` if `topic` matches
/// `homecore/plugins/{id}/commands/{rid}/events`.
fn parse_stream_topic(topic: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 6
        && parts[0] == "homecore"
        && parts[1] == "plugins"
        && parts[3] == "commands"
        && parts[5] == "events"
    {
        Some((parts[2].to_string(), parts[4].to_string()))
    } else {
        None
    }
}

fn is_terminal(stage: &str) -> bool {
    matches!(stage, "complete" | "error" | "canceled" | "timeout")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_stream_topic() {
        let got = parse_stream_topic("homecore/plugins/hc-captest/commands/r-abc/events");
        assert_eq!(got, Some(("hc-captest".into(), "r-abc".into())));
    }

    #[test]
    fn rejects_unrelated_topics() {
        assert!(parse_stream_topic("homecore/devices/x/state").is_none());
        assert!(parse_stream_topic("homecore/plugins/x/capabilities").is_none());
        assert!(parse_stream_topic("homecore/plugins/x/commands/r/events/extra").is_none());
    }

    #[tokio::test]
    async fn reserve_and_release_roundtrip() {
        let reg = StreamingRegistry::new();
        let entry = StreamingEntry {
            plugin_id: "p".into(),
            action_id: "a".into(),
            concurrency: Concurrency::Single,
        };
        reg.reserve("r-1".into(), entry.clone()).await;
        assert_eq!(reg.active_single("p", "a").await.as_deref(), Some("r-1"));
        let out = reg.release("r-1").await;
        assert!(out.is_some());
        assert!(reg.active_single("p", "a").await.is_none());
    }

    #[tokio::test]
    async fn multi_concurrency_does_not_block() {
        let reg = StreamingRegistry::new();
        reg.reserve(
            "r-1".into(),
            StreamingEntry {
                plugin_id: "p".into(),
                action_id: "a".into(),
                concurrency: Concurrency::Multi,
            },
        )
        .await;
        assert!(reg.active_single("p", "a").await.is_none());
    }
}
