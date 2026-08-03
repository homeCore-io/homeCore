//! Streaming action support — SDK side of the capability manifest's
//! `stream:true` actions (spec: `pluginCapabilitiesPlan.md` §3-§6).
//!
//! A streaming action gets a [`StreamContext`] and publishes events on
//! `homecore/plugins/{id}/commands/{request_id}/events`. The 6 stages
//! (`awaiting_user`, `progress`, `item`, `warning`, `complete`, `error`)
//! each have a method on the context. Terminal stages (`complete`,
//! `error`, plus SDK-internal `canceled`) are latched — a second call
//! returns an error and nothing is published.
//!
//! Cancel and respond routing: the SDK keeps a per-request registry of
//! cancel tokens and respond channels. When a `cancel` or `respond`
//! management command arrives, the management-cmd dispatcher looks up
//! the active stream by `target_request_id` and delivers the signal
//! directly to the corresponding [`StreamContext`].

use anyhow::{anyhow, Result};
use chrono::Utc;
use rumqttc::{AsyncClient, QoS};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Mutex as AsyncMutex};

/// Shared registry of live streams, keyed by `request_id`. Populated on
/// `status:"accepted"` dispatch and cleaned by the dispatcher after the
/// closure returns.
pub(crate) type ActiveStreams = Arc<Mutex<HashMap<String, ActiveStreamEntry>>>;

/// Per-stream in-process state that `cancel`/`respond` commands route to.
pub(crate) struct ActiveStreamEntry {
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) respond_tx: mpsc::UnboundedSender<Value>,
}

/// Future type returned by a streaming action closure.
pub type StreamingFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Boxed streaming action handler. Stored in `ManagementHandle` and
/// dispatched on matching `action` ids arriving on
/// `homecore/plugins/{id}/manage/cmd`.
pub type StreamingHandler =
    Arc<dyn Fn(StreamContext, Value) -> StreamingFuture + Send + Sync + 'static>;

/// Declared streaming action bound to its handler closure.
#[derive(Clone)]
pub struct StreamingAction {
    pub(crate) id: String,
    pub(crate) handler: StreamingHandler,
}

