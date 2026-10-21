//! Adding and removing devices, and rebuilding routes — the plugin's
//! streaming actions.
//!
//! **Add** (`include_node`): the controller listens for a device in pairing
//! mode (secure inclusion, zwave-js's Default strategy — S2 where the device
//! has it, S0 or none where it does not). A secure device asks which keys it
//! may have; the screen answers once, with the PIN from the device's label
//! when the keys asked for need one ("Security"). zwave-js then asks for that
//! PIN, and gets the one already typed. The interview follows ("Getting to
//! know it"); when the node is ready the screen asks for a name and a room,
//! which go to zwave-js (`node.set_name` / `node.set_location`) and so to
//! homeCore's registration. With `replace_device_id`, the same flow replaces a
//! failed node: the new device takes its node id, so its homeCore device —
//! room, history, automations — carries on.
//!
//! The DSK zwave-js shows is only known *after* the keys are granted (it comes
//! from the key exchange), so the PIN is asked for before it — exactly what
//! the device's label shows — and the DSK is not shown.
//!
//! **Remove** (`exclude_node`): the controller listens for a device's pairing
//! button; the device removed leaves homeCore with it.
//!
//! **Rebuild routes** (`rebuild_routes`): every node finds its best path to
//! the controller again, with each node's progress.
//!
//! zwave-js-server puts a controller event's fields at the top level of the
//! event — `{event: "node added", node, result}`, `{event: "node removed",
//! node, reason}`, `{event: "grant security classes", requested}`,
//! `{event: "validate dsk and enter pin", dsk}` — which is where they are
//! read. (They used to be read from `args`, where they never are: nothing
//! decoded, so exclusion never removed a device and inclusion streamed
//! nothing.)
//!
//! **The PIN is not logged** and goes nowhere but zwave-js.

use crate::bridge::{self, node_device_id, Shared};
use crate::types::{NodeState, RawEvent};
use anyhow::Result;
use plugin_sdk_rs::{ManagementHandle, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tracing::{debug, info};

/// How long the controller listens for a device to add.
pub const INCLUSION_WINDOW: Duration = Duration::from_secs(240);
/// How long it listens for a device to remove.
pub const EXCLUSION_WINDOW: Duration = Duration::from_secs(60);

/// SecurityClass numbers that need the PIN from the device's label.
const NEEDS_PIN: &[i64] = &[1, 2];
/// The classes "basic" security grants: S2 Unauthenticated and S0.
const BASIC: &[i64] = &[0, 7];

/// Decoded controller-scope (and interview) events, for the streaming actions.
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
    NodeAdded {
        node_id: u32,
        node: Value,
        low_security: bool,
    },
    NodeRemoved {
        node_id: u32,
        name: Option<String>,
        reason: Option<u64>,
    },
    InterviewStarted {
        node_id: u32,
    },
    InterviewStage {
        node_id: u32,
        stage: String,
    },
    InterviewFailed {
        node_id: u32,
        message: Option<String>,
    },
    NodeReady {
        node_id: u32,
        node: Value,
    },
    GrantSecurityClasses {
        requested: Value,
    },
    ValidateDskAndEnterPin {
        dsk: String,
    },
    RebuildProgress {
        progress: Value,
    },
    RebuildDone {
        result: Value,
    },
}

fn text(v: Option<&Value>, key: &str) -> Option<String> {
    v?.get(key)?.as_str().map(str::to_string)
}

