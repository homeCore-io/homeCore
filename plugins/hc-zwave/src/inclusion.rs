//! Z-Wave inclusion / exclusion via the capability-streaming contract.
//!
//! Phase 2b of `pluginCapabilitiesPlan.md`: exposes `include_node` and
//! `exclude_node` streaming actions that drive `controller.begin_inclusion`
//! / `controller.begin_exclusion` on the zwave-js-server WebSocket and
//! surface per-node progress as `item` events.
//!
//! The bridge owns the WebSocket connection. This module talks to it
//! through two channels on an [`InclusionHandle`]:
//! - `control_tx` — raw WS commands (serialised JSON) that the bridge
//!   forwards onto the socket.
//! - `events` — a broadcast of controller-scope events decoded from
//!   incoming WS frames. Each streaming action subscribes for the
//!   duration of one invocation.
//!
//! The handle is long-lived (created once in `main`) so bridge
//! reconnects don't invalidate the streaming action registration.

use anyhow::{anyhow, Result};
use plugin_sdk_rs::{ManagementHandle, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Decoded controller-scope event surfaced to streaming action handlers.
///
/// Matches the subset of zwave-js-server controller events the plugin
/// cares about for inclusion/exclusion flows. Anything not in this enum
/// stays on the WS frame path and is discarded for streaming purposes.
#[derive(Debug, Clone)]
pub enum ControllerEvent {
    InclusionStarted,
    InclusionStopped,
    InclusionFailed {
        message: Option<String>,
    },
    ExclusionStarted,
    ExclusionStopped,
    ExclusionFailed {
        message: Option<String>,
    },
    /// A node was added to the network. `raw` carries whatever zwave-js
    /// sends in the `node` payload (id, manufacturer, device type, etc.)
    /// so the UI can surface richer metadata. Usually followed by a
    /// pair of `interview started` / `node ready` events as the device
    /// completes its interview.
    NodeAdded {
        node_id: u32,
        raw: Value,
    },
    NodeRemoved {
        node_id: u32,
    },
    /// zwave-js has begun interviewing a freshly-added node. Used to flip
    /// the inclusion item from `interviewing` (its initial post-add state)
    /// to a more informative label.
    NodeInterviewStarted {
        node_id: u32,
    },
    /// Interview finished — node is fully ready. `raw` is the `nodeState`
    /// payload zwave-js attaches to `node ready`, suitable for upserting
    /// into the inclusion item with richer fields (firmware, name, etc.).
    NodeReady {
        node_id: u32,
        raw: Value,
    },
    /// Interview failed; the node may still be partially usable but the
    /// inclusion UI should mark the item as failed rather than stuck on
    /// "interviewing" forever.
    NodeInterviewFailed {
        node_id: u32,
        message: Option<String>,
    },
    /// S2 bootstrap asking the client which security classes to grant.
    /// Plugin auto-accepts all requested classes in v1.
    GrantSecurityClasses {
        request_id: String,
        requested: Value,
    },
    /// S2 bootstrap asking for DSK PIN entry. Plugin cannot prompt for
    /// this in v1 — logs a warning and lets zwave-js time out.
    /// `request_id` kept for when PIN entry lands in v2.
    ValidateDskAndEnterPin {
        #[allow(dead_code)]
        request_id: String,
        dsk: String,
    },
}

/// Cloneable handle shared between the bridge (which decodes events and
/// forwards WS commands) and the streaming action closures.
#[derive(Clone)]
pub struct InclusionHandle {
    control_tx: mpsc::Sender<Value>,
    events: broadcast::Sender<ControllerEvent>,
    /// Pings the bridge's rescan path so a freshly-completed inclusion
    /// can refresh device registrations without the user manually
    /// invoking `rescan_nodes`.
    rescan_tx: mpsc::Sender<()>,
}

impl InclusionHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<ControllerEvent> {
        self.events.subscribe()
    }

    /// Send a raw zwave-js-server command. The bridge forwards it onto
    /// the WebSocket; the result surfaces as an event later.
    pub async fn send_command(&self, cmd: Value) -> Result<()> {
        self.control_tx
            .send(cmd)
            .await
            .map_err(|_| anyhow!("inclusion control channel closed; bridge not running"))
    }

    /// Ask the bridge to rescan all nodes from zwave-js and republish
    /// their registrations. Best-effort — if the channel is full or the
    /// bridge isn't running we just drop the request.
    pub fn request_rescan(&self) {
        let _ = self.rescan_tx.try_send(());
    }
}

