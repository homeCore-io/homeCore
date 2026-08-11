//! Bridge between zwave-js-server WebSocket and HomeCore MQTT.
//!
//! # Flow
//!
//! 1. Connect WebSocket → handshake (set_api_schema → start_listening)
//! 2. Publish full state for all nodes from the start_listening result
//! 3. Subscribe to commands for each device via DevicePublisher
//! 4. Event loop:
//!    - WS `value updated`       → translate → partial state via DevicePublisher
//!    - WS `node status changed` → availability via DevicePublisher
//!    - WS `node name updated`   → partial state `{"name": "..."}`
//!    - WS `node ready`          → republish full node state
//!    - SDK cmd channel          → `node.set_value` WebSocket command
//! 5. Reconnect on WS disconnect with exponential back-off

use crate::config::Config;
use crate::inclusion::{decode_controller_event, ControllerEvent};
use crate::translator::{property_key_str, synthetic_attr_name, Translator};
use crate::types::{NodeState, NodeValue, ResultMsg, ServerMsg, ValueUpdatedArgs};
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Node state → MQTT
// ---------------------------------------------------------------------------

fn node_device_id(node_id: u32) -> String {
    format!("zwave_{node_id}")
}

/// Build a full state map from a NodeState, translating each value via the alias table.
fn build_state(node: &NodeState, translator: &Translator) -> Value {
    let mut map = serde_json::Map::new();

    // Include name/location if present
    if let Some(name) = &node.name {
        if !name.is_empty() {
            map.insert("name".into(), Value::String(name.clone()));
        }
    }
    if let Some(loc) = &node.location {
        if !loc.is_empty() {
            map.insert("location".into(), Value::String(loc.clone()));
        }
    }

    for v in &node.values {
        translate_node_value(v, translator, &mut map);
    }

    Value::Object(map)
}

fn translate_node_value(
    v: &NodeValue,
    translator: &Translator,
    map: &mut serde_json::Map<String, Value>,
) {
    let raw = match &v.value {
        Some(val) if !val.is_null() => val,
        _ => return,
    };
    let pk_str = v.property_key.as_ref().and_then(property_key_str);
    let pk = pk_str.as_deref();

    if let Some((attr, val)) =
        translator.translate(v.command_class, v.endpoint, &v.property, pk, raw)
    {
        map.insert(attr, val);
    } else {
        // Generic fallback so every device value is visible even when the
        // alias table doesn't know the canonical name. Identical naming
        // logic in `handle_event` so the initial state and subsequent
        // value-updated events agree on the attribute name.
        let attr = synthetic_attr_name(v.command_class, v.endpoint, &v.property, pk);
        map.insert(attr, raw.clone());
    }
}

async fn publish_node(
    publisher: &DevicePublisher,
    node: &NodeState,
    translator: &Translator,
) -> Result<()> {
    let device_id = node_device_id(node.node_id);
    let display_name = node
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&device_id)
        .to_string();
    // Treat zwave-js's per-node `location` as the homeCore area. The
    // include flow's name+area prompt writes both via `node.set_name` /
    // `node.set_location` to zwave-js, then triggers a rescan that lands
    // here.
    let area = node.location.as_deref().filter(|s| !s.is_empty());

    // What the node actually is, once zwave-js has interviewed it. Absent
    // before then, and absent means "not said" rather than "unknown" — core
    // keeps whatever an earlier registration established, so the bare
    // registration of a freshly-included node is not a step backwards.
    let mut hardware = plugin_sdk_rs::DeviceHardware::new();
    if let Some(v) = node.manufacturer.as_deref().filter(|s| !s.is_empty()) {
        hardware = hardware.manufacturer(v);
    }
    if let Some(v) = node.label.as_deref().filter(|s| !s.is_empty()) {
        hardware = hardware.model(v);
    }
    if let Some(v) = node.firmware_version.as_deref().filter(|s| !s.is_empty()) {
        hardware = hardware.sw_version(v);
    }

    publisher
        .register_device_detailed(
            &device_id,
            &display_name,
            Some("zwave"),
            area,
            None,
            Some(&hardware),
        )
        .await?;

    // Diagnostic: log all CC 98 (Door Lock) value IDs so we can see exactly what the
    // device reports and verify the write target (targetMode) exists on this node.
    let cc98_values: Vec<String> = node
        .values
        .iter()
        .filter(|v| v.command_class == 98)
        .map(|v| {
            let pk = v
                .property_key
                .as_ref()
                .map(|pk| format!("[{pk}]"))
                .unwrap_or_default();
            let val = v
                .value
                .as_ref()
                .map(|val| format!("={val}"))
                .unwrap_or_default();
            format!("{}{}{}", v.property, pk, val)
        })
        .collect();
    if !cc98_values.is_empty() {
        info!(node_id = node.node_id, values = ?cc98_values, "Door Lock CC 98 value IDs on this node");
    }

    let state = build_state(node, translator);
    // Retained, so a client connecting later knows what this node's attributes
    // mean and which of them can actually be written.
    if let Some(schema) = crate::schema::schema_json(&state, translator) {
        publisher
            .register_device_schema_json(&device_id, &schema)
            .await?;
    }
    // Partial-merge, NOT full replace. zwave-js's `start_listening` /
    // `node ready` snapshots can be transiently sparse — particularly
    // for a freshly-included node whose interview hasn't yet populated
    // every CC's values, or after a brief reconnect during an active
    // interview. A full replace wipes any attribute not in the current
    // snapshot, so the device's homeCore record flickers back to
    // "empty" and the auto-create path then loses device_type. Partial
    // merge preserves prior attributes; missing values just don't get
    // updated this round, and the next `value updated` event refreshes
    // them. Trade-off: a CC removed in firmware would leave a stale
    // attribute in homeCore until manually cleared. Acceptable.
    publisher.publish_state_partial(&device_id, &state).await?;
    publisher
        .publish_availability(&device_id, node.is_available())
        .await?;
    publisher.subscribe_commands(&device_id).await?;
    debug!(
        node_id = node.node_id,
        device_id, "Published full node state"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket handshake
// ---------------------------------------------------------------------------

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn ws_send(tx: &mut WsSink, msg: &Value) -> Result<()> {
    let text = serde_json::to_string(msg)?;
    tx.send(Message::Text(text.into()))
        .await
        .context("ws send")?;
    Ok(())
}

async fn ws_recv_result(rx: &mut WsStream, expected_id: &str) -> Result<ResultMsg> {
    loop {
        match rx.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg: ServerMsg = serde_json::from_str(&text)
                    .with_context(|| format!("parse WS message: {text}"))?;
                if let ServerMsg::Result(r) = msg {
                    if r.message_id == expected_id {
                        if !r.success {
                            bail!("zwave-js command {expected_id} failed: {:?}", r.error_code);
                        }
                        return Ok(r);
                    }
                }
            }
            Some(Ok(Message::Ping(d))) => {
                // tungstenite auto-responds to pings, but we log it
                debug!("WS ping: {} bytes", d.len());
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => bail!("ws recv error: {e}"),
            None => bail!("WS stream ended during handshake"),
        }
    }
}