/// The events the streaming actions follow, from an event frame.
pub fn decode_controller_event(ev: &RawEvent) -> Option<ControllerEvent> {
    if let Some(node_id) = ev.node_id {
        return match ev.event.as_str() {
            "interview started" => Some(ControllerEvent::InterviewStarted { node_id }),
            "interview stage completed" => Some(ControllerEvent::InterviewStage {
                node_id,
                stage: ev
                    .get("stageName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "ready" => Some(ControllerEvent::NodeReady {
                node_id,
                node: ev
                    .node_state
                    .clone()
                    .unwrap_or_else(|| json!({ "nodeId": node_id })),
            }),
            "interview failed" => Some(ControllerEvent::InterviewFailed {
                node_id,
                message: text(ev.args.as_ref(), "errorMessage")
                    .or_else(|| text(ev.args.as_ref(), "message")),
            }),
            _ => None,
        };
    }
    let message = || text(Some(&ev.body), "message").or_else(|| text(ev.args.as_ref(), "message"));
    match ev.event.as_str() {
        "inclusion started" => Some(ControllerEvent::InclusionStarted),
        "inclusion stopped" => Some(ControllerEvent::InclusionStopped),
        "inclusion failed" => Some(ControllerEvent::InclusionFailed { message: message() }),
        "inclusion aborted" => Some(ControllerEvent::InclusionFailed {
            message: Some("The device stopped the secure pairing".into()),
        }),
        "exclusion started" => Some(ControllerEvent::ExclusionStarted),
        "exclusion stopped" => Some(ControllerEvent::ExclusionStopped),
        "exclusion failed" => Some(ControllerEvent::ExclusionFailed { message: message() }),
        "node added" => {
            let node = ev.get("node")?.clone();
            let node_id = node.get("nodeId")?.as_u64()? as u32;
            let low_security = ev
                .get("result")
                .and_then(|r| r.get("lowSecurity"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(ControllerEvent::NodeAdded {
                node_id,
                node,
                low_security,
            })
        }
        "node removed" => {
            let node = ev.get("node")?;
            Some(ControllerEvent::NodeRemoved {
                node_id: node.get("nodeId")?.as_u64()? as u32,
                name: node
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string),
                reason: ev.get("reason").and_then(Value::as_u64),
            })
        }
        "grant security classes" => Some(ControllerEvent::GrantSecurityClasses {
            requested: ev.get("requested").cloned().unwrap_or(Value::Null),
        }),
        "validate dsk and enter pin" => Some(ControllerEvent::ValidateDskAndEnterPin {
            dsk: ev
                .get("dsk")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "rebuild routes progress" | "heal network progress" => {
            Some(ControllerEvent::RebuildProgress {
                progress: ev.get("progress").cloned().unwrap_or(Value::Null),
            })
        }
        "rebuild routes done" | "heal network done" => Some(ControllerEvent::RebuildDone {
            result: ev.get("result").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

/// Register the streaming actions.
pub fn register_actions(mgmt: ManagementHandle, s: Arc<Shared>) -> ManagementHandle {
    let (a, b, c) = (s.clone(), s.clone(), s);
    mgmt.with_streaming_action(StreamingAction::new("include_node", move |ctx, params| {
        let s = a.clone();
        async move { include_node(ctx, params, s).await }
    }))
    .with_streaming_action(StreamingAction::new("exclude_node", move |ctx, params| {
        let s = b.clone();
        async move { exclude_node(ctx, params, s).await }
    }))
    .with_streaming_action(StreamingAction::new(
        "rebuild_routes",
        move |ctx, params| {
            let s = c.clone();
            async move { rebuild_routes(ctx, params, s).await }
        },
    ))
}

/// How far an interview is, by its last completed stage.
fn stage_percent(stage: &str) -> u8 {
    match stage {
        "ProtocolInfo" => 40,
        "NodeInfo" => 55,
        "CommandClasses" => 80,
        "OverwriteConfig" => 92,
        "Complete" => 100,
        _ => 35,
    }
}

/// What it is doing at that stage, in words.
fn stage_words(stage: &str) -> &'static str {
    match stage {
        "ProtocolInfo" => "asking what kind of device it is",
        "NodeInfo" => "learning what it can do",
        "CommandClasses" => "reading its settings",
        "OverwriteConfig" => "applying its device file",
        "Complete" => "done",
        _ => "starting",
    }
}

/// Security class names, as a person reads them.
fn class_name(c: i64) -> &'static str {
    match c {
        2 => "S2 Access Control",
        1 => "S2 Authenticated",
        0 => "S2 Unauthenticated",
        7 => "S0",
        _ => "unknown",
    }
}

/// The classes to grant for an answer: `strongest` grants what was asked,
/// `basic` only S2 Unauthenticated and S0 (no PIN needed), `none` nothing.
pub fn classes_to_grant(requested: &[i64], answer: &str) -> Vec<i64> {
    match answer {
        "none" => Vec::new(),
        "basic" => requested
            .iter()
            .copied()
            .filter(|c| BASIC.contains(c))
            .collect(),
        _ => requested.to_vec(),
    }
}

/// Whether a grant needs the PIN from the device's label.
pub fn needs_pin(classes: &[i64], client_side_auth: bool) -> bool {
    !client_side_auth && classes.iter().any(|c| NEEDS_PIN.contains(c))
}

/// A PIN: the first five digits of the device's DSK.
pub fn valid_pin(pin: &str) -> bool {
    pin.len() == 5 && pin.chars().all(|c| c.is_ascii_digit())
}

/// Wait for a `respond`, or a cancel. `None` on cancel.
async fn answer_or_cancel(ctx: &StreamContext) -> Option<Value> {
    tokio::select! {
        r = ctx.await_respond() => r.ok(),
        _ = ctx.wait_canceled() => None,
    }
}

async fn stop_inclusion(s: &Shared) {
    let _ = s
        .client
        .request(
            json!({ "command": "controller.stop_inclusion" }),
            Duration::from_secs(10),
        )
        .await;
}

/// Streaming handler: add a device (or replace a failed one).
async fn include_node(ctx: StreamContext, params: Value, s: Arc<Shared>) -> Result<()> {
    let replace = params
        .get("replace_device_id")
        .and_then(Value::as_str)
        .and_then(bridge::node_of_device);
    let mut events = s.events.subscribe();
    let start = match replace {
        Some(node_id) => json!({
            "command": "controller.replace_failed_node",
            "nodeId": node_id,
            "options": { "strategy": 0 },
        }),
        None => json!({
            "command": "controller.begin_inclusion",
            "options": { "strategy": 0 },
        }),
    };
    match s.client.request(start, Duration::from_secs(30)).await {
        Ok(r) if r.get("success").and_then(Value::as_bool) == Some(false) => {
            return ctx
                .error("The controller would not start listening — is it already adding or removing a device?")
                .await;
        }
        Ok(_) => {}
        Err(e) => return ctx.error(e.message).await,
    }
    ctx.progress(
        Some(5),
        Some("pairing"),
        Some(&format!(
            "Listening for a new device… {} left",
            clock(INCLUSION_WINDOW)
        )),
    )
    .await?;

    let deadline = Instant::now() + INCLUSION_WINDOW;
    let mut added: Option<u32> = None;
    let mut pin: Option<String> = None;
    let mut model: Option<String> = None;
    let mut security: Option<String> = None;

    loop {
        tokio::select! {
            biased;
            _ = ctx.wait_canceled() => {
                stop_inclusion(&s).await;
                return ctx.canceled().await;
            }
            _ = tokio::time::sleep_until(deadline), if added.is_none() => {
                stop_inclusion(&s).await;
                return ctx.error("No device was found in 4 minutes. Put it in pairing mode near the controller and try again.").await;
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return ctx.error("The plugin stopped listening to the controller").await;
                    }
                };
                match ev {
                    ControllerEvent::InclusionStarted => {
                        ctx.progress(Some(10), Some("pairing"), Some("Listening for a new device")).await?;
                    }
                    ControllerEvent::GrantSecurityClasses { requested } => {
                        let classes: Vec<i64> = requested
                            .get("securityClasses")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_i64).collect())
                            .unwrap_or_default();
                        let csa = requested
                            .get("clientSideAuth")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let pin_needed = needs_pin(&classes, csa);
                        ctx.emit_awaiting_user_with_schema(
                            if pin_needed {
                                "Found a secure device. Type the first five digits of the PIN on its label."
                            } else {
                                "Found a secure device."
                            },
                            json!({
                                "grant": { "type": "string", "enum": ["strongest", "basic", "none"], "default": "strongest" },
                                "pin": { "type": "string", "pattern": "^[0-9]{5}$" },
                                "x-zwave": {
                                    "step": "security",
                                    "needs_pin": pin_needed,
                                    "requested": classes.iter().map(|c| class_name(*c)).collect::<Vec<_>>(),
                                },
                            }),
                        )
                        .await?;
                        let Some(resp) = answer_or_cancel(&ctx).await else {
                            stop_inclusion(&s).await;
                            return ctx.canceled().await;
                        };
                        let answer = resp.get("grant").and_then(Value::as_str).unwrap_or("strongest");
                        let grant = classes_to_grant(&classes, answer);
                        if needs_pin(&grant, csa) {
                            match resp.get("pin").and_then(Value::as_str).map(str::trim) {
                                Some(p) if valid_pin(p) => pin = Some(p.to_string()),
                                _ => {
                                    stop_inclusion(&s).await;
                                    return ctx.error("The PIN is the five digits printed first on the device's label").await;
                                }
                            }
                        }
                        security = grant.iter().map(|c| class_name(*c)).next().map(str::to_string);
                        info!(granted = ?grant, "Granting security classes");
                        if let Err(e) = s
                            .client
                            .request(
                                json!({
                                    "command": "controller.grant_security_classes",
                                    "inclusionGrant": { "securityClasses": grant, "clientSideAuth": csa },
                                }),
                                Duration::from_secs(10),
                            )
                            .await
                        {
                            return ctx.error(format!("The controller did not take the answer: {e}")).await;
                        }
                        ctx.progress(Some(20), Some("security"), Some("Exchanging keys with the device")).await?;
                    }
                    ControllerEvent::ValidateDskAndEnterPin { dsk } => {
                        // Asked for before the keys were granted (see the
                        // module's notes); asked again only if it wasn't.
                        let entered = match pin.take() {
                            Some(p) => p,
                            None => {
                                ctx.emit_awaiting_user_with_schema(
                                    "Type the first five digits of the PIN on the device's label.",
                                    json!({
                                        "pin": { "type": "string", "pattern": "^[0-9]{5}$" },
                                        "x-zwave": { "step": "pin", "dsk": dsk },
                                    }),
                                )
                                .await?;
                                let Some(resp) = answer_or_cancel(&ctx).await else {
                                    stop_inclusion(&s).await;
                                    return ctx.canceled().await;
                                };
                                resp.get("pin").and_then(Value::as_str).unwrap_or_default().trim().to_string()
                            }
                        };
                        if let Err(e) = s
                            .client
                            .request(
                                json!({ "command": "controller.validate_dsk_and_enter_pin", "pin": entered }),
                                Duration::from_secs(10),
                            )
                            .await
                        {
                            return ctx.error(format!("The controller did not take the PIN: {e}")).await;
                        }
                    }
                    ControllerEvent::NodeAdded { node_id, node, low_security } => {
                        if added.is_some() {
                            continue;
                        }
                        added = Some(node_id);
                        let n = NodeState::from_value(&node).unwrap_or_default();
                        model = n.model_name();
                        ctx.item_add(json!({
                            "node_id": node_id,
                            "device_id": node_device_id(node_id),
                            "status": "added",
                            "model": model,
                            "low_security": low_security,
                        }))
                        .await?;
                        if low_security {
                            ctx.warning(
                                "It was added without the security it asked for — the PIN may not have matched. It works, but its messages are not encrypted.",
                                Some(json!({ "node_id": node_id })),
                            )
                            .await?;
                        }
                        ctx.progress(Some(30), Some("interview"), Some(&interview_line(node_id, model.as_deref(), security.as_deref(), "starting"))).await?;
                    }
                    ControllerEvent::InterviewStarted { node_id } if Some(node_id) == added => {
                        ctx.progress(Some(35), Some("interview"), Some(&interview_line(node_id, model.as_deref(), security.as_deref(), "starting"))).await?;
                    }
                    ControllerEvent::InterviewStage { node_id, stage } if Some(node_id) == added => {
                        // What it is becomes known as the interview goes.
                        if model.is_none() {
                            if let Ok(r) = s
                                .client
                                .request(json!({ "command": "node.get_state", "nodeId": node_id }), Duration::from_secs(5))
                                .await
                            {
                                model = r.get("state").and_then(NodeState::from_value).and_then(|n| {
                                    match (n.manufacturer_name(), n.model_name()) {
                                        (Some(m), Some(l)) => Some(format!("{m} {l}")),
                                        (_, l) => l,
                                    }
                                });
                            }
                        }
                        ctx.progress(
                            Some(stage_percent(&stage)),
                            Some("interview"),
                            Some(&interview_line(node_id, model.as_deref(), security.as_deref(), stage_words(&stage))),
                        )
                        .await?;
                    }
                    ControllerEvent::InterviewFailed { node_id, message } if Some(node_id) == added => {
                        ctx.warning(
                            message.unwrap_or_else(|| "Its interview did not finish; Z-Wave JS will try again".into()),
                            Some(json!({ "node_id": node_id })),
                        )
                        .await?;
                    }
                    ControllerEvent::NodeReady { node_id, node } if Some(node_id) == added => {
                        let n = NodeState::from_value(&node).unwrap_or_default();
                        let suggested = n.name.clone().filter(|x| !x.is_empty()).or_else(|| n.model_name()).or(model.clone());
                        ctx.progress(Some(100), Some("interview"), Some(&interview_line(node_id, n.model_name().as_deref(), security.as_deref(), "done"))).await?;
                        return name_step(&ctx, &s, node_id, suggested, n.model_name()).await;
                    }
                    ControllerEvent::InclusionFailed { message } => {
                        return ctx.error(message.unwrap_or_else(|| "Adding the device failed".into())).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn interview_line(
    node_id: u32,
    model: Option<&str>,
    security: Option<&str>,
    doing: &str,
) -> String {
    let mut parts = vec![format!("Node {node_id}")];
    if let Some(m) = model {
        parts.push(m.to_string());
    }
    if let Some(sec) = security {
        parts.push(sec.to_string());
    }
    parts.push(doing.to_string());
    parts.join(" · ")
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The last step: a name and a room, written to zwave-js and registered.
async fn name_step(
    ctx: &StreamContext,
    s: &Shared,
    node_id: u32,
    suggested: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let device_id = node_device_id(node_id);
    ctx.emit_awaiting_user_with_schema(
        "Name it and put it in a room.",
        json!({
            "name": { "type": "string", "default": suggested },
            "room": { "type": "string" },
            "x-zwave": { "step": "name", "node_id": node_id, "device_id": device_id, "model": model },
        }),
    )
    .await?;
    let Some(resp) = answer_or_cancel(ctx).await else {
        // Closing at this step leaves the device as it was added.
        return ctx
            .complete(json!({ "nodes_added": [node_id], "device_id": device_id }))
            .await;
    };
    let name = resp
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let room = resp
        .get("room")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if let Some(n) = name {
        if let Err(e) = s
            .client
            .request(
                json!({ "command": "node.set_name", "nodeId": node_id, "name": n }),
                Duration::from_secs(20),
            )
            .await
        {
            ctx.warning(format!("The name did not reach Z-Wave JS: {e}"), None)
                .await?;
        }
    }
    if let Some(r) = room {
        if let Err(e) = s
            .client
            .request(
                json!({ "command": "node.set_location", "nodeId": node_id, "location": r }),
                Duration::from_secs(20),
            )
            .await
        {
            ctx.warning(format!("The room did not reach Z-Wave JS: {e}"), None)
                .await?;
        }
    }
    if let Err(e) = bridge::refresh_node(s, node_id).await {
        debug!(node_id, error = %e, "Could not read the new node back");
    }
    ctx.complete(json!({
        "nodes_added": [node_id],
        "device_id": device_id,
        "name": name,
        "room": room,
    }))
    .await
}

/// Streaming handler: remove a device by its pairing button.
async fn exclude_node(ctx: StreamContext, _params: Value, s: Arc<Shared>) -> Result<()> {
    let mut events = s.events.subscribe();
    match s
        .client
        .request(
            json!({ "command": "controller.begin_exclusion" }),
            Duration::from_secs(30),
        )
        .await
    {
        Ok(r) if r.get("success").and_then(Value::as_bool) == Some(false) => {
            return ctx
                .error("The controller would not start listening — is it already adding or removing a device?")
                .await;
        }
        Ok(_) => {}
        Err(e) => return ctx.error(e.message).await,
    }
    let stop = |s: Arc<Shared>| async move {
        let _ = s
            .client
            .request(
                json!({ "command": "controller.stop_exclusion" }),
                Duration::from_secs(10),
            )
            .await;
    };
    ctx.progress(
        Some(5),
        Some("waiting"),
        Some(&format!("Waiting… {} left", clock(EXCLUSION_WINDOW))),
    )
    .await?;
    let deadline = Instant::now() + EXCLUSION_WINDOW;
    loop {
        tokio::select! {
            biased;
            _ = ctx.wait_canceled() => {
                stop(s.clone()).await;
                return ctx.canceled().await;
            }
            _ = tokio::time::sleep_until(deadline) => {
                stop(s.clone()).await;
                return ctx.error("Nothing was removed in a minute. Press the device's pairing button while this is waiting.").await;
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return ctx.error("The plugin stopped listening to the controller").await,
                };
                match ev {
                    ControllerEvent::NodeRemoved { node_id, name, reason } => {
                        let device_id = node_device_id(node_id);
                        ctx.item_remove(json!({ "node_id": node_id, "device_id": device_id, "name": name, "reason": reason })).await?;
                        return ctx
                            .complete(json!({ "nodes_removed": [node_id], "device_id": device_id, "name": name }))
                            .await;
                    }
                    ControllerEvent::ExclusionFailed { message } => {
                        return ctx.error(message.unwrap_or_else(|| "Removing the device failed".into())).await;
                    }
                    ControllerEvent::ExclusionStarted => {
                        ctx.progress(Some(10), Some("waiting"), Some("Waiting for the device's pairing button")).await?;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Streaming handler: rebuild every node's routes.
async fn rebuild_routes(ctx: StreamContext, _params: Value, s: Arc<Shared>) -> Result<()> {
    let mut events = s.events.subscribe();
    match s
        .client
        .request(
            json!({ "command": "controller.begin_rebuilding_routes", "options": { "includeSleeping": true } }),
            Duration::from_secs(30),
        )
        .await
    {
        Ok(r) if r.get("success").and_then(Value::as_bool) == Some(false) => {
            return ctx.error("The controller would not start — is it already rebuilding?").await;
        }
        Ok(_) => {}
        Err(e) => return ctx.error(e.message).await,
    }
    {
        let mut net = s.net();
        net.rebuilding = true;
        net.rebuild_progress.clear();
    }
    ctx.progress(Some(0), Some("rebuilding"), Some("Starting"))
        .await?;
    loop {
        tokio::select! {
            biased;
            _ = ctx.wait_canceled() => {
                let _ = s.client.request(json!({ "command": "controller.stop_rebuilding_routes" }), Duration::from_secs(10)).await;
                s.net().rebuilding = false;
                return ctx.canceled().await;
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return ctx.error("The plugin stopped listening to the controller").await,
                };
                match ev {
                    ControllerEvent::RebuildProgress { progress } => {
                        let (done, total) = tally(&progress);
                        let pct = (done * 100).checked_div(total).unwrap_or(0) as u8;
                        ctx.progress(Some(pct), Some("rebuilding"), Some(&format!("{done} of {total} devices done"))).await?;
                    }
                    ControllerEvent::RebuildDone { result } => {
                        let counts = |state: &str| result.as_object().map_or(0, |m| m.values().filter(|v| v.as_str() == Some(state)).count());
                        return ctx
                            .complete(json!({
                                "done": counts("done"),
                                "failed": counts("failed"),
                                "skipped": counts("skipped"),
                                "result": result,
                            }))
                            .await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// `(finished, total)` from a rebuild's progress map.
fn tally(progress: &Value) -> (usize, usize) {
    let Some(m) = progress.as_object() else {
        return (0, 0);
    };
    let done = m
        .values()
        .filter(|v| v.as_str().is_some_and(|s| s != "pending"))
        .count();
    (done, m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(v: Value) -> RawEvent {
        RawEvent::from_value(v).unwrap()
    }

    /// H2: zwave-js-server puts a controller event's fields at the top level.
    #[test]
    fn controller_events_are_read_from_the_top_level() {
        match decode_controller_event(&ev(json!({
            "source": "controller", "event": "node added",
            "node": { "nodeId": 48, "label": "BE469" }, "result": { "lowSecurity": false }
        }))) {
            Some(ControllerEvent::NodeAdded {
                node_id,
                low_security,
                ..
            }) => {
                assert_eq!(node_id, 48);
                assert!(!low_security);
            }
            other => panic!("expected NodeAdded, got {other:?}"),
        }
        match decode_controller_event(&ev(json!({
            "source": "controller", "event": "node removed",
            "node": { "nodeId": 9, "name": "Outlet - MBR Door" }, "reason": 0
        }))) {
            Some(ControllerEvent::NodeRemoved {
                node_id,
                name,
                reason,
            }) => {
                assert_eq!(node_id, 9);
                assert_eq!(name.as_deref(), Some("Outlet - MBR Door"));
                assert_eq!(reason, Some(0));
            }
            other => panic!("expected NodeRemoved, got {other:?}"),
        }
        match decode_controller_event(&ev(json!({
            "source": "controller", "event": "grant security classes",
            "requested": { "securityClasses": [2, 0], "clientSideAuth": false }
        }))) {
            Some(ControllerEvent::GrantSecurityClasses { requested }) => {
                assert_eq!(requested["securityClasses"], json!([2, 0]));
            }
            other => panic!("expected GrantSecurityClasses, got {other:?}"),
        }
        match decode_controller_event(&ev(json!({
            "source": "controller", "event": "validate dsk and enter pin", "dsk": "-11111-22222-33333"
        }))) {
            Some(ControllerEvent::ValidateDskAndEnterPin { dsk }) => {
                assert_eq!(dsk, "-11111-22222-33333")
            }
            other => panic!("expected ValidateDskAndEnterPin, got {other:?}"),
        }
        // The old shape, with everything under `args`, is not a node added.
        assert!(decode_controller_event(&ev(json!({
            "source": "controller", "event": "node added", "args": { "node": { "nodeId": 3 } }
        })))
        .is_none());
    }

    /// H1: the interview's end is `ready`.
    #[test]
    fn the_end_of_an_interview_is_ready() {
        match decode_controller_event(&ev(json!({
            "source": "node", "event": "ready", "nodeId": 14,
            "nodeState": { "nodeId": 14, "label": "ZSE18" }
        }))) {
            Some(ControllerEvent::NodeReady { node_id, node }) => {
                assert_eq!(node_id, 14);
                assert_eq!(node["label"], "ZSE18");
            }
            other => panic!("expected NodeReady, got {other:?}"),
        }
        assert!(decode_controller_event(&ev(json!({
            "source": "node", "event": "node ready", "nodeId": 14
        })))
        .is_none());
        match decode_controller_event(&ev(json!({
            "source": "node", "event": "interview stage completed", "nodeId": 14, "stageName": "NodeInfo"
        }))) {
            Some(ControllerEvent::InterviewStage { stage, .. }) => assert_eq!(stage, "NodeInfo"),
            other => panic!("expected InterviewStage, got {other:?}"),
        }
        match decode_controller_event(&ev(json!({
            "source": "node", "event": "interview failed", "nodeId": 14,
            "args": { "errorMessage": "Timeout" }
        }))) {
            Some(ControllerEvent::InterviewFailed { message, .. }) => {
                assert_eq!(message.as_deref(), Some("Timeout"))
            }
            other => panic!("expected InterviewFailed, got {other:?}"),
        }
    }

    #[test]
    fn grants() {
        assert_eq!(
            classes_to_grant(&[2, 1, 0, 7], "strongest"),
            vec![2, 1, 0, 7]
        );
        assert_eq!(classes_to_grant(&[2, 1, 0, 7], "basic"), vec![0, 7]);
        assert!(classes_to_grant(&[2, 0], "none").is_empty());
        assert!(needs_pin(&[2, 0], false));
        assert!(needs_pin(&[1], false));
        assert!(!needs_pin(&[0, 7], false));
        assert!(!needs_pin(&[2], true));
        assert!(valid_pin("12345"));
        assert!(!valid_pin("1234"));
        assert!(!valid_pin("12a45"));
    }

    #[test]
    fn rebuild_progress_tallies() {
        assert_eq!(
            tally(&json!({ "2": "done", "3": "pending", "4": "failed", "5": "skipped" })),
            (3, 4)
        );
        assert_eq!(tally(&Value::Null), (0, 0));
        assert_eq!(stage_percent("Complete"), 100);
        assert_eq!(clock(Duration::from_secs(222)), "3:42");
    }
}