/// Create the inclusion handle + the bridge-side receiver ends it needs.
/// `control_rx` is consumed by the bridge's WS loop; `events_tx` is how
/// the bridge publishes decoded controller events; `rescan_tx` is the
/// post-inclusion auto-rescan trigger that mirrors the manifest's
/// `rescan_nodes` action.
pub fn new_handle(
    rescan_tx: mpsc::Sender<()>,
) -> (
    InclusionHandle,
    mpsc::Receiver<Value>,
    broadcast::Sender<ControllerEvent>,
) {
    let (control_tx, control_rx) = mpsc::channel::<Value>(32);
    let (events_tx, _events_rx) = broadcast::channel::<ControllerEvent>(128);
    let handle = InclusionHandle {
        control_tx,
        events: events_tx.clone(),
        rescan_tx,
    };
    (handle, control_rx, events_tx)
}

/// Best-effort decode of a zwave-js-server event frame into a
/// [`ControllerEvent`]. Returns `None` for frames we don't care about.
///
/// Both controller-scope and node-scope events are handled — the latter
/// drives item lifecycle updates during an inclusion session (interview
/// progress, ready, failed). Node-scope events also keep flowing through
/// `bridge::handle_event` for state publishing; the broadcast here is
/// purely additive.
pub fn decode_controller_event(ev: &crate::types::RawEvent) -> Option<ControllerEvent> {
    // Node-scope events relevant to the inclusion UI. These come through
    // with `node_id` set at the top level of the RawEvent and the source
    // set to "node".
    if let Some(node_id) = ev.node_id {
        match ev.event.as_str() {
            "interview started" => {
                return Some(ControllerEvent::NodeInterviewStarted { node_id });
            }
            "node ready" => {
                let raw = ev
                    .node_state
                    .clone()
                    .unwrap_or_else(|| json!({ "nodeId": node_id }));
                return Some(ControllerEvent::NodeReady { node_id, raw });
            }
            "interview failed" => {
                let message = ev
                    .args
                    .as_ref()
                    .and_then(|a| a.get("errorMessage"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        ev.args
                            .as_ref()
                            .and_then(|a| a.get("message"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    });
                return Some(ControllerEvent::NodeInterviewFailed { node_id, message });
            }
            _ => {}
        }
    }

    // Controller-scope events (inclusion lifecycle).
    let args = ev.args.as_ref();
    match ev.event.as_str() {
        "inclusion started" => Some(ControllerEvent::InclusionStarted),
        "inclusion stopped" => Some(ControllerEvent::InclusionStopped),
        "inclusion failed" => {
            let msg = args
                .and_then(|a| a.get("message"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ControllerEvent::InclusionFailed { message: msg })
        }
        "exclusion started" => Some(ControllerEvent::ExclusionStarted),
        "exclusion stopped" => Some(ControllerEvent::ExclusionStopped),
        "exclusion failed" => {
            let msg = args
                .and_then(|a| a.get("message"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ControllerEvent::ExclusionFailed { message: msg })
        }
        "node added" => {
            let node_id = args
                .and_then(|a| a.get("node"))
                .and_then(|n| n.get("nodeId"))
                .and_then(|n| n.as_u64())
                .or_else(|| args.and_then(|a| a.get("nodeId")).and_then(|n| n.as_u64()))?
                as u32;
            let raw = args
                .and_then(|a| a.get("node"))
                .cloned()
                .unwrap_or_else(|| json!({ "nodeId": node_id }));
            Some(ControllerEvent::NodeAdded { node_id, raw })
        }
        "node removed" => {
            let node_id = args
                .and_then(|a| a.get("node"))
                .and_then(|n| n.get("nodeId"))
                .and_then(|n| n.as_u64())? as u32;
            Some(ControllerEvent::NodeRemoved { node_id })
        }
        "grant security classes" => {
            let req_id = args
                .and_then(|a| a.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let requested = args
                .and_then(|a| a.get("requested"))
                .cloned()
                .unwrap_or(Value::Null);
            Some(ControllerEvent::GrantSecurityClasses {
                request_id: req_id,
                requested,
            })
        }
        "validate dsk and enter pin" => {
            let req_id = args
                .and_then(|a| a.get("requestId"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let dsk = args
                .and_then(|a| a.get("dsk"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(ControllerEvent::ValidateDskAndEnterPin {
                request_id: req_id,
                dsk,
            })
        }
        _ => None,
    }
}

/// Register `include_node` and `exclude_node` streaming actions on the
/// management handle. The handle is cloned into each closure so every
/// invocation gets a fresh `subscribe()` receiver.
pub fn register_actions(mgmt: ManagementHandle, handle: InclusionHandle) -> ManagementHandle {
    let include_handle = handle.clone();
    let exclude_handle = handle;
    mgmt.with_streaming_action(StreamingAction::new("include_node", move |ctx, params| {
        let h = include_handle.clone();
        async move { include_node(ctx, params, h).await }
    }))
    .with_streaming_action(StreamingAction::new("exclude_node", move |ctx, params| {
        let h = exclude_handle.clone();
        async move { exclude_node(ctx, params, h).await }
    }))
}

/// Streaming handler: put the controller into inclusion mode and
/// surface per-node progress. Ends on user respond (done), cancel, or
/// controller-reported failure.
async fn include_node(ctx: StreamContext, _params: Value, handle: InclusionHandle) -> Result<()> {
    let mut events = handle.subscribe();
    let ctx = Arc::new(ctx);
    let nodes_added: Arc<tokio::sync::Mutex<Vec<u32>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    // Tracks nodes whose interview has completed during this inclusion
    // session. Drives the end-of-include name/area prompt loop — only
    // ready nodes can have set_name/set_location pushed back to zwave-js
    // because the controller has finalized the device identity.
    let nodes_ready: Arc<tokio::sync::Mutex<Vec<u32>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    // Kick the controller into inclusion mode. Schema 24+ returns the
    // result in an object; older schemas return a bare boolean. We don't
    // block on the reply — the `inclusion started` event is the real
    // confirmation that listening has begun.
    handle
        .send_command(json!({
            "messageId": format!("hc-inc-{}", Uuid::new_v4()),
            "command": "controller.begin_inclusion",
        }))
        .await?;

    ctx.progress(Some(0), Some("waiting for controller"), None)
        .await?;

    // Advisory prompt — UI shows "Press include on device, click Done
    // when finished". `emit_awaiting_user_with_schema` is the emit-only
    // variant so we can concurrently process controller events while
    // waiting on the respond.
    ctx.emit_awaiting_user_with_schema(
        "Press the include button on each device. Reply when finished.",
        json!({ "done": { "type": "boolean", "default": true } }),
    )
    .await?;

    let respond_fut = ctx.await_respond();
    tokio::pin!(respond_fut);
    let cancel_fut = ctx.wait_canceled();
    tokio::pin!(cancel_fut);

    loop {
        tokio::select! {
            biased;

            // User said "done" → stop inclusion, prompt for name/area on
            // each ready node, then emit complete.
            _resp = &mut respond_fut => {
                let _ = handle
                    .send_command(json!({
                        "messageId": format!("hc-inc-{}", Uuid::new_v4()),
                        "command": "controller.stop_inclusion",
                    }))
                    .await;

                // Per-node configure prompt loop. Only nodes whose
                // interview already finished are eligible — for the others,
                // zwave-js still doesn't know the full device shape, so
                // pushing a name/location now would race against the
                // interview's own writes. They get default registration
                // via the post-rescan + later `node ready` path.
                let ready_ids = nodes_ready.lock().await.clone();
                for node_id in &ready_ids {
                    ctx.progress(
                        None,
                        Some("configuring"),
                        Some(&format!("Configure node {node_id}")),
                    )
                    .await?;
                    ctx.emit_awaiting_user_with_schema(
                        format!(
                            "Configure node {node_id} — set a name and area, or check Skip to leave defaults."
                        ),
                        json!({
                            "name": {
                                "type": "string",
                                "description": "Display name in homeCore (also written to zwave-js)",
                            },
                            "area": {
                                "type": "string",
                                "description": "Area / location slug (also written to zwave-js as `location`)",
                            },
                            "skip": {
                                "type": "boolean",
                                "default": false,
                                "description": "Don't apply name/area; leave defaults",
                            },
                        }),
                    )
                    .await?;
                    let resp = ctx.await_respond().await?;
                    let skip = resp.get("skip").and_then(Value::as_bool).unwrap_or(false);
                    if skip {
                        continue;
                    }
                    let name = resp
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    let area = resp
                        .get("area")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty());

                    if let Some(n) = name {
                        let _ = handle
                            .send_command(json!({
                                "messageId": format!("hc-name-{}", Uuid::new_v4()),
                                "command": "node.set_name",
                                "nodeId": node_id,
                                "name": n,
                            }))
                            .await;
                    }
                    if let Some(a) = area {
                        let _ = handle
                            .send_command(json!({
                                "messageId": format!("hc-loc-{}", Uuid::new_v4()),
                                "command": "node.set_location",
                                "nodeId": node_id,
                                "location": a,
                            }))
                            .await;
                    }
                    let mut item = json!({ "node_id": node_id, "status": "configured" });
                    if let (Some(obj), Some(n)) = (item.as_object_mut(), name) {
                        obj.insert("name".into(), json!(n));
                    }
                    if let (Some(obj), Some(a)) = (item.as_object_mut(), area) {
                        obj.insert("area".into(), json!(a));
                    }
                    ctx.item_update(item).await?;
                }

                let ids = nodes_added.lock().await.clone();
                // Rescan now picks up the new names/locations — publish_node
                // reads node.location and passes it as the area to
                // register_device_full so homeCore reflects the choice.
                handle.request_rescan();
                return ctx.complete(json!({ "nodes_added": ids })).await;
            }

            // Cancel → stop inclusion, emit canceled terminal.
            _ = &mut cancel_fut => {
                let _ = handle
                    .send_command(json!({
                        "messageId": format!("hc-inc-{}", Uuid::new_v4()),
                        "command": "controller.stop_inclusion",
                    }))
                    .await;
                return ctx.canceled().await;
            }

            // Controller event → translate to stream stage.
            ev = events.recv() => {
                match ev {
                    Ok(ControllerEvent::InclusionStarted) => {
                        ctx.progress(Some(10), Some("listening"), Some("Inclusion mode active")).await?;
                    }
                    Ok(ControllerEvent::NodeAdded { node_id, raw }) => {
                        nodes_added.lock().await.push(node_id);
                        ctx.progress(
                            None,
                            Some("included"),
                            Some(&format!("Node {node_id} included; interviewing…")),
                        )
                        .await?;
                        let mut item = json!({
                            "node_id": node_id,
                            "status": "added",
                        });
                        if let Some(obj) = item.as_object_mut() {
                            for key in ["manufacturer", "label", "productType", "productId"] {
                                if let Some(v) = raw.get(key) {
                                    obj.insert(key.into(), v.clone());
                                }
                            }
                        }
                        ctx.item_add(item).await?;
                    }
                    Ok(ControllerEvent::NodeInterviewStarted { node_id }) => {
                        ctx.progress(
                            None,
                            Some("interviewing"),
                            Some(&format!("Interviewing node {node_id}")),
                        )
                        .await?;
                        ctx.item_update(json!({
                            "node_id": node_id,
                            "status": "interviewing",
                        }))
                        .await?;
                    }
                    Ok(ControllerEvent::NodeReady { node_id, raw }) => {
                        nodes_ready.lock().await.push(node_id);
                        let mut item = json!({
                            "node_id": node_id,
                            "status": "ready",
                        });
                        if let Some(obj) = item.as_object_mut() {
                            // Carry forward whatever zwave-js gave us in the
                            // nodeState payload — manufacturer/label/firmware
                            // are typically present once the interview lands.
                            for key in [
                                "manufacturer",
                                "label",
                                "productType",
                                "productId",
                                "firmwareVersion",
                                "name",
                            ] {
                                if let Some(v) = raw.get(key) {
                                    obj.insert(key.into(), v.clone());
                                }
                            }
                        }
                        ctx.item_update(item).await?;
                        ctx.progress(
                            None,
                            Some("interviewed"),
                            Some(&format!("Node {node_id} interview complete")),
                        )
                        .await?;
                    }
                    Ok(ControllerEvent::NodeInterviewFailed { node_id, message }) => {
                        let mut item = json!({
                            "node_id": node_id,
                            "status": "failed",
                        });
                        if let (Some(obj), Some(msg)) = (item.as_object_mut(), message.as_ref()) {
                            obj.insert("error".into(), json!(msg));
                        }
                        ctx.item_update(item).await?;
                        ctx.warning(
                            message.unwrap_or_else(|| {
                                format!("Interview failed for node {node_id}")
                            }),
                            Some(json!({ "node_id": node_id })),
                        )
                        .await?;
                    }
                    Ok(ControllerEvent::InclusionFailed { message }) => {
                        return ctx
                            .error(message.unwrap_or_else(|| "inclusion failed".into()))
                            .await;
                    }
                    Ok(ControllerEvent::InclusionStopped) => {
                        // Either user-driven (we send stop) or controller-
                        // driven abort. If our caller hasn't responded yet,
                        // we still keep the stream open so they can Done
                        // — but signal via progress.
                        ctx.progress(
                            None,
                            Some("stopped"),
                            Some("Inclusion mode exited; reply to finish."),
                        )
                        .await?;
                    }
                    Ok(ControllerEvent::GrantSecurityClasses { request_id, requested }) => {
                        info!(
                            ?requested,
                            "Auto-granting all requested S2 security classes"
                        );
                        ctx.progress(None, Some("s2_bootstrap"), Some("Granting S2 keys")).await?;
                        // Echo the requested classes back as granted.
                        let _ = handle
                            .send_command(json!({
                                "messageId": format!("hc-inc-{}", Uuid::new_v4()),
                                "command": "controller.grant_security_classes",
                                "inclusionGrant": {
                                    "securityClasses":
                                        requested.get("securityClasses").cloned().unwrap_or(json!([])),
                                    "clientSideAuth":
                                        requested.get("clientSideAuth").cloned().unwrap_or(json!(false)),
                                },
                                "requestId": request_id,
                            }))
                            .await;
                    }
                    Ok(ControllerEvent::ValidateDskAndEnterPin { dsk, .. }) => {
                        warn!(
                            %dsk,
                            "S2 device requires DSK PIN entry — not supported in v1; \
                             consider using zwave-js-ui for this device"
                        );
                        ctx.warning(
                            "Device requires DSK PIN entry; inclusion will fail",
                            Some(json!({ "dsk": dsk })),
                        )
                        .await?;
                    }
                    Ok(_) => {
                        // Exclusion events during an inclusion session — ignore.
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        debug!("inclusion event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return ctx.error("controller event stream closed").await;
                    }
                }
            }
        }
    }
}

/// Streaming handler: put the controller into exclusion mode. Emits
/// `item_remove` events per removed node until the user responds
/// (done), cancels, or the controller reports failure.
async fn exclude_node(ctx: StreamContext, _params: Value, handle: InclusionHandle) -> Result<()> {
    let mut events = handle.subscribe();
    let ctx = Arc::new(ctx);
    let nodes_removed: Arc<tokio::sync::Mutex<Vec<u32>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    handle
        .send_command(json!({
            "messageId": format!("hc-exc-{}", Uuid::new_v4()),
            "command": "controller.begin_exclusion",
        }))
        .await?;

    ctx.progress(Some(0), Some("waiting for controller"), None)
        .await?;

    ctx.emit_awaiting_user_with_schema(
        "Press the exclude/reset button on each device. Reply when finished.",
        json!({ "done": { "type": "boolean", "default": true } }),
    )
    .await?;

    let respond_fut = ctx.await_respond();
    tokio::pin!(respond_fut);
    let cancel_fut = ctx.wait_canceled();
    tokio::pin!(cancel_fut);

    loop {
        tokio::select! {
            biased;

            _resp = &mut respond_fut => {
                let _ = handle
                    .send_command(json!({
                        "messageId": format!("hc-exc-{}", Uuid::new_v4()),
                        "command": "controller.stop_exclusion",
                    }))
                    .await;
                let ids = nodes_removed.lock().await.clone();
                return ctx.complete(json!({ "nodes_removed": ids })).await;
            }

            _ = &mut cancel_fut => {
                let _ = handle
                    .send_command(json!({
                        "messageId": format!("hc-exc-{}", Uuid::new_v4()),
                        "command": "controller.stop_exclusion",
                    }))
                    .await;
                return ctx.canceled().await;
            }

            ev = events.recv() => {
                match ev {
                    Ok(ControllerEvent::ExclusionStarted) => {
                        ctx.progress(Some(10), Some("listening"), Some("Exclusion mode active")).await?;
                    }
                    Ok(ControllerEvent::NodeRemoved { node_id }) => {
                        nodes_removed.lock().await.push(node_id);
                        ctx.item_remove(json!({ "node_id": node_id })).await?;
                    }
                    Ok(ControllerEvent::ExclusionFailed { message }) => {
                        return ctx
                            .error(message.unwrap_or_else(|| "exclusion failed".into()))
                            .await;
                    }
                    Ok(ControllerEvent::ExclusionStopped) => {
                        ctx.progress(
                            None,
                            Some("stopped"),
                            Some("Exclusion mode exited; reply to finish."),
                        )
                        .await?;
                    }
                    Ok(_) => {
                        // Inclusion events during an exclusion session — ignore.
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        debug!("exclusion event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return ctx.error("controller event stream closed").await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RawEvent;

    fn raw(source: &str, event: &str, args: Value) -> RawEvent {
        serde_json::from_value(json!({
            "source": source,
            "event": event,
            "args": args,
        }))
        .unwrap()
    }

    #[test]
    fn decodes_inclusion_started() {
        let e = raw("controller", "inclusion started", json!({}));
        assert!(matches!(
            decode_controller_event(&e),
            Some(ControllerEvent::InclusionStarted)
        ));
    }

    #[test]
    fn decodes_node_added_from_nested_node() {
        let e = raw(
            "controller",
            "node added",
            json!({ "node": { "nodeId": 12 } }),
        );
        match decode_controller_event(&e) {
            Some(ControllerEvent::NodeAdded { node_id, .. }) => assert_eq!(node_id, 12),
            other => panic!("expected NodeAdded, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unrelated_node_scope_events() {
        // Node-scope events the inclusion stream doesn't track stay None.
        let e = raw("node", "value updated", json!({}));
        assert!(decode_controller_event(&e).is_none());
    }

    fn raw_with_node_id(source: &str, event: &str, node_id: u32, extras: Value) -> RawEvent {
        let mut v = json!({
            "source": source,
            "event": event,
            "nodeId": node_id,
        });
        if let Some(o) = v.as_object_mut() {
            if let Some(extras_obj) = extras.as_object() {
                for (k, val) in extras_obj {
                    o.insert(k.clone(), val.clone());
                }
            }
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn decodes_node_interview_started() {
        let e = raw_with_node_id("node", "interview started", 14, json!({}));
        match decode_controller_event(&e) {
            Some(ControllerEvent::NodeInterviewStarted { node_id }) => assert_eq!(node_id, 14),
            other => panic!("expected NodeInterviewStarted, got {other:?}"),
        }
    }

    #[test]
    fn decodes_node_ready_carries_node_state() {
        let e = raw_with_node_id(
            "node",
            "node ready",
            14,
            json!({ "nodeState": { "nodeId": 14, "manufacturer": "Aeotec" } }),
        );
        match decode_controller_event(&e) {
            Some(ControllerEvent::NodeReady { node_id, raw }) => {
                assert_eq!(node_id, 14);
                assert_eq!(
                    raw.get("manufacturer").and_then(|v| v.as_str()),
                    Some("Aeotec")
                );
            }
            other => panic!("expected NodeReady, got {other:?}"),
        }
    }

    #[test]
    fn decodes_node_interview_failed_with_message() {
        let e = raw_with_node_id(
            "node",
            "interview failed",
            14,
            json!({ "args": { "errorMessage": "Timeout" } }),
        );
        match decode_controller_event(&e) {
            Some(ControllerEvent::NodeInterviewFailed { node_id, message }) => {
                assert_eq!(node_id, 14);
                assert_eq!(message.as_deref(), Some("Timeout"));
            }
            other => panic!("expected NodeInterviewFailed, got {other:?}"),
        }
    }
}
