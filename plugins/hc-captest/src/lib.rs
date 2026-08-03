//! hc-captest — conformance test plugin for the capability-streaming
//! contract (Phase 2a of `pluginCapabilitiesPlan.md`).
//!
//! Exposes four streaming actions that exercise every convention in §6
//! so the SDK + core plumbing can be validated without any real-device
//! dependency. Used by `core/tests/streaming_test.rs`.
//!
//! The four actions:
//!
//! 1. `demo_item_stream` — multi-item stream with `item_key:"id"`,
//!    progress updates, and a terminal `complete`.
//! 2. `demo_awaiting_user` — emits an advisory `awaiting_user` then an
//!    interactive one with `response_schema`. The caller's response
//!    determines whether the stream completes or errors.
//! 3. `demo_cancelable` — long progress loop that checks `is_canceled()`
//!    between iterations and acknowledges with a terminal `canceled`.
//! 4. `demo_error_vs_warning` — emits N `warning` events followed by a
//!    terminal `error` (exercising the "error is always terminal, retry
//!    via warning" rule).
//!
//! All actions are designed to run quickly in tests (milliseconds).

use anyhow::Result;
use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, ItemOp, RequiresRole};
use plugin_sdk_rs::{ManagementHandle, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::time::Duration;

/// Plugin id used by hc-captest both as the MQTT client id and inside
/// the capability manifest.
pub const PLUGIN_ID: &str = "hc-captest";

/// Register the four demo streaming actions and the capability manifest
/// on a `ManagementHandle`. Returns the updated handle.
pub fn register_actions(mgmt: ManagementHandle) -> ManagementHandle {
    mgmt.with_capabilities(capabilities_manifest())
        .with_streaming_action(StreamingAction::new(
            "demo_item_stream",
            |ctx, params| async move { demo_item_stream(ctx, params).await },
        ))
        .with_streaming_action(StreamingAction::new(
            "demo_awaiting_user",
            |ctx, params| async move { demo_awaiting_user(ctx, params).await },
        ))
        .with_streaming_action(StreamingAction::new(
            "demo_cancelable",
            |ctx, params| async move { demo_cancelable(ctx, params).await },
        ))
        .with_streaming_action(StreamingAction::new(
            "demo_error_vs_warning",
            |ctx, params| async move { demo_error_vs_warning(ctx, params).await },
        ))
        .with_streaming_action(StreamingAction::new(
            "demo_never_completes",
            |ctx, params| async move { demo_never_completes(ctx, params).await },
        ))
        .with_streaming_action(StreamingAction::new(
            "demo_admin_only",
            |ctx, params| async move { demo_admin_only(ctx, params).await },
        ))
}

/// The capability manifest hc-captest publishes. Exposed so integration
/// tests can diff against what the plugin declares.
pub fn capabilities_manifest() -> Capabilities {
    Capabilities {
        spec: "1".into(),
        plugin_id: PLUGIN_ID.into(),
        actions: vec![
            Action {
                id: "demo_item_stream".into(),
                label: "Demo: multi-item stream".into(),
                description: Some(
                    "Emits progress + a series of item add/update events, then completes.".into(),
                ),
                params: Some(json!({
                    "item_count": {"type": "integer", "minimum": 1, "maximum": 20, "default": 3}
                })),
                result: Some(json!({
                    "ids_added": {"type": "array"}
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Multi,
                item_key: Some("id".into()),
                item_operations: Some(vec![ItemOp::Add, ItemOp::Update]),
                requires_role: RequiresRole::User,
                timeout_ms: Some(10_000),
            },
            Action {
                id: "demo_awaiting_user".into(),
                label: "Demo: awaiting-user prompts".into(),
                description: Some(
                    "Emits an advisory prompt then an interactive prompt with schema.".into(),
                ),
                params: None,
                result: None,
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Multi,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(30_000),
            },
            Action {
                id: "demo_cancelable".into(),
                label: "Demo: cancelable long-running".into(),
                description: Some(
                    "Long progress loop that honours cancel. concurrency:single.".into(),
                ),
                params: None,
                result: None,
                stream: true,
                cancelable: true,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(30_000),
            },
            Action {
                id: "demo_error_vs_warning".into(),
                label: "Demo: warning vs error".into(),
                description: Some(
                    "Emits N warnings then terminal error. Validates the \
                     'error is always terminal, retries emit warning' rule."
                        .into(),
                ),
                params: Some(json!({
                    "warnings": {"type": "integer", "minimum": 0, "maximum": 10, "default": 2}
                })),
                result: None,
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Multi,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(10_000),
            },
            Action {
                id: "demo_never_completes".into(),
                label: "Demo: never-completing (timeout target)".into(),
                description: Some(
                    "Sleeps past the manifest timeout_ms so core injects a \
                     synthetic timeout terminal. Used to validate timeout \
                     injection."
                        .into(),
                ),
                params: None,
                result: None,
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Multi,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                // Intentionally short — core should inject timeout first.
                timeout_ms: Some(300),
            },
            Action {
                id: "demo_admin_only".into(),
                label: "Demo: admin-only action".into(),
                description: Some(
                    "Completes immediately. requires_role:\"admin\" — used to \
                     validate role enforcement."
                        .into(),
                ),
                params: None,
                result: None,
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Multi,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(5_000),
            },
        ],
    }
}

async fn demo_item_stream(ctx: StreamContext, params: Value) -> Result<()> {
    let n = params
        .get("item_count")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .min(20) as usize;

    ctx.progress(Some(0), Some("starting"), None).await?;

    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = format!("item-{i}");
        ids.push(id.clone());
        ctx.item_add(json!({ "id": id, "status": "new" })).await?;
        // Tiny yield so the event ordering is observable; in prod the
        // plugin would just do real work here.
        tokio::time::sleep(Duration::from_millis(5)).await;
        ctx.item_update(json!({ "id": id, "status": "ready" }))
            .await?;
        let pct = ((i + 1) * 100 / n) as u8;
        ctx.progress(Some(pct), Some("filling"), None).await?;
    }

    ctx.complete(json!({ "ids_added": ids })).await
}

async fn demo_awaiting_user(ctx: StreamContext, _params: Value) -> Result<()> {
    ctx.awaiting_user("Advisory prompt — press any button then send respond.")
        .await?;
    // Advisory: dispatcher delivers the next respond payload here.
    // We still want to wait for it so the flow is a realistic two-step.
    let _ack = ctx
        .awaiting_user_with_schema(
            "Interactive prompt — choose an option.",
            json!({
                "proceed": { "type": "boolean", "required": true }
            }),
        )
        .await?;

    if _ack
        .get("proceed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ctx.complete(json!({ "accepted": true })).await
    } else {
        ctx.error("user declined").await
    }
}

async fn demo_cancelable(ctx: StreamContext, _params: Value) -> Result<()> {
    for i in 0..100 {
        if ctx.is_canceled() {
            return ctx.canceled().await;
        }
        ctx.progress(Some(i), Some("working"), None).await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ctx.complete(json!({ "reached": "end" })).await
}

async fn demo_error_vs_warning(ctx: StreamContext, params: Value) -> Result<()> {
    let warnings = params
        .get("warnings")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .min(10);

    for i in 0..warnings {
        ctx.warning(
            format!("retryable condition {i}"),
            Some(json!({ "attempt": i })),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    ctx.error("unrecoverable after retries").await
}

/// Sleeps past the manifest-declared `timeout_ms` so core injects a
/// synthetic `timeout` terminal on the stream. The plugin should never
/// emit anything other than a single progress event — if we win the
/// race and reach `complete`, the test fails.
async fn demo_never_completes(ctx: StreamContext, _params: Value) -> Result<()> {
    ctx.progress(Some(0), Some("sleeping past timeout"), None)
        .await?;
    // Sleep for much longer than demo_never_completes's timeout_ms (300ms).
    tokio::time::sleep(Duration::from_secs(10)).await;
    ctx.complete(json!({ "never": true })).await
}

/// Immediate complete — the body never runs unless the caller's role
/// satisfies `requires_role:"admin"`. Used to validate core's role
/// enforcement in `post_plugin_command`.
async fn demo_admin_only(ctx: StreamContext, _params: Value) -> Result<()> {
    ctx.complete(json!({ "admin_action": "ok" })).await
}