/// Perform the three-step handshake and return the initial node list.
async fn handshake(
    tx: &mut WsSink,
    rx: &mut WsStream,
    schema_version: u32,
) -> Result<Vec<NodeState>> {
    // Step 1: receive version announcement
    let version_msg = loop {
        match rx.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg: ServerMsg = serde_json::from_str(&text)
                    .with_context(|| format!("parse version msg: {text}"))?;
                if let ServerMsg::Version(v) = msg {
                    break v;
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => bail!("ws error awaiting version: {e}"),
            None => bail!("WS closed before version message"),
        }
    };

    info!(
        server_version = %version_msg.server_version,
        driver_version = %version_msg.driver_version,
        min_schema = version_msg.min_schema_version,
        max_schema = version_msg.max_schema_version,
        "Connected to zwave-js-server"
    );

    let negotiated = schema_version.clamp(
        version_msg.min_schema_version,
        version_msg.max_schema_version,
    );

    // Step 2: set API schema version
    let init_id = "hc-zwave-init";
    ws_send(
        tx,
        &json!({
            "messageId": init_id,
            "command": "set_api_schema",
            "schemaVersion": negotiated,
        }),
    )
    .await?;
    ws_recv_result(rx, init_id).await?;
    info!(schema_version = negotiated, "Schema negotiated");

    // Step 3: start listening — returns full Z-Wave state
    let listen_id = "hc-zwave-listen";
    ws_send(
        tx,
        &json!({ "messageId": listen_id, "command": "start_listening" }),
    )
    .await?;
    let result = ws_recv_result(rx, listen_id).await?;

    let nodes = result
        .result
        .as_ref()
        .and_then(|r| r.get("state"))
        .and_then(|s| s.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(NodeState::from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    info!(node_count = nodes.len(), "Received initial Z-Wave state");
    Ok(nodes)
}

// ---------------------------------------------------------------------------
// Startup value refresh
// ---------------------------------------------------------------------------

/// Whether to ask zwave-js to refresh primary-state values for this node on
/// startup. We poll mains-powered nodes (`isListening=true`) and FLiRS nodes
/// (`isFrequentListening` set to a wake-interval string — door locks are
/// typical). Sleeping battery devices are skipped: they can't answer until
/// they wake on their own schedule, and zwave-js will queue the request and
/// flood the air when wake-up arrives.
fn should_poll_initial_values(node: &NodeState) -> bool {
    if node.is_listening.unwrap_or(false) {
        return true;
    }
    matches!(
        node.is_frequent_listening.as_ref(),
        Some(v) if !v.is_null() && !matches!(v, Value::Bool(false))
    )
}

/// One value to refresh on a node: `(commandClass, endpoint, property)`.
///
/// Endpoint is part of the identity rather than an afterthought — a dual-relay
/// plug exposes the same command class and property on endpoints 1 and 2, and
/// polling only one leaves the other stale.
type PollTarget = (u32, u32, String);

/// One node's worth of work: the node, and every value to refresh on it.
type NodePolls = (u32, Vec<PollTarget>);

/// Primary-state value IDs we ask zwave-js to refresh on startup. zwave-js
/// caches each value's last seen state, but devices that only emit meter or
/// sensor reports — and actuators that don't auto-report on local actuation —
/// can leave their primary state (on / level / setpoint / mode / locked /
/// barrier-state / color) stale across plugin restarts. Our snapshot then
/// publishes those stale values over the live state in homeCore. Polling
/// triggers a real Get to the device; the reply arrives as a `value updated`
/// event and corrects everything downstream.
///
/// Endpoint is intentionally not constrained — multi-endpoint devices
/// (e.g., a dual-relay smart plug exposing endpoints 1 and 2) need every
/// endpoint refreshed.
fn primary_state_values_to_poll(node: &NodeState) -> Vec<PollTarget> {
    // (commandClass, property) pairs whose primary state we refresh. Add
    // new actuator command classes here as they appear on the network.
    const TARGETS: &[(u32, &str)] = &[
        (37, "currentValue"),  // Binary Switch — on/off
        (38, "currentValue"),  // Multilevel Switch — level (dimmer / shade / fan)
        (64, "mode"),          // Thermostat Mode
        (66, "state"),         // Thermostat Operating State
        (67, "setpoint"),      // Thermostat Setpoint (per-type via propertyKey)
        (68, "mode"),          // Thermostat Fan Mode
        (98, "currentMode"),   // Door Lock
        (102, "currentState"), // Barrier Operator
        (117, "currentColor"), // Color Switch
    ];
    let mut out = Vec::new();
    for v in &node.values {
        for (cc, prop) in TARGETS {
            if v.command_class == *cc && v.property == *prop {
                out.push((*cc, v.endpoint, (*prop).to_string()));
                break;
            }
        }
    }
    out
}

async fn send_poll_value(
    ws_tx: &mut WsSink,
    node_id: u32,
    cc: u32,
    endpoint: u32,
    property: &str,
) -> Result<()> {
    let msg_id = format!("hc-poll-{}", Uuid::new_v4());
    let msg = json!({
        "messageId": msg_id,
        "command": "node.poll_value",
        "nodeId": node_id,
        "valueId": {
            "commandClass": cc,
            "endpoint": endpoint,
            "property": property,
        },
    });
    ws_send(ws_tx, &msg).await
}

/// Inter-poll throttle. Each `node.poll_value` triggers a Get → device →
/// response round-trip on the Z-Wave radio; firing dozens back-to-back
/// causes routing congestion on the controller and starves real traffic.
/// 200ms is comfortable for a 700-series controller — ~5 polls/sec, so a
/// 100-poll startup completes in ~20s of background chatter.
const POLL_DELAY: Duration = Duration::from_millis(200);

/// Outcome of the planning step in [`refresh_primary_state`]. Splitting the
/// pure categorization off from the async send loop keeps the counter logic
/// directly testable without WS plumbing.
#[derive(Debug, Default, PartialEq, Eq)]
struct PollPlan {
    /// Per-node targets to poll, in the order encountered.
    /// `(node_id, [(commandClass, endpoint, property), ...])`
    polls: Vec<NodePolls>,
    /// Sleeping battery devices we won't bother — zwave-js would just queue
    /// the request and flood the air at wake-up.
    skipped_battery: usize,
    /// Mains/FLiRS nodes that expose no command class we currently refresh
    /// (controller, repeater, sensor-only device). Tracked separately so
    /// coverage gaps are visible in the log.
    eligible_no_targets: usize,
}

/// Pure categorization step: walks the node list, decides which to poll, and
/// records the bookkeeping counters. Side-effect free.
fn plan_primary_state_refresh(nodes: &[NodeState]) -> PollPlan {
    let mut plan = PollPlan::default();
    for node in nodes {
        if !should_poll_initial_values(node) {
            plan.skipped_battery += 1;
            continue;
        }
        let targets = primary_state_values_to_poll(node);
        if targets.is_empty() {
            plan.eligible_no_targets += 1;
            continue;
        }
        plan.polls.push((node.node_id, targets));
    }
    plan
}

/// Best-effort refresh of primary-state values across all polled nodes.
/// Failures log a warning and continue — a single unreachable node should
/// not block startup.
async fn refresh_primary_state(ws_tx: &mut WsSink, nodes: &[NodeState]) {
    let plan = plan_primary_state_refresh(nodes);
    let mut polled_values = 0usize;
    for (node_id, targets) in &plan.polls {
        for (cc, ep, prop) in targets {
            polled_values += 1;
            if let Err(e) = send_poll_value(ws_tx, *node_id, *cc, *ep, prop).await {
                warn!(
                    node_id = *node_id,
                    cc, prop = %prop, error = %e,
                    "Failed to send node.poll_value"
                );
            }
            sleep(POLL_DELAY).await;
        }
    }
    info!(
        polled_nodes = plan.polls.len(),
        polled_values,
        skipped_battery = plan.skipped_battery,
        eligible_no_targets = plan.eligible_no_targets,
        "Refreshed primary-state values"
    );
}

// ---------------------------------------------------------------------------
// Main bridge loop
// ---------------------------------------------------------------------------

/// Messages sent from the command channel to the WS task to trigger `node.set_value`.
#[derive(Debug)]
struct SetValueCmd {
    node_id: u32,
    command_class: u32,
    endpoint: u32,
    property: String,
    value: Value,
}

pub struct Bridge {
    pub config: Config,
    pub publisher: DevicePublisher,
    pub cmd_rx: mpsc::Receiver<(String, Value)>,
    /// Raw zwave-js-server commands pushed by streaming action handlers
    /// (include_node, exclude_node). The WS loop drains this into the
    /// live WebSocket. Long-lived across reconnects — buffer persists
    /// while the plugin is down.
    pub control_rx: mpsc::Receiver<Value>,
    /// Broadcaster for controller-scope events the streaming action
    /// handlers subscribe to. Cloned into the WS loop; long-lived so
    /// active subscribers survive reconnects.
    pub event_tx: broadcast::Sender<ControllerEvent>,
    /// `rescan_nodes` management action pings this; the WS loop sends a
    /// fresh `start_listening` and republishes every node from the result.
    pub rescan_rx: mpsc::Receiver<()>,
    /// What the operator sees on the plugin page when zwave-js-server is not
    /// answering. The loop below retries forever and says so only in the log.
    pub notices: PluginNotices,
}

impl Bridge {
    pub async fn run(mut self) -> Result<()> {
        let mut backoff_secs = 2u64;
        loop {
            match self.run_once().await {
                Ok(()) => {
                    info!("Bridge exited cleanly");
                    break;
                }
                Err(e) => {
                    error!(error = %e, backoff_secs, "Bridge error; reconnecting");
                    self.notices.raise(
                        PluginNotice::error(
                            "server_unreachable",
                            format!(
                                "Cannot reach zwave-js-server at {} — {e}. Every Z-Wave \
                                 device is unavailable until it comes back.",
                                self.config.server.url
                            ),
                        )
                        .with_remedy(
                            "Check that zwave-js-server is running and that [server].url \
                             points at its WebSocket address (ws://host:3000 by default). \
                             It is a separate service from homeCore and has to be started \
                             on its own.",
                        ),
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            }
        }
        Ok(())
    }

    async fn run_once(&mut self) -> Result<()> {
        let cfg = &self.config;

        // --- WebSocket ---
        let (ws_stream, _) = connect_async(&cfg.server.url)
            .await
            .with_context(|| format!("WS connect to {}", cfg.server.url))?;
        // The socket is up — whatever the last failure was, it is over.
        self.notices.clear("server_unreachable");
        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // --- Handshake ---
        let translator = Translator::new();
        let nodes = handshake(&mut ws_tx, &mut ws_rx, cfg.server.schema_version).await?;
        let current_ids: std::collections::HashSet<String> = nodes
            .iter()
            .map(|node| node_device_id(node.node_id))
            .collect();

        // --- Unregister stale devices via SDK reconcile ---
        // SDK has the persisted set from prior sessions plus anything
        // we've registered this session; diff gives us stale devices
        // (nodes excluded from the controller while this plugin was
        // offline) which it unregisters and prunes from the snapshot.
        if let Err(e) = self.publisher.reconcile_devices(current_ids.clone()).await {
            warn!(error = %e, "reconcile_devices failed");
        }

        // --- Publish initial state + subscribe to commands ---
        for node in &nodes {
            if let Err(e) = publish_node(&self.publisher, node, &translator).await {
                warn!(node_id = node.node_id, error = %e, "Failed to publish initial node state");
            }
        }

        // --- Refresh primary-state values ---
        // zwave-js's per-node cache holds whatever value the device last
        // reported; for switches/plugs that only emit meter reports and
        // dimmers/locks that don't auto-report on local actuation, that
        // cache can drift from reality and the snapshot above will then
        // publish a stale `on`/`brightness`/`locked`. Poll the primary
        // value for each non-sleeping node so a fresh Get is issued; the
        // reply arrives as a `value updated` event and corrects state.
        refresh_primary_state(&mut ws_tx, &nodes).await;

        // --- Command channel: SDK cmd_rx → WS sender ---
        let (sv_tx, mut sv_rx) = mpsc::channel::<SetValueCmd>(64);

        // Everything runs inline under tokio::select so we can borrow
        // disjoint fields of `self` (cmd_rx, control_rx) mutably. The WS
        // loop can't be tokio::spawn'd without moving those borrows.
        let cmd_translator = Translator::new();
        let publisher = self.publisher.clone();
        let event_tx = self.event_tx.clone();
        tokio::select! {
            res = ws_event_loop(WsLoopDeps {
                ws_tx: &mut ws_tx,
                ws_rx: &mut ws_rx,
                cmd_rx: &mut sv_rx,
                control_rx: &mut self.control_rx,
                rescan_rx: &mut self.rescan_rx,
                publisher: &publisher,
                translator: &translator,
                event_tx: &event_tx,
            }) => res,
            res = cmd_dispatch_loop(&mut self.cmd_rx, &sv_tx, &cmd_translator) => res,
        }
    }
}

// ---------------------------------------------------------------------------
// WS event loop
// ---------------------------------------------------------------------------

/// Bundles the WS connection + channels + helpers that `ws_event_loop`
/// needs. Grouping keeps the function below clippy's `too_many_arguments`
/// threshold without splitting the loop itself.
struct WsLoopDeps<'a> {
    ws_tx: &'a mut WsSink,
    ws_rx: &'a mut WsStream,
    cmd_rx: &'a mut mpsc::Receiver<SetValueCmd>,
    control_rx: &'a mut mpsc::Receiver<Value>,
    rescan_rx: &'a mut mpsc::Receiver<()>,
    publisher: &'a DevicePublisher,
    translator: &'a Translator,
    event_tx: &'a broadcast::Sender<ControllerEvent>,
}

async fn ws_event_loop(deps: WsLoopDeps<'_>) -> Result<()> {
    let WsLoopDeps {
        ws_tx,
        ws_rx,
        cmd_rx,
        control_rx,
        rescan_rx,
        publisher,
        translator,
        event_tx,
    } = deps;
    // Tracks an in-flight `start_listening` issued by a rescan request.
    // We refuse a second rescan while one is pending — `start_listening`
    // re-snapshots the entire controller and re-running it concurrently
    // is wasteful.
    let mut pending_rescan_id: Option<String> = None;

    loop {
        tokio::select! {
            // Incoming WebSocket message
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        // Rescan completion takes precedence — if this is
                        // the Result for our pending start_listening,
                        // republish all nodes here instead of letting it
                        // drop into handle_ws_message (which only logs
                        // failures).
                        let mut consumed = false;
                        if let Some(expected) = pending_rescan_id.as_ref() {
                            if let Ok(val) = serde_json::from_str::<Value>(&text) {
                                let is_result = val.get("type").and_then(Value::as_str)
                                    == Some("result");
                                let id_match = val.get("messageId").and_then(Value::as_str)
                                    == Some(expected.as_str());
                                if is_result && id_match {
                                    let success = val.get("success")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    if success {
                                        let nodes: Vec<NodeState> = val
                                            .get("result")
                                            .and_then(|r| r.get("state"))
                                            .and_then(|s| s.get("nodes"))
                                            .and_then(|n| n.as_array())
                                            .map(|arr| {
                                                arr.iter()
                                                    .filter_map(NodeState::from_value)
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        info!(
                                            count = nodes.len(),
                                            "Rescan: republishing node states"
                                        );
                                        for node in &nodes {
                                            if let Err(e) =
                                                publish_node(publisher, node, translator).await
                                            {
                                                warn!(
                                                    node_id = node.node_id,
                                                    error = %e,
                                                    "Rescan publish_node failed"
                                                );
                                            }
                                        }
                                        refresh_primary_state(ws_tx, &nodes).await;
                                    } else {
                                        warn!("Rescan start_listening returned success=false");
                                    }
                                    pending_rescan_id = None;
                                    consumed = true;
                                }
                            }
                        }
                        if !consumed {
                            if let Err(e) =
                                handle_ws_message(&text, publisher, translator, event_tx).await
                            {
                                warn!(error = %e, "Error handling WS message");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Close(_))) => bail!("WS closed by server"),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => bail!("WS read error: {e}"),
                    None => bail!("WS stream ended"),
                }
            }
            // Outgoing SetValueCmd from the device command path
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(c) => {
                        if let Err(e) = send_set_value(ws_tx, &c).await {
                            warn!(error = %e, "Failed to send set_value");
                        }
                    }
                    None => break, // sender dropped
                }
            }
            // Raw control JSON from streaming action handlers
            // (include_node, exclude_node).
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(msg) => {
                        if let Err(e) = ws_send(ws_tx, &msg).await {
                            warn!(error = %e, cmd = ?msg.get("command"), "Failed to send control command");
                        }
                    }
                    None => {
                        // Control channel closed (bridge being torn down).
                        // Continue so we don't prematurely exit the WS loop.
                    }
                }
            }
            // Rescan ping from the management custom_handler.
            sig = rescan_rx.recv() => {
                match sig {
                    Some(()) => {
                        if pending_rescan_id.is_some() {
                            debug!("Rescan already pending; ignoring duplicate request");
                            continue;
                        }
                        let msg_id = format!("hc-zwave-rescan-{}", Uuid::new_v4());
                        if let Err(e) = ws_send(
                            ws_tx,
                            &json!({ "messageId": &msg_id, "command": "start_listening" }),
                        )
                        .await
                        {
                            warn!(error = %e, "Rescan: failed to send start_listening");
                        } else {
                            info!(message_id = %msg_id, "Rescan: start_listening sent");
                            pending_rescan_id = Some(msg_id);
                        }
                    }
                    None => {
                        // Sender dropped — keep the loop running.
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_ws_message(
    text: &str,
    publisher: &DevicePublisher,
    translator: &Translator,
    event_tx: &broadcast::Sender<ControllerEvent>,
) -> Result<()> {
    let msg: ServerMsg = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            debug!(error = %e, msg = %&text[..text.len().min(120)], "Unrecognised WS frame");
            return Ok(());
        }
    };

    match msg {
        ServerMsg::Event(wrapper) => {
            // Broadcast controller-scope events for streaming handlers
            // (include_node / exclude_node). Node-scope events keep
            // flowing through the existing handle_event pipeline.
            if let Some(ctrl_ev) = decode_controller_event(&wrapper.event) {
                // Exclusion is unambiguous — the device is gone, so we
                // unregister immediately. (handle_event's "node removed"
                // arm requires a top-level `nodeId`, which the
                // controller-scope event may omit, so we cover that gap
                // here.) Inclusion is NOT handled here on purpose:
                // zwave-js can't tell us what the device is until its
                // interview completes — registering on NodeAdded would
                // produce a placeholder with no command classes and no
                // schema. We instead rely on the node-scope `node ready`
                // path in handle_event, plus the explicit `rescan_nodes`
                // manifest action for users who don't want to wait.
                if let ControllerEvent::NodeRemoved { node_id } = &ctrl_ev {
                    let device_id = node_device_id(*node_id);
                    let plugin_id = publisher.plugin_id().to_string();
                    if let Err(e) = publisher.unregister_device(&plugin_id, &device_id).await {
                        warn!(node_id, error = %e, "unregister_device on NodeRemoved failed");
                    } else {
                        info!(node_id, device_id, "Unregistered node on exclusion");
                    }
                }
                // Ignore send failures — broadcast returns an error only
                // when there are no subscribers, which is the normal
                // case between inclusion sessions.
                let _ = event_tx.send(ctrl_ev);
            }
            handle_event(wrapper.event, publisher, translator).await?
        }
        ServerMsg::Result(r) if !r.success => {
            warn!(
                message_id = %r.message_id,
                error_code = ?r.error_code,
                result = ?r.result,
                "zwave-js command failed"
            );
        }
        _ => {}
    }
    Ok(())
}

async fn handle_event(
    ev: crate::types::RawEvent,
    publisher: &DevicePublisher,
    translator: &Translator,
) -> Result<()> {
    let node_id = match ev.node_id {
        Some(id) => id,
        None => return Ok(()),
    };
    let device_id = node_device_id(node_id);

    match ev.event.as_str() {
        "value updated" | "value added" => {
            if let Some(args_val) = ev.args {
                if let Ok(args) = serde_json::from_value::<ValueUpdatedArgs>(args_val) {
                    let pk_str = args.property_key.as_ref().and_then(property_key_str);
                    let pk = pk_str.as_deref();
                    let (attr, val) = match translator.translate(
                        args.command_class,
                        args.endpoint,
                        &args.property,
                        pk,
                        &args.new_value,
                    ) {
                        Some((a, v)) => (a, v),
                        None => {
                            // Generic fallback — synthesise a stable attribute
                            // name so unaliased values still land in homeCore.
                            let attr = synthetic_attr_name(
                                args.command_class,
                                args.endpoint,
                                &args.property,
                                pk,
                            );
                            debug!(
                                node_id,
                                cc = args.command_class,
                                prop = %args.property,
                                prop_key = ?pk,
                                synth_attr = %attr,
                                value = ?args.new_value,
                                "Unaliased value — publishing under synthetic name"
                            );
                            (attr, args.new_value.clone())
                        }
                    };
                    // Skip null values — partial-merge semantics treat
                    // null as "delete this attribute" on the homeCore
                    // side, which would silently wipe whatever was there
                    // (state_bridge::apply_partial_merge_patch). zwave-js
                    // can fire `value updated` with a null `newValue` for
                    // values that go idle or briefly drop out; we don't
                    // want those to clear data.
                    if val.is_null() {
                        debug!(node_id, %attr, "Null value — skipping partial publish");
                        return Ok(());
                    }
                    debug!(node_id, %attr, value = ?val, "Value → publishing");
                    let patch = json!({ attr: val });
                    publisher.publish_state_partial(&device_id, &patch).await?;
                }
            }
        }

        "node status changed" => {
            // NodeStatus: 0=Unknown, 1=Asleep, 2=Awake, 3=Dead, 4=Alive.
            // Battery sensors regularly go Asleep between readings — that is normal
            // operation, not an outage.  Only Dead (3) means the node is unreachable.
            let status = ev
                .args
                .as_ref()
                .and_then(|a| a.get("status"))
                .and_then(|s| s.as_u64());
            let available = !matches!(status, Some(3));
            publisher
                .publish_availability(&device_id, available)
                .await?;
            info!(node_id, ?status, available, "Node status changed");
        }

        // Direct node lifecycle events forwarded by zwave-js-server
        "dead" => {
            publisher.publish_availability(&device_id, false).await?;
            info!(node_id, "Node dead");
        }
        "alive" | "wake up" => {
            publisher.publish_availability(&device_id, true).await?;
            info!(node_id, event = %ev.event, "Node alive/awake");
        }
        "sleep" => {
            // Sleeping is normal for battery devices — keep current availability,
            // do not mark offline.
            debug!(node_id, "Node sleeping (battery device)");
        }

        "node ready" => {
            if let Some(ns_val) = ev.node_state {
                if let Some(node) = NodeState::from_value(&ns_val) {
                    publish_node(publisher, &node, translator).await?;
                    info!(node_id, "Node ready — published full state");
                }
            }
        }

        "node name updated" => {
            if let Some(name) = ev.name {
                let display_name = if name.is_empty() { &device_id } else { &name };
                publisher
                    .register_device_full(&device_id, display_name, Some("zwave"), None, None)
                    .await?;
                let patch = json!({ "name": name });
                publisher.publish_state_partial(&device_id, &patch).await?;
                debug!(node_id, %name, "Node name updated");
            }
        }

        "node location updated" => {
            if let Some(loc) = ev.location {
                let patch = json!({ "location": loc });
                publisher.publish_state_partial(&device_id, &patch).await?;
                debug!(node_id, %loc, "Node location updated");
            }
        }

        "node removed" => {
            let plugin_id = publisher.plugin_id().to_string();
            publisher.unregister_device(&plugin_id, &device_id).await?;
            info!(node_id, "Node removed and unregistered");
        }

        _ => {
            debug!(event = %ev.event, node_id, "Unhandled event");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// node.set_value
// ---------------------------------------------------------------------------

async fn send_set_value(ws_tx: &mut WsSink, cmd: &SetValueCmd) -> Result<()> {
    let msg_id = format!("hc-sv-{}", Uuid::new_v4());
    let msg = json!({
        "messageId": msg_id,
        "command": "node.set_value",
        "nodeId": cmd.node_id,
        "valueId": {
            "commandClass": cmd.command_class,
            "endpoint": cmd.endpoint,
            "property": cmd.property,
        },
        "value": cmd.value,
    });
    // Log the exact JSON so we can compare against the zwave-js-server protocol spec.
    let raw_json = serde_json::to_string(&msg).unwrap_or_default();
    info!(node_id = cmd.node_id, cc = cmd.command_class, prop = %cmd.property, value = ?cmd.value, json = %raw_json, "Sending node.set_value");
    ws_send(ws_tx, &msg).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Command dispatch loop — reads SDK command channel, translates to SetValueCmd
// ---------------------------------------------------------------------------

async fn cmd_dispatch_loop(
    cmd_rx: &mut mpsc::Receiver<(String, Value)>,
    sv_tx: &mpsc::Sender<SetValueCmd>,
    translator: &Translator,
) -> Result<()> {
    loop {
        match cmd_rx.recv().await {
            Some((device_id, cmd)) => {
                handle_cmd(&device_id, &cmd, translator, sv_tx).await;
            }
            None => bail!("SDK command channel closed"),
        }
    }
}

async fn handle_cmd(
    device_id: &str,
    cmd: &Value,
    translator: &Translator,
    sv_tx: &mpsc::Sender<SetValueCmd>,
) {
    // device_id format: "zwave_{nodeId}"
    let node_id: u32 = match device_id
        .strip_prefix("zwave_")
        .and_then(|s| s.parse().ok())
    {
        Some(id) => id,
        None => return, // not a zwave device — ignore
    };
    info!(node_id, "Received cmd from HomeCore");

    let obj = match cmd.as_object() {
        Some(o) => o,
        None => return,
    };

    // HomeCore command metadata keys (`_hc`, top-level `correlation_id`) are
    // not device attributes — skip them when tallying dispatch outcomes so
    // the "nothing dispatched" warn doesn't fire on metadata-only noise.
    let mut dispatched = 0usize;
    let mut unrecognised: Vec<&str> = Vec::new();
    for (attr, hc_value) in obj {
        if attr.starts_with('_') || attr == "correlation_id" {
            continue;
        }
        let target = match translator.write_target(attr) {
            Some(t) => t,
            None => {
                unrecognised.push(attr.as_str());
                continue;
            }
        };
        let native_value = target.transform.reverse(hc_value);
        let sv_cmd = SetValueCmd {
            node_id,
            command_class: target.command_class,
            endpoint: target.endpoint,
            property: target.property.clone(),
            value: native_value,
        };
        if sv_tx.send(sv_cmd).await.is_err() {
            warn!("WS task gone; dropping cmd");
        } else {
            dispatched += 1;
        }
    }

    if dispatched == 0 && !unrecognised.is_empty() {
        // The command had a payload but no attribute mapped to a Z-Wave
        // writable. Likely a shape mismatch between sender and this plugin.
        // Surfacing as warn makes silent drops visible.
        warn!(
            node_id,
            attributes = ?unrecognised,
            "Command had no writable attributes — nothing was dispatched to Z-Wave"
        );
    } else if !unrecognised.is_empty() {
        debug!(
            attributes = ?unrecognised,
            "Some command attributes had no write target (others dispatched)"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests — startup primary-state value refresh
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(j: serde_json::Value) -> NodeState {
        serde_json::from_value(j).expect("test fixture must parse")
    }

    fn val(cc: u32, ep: u32, prop: &str) -> serde_json::Value {
        json!({ "commandClass": cc, "endpoint": ep, "property": prop })
    }

    fn node_with_values(node_id: u32, values: Vec<serde_json::Value>) -> NodeState {
        node(json!({
            "nodeId": node_id,
            "isListening": true,
            "values": values,
        }))
    }

    // ---- should_poll_initial_values --------------------------------------

    #[test]
    fn poll_eligible_when_mains_powered() {
        let n = node(json!({ "nodeId": 1, "isListening": true }));
        assert!(should_poll_initial_values(&n));
    }

    #[test]
    fn poll_eligible_when_flirs_250ms() {
        let n = node(json!({
            "nodeId": 1, "isListening": false, "isFrequentListening": "250ms"
        }));
        assert!(should_poll_initial_values(&n));
    }

    #[test]
    fn poll_eligible_when_flirs_1000ms() {
        let n = node(json!({
            "nodeId": 1, "isListening": false, "isFrequentListening": "1000ms"
        }));
        assert!(should_poll_initial_values(&n));
    }

    #[test]
    fn poll_skipped_when_sleeping_battery() {
        let n = node(json!({
            "nodeId": 1, "isListening": false, "isFrequentListening": false
        }));
        assert!(!should_poll_initial_values(&n));
    }

    #[test]
    fn poll_skipped_when_flirs_field_null() {
        let n = node(json!({
            "nodeId": 1, "isListening": false, "isFrequentListening": null
        }));
        assert!(!should_poll_initial_values(&n));
    }

    #[test]
    fn poll_skipped_when_both_fields_missing() {
        // Be conservative when zwave-js omits the metadata.
        let n = node(json!({ "nodeId": 1 }));
        assert!(!should_poll_initial_values(&n));
    }

    // ---- primary_state_values_to_poll: each TARGET command class ---------

    #[test]
    fn matches_binary_switch() {
        let n = node_with_values(1, vec![val(37, 0, "currentValue")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(37, 0, "currentValue".into())]
        );
    }

    #[test]
    fn matches_multilevel_switch() {
        let n = node_with_values(1, vec![val(38, 0, "currentValue")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(38, 0, "currentValue".into())]
        );
    }

    #[test]
    fn matches_thermostat_mode() {
        let n = node_with_values(1, vec![val(64, 0, "mode")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(64, 0, "mode".into())]
        );
    }

    #[test]
    fn matches_thermostat_operating_state() {
        let n = node_with_values(1, vec![val(66, 0, "state")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(66, 0, "state".into())]
        );
    }

    #[test]
    fn matches_thermostat_setpoint() {
        let n = node_with_values(1, vec![val(67, 0, "setpoint")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(67, 0, "setpoint".into())]
        );
    }

    #[test]
    fn matches_thermostat_fan_mode() {
        let n = node_with_values(1, vec![val(68, 0, "mode")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(68, 0, "mode".into())]
        );
    }

    #[test]
    fn matches_door_lock() {
        let n = node_with_values(1, vec![val(98, 0, "currentMode")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(98, 0, "currentMode".into())]
        );
    }

    #[test]
    fn matches_barrier_operator() {
        let n = node_with_values(1, vec![val(102, 0, "currentState")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(102, 0, "currentState".into())]
        );
    }

    #[test]
    fn matches_color_switch() {
        let n = node_with_values(1, vec![val(117, 0, "currentColor")]);
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![(117, 0, "currentColor".into())]
        );
    }

    // ---- primary_state_values_to_poll: false-positive guards -------------

    #[test]
    fn ignores_target_value_property() {
        // Target values are commands, not state; only currentValue is state.
        let n = node_with_values(1, vec![val(37, 0, "targetValue"), val(38, 0, "duration")]);
        assert!(primary_state_values_to_poll(&n).is_empty());
    }

    #[test]
    fn ignores_unrelated_command_class() {
        // CC 50 (Meter) intentionally not in TARGETS — meter reports come
        // unsolicited and don't drift like actuator state.
        let n = node_with_values(1, vec![val(50, 0, "value")]);
        assert!(primary_state_values_to_poll(&n).is_empty());
    }

    // ---- multi-endpoint coverage -----------------------------------------

    #[test]
    fn matches_every_endpoint_for_multi_endpoint_devices() {
        // Dual-relay smart plug: same (cc, prop) on endpoints 1 and 2.
        let n = node_with_values(
            1,
            vec![val(37, 1, "currentValue"), val(37, 2, "currentValue")],
        );
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![
                (37, 1, "currentValue".into()),
                (37, 2, "currentValue".into()),
            ]
        );
    }

    // ---- mixed value list -------------------------------------------------

    #[test]
    fn extracts_only_targets_from_mixed_value_list() {
        let n = node_with_values(
            1,
            vec![
                val(37, 0, "currentValue"), // match
                val(37, 0, "targetValue"),  // ignore
                val(50, 0, "value"),        // ignore (Meter)
                val(98, 0, "currentMode"),  // match
            ],
        );
        let got = primary_state_values_to_poll(&n);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&(37, 0, "currentValue".into())));
        assert!(got.contains(&(98, 0, "currentMode".into())));
    }

    #[test]
    fn empty_value_list_yields_no_targets() {
        let n = node_with_values(1, vec![]);
        assert!(primary_state_values_to_poll(&n).is_empty());
    }

    // ---- plan_primary_state_refresh: counter bookkeeping -----------------

    #[test]
    fn plan_counts_skipped_battery_and_eligible_no_targets() {
        let nodes = vec![
            // 1: mains + has switch → polled
            node(json!({
                "nodeId": 1, "isListening": true,
                "values": [val(37, 0, "currentValue")]
            })),
            // 2: FLiRS + has lock → polled
            node(json!({
                "nodeId": 2, "isListening": false, "isFrequentListening": "250ms",
                "values": [val(98, 0, "currentMode")]
            })),
            // 3: mains, only meter → eligible_no_targets
            node(json!({
                "nodeId": 3, "isListening": true,
                "values": [val(50, 0, "value")]
            })),
            // 4: sleeping battery → skipped_battery
            node(json!({
                "nodeId": 4, "isListening": false, "isFrequentListening": false,
                "values": [val(38, 0, "currentValue")]
            })),
            // 5: missing power-class metadata → skipped_battery (conservative)
            node(json!({ "nodeId": 5 })),
        ];

        let plan = plan_primary_state_refresh(&nodes);

        assert_eq!(plan.polls.len(), 2);
        assert_eq!(plan.polls[0].0, 1);
        assert_eq!(plan.polls[1].0, 2);
        assert_eq!(plan.skipped_battery, 2);
        assert_eq!(plan.eligible_no_targets, 1);
    }

    #[test]
    fn plan_handles_empty_node_list() {
        let plan = plan_primary_state_refresh(&[]);
        assert_eq!(plan, PollPlan::default());
    }

    // ---- POLL_DELAY guard -------------------------------------------------

    #[test]
    fn poll_delay_is_200ms() {
        // Sentinel — protects against accidental tightening that would
        // saturate the Z-Wave radio on startup.
        assert_eq!(POLL_DELAY, Duration::from_millis(200));
    }
}