impl StreamingAction {
    /// Create a streaming action handler for action `id`. The closure gets
    /// an owned [`StreamContext`] per invocation and the parsed `params`
    /// object from the command payload.
    pub fn new<F, Fut>(id: impl Into<String>, handler: F) -> Self
    where
        F: Fn(StreamContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let handler: StreamingHandler = Arc::new(move |ctx, params| Box::pin(handler(ctx, params)));
        Self {
            id: id.into(),
            handler,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Per-invocation handle passed to a streaming action closure. Emits
/// events on the stream topic and watches for cancellation + responses.
///
/// Dropping without calling a terminal method is a bug — the dispatcher
/// synthesises an `error { reason: "plugin_dropped_stream" }` in that
/// case to keep the stream contract honest.
pub struct StreamContext {
    request_id: String,
    plugin_id: String,
    stream_topic: String,
    action_id: String,
    client: AsyncClient,
    cancel: Arc<AtomicBool>,
    terminal: Arc<AtomicBool>,
    respond_rx: AsyncMutex<mpsc::UnboundedReceiver<Value>>,
}

impl StreamContext {
    pub(crate) fn new(
        request_id: String,
        plugin_id: String,
        action_id: String,
        client: AsyncClient,
        cancel: Arc<AtomicBool>,
        terminal: Arc<AtomicBool>,
        respond_rx: mpsc::UnboundedReceiver<Value>,
    ) -> Self {
        let stream_topic = format!("homecore/plugins/{plugin_id}/commands/{request_id}/events");
        Self {
            request_id,
            plugin_id,
            stream_topic,
            action_id,
            client,
            cancel,
            terminal,
            respond_rx: AsyncMutex::new(respond_rx),
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn action_id(&self) -> &str {
        &self.action_id
    }

    pub fn stream_topic(&self) -> &str {
        &self.stream_topic
    }

    /// Returns true if a `cancel` command was received for this
    /// `request_id`. Closures running cooperative loops should check
    /// this between iterations and call [`StreamContext::canceled`].
    pub fn is_canceled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Emit an `awaiting_user` event with a human-readable prompt and no
    /// `response_schema`. Advisory — UI shows a "Done" button; the
    /// caller sends `respond` with an empty response.
    pub async fn awaiting_user(&self, prompt: impl Into<String>) -> Result<()> {
        self.emit_non_terminal(json!({
            "stage": "awaiting_user",
            "prompt": prompt.into(),
        }))
        .await
    }

    /// Emit an `awaiting_user` event with a structured `response_schema`
    /// and wait for a matching `respond` command. Returns the
    /// caller-supplied response payload.
    pub async fn awaiting_user_with_schema(
        &self,
        prompt: impl Into<String>,
        response_schema: Value,
    ) -> Result<Value> {
        self.emit_awaiting_user_with_schema(prompt, response_schema)
            .await?;
        self.await_respond().await
    }

    /// Emit only the `awaiting_user` event with `response_schema`, without
    /// blocking on the respond. Use with [`StreamContext::await_respond`]
    /// when the plugin needs to handle other work (e.g. live controller
    /// events) concurrently with the prompt.
    pub async fn emit_awaiting_user_with_schema(
        &self,
        prompt: impl Into<String>,
        response_schema: Value,
    ) -> Result<()> {
        self.emit_non_terminal(json!({
            "stage": "awaiting_user",
            "prompt": prompt.into(),
            "response_schema": response_schema,
        }))
        .await
    }

    /// Await the next `respond` command payload. Pairs with
    /// [`StreamContext::emit_awaiting_user_with_schema`] when the plugin
    /// is also processing other async work.
    pub async fn await_respond(&self) -> Result<Value> {
        let mut rx = self.respond_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| anyhow!("respond channel closed before response arrived"))
    }

    /// Resolves when a `cancel` command has flipped the cancel token.
    /// Cooperative — use inside `tokio::select!` so a long-running action
    /// exits promptly. Plugins must still call [`StreamContext::canceled`]
    /// to emit the terminal stage.
    pub async fn wait_canceled(&self) {
        while !self.is_canceled() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Emit a `progress` event.
    pub async fn progress(
        &self,
        percent: Option<u8>,
        label: Option<&str>,
        message: Option<&str>,
    ) -> Result<()> {
        let mut ev = json!({ "stage": "progress" });
        if let Some(p) = percent {
            ev["percent"] = json!(p);
        }
        if let Some(l) = label {
            ev["label"] = json!(l);
        }
        if let Some(m) = message {
            ev["message"] = json!(m);
        }
        self.emit_non_terminal(ev).await
    }

    /// Emit an `item` event with op `add`.
    pub async fn item_add(&self, data: Value) -> Result<()> {
        self.emit_non_terminal(json!({ "stage": "item", "op": "add", "data": data }))
            .await
    }

    /// Emit an `item` event with op `update`.
    pub async fn item_update(&self, data: Value) -> Result<()> {
        self.emit_non_terminal(json!({ "stage": "item", "op": "update", "data": data }))
            .await
    }

    /// Emit an `item` event with op `remove`.
    pub async fn item_remove(&self, data: Value) -> Result<()> {
        self.emit_non_terminal(json!({ "stage": "item", "op": "remove", "data": data }))
            .await
    }

    /// Emit a `warning` event. Non-terminal — stream continues. Use for
    /// recoverable conditions the user should see (retry, transient
    /// failure). Unrecoverable failures use [`StreamContext::error`].
    pub async fn warning(&self, message: impl Into<String>, data: Option<Value>) -> Result<()> {
        let mut ev = json!({
            "stage": "warning",
            "message": message.into(),
        });
        if let Some(d) = data {
            ev["data"] = d;
        }
        self.emit_non_terminal(ev).await
    }

    /// Terminal `complete` — success. `data` should match the manifest's
    /// advisory `result` shape.
    pub async fn complete(&self, data: Value) -> Result<()> {
        self.emit_terminal(json!({ "stage": "complete", "data": data }))
            .await
    }

    /// Terminal `error` — unrecoverable failure. Always terminal per spec
    /// §4: retries emit `warning` and keep going, errors do not.
    pub async fn error(&self, message: impl Into<String>) -> Result<()> {
        self.emit_terminal(json!({
            "stage": "error",
            "message": message.into(),
        }))
        .await
    }

    /// Terminal `canceled` — use after observing `is_canceled()`. The
    /// dispatcher does not emit this automatically; the closure must
    /// call it so the plugin can perform any rollback before
    /// acknowledging cancellation.
    pub async fn canceled(&self) -> Result<()> {
        self.emit_terminal(json!({ "stage": "canceled" })).await
    }

    async fn emit_non_terminal(&self, mut ev: Value) -> Result<()> {
        if self.terminal.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "stream already terminated; no further events may be emitted"
            ));
        }
        self.fill_common_fields(&mut ev);
        self.publish(ev).await
    }

    async fn emit_terminal(&self, mut ev: Value) -> Result<()> {
        if self.terminal.swap(true, Ordering::SeqCst) {
            return Err(anyhow!(
                "stream already terminated; terminal stage already emitted"
            ));
        }
        self.fill_common_fields(&mut ev);
        self.publish(ev).await
    }

    fn fill_common_fields(&self, ev: &mut Value) {
        if let Some(obj) = ev.as_object_mut() {
            obj.insert("request_id".into(), json!(self.request_id));
            obj.insert("ts".into(), json!(Utc::now().to_rfc3339()));
        }
    }

    async fn publish(&self, ev: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&ev)?;
        self.client
            .publish(&self.stream_topic, QoS::AtLeastOnce, true, bytes)
            .await
            .map_err(|e| anyhow!("publish stream event failed: {e}"))
    }
}

/// Called by the dispatcher after the closure exits, whether normally or
/// with an error. Ensures exactly one terminal stage lands on the stream
/// and retained-clears the topic per spec §5.3.
pub(crate) async fn finalize_stream(
    client: &AsyncClient,
    stream_topic: &str,
    request_id: &str,
    terminal_latch: &Arc<AtomicBool>,
    closure_result: Result<()>,
) {
    // If the closure returned without emitting a terminal, synthesise
    // one so the stream contract stays honest. We use the terminal
    // latch to detect "did the closure already terminate?"
    if !terminal_latch.swap(true, Ordering::SeqCst) {
        let msg = match closure_result {
            Ok(()) => "plugin dropped stream without emitting a terminal stage".to_string(),
            Err(e) => format!("plugin action failed: {e}"),
        };
        let ev = json!({
            "stage": "error",
            "request_id": request_id,
            "ts": Utc::now().to_rfc3339(),
            "message": msg,
            "data": { "reason": "plugin_dropped_stream" },
        });
        if let Ok(bytes) = serde_json::to_vec(&ev) {
            let _ = client
                .publish(stream_topic, QoS::AtLeastOnce, true, bytes)
                .await;
        }
    }

    // Retained clear — an empty retained payload deletes the prior
    // retained event so late subscribers don't see a stale terminal.
    let _ = client
        .publish(stream_topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
        .await;
}
