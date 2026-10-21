//! Bridge between zwave-js-server and homeCore.
//!
//! # Flow
//!
//! 1. Connect ([`ZwClient::open`]) → `set_api_schema` → `start_listening`.
//! 2. Register and publish every node in the result.
//! 3. Handle events as they come: values, notifications, statistics, a node
//!    waking, sleeping, dying or coming back, a node ready after its
//!    interview, nodes added and removed.
//! 4. Device commands go out as `node.set_value`, each waiting for its own
//!    answer; a failure is said on the device (`last_command_error`) and as a
//!    `plugin_command_result` event.
//! 5. When the connection goes, every device is marked offline; on the next
//!    connection everything is published again, online.
//!
//! **Devices are removed only when the network says so** — a `node removed`
//! event (excluded, or removed as failed) — or when someone asks
//! (`forget_device`). They used to be removed by difference: whatever was
//! missing from `start_listening` was deleted from homeCore, so one node that
//! failed to parse, or a result with no node list, deleted devices with their
//! rooms and their place in every automation.
//!
//! **Commands are never queued for later.** One that arrives while Z-Wave JS
//! is unreachable waits [`COMMAND_TTL`] for the connection and otherwise fails
//! and says so. It used to sit in a channel and be sent whenever the
//! connection came back — an unlock an hour late.

use crate::config::Config;
use crate::inclusion::{decode_controller_event, ControllerEvent};
use crate::lock::{self, CodeNames, LockEvent};
use crate::network::Network;
use crate::translator::{self, property_key_str, Translator};
use crate::types::{NodeState, NodeValue, RawEvent, ValueUpdatedArgs};
use crate::zwclient::{self, Incoming, ZwClient, ZwError, REQUEST_TIMEOUT};
use anyhow::{bail, Result};
use chrono::Utc;
use plugin_sdk_rs::types::{PluginAlert, PluginNotice};
use plugin_sdk_rs::{DevicePublisher, PluginNotices};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// How long a device command waits for the connection before it fails.
pub const COMMAND_TTL: Duration = Duration::from_secs(5);
/// How long a `node.set_value` may take: a lock is battery-powered and
/// answers slowly.
pub const SET_VALUE_TIMEOUT: Duration = Duration::from_secs(25);
/// Gap between startup polls, so they don't crowd the radio.
const POLL_DELAY: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

pub fn node_device_id(node_id: u32) -> String {
    format!("zwave_{node_id}")
}

pub fn node_of_device(device_id: &str) -> Option<u32> {
    device_id.strip_prefix("zwave_")?.parse().ok()
}

// ---------------------------------------------------------------------------
// What every part of the plugin shares
// ---------------------------------------------------------------------------

/// A controller backup waiting to be downloaded.
pub struct Backup {
    pub id: String,
    /// Base64, as zwave-js sent it.
    pub data: String,
    pub size: usize,
    pub made: Instant,
}

pub struct Shared {
    pub cfg: Config,
    pub client: ZwClient,
    pub publisher: DevicePublisher,
    pub notices: PluginNotices,
    pub translator: Translator,
    pub net: Mutex<Network>,
    pub names: CodeNames,
    /// Controller-scope events, for the streaming actions.
    pub events: broadcast::Sender<ControllerEvent>,
    /// Nodes whose retired attribute names were cleared this run.
    cleaned: Mutex<HashSet<u32>>,
    /// The last command error said on each device.
    last_errors: Mutex<HashMap<String, Option<String>>>,
    /// A v1 lock alarm's type, waiting for its level.
    alarm_types: Mutex<HashMap<u32, (u64, Instant)>>,
    /// Alerts said on each device, so they are only said again when they
    /// change.
    alerts: Mutex<HashMap<String, Vec<String>>>,
    pub backup: Mutex<Option<Backup>>,
    /// The last command sent to each node, with the attributes it wrote and
    /// when — so the change it causes is credited to whoever asked.
    recent: Mutex<HashMap<u32, Sent>>,
}

/// A command sent to a node: its payload, the attributes it wrote, and when.
type Sent = (Value, Vec<String>, Instant);

/// How long after a command the changes it causes are credited to it.
const CREDIT_WINDOW: Duration = Duration::from_secs(30);

impl Shared {
    pub fn new(
        cfg: Config,
        config_path: &str,
        publisher: DevicePublisher,
        notices: PluginNotices,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            cfg,
            client: ZwClient::new(),
            publisher,
            notices,
            translator: Translator::new(),
            net: Mutex::new(Network::default()),
            names: CodeNames::load(CodeNames::path_for(config_path)),
            events,
            cleaned: Mutex::new(HashSet::new()),
            last_errors: Mutex::new(HashMap::new()),
            alarm_types: Mutex::new(HashMap::new()),
            alerts: Mutex::new(HashMap::new()),
            backup: Mutex::new(None),
            recent: Mutex::new(HashMap::new()),
        })
    }

    /// The command that caused a change to these attributes, if one was
    /// sent moments ago.
    fn cause(&self, node_id: u32, attrs: &[&str], any: bool) -> Option<Value> {
        let held = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let (cmd, wrote, at) = held.get(&node_id)?;
        let matches = any || attrs.iter().any(|a| wrote.iter().any(|w| w == a));
        (matches && at.elapsed() < CREDIT_WINDOW).then(|| cmd.clone())
    }

    /// Publish a partial, credited to the command that caused it if there
    /// was one.
    async fn publish_caused(
        &self,
        node_id: u32,
        patch: &Value,
        cause: Option<Value>,
    ) -> Result<()> {
        let device_id = node_device_id(node_id);
        match cause {
            Some(cmd) => {
                self.publisher
                    .publish_state_partial_for_command(&device_id, patch, &cmd, "plugin.zwave")
                    .await
            }
            None => {
                self.publisher
                    .publish_state_partial(&device_id, patch)
                    .await
            }
        }
    }

    pub fn net(&self) -> std::sync::MutexGuard<'_, Network> {
        self.net.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of one node as last heard.
    pub fn node(&self, node_id: u32) -> Option<NodeState> {
        self.net().nodes.get(&node_id).cloned()
    }
}

// ---------------------------------------------------------------------------
// Node state → homeCore
// ---------------------------------------------------------------------------

/// What kind of thing this node is, in homeCore's words.
///
/// **Every Z-Wave node used to register as `device_type: "zwave"`**, which is
/// the protocol rather than the device. zwave-js has known the answer all
/// along and sends it on every node: `deviceClass.generic` / `.specific`,
/// filled in once the interview completes.
///
/// **Where the class is decisive, it decides.** Where it is not, what the node
/// *reports* does — "Notification Sensor" is the generic and the specific class
/// of both a door sensor and a motion sensor, and the only thing telling them
/// apart is that one publishes `open` and the other `motion`.
///
/// `None` means "say nothing" rather than "unknown": an uninterviewed node has
/// no class yet, and a registration that omits `device_type` leaves whatever
/// core already stored alone.
fn device_type_for(node: &NodeState, state: &Value) -> Option<&'static str> {
    let has = |name: &str| state.get(name).is_some();

    let by_reading = || -> Option<&'static str> {
        if has("motion") {
            Some("motion_sensor")
        } else if has("open") && !has("locked") {
            Some("contact_sensor")
        } else if has("water_detected") || has("leak") {
            Some("water_sensor")
        } else if has("smoke") {
            Some("smoke_sensor")
        } else if has("locked") {
            Some("lock")
        } else if has("temperature") {
            Some("temperature_sensor")
        } else {
            None
        }
    };

    if node.is_controller_node == Some(true) {
        return Some("gateway");
    }
    let class = node.device_class.as_ref();
    let generic = class.and_then(|c| c.generic_label()).unwrap_or_default();
    let specific = class.and_then(|c| c.specific_label()).unwrap_or_default();

    match generic {
        "Binary Switch" => Some("switch"),
        "Multilevel Switch" if translator::is_cover_class(generic, specific) => Some("cover"),
        "Multilevel Switch" => Some("light"),
        "Entry Control" if specific.contains("Barrier") => Some("cover"),
        "Entry Control" => Some("lock"),
        "Window Covering" => Some("cover"),
        "Thermostat" => Some("thermostat"),
        "Binary Sensor" | "Notification Sensor" | "Multilevel Sensor" | "Alarm Sensor" => {
            by_reading().or(Some("sensor"))
        }
        "Static Controller" => Some("gateway"),
        _ => by_reading(),
    }
}

/// The node's state, translated. Its name and room are its registration's,
/// not state: published as attributes, core copied `name` over the device's
/// own on every update.
fn build_state(node: &NodeState, translator: &Translator) -> Value {
    let mut map = Map::new();
    for v in &node.values {
        translate_node_value(node.node_id, v, translator, &mut map);
    }
    Value::Object(map)
}

/// One value's attributes, or none.
fn value_pairs(
    node_id: u32,
    cc: u32,
    endpoint: u32,
    property: &str,
    pk: Option<&str>,
    raw: &Value,
    translator: &Translator,
) -> Vec<(String, Value)> {
    if raw.is_null() || translator.is_unpublished(cc, property, pk) {
        return Vec::new();
    }
    match translator.translate(node_id, cc, endpoint, property, pk, raw) {
        Some(pairs) => pairs,
        None => vec![(
            translator.synthetic_name(node_id, cc, endpoint, property, pk),
            raw.clone(),
        )],
    }
}

fn translate_node_value(
    node_id: u32,
    v: &NodeValue,
    translator: &Translator,
    map: &mut Map<String, Value>,
) {
    let Some(raw) = v.value.as_ref() else {
        return;
    };
    let pk = v.property_key.as_ref().and_then(property_key_str);
    for (k, val) in value_pairs(
        node_id,
        v.command_class,
        v.endpoint,
        &v.property,
        pk.as_deref(),
        raw,
        translator,
    ) {
        // "Unknown" (a lock mode of 254) on a full publish is simply absent.
        if !val.is_null() {
            map.insert(k, val);
        }
    }
}

/// Names published before, replaced or retired: the device vocabulary's old
/// spellings, the node's name and room (registration now), and the lock
/// attributes that said the same as another.
const RETIRED_NAMES: &[&str] = &[
    "brightness",
    "contact_open",
    "tamper",
    "battery",
    "co2_ppm",
    "target_temp",
    "mode",
    "name",
    "location",
    "bolt_status",
    "latch_status",
    "door_status",
];

/// The partial of `null`s that clears from homeCore's stored state every
/// attribute this node used to publish and no longer does:
///
/// - [`RETIRED_NAMES`];
/// - the raw `cc…` name of any value that now publishes under a shared name
///   or is never published (a write target, a lock's code slots, a
///   configuration parameter, identity).
///
/// Anything the node publishes now is left alone. Sent once per node per run
/// — it used to go out on every publish.
fn retired_state(node: &NodeState, translator: &Translator, state: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for name in RETIRED_NAMES {
        out.insert((*name).to_string(), Value::Null);
    }
    for v in &node.values {
        let pk = v.property_key.as_ref().and_then(property_key_str);
        let pk = pk.as_deref();
        let gone = translator.is_unpublished(v.command_class, &v.property, pk)
            || v.value.as_ref().is_some_and(|raw| {
                !raw.is_null()
                    && translator
                        .translate(
                            node.node_id,
                            v.command_class,
                            v.endpoint,
                            &v.property,
                            pk,
                            raw,
                        )
                        .is_some()
            });
        if gone {
            let attr =
                translator::synthetic_attr_name(v.command_class, v.endpoint, &v.property, pk);
            out.insert(attr, Value::Null);
        }
    }
    if let Some(current) = state.as_object() {
        out.retain(|k, _| !current.contains_key(k));
    }
    out
}

/// The name a node registers under: what zwave-js calls it, else its model,
/// else its id.
fn display_name(node: &NodeState) -> String {
    node.name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| node.model_name())
        .unwrap_or_else(|| node_device_id(node.node_id))
}

/// Register a node, describe it and publish its state and availability.
pub async fn publish_node(s: &Shared, node: &NodeState) -> Result<()> {
    let device_id = node_device_id(node.node_id);
    let mut hardware = plugin_sdk_rs::DeviceHardware::new();
    if let Some(v) = node.manufacturer_name() {
        hardware = hardware.manufacturer(v);
    }
    if let Some(v) = node.model_name() {
        hardware = hardware.model(v);
    }
    if let Some(v) = node.firmware_version.as_deref().filter(|s| !s.is_empty()) {
        hardware = hardware.sw_version(v);
    }

    // What the node is decides what some of its values mean, so learn it
    // before translating anything.
    s.translator.learn_node(node);
    let state = build_state(node, &s.translator);
    // zwave-js's location is the room the include flow put it in; core keeps
    // a room someone chose in homeCore over it.
    let area = node.location.as_deref().filter(|l| !l.trim().is_empty());
    s.publisher
        .register_device_detailed(
            &device_id,
            &display_name(node),
            device_type_for(node, &state),
            area,
            None,
            Some(&hardware),
        )
        .await?;

    publish_schema(s, node, &state).await?;
    // Partial-merge, not full replace: a node mid-interview reports sparsely,
    // and a full replace would wipe what it reported before.
    if state.as_object().is_some_and(|o| !o.is_empty()) {
        s.publisher
            .publish_state_partial(&device_id, &state)
            .await?;
    }
    let first = s
        .cleaned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(node.node_id);
    if first {
        let retired = retired_state(node, &s.translator, &state);
        if !retired.is_empty() {
            s.publisher
                .publish_state_partial(&device_id, &Value::Object(retired))
                .await?;
        }
    }
    s.publisher
        .publish_availability(&device_id, node.is_available())
        .await?;
    s.publisher.subscribe_commands(&device_id).await?;
    say_alerts(s, &device_id, &state, first).await;
    debug!(node_id = node.node_id, device_id, "Published node");
    Ok(())
}

/// Describe a node (its schema, with a lock's actions and named slots).
pub async fn publish_schema(s: &Shared, node: &NodeState, state: &Value) -> Result<()> {
    let names = s.names.of(node.node_id);
    if let Some(schema) = crate::schema::node_schema(node, state, &s.translator, &names) {
        s.publisher
            .register_device_schema_json(&node_device_id(node.node_id), &schema)
            .await?;
    }
    Ok(())
}

/// Describe a node again, from what is known of it now.
pub async fn republish_schema(s: &Shared, node_id: u32) {
    let Some(node) = s.node(node_id) else {
        return;
    };
    let state = build_state(&node, &s.translator);
    if let Err(e) = publish_schema(s, &node, &state).await {
        debug!(node_id, error = %e, "Could not republish the schema");
    }
}

/// The plugin's alerts on a device, from its state: an overload. (A jammed
/// lock is homeCore's own alert, from `jammed`.) Said again only when they
/// change.
async fn update_alerts(s: &Shared, device_id: &str, patch: &Value) {
    say_alerts(s, device_id, patch, false).await;
}

/// `force`: say the list even if nothing changed — the first time a device is
/// published in a run, so an alert left from before (retained) is cleared.
async fn say_alerts(s: &Shared, device_id: &str, patch: &Value, force: bool) {
    let Some(obj) = patch.as_object() else {
        return;
    };
    let watched = ["overload", "overcurrent"];
    if !force && !watched.iter().any(|k| obj.contains_key(*k)) {
        return;
    }
    let mut now: Vec<String> = s
        .alerts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device_id)
        .cloned()
        .unwrap_or_default();
    for k in watched {
        match obj.get(k).and_then(Value::as_bool) {
            Some(true) if !now.iter().any(|x| x == k) => now.push(k.to_string()),
            Some(false) => now.retain(|x| x != k),
            _ => {}
        }
    }
    let changed = {
        let mut held = s.alerts.lock().unwrap_or_else(|e| e.into_inner());
        let before = held.get(device_id).cloned().unwrap_or_default();
        held.insert(device_id.to_string(), now.clone());
        before != now
    };
    if !changed && !force {
        return;
    }
    let alerts: Vec<PluginAlert> = now
        .iter()
        .map(|k| match k.as_str() {
            "overload" => PluginAlert::new("overload", "Overloaded — it may have switched off"),
            _ => PluginAlert::new("overload", "Drawing too much current").key("overcurrent"),
        })
        .collect();
    if let Err(e) = s.publisher.set_alerts(device_id, &alerts).await {
        debug!(device_id, error = %e, "Could not say the alerts");
    }
}

// ---------------------------------------------------------------------------
// The connection's life
// ---------------------------------------------------------------------------

/// Run forever: connect, keep the network current, reconnect with back-off.
pub async fn run(s: Arc<Shared>, cmd_rx: mpsc::Receiver<(String, Value)>) {
    tokio::spawn(command_intake(s.clone(), cmd_rx));
    let mut backoff = 2u64;
    let mut polled = false;
    loop {
        s.net().connection.state = "connecting";
        match session(&s, &mut polled).await {
            Ok(why) => {
                warn!(reason = %why, "Lost the connection to Z-Wave JS; reconnecting");
                backoff = 2;
                went_offline(&s, &why).await;
            }
            Err(e) => {
                let why = format!("{e:#}");
                warn!(error = %why, backoff_secs = backoff, "Cannot reach Z-Wave JS; retrying");
                went_offline(&s, &why).await;
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

/// One connection, from connecting to its end. `Ok` with why it ended, once
/// it was up; `Err` if it never came up.
async fn session(s: &Arc<Shared>, polled: &mut bool) -> Result<String> {
    let url = s.cfg.server.url.clone();
    let mut session = s.client.open(&url).await?;
    let v = session.version.clone();
    info!(
        server_version = %v.server_version,
        driver_version = %v.driver_version,
        min_schema = v.min_schema_version,
        max_schema = v.max_schema_version,
        "Connected to zwave-js-server"
    );
    let schema = s.cfg.server.schema_version.clamp(
        v.min_schema_version,
        v.max_schema_version.max(v.min_schema_version),
    );
    s.client
        .request_raw(
            json!({ "command": "set_api_schema", "schemaVersion": schema }),
            REQUEST_TIMEOUT,
        )
        .await?;
    let answer = s
        .client
        .request_raw(
            json!({ "command": "start_listening" }),
            Duration::from_secs(60),
        )
        .await?;
    let nodes = apply_state(s, &answer)?;
    {
        let mut net = s.net();
        net.connection.state = "connected";
        net.connection.since = Utc::now();
        net.connection.last_error = None;
        net.connection.server_version = Some(v.server_version.clone());
        net.connection.driver_version = Some(v.driver_version.clone());
        net.connection.schema = Some(schema);
    }
    s.client.set_ready(true);
    s.notices.clear("server_unreachable");
    info!(nodes = nodes.len(), schema, "Z-Wave network ready");
    for node in &nodes {
        if let Err(e) = publish_node(s, node).await {
            warn!(node_id = node.node_id, error = %e, "Could not publish a node");
        }
    }
    if !*polled {
        *polled = true;
        tokio::spawn(refresh_primary_state(s.clone(), nodes));
    }

    loop {
        match session.incoming.recv().await {
            Some(Incoming::Event(ev)) => on_event(s, ev).await,
            Some(Incoming::Closed(why)) => return Ok(why),
            None => return Ok("the connection ended".into()),
        }
    }
}

/// The connection is gone: every device offline, and the page says why.
async fn went_offline(s: &Shared, why: &str) {
    s.client.set_ready(false);
    let ids: Vec<u32> = {
        let mut net = s.net();
        net.connection.state = "disconnected";
        net.connection.since = Utc::now();
        net.connection.last_error = Some(why.to_string());
        net.nodes.keys().copied().collect()
    };
    for id in ids {
        let _ = s
            .publisher
            .publish_availability(&node_device_id(id), false)
            .await;
    }
    s.notices.raise(
        PluginNotice::error(
            "server_unreachable",
            format!(
                "Cannot reach zwave-js-server at {} — {why}. Every Z-Wave device is \
                 unavailable until it comes back.",
                crate::network::redact_url(&s.cfg.server.url)
            ),
        )
        .with_remedy(
            "Check that zwave-js-server is running and that [server].url points at its \
             WebSocket address (ws://host:3000 by default). It is a separate service from \
             homeCore and has to be started on its own.",
        ),
    );
}

/// Take a `start_listening` answer: the controller and the nodes. A result
/// without a node list is refused — it would otherwise read as "no nodes".
fn apply_state(s: &Shared, answer: &Value) -> Result<Vec<NodeState>> {
    let state = answer.get("state");
    let Some(raw_nodes) = state
        .and_then(|st| st.get("nodes"))
        .and_then(Value::as_array)
    else {
        bail!("Z-Wave JS sent a start_listening result with no node list; nothing was changed");
    };
    let mut nodes = Vec::with_capacity(raw_nodes.len());
    for raw in raw_nodes {
        if let Some(n) = NodeState::from_value(raw) {
            nodes.push(n);
        }
    }
    let mut net = s.net();
    if let Some(c) = state.and_then(|st| st.get("controller")) {
        net.controller = c.clone();
        net.rebuilding = c
            .get("isRebuildingRoutes")
            .or_else(|| c.get("isHealNetworkActive"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }
    // Nodes that did not parse keep what was known of them.
    for n in &nodes {
        net.nodes.insert(n.node_id, n.clone());
    }
    Ok(nodes)
}

/// `start_listening` again, and publish everything from it.
pub async fn rescan(s: &Shared) -> Result<usize, ZwError> {
    let answer = s
        .client
        .request(
            json!({ "command": "start_listening" }),
            Duration::from_secs(60),
        )
        .await?;
    let nodes = apply_state(s, &answer).map_err(|e| ZwError::new("malformed", e.to_string()))?;
    for node in &nodes {
        if let Err(e) = publish_node(s, node).await {
            warn!(node_id = node.node_id, error = %e, "Rescan: could not publish a node");
        }
    }
    Ok(nodes.len())
}

/// Ask zwave-js for one node afresh and publish it.
pub async fn refresh_node(s: &Shared, node_id: u32) -> Result<NodeState, ZwError> {
    let answer = s
        .client
        .request(
            json!({ "command": "node.get_state", "nodeId": node_id }),
            REQUEST_TIMEOUT,
        )
        .await?;
    let node = answer
        .get("state")
        .and_then(NodeState::from_value)
        .ok_or_else(|| ZwError::new("malformed", "Z-Wave JS sent no node"))?;
    s.net().nodes.insert(node_id, node.clone());
    if let Err(e) = publish_node(s, &node).await {
        warn!(node_id, error = %e, "Could not publish a node");
    }
    Ok(node)
}

// ---------------------------------------------------------------------------
// Startup value refresh
// ---------------------------------------------------------------------------

/// One value to refresh on a node: `(commandClass, endpoint, property)`.
type PollTarget = (u32, u32, String);

/// Primary-state value IDs to refresh at startup: zwave-js's cache can be
/// stale for actuators that don't report local changes.
fn primary_state_values_to_poll(node: &NodeState) -> Vec<PollTarget> {
    const TARGETS: &[(u32, &str)] = &[
        (37, "currentValue"),
        (38, "currentValue"),
        (64, "mode"),
        (66, "state"),
        (67, "setpoint"),
        (68, "mode"),
        (98, "currentMode"),
        (102, "currentState"),
        (51, "currentColor"),
    ];
    let mut out = Vec::new();
    for v in &node.values {
        if TARGETS
            .iter()
            .any(|(cc, prop)| v.command_class == *cc && v.property == *prop)
        {
            let t = (v.command_class, v.endpoint, v.property.clone());
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// Which nodes are polled at startup: mains-powered ones. A lock (FLiRS)
/// answers, but costs its battery a wake-up per poll, and reports its own
/// changes anyway; a sleeping sensor can't answer at all.
fn should_poll(node: &NodeState) -> bool {
    node.is_mains() && node.is_controller_node != Some(true)
}

/// Poll each mains node's primary state, in the background, one at a time.
async fn refresh_primary_state(s: Arc<Shared>, nodes: Vec<NodeState>) {
    let mut polled = 0usize;
    for node in nodes.iter().filter(|n| should_poll(n)) {
        for (cc, ep, prop) in primary_state_values_to_poll(node) {
            let r = s
                .client
                .request(
                    json!({
                        "command": "node.poll_value",
                        "nodeId": node.node_id,
                        "valueId": { "commandClass": cc, "endpoint": ep, "property": prop },
                    }),
                    Duration::from_secs(10),
                )
                .await;
            if let Err(e) = r {
                debug!(node_id = node.node_id, cc, error = %e, "A startup poll failed");
                if e.code == "not_connected" || e.code == "disconnected" {
                    return;
                }
            }
            polled += 1;
            tokio::time::sleep(POLL_DELAY).await;
        }
    }
    info!(polled, "Refreshed primary-state values");
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

async fn on_event(s: &Arc<Shared>, ev: RawEvent) {
    if let Some(ce) = decode_controller_event(&ev) {
        let _ = s.events.send(ce);
    }
    let r = match (ev.source.as_str(), ev.event.as_str()) {
        ("controller", _) | ("driver", _) => on_controller_event(s, &ev).await,
        _ => on_node_event(s, &ev).await,
    };
    if let Err(e) = r {
        debug!(event = %ev.event, error = %e, "An event could not be handled");
    }
}

async fn on_controller_event(s: &Arc<Shared>, ev: &RawEvent) -> Result<()> {
    match ev.event.as_str() {
        "node added" => {
            // Known now, published once its interview is done (`ready`).
            if let Some(node) = ev.get("node").and_then(NodeState::from_value) {
                info!(node_id = node.node_id, "Node added; interviewing");
                s.net().nodes.insert(node.node_id, node);
            }
        }
        "node removed" => {
            let Some(node_id) = ev
                .get("node")
                .and_then(|n| n.get("nodeId"))
                .and_then(Value::as_u64)
                .map(|n| n as u32)
            else {
                return Ok(());
            };
            // RemoveNodeReason 3/4: replaced. The new device takes the same
            // node id, so the homeCore device — its room, its automations —
            // stays for it.
            let reason = ev.get("reason").and_then(Value::as_u64);
            if matches!(reason, Some(3 | 4)) {
                info!(node_id, "Node replaced; its device is kept for the new one");
                return Ok(());
            }
            forget_node(s, node_id).await?;
        }
        "rebuild routes progress" | "heal network progress" => {
            if let Some(p) = ev.get("progress").and_then(Value::as_object) {
                let mut net = s.net();
                net.rebuilding = true;
                net.rebuild_progress = p
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("pending").to_string()))
                    .collect();
            }
        }
        "rebuild routes done" | "heal network done" => {
            let mut net = s.net();
            net.rebuilding = false;
            net.rebuild_finished = Some(Utc::now());
            if let Some(p) = ev.get("result").and_then(Value::as_object) {
                net.rebuild_progress = p
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("done").to_string()))
                    .collect();
            }
        }
        "inclusion started" => s.net().inclusion = Some("including".into()),
        "exclusion started" => s.net().inclusion = Some("excluding".into()),
        "inclusion stopped" | "exclusion stopped" | "inclusion failed" | "exclusion failed"
        | "inclusion aborted" => s.net().inclusion = Some("idle".into()),
        "statistics updated" => {
            if let Some(st) = ev.get("statistics") {
                let mut net = s.net();
                if net.controller.is_object() {
                    net.controller["statistics"] = st.clone();
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// A node that left the network: its device goes from homeCore.
pub async fn forget_node(s: &Shared, node_id: u32) -> Result<()> {
    let device_id = node_device_id(node_id);
    s.net().nodes.remove(&node_id);
    s.translator.forget_node(node_id);
    s.names.forget_node(node_id);
    s.cleaned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&node_id);
    let plugin_id = s.publisher.plugin_id().to_string();
    s.publisher
        .unregister_device(&plugin_id, &device_id)
        .await?;
    info!(node_id, device_id, "Node left the network; device removed");
    Ok(())
}

async fn on_node_event(s: &Arc<Shared>, ev: &RawEvent) -> Result<()> {
    let Some(node_id) = ev.node_id else {
        return Ok(());
    };
    let device_id = node_device_id(node_id);
    match ev.event.as_str() {
        "value updated" | "value added" | "value notification" => {
            if let Some(args) = &ev.args {
                on_value(s, node_id, args).await?;
            }
        }
        "value removed" => {
            if let Some(args) = &ev.args {
                on_value_removed(s, node_id, args).await?;
            }
        }
        "metadata updated" => {
            if let Some(args) = &ev.args {
                on_metadata(s, node_id, args);
            }
        }
        "notification" => {
            let cc = ev.get("ccId").and_then(Value::as_u64);
            if let Some(n) = s.net().nodes.get_mut(&node_id) {
                n.last_seen = Some(json!(Utc::now().to_rfc3339()));
            }
            if let (Some(113), Some(args)) = (cc, &ev.args) {
                if let Some(e) = lock::from_notification(args) {
                    publish_lock_event(s, node_id, e).await?;
                }
            }
        }
        "statistics updated" => {
            if let Some(st) = ev.get("statistics") {
                if let Some(n) = s.net().nodes.get_mut(&node_id) {
                    if let Some(seen) = st.get("lastSeen") {
                        n.last_seen = Some(seen.clone());
                    }
                    n.statistics = Some(st.clone());
                }
            }
        }
        "dead" | "alive" | "sleep" | "wake up" => {
            let status = match ev.event.as_str() {
                "dead" => 3,
                "alive" => 4,
                "sleep" => 1,
                _ => 2,
            };
            if let Some(n) = s.net().nodes.get_mut(&node_id) {
                n.status = Some(status);
                if status != 3 {
                    n.last_seen = Some(json!(Utc::now().to_rfc3339()));
                }
            }
            // Asleep is normal for a battery device; only dead is offline.
            if status != 1 {
                s.publisher
                    .publish_availability(&device_id, status != 3)
                    .await?;
            }
            info!(node_id, event = %ev.event, "Node status");
        }
        "node status changed" => {
            let status = ev
                .args
                .as_ref()
                .and_then(|a| a.get("status"))
                .and_then(Value::as_u64);
            if let Some(n) = s.net().nodes.get_mut(&node_id) {
                n.status = status.and_then(|v| u8::try_from(v).ok());
            }
            s.publisher
                .publish_availability(&device_id, status != Some(3))
                .await?;
        }
        // The interview is done: now the node can be described (H1 — the
        // event is `ready`, not "node ready").
        "ready" => {
            let parsed = ev.node_state.as_ref().and_then(NodeState::from_value);
            let node = match parsed {
                Some(n) => n,
                None => match refresh_node(s, node_id).await {
                    Ok(n) => n,
                    Err(e) => bail!("a ready node could not be read: {e}"),
                },
            };
            s.net().nodes.insert(node_id, node.clone());
            publish_node(s, &node).await?;
            info!(node_id, "Node ready — published");
        }
        "interview stage completed" => {
            if let Some(stage) = ev.get("stageName") {
                if let Some(n) = s.net().nodes.get_mut(&node_id) {
                    n.interview_stage = Some(stage.clone());
                }
            }
        }
        "interview completed" => {
            if let Some(n) = s.net().nodes.get_mut(&node_id) {
                n.interview_stage = Some(json!("Complete"));
            }
        }
        _ => debug!(event = %ev.event, node_id, "Unhandled node event"),
    }
    Ok(())
}

async fn on_value(s: &Arc<Shared>, node_id: u32, args: &Value) -> Result<()> {
    let Ok(a) = serde_json::from_value::<ValueUpdatedArgs>(args.clone()) else {
        return Ok(());
    };
    // A lock's code: not held, not logged, not published.
    if crate::types::is_code_value(a.command_class, &a.property) {
        return Ok(());
    }
    let pk = a.property_key.as_ref().and_then(property_key_str);
    if let Some(n) = s.net().nodes.get_mut(&node_id) {
        // It reported, so it was heard.
        n.last_seen = Some(json!(Utc::now().to_rfc3339()));
        n.upsert_value(NodeValue {
            command_class: a.command_class,
            endpoint: a.endpoint,
            property: a.property.clone(),
            property_key: a.property_key.clone(),
            value: Some(a.new_value.clone()),
            metadata: args.get("metadata").cloned(),
        });
    }
    // A v1 lock alarm: the type, then its level.
    if a.command_class == 113 && matches!(a.property.as_str(), "alarmType" | "alarmLevel") {
        if let Some(e) = v1_alarm(s, node_id, &a) {
            publish_lock_event(s, node_id, e).await?;
        }
        return Ok(());
    }
    // A slot's code switched on or off, or cleared: its name may go.
    if a.command_class == 99 && a.property == "userIdStatus" {
        if let (Some(slot), Some(0)) = (
            pk.as_deref().and_then(|p| p.parse::<u32>().ok()),
            a.new_value.as_u64(),
        ) {
            if s.names.of(node_id).contains_key(&slot) {
                s.names.set(node_id, slot, None);
                republish_schema(s, node_id).await;
            }
        }
        return Ok(());
    }
    // Raw nulls (a value going idle, briefly unknown) are not published: a
    // null in a partial deletes the attribute. A null the translation means
    // ("a lock in an unknown state") is.
    if a.new_value.is_null() {
        return Ok(());
    }
    let pairs = value_pairs(
        node_id,
        a.command_class,
        a.endpoint,
        &a.property,
        pk.as_deref(),
        &a.new_value,
        &s.translator,
    );
    if pairs.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let cause = s.cause(node_id, &keys, false);
    let patch: Map<String, Value> = pairs.into_iter().collect();
    let patch = Value::Object(patch);
    let device_id = node_device_id(node_id);
    s.publish_caused(node_id, &patch, cause).await?;
    update_alerts(s, &device_id, &patch).await;
    Ok(())
}

/// A v1 alarm's type and level arrive as two value updates; decode once both
/// are in.
fn v1_alarm(s: &Shared, node_id: u32, a: &ValueUpdatedArgs) -> Option<LockEvent> {
    let mut held = s.alarm_types.lock().unwrap_or_else(|e| e.into_inner());
    let n = a.new_value.as_u64()?;
    if a.property == "alarmType" {
        held.insert(node_id, (n, Instant::now()));
        return None;
    }
    let (kind, at) = held.remove(&node_id)?;
    if at.elapsed() > Duration::from_secs(3) {
        return None;
    }
    lock::from_v1_alarm(kind, n)
}

async fn publish_lock_event(s: &Shared, node_id: u32, e: LockEvent) -> Result<()> {
    let device_id = node_device_id(node_id);
    let mut patch = e.patch(&Utc::now().to_rfc3339());
    match e.action {
        "jammed" => patch["jammed"] = json!(true),
        "locked" | "unlocked" => patch["jammed"] = json!(false),
        _ => {}
    }
    info!(node_id, action = e.action, method = ?e.method, slot = ?e.slot, "Lock event");
    // A lock or unlock "by radio" right after homeCore asked for one is
    // homeCore's: say who.
    let cause = (e.method == Some("rf"))
        .then(|| s.cause(node_id, &["locked"], false))
        .flatten();
    s.publish_caused(node_id, &patch, cause).await?;
    update_alerts(s, &device_id, &patch).await;
    Ok(())
}

async fn on_value_removed(s: &Shared, node_id: u32, args: &Value) -> Result<()> {
    let Ok(a) = serde_json::from_value::<ValueUpdatedArgs>(args.clone()) else {
        return Ok(());
    };
    let key = crate::types::value_key(
        a.command_class,
        a.endpoint,
        &a.property,
        a.property_key.as_ref(),
    );
    let removed = s
        .net()
        .nodes
        .get_mut(&node_id)
        .and_then(|n| n.remove_value(&key));
    let pk = a.property_key.as_ref().and_then(property_key_str);
    let probe = removed
        .and_then(|v| v.value)
        .filter(|v| !v.is_null())
        .unwrap_or(json!(0));
    let names: Vec<String> = value_pairs(
        node_id,
        a.command_class,
        a.endpoint,
        &a.property,
        pk.as_deref(),
        &probe,
        &s.translator,
    )
    .into_iter()
    .map(|(k, _)| k)
    .collect();
    if names.is_empty() {
        return Ok(());
    }
    let patch: Map<String, Value> = names.into_iter().map(|k| (k, Value::Null)).collect();
    s.publisher
        .publish_state_partial(&node_device_id(node_id), &Value::Object(patch))
        .await?;
    Ok(())
}

fn on_metadata(s: &Shared, node_id: u32, args: &Value) {
    let cc = args.get("commandClass").and_then(Value::as_u64);
    let ep = args.get("endpoint").and_then(Value::as_u64).unwrap_or(0);
    let prop = args.get("property").map(|p| match p {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let pk = args.get("propertyKey").and_then(property_key_str);
    let (Some(cc), Some(prop), Some(meta)) = (cc, prop, args.get("metadata")) else {
        return;
    };
    s.translator
        .learn_metadata(node_id, cc as u32, ep as u32, &prop, pk.as_deref(), meta);
    if let Some(n) = s.net().nodes.get_mut(&node_id) {
        let key = crate::types::value_key(cc as u32, ep as u32, &prop, args.get("propertyKey"));
        match n.values.iter_mut().find(|v| v.key() == key) {
            Some(v) => v.metadata = Some(meta.clone()),
            None if !crate::types::is_code_value(cc as u32, &prop) => n.values.push(NodeValue {
                command_class: cc as u32,
                endpoint: ep as u32,
                property: prop,
                property_key: args.get("propertyKey").cloned(),
                value: None,
                metadata: Some(meta.clone()),
            }),
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Device commands
// ---------------------------------------------------------------------------

/// Take device commands as they come, each on its own task so a slow lock
/// does not hold up a switch.
async fn command_intake(s: Arc<Shared>, mut cmd_rx: mpsc::Receiver<(String, Value)>) {
    while let Some((device_id, cmd)) = cmd_rx.recv().await {
        let s = s.clone();
        tokio::spawn(async move { execute_command(&s, &device_id, &cmd).await });
    }
}

/// The `node.set_value` for one write.
pub fn set_value_msg(node_id: u32, w: &translator::Write) -> Value {
    let mut value_id = json!({
        "commandClass": w.command_class,
        "endpoint": w.endpoint,
        "property": w.property,
    });
    if let Some(pk) = &w.property_key {
        value_id["propertyKey"] = pk.clone();
    }
    let mut msg = json!({
        "command": "node.set_value",
        "nodeId": node_id,
        "valueId": value_id,
        "value": w.value,
    });
    if let Some(options) = &w.options {
        msg["options"] = options.clone();
    }
    msg
}

/// Send one `node.set_value` and say how it went. A code's value is never in
/// the answer's error.
pub async fn set_value(s: &Shared, node_id: u32, w: &translator::Write) -> Result<(), String> {
    info!(
        node_id,
        cc = w.command_class,
        endpoint = w.endpoint,
        property = %w.property,
        "Sending node.set_value"
    );
    if w.command_class != 99 {
        debug!(node_id, value = %w.value, "node.set_value value");
    }
    match s
        .client
        .request(set_value_msg(node_id, w), SET_VALUE_TIMEOUT)
        .await
    {
        Ok(answer) => zwclient::set_value_outcome(&answer),
        Err(e) => Err(e.message),
    }
}

async fn execute_command(s: &Shared, device_id: &str, cmd: &Value) {
    let Some(node_id) = node_of_device(device_id) else {
        return;
    };
    let Some(obj) = cmd.as_object() else {
        return;
    };
    if let Some(action) = obj.get("action").and_then(Value::as_str) {
        let r = device_action(s, node_id, action, obj).await;
        report(s, device_id, action, r).await;
        return;
    }
    let (writes, unrecognised) = s.translator.commands(node_id, obj);
    if writes.is_empty() {
        if !unrecognised.is_empty() {
            warn!(node_id, attributes = ?unrecognised, "Nothing in the command can be written to this device");
            report(
                s,
                device_id,
                "set",
                Err(format!(
                    "this device can't be set to {}",
                    unrecognised.join(", ")
                )),
            )
            .await;
        }
        return;
    }
    let operation = writes
        .iter()
        .map(|w| w.attribute.as_str())
        .collect::<Vec<_>>()
        .join(",");
    // Wait a moment for a connection that is coming back — no longer.
    if !s.client.wait_ready(COMMAND_TTL).await {
        report(
            s,
            device_id,
            &operation,
            Err("Z-Wave JS is not connected, so it was not sent".into()),
        )
        .await;
        return;
    }
    s.recent.lock().unwrap_or_else(|e| e.into_inner()).insert(
        node_id,
        (
            cmd.clone(),
            writes.iter().map(|w| w.attribute.clone()).collect(),
            Instant::now(),
        ),
    );
    let mut failed = Vec::new();
    for w in &writes {
        if let Err(e) = set_value(s, node_id, w).await {
            failed.push(format!("{}: {e}", w.attribute));
        }
    }
    let mut result = if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    };
    if result.is_ok() && !unrecognised.is_empty() {
        result = Err(format!(
            "this device can't be set to {}",
            unrecognised.join(", ")
        ));
    }
    report(s, device_id, &operation, result).await;
}

/// A device action (`{"action": …}`) on a node.
async fn device_action(
    s: &Shared,
    node_id: u32,
    action: &str,
    obj: &Map<String, Value>,
) -> Result<(), String> {
    match action {
        "set_code_enabled" => {
            let slot = obj
                .get("slot")
                .and_then(num_u64)
                .ok_or("set_code_enabled needs 'slot'")?;
            let enabled = obj
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("set_code_enabled needs 'enabled' (true or false)")?;
            if !s.client.wait_ready(COMMAND_TTL).await {
                return Err("Z-Wave JS is not connected, so it was not sent".into());
            }
            crate::actions::set_code_enabled(s, node_id, slot as u32, enabled).await
        }
        other => Err(format!("unknown action \"{other}\"")),
    }
}

fn num_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// A failure's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("not connected") {
        "not_connected"
    } else if l.contains("did not answer") || l.contains("closed before") {
        "no_answer"
    } else if l.contains("unknown action") {
        "unknown_action"
    } else if l.contains("needs") || l.contains("can't be set") || l.contains("does not take") {
        "invalid_params"
    } else {
        "rejected"
    }
}

/// Say how a command went: on the device (`last_command_error`, null once
/// one succeeds) and as a `plugin_command_result` event.
pub async fn report(s: &Shared, device_id: &str, operation: &str, result: Result<(), String>) {
    if let Err(e) = &result {
        warn!(device_id, operation, error = %e, "Command failed");
    }
    let now = result.as_ref().err().cloned();
    let changed = {
        let mut held = s.last_errors.lock().unwrap_or_else(|e| e.into_inner());
        let before = held.insert(device_id.to_string(), now.clone());
        before.flatten() != now
    };
    if changed {
        let _ = s
            .publisher
            .publish_state_partial(device_id, &json!({ "last_command_error": now }))
            .await;
        // homeCore takes any state as a sign of life; while Z-Wave JS is
        // away, say again that the device is not reachable.
        if !s.client.is_ready() {
            let _ = s.publisher.publish_availability(device_id, false).await;
        }
    }
    let mut payload = json!({
        "plugin_id": s.publisher.plugin_id(),
        "device_id": device_id,
        "operation": operation,
        "success": result.is_ok(),
    });
    if let Err(e) = &result {
        payload["error"] = json!(e);
        payload["error_code"] = json!(error_code(e));
    }
    if let Err(e) = s
        .publisher
        .publish_event("plugin_command_result", &payload)
        .await
    {
        debug!(device_id, error = %e, "Could not publish the command result");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// What other modules' tests need from here.
#[cfg(test)]
pub mod tests_support {
    use super::*;

    pub fn state_of(node: &NodeState, translator: &Translator) -> Value {
        let state = build_state(node, translator);
        let mut all = state.as_object().cloned().unwrap_or_default();
        all.extend(retired_state(node, translator, &state));
        Value::Object(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(j: Value) -> NodeState {
        NodeState::from_value(&j).expect("test fixture must parse")
    }

    fn classed(generic: &str, specific: &str) -> NodeState {
        node(json!({
            "nodeId": 1,
            "deviceClass": {
                "generic":  { "key": 0, "label": generic },
                "specific": { "key": 0, "label": specific },
            },
        }))
    }

    #[test]
    fn the_class_decides_where_it_is_decisive() {
        assert_eq!(
            device_type_for(&classed("Binary Switch", "Binary Power Switch"), &json!({})),
            Some("switch")
        );
        assert_eq!(
            device_type_for(
                &classed("Entry Control", "Secure Keypad Door Lock"),
                &json!({})
            ),
            Some("lock")
        );
        assert_eq!(
            device_type_for(&classed("Static Controller", "PC Controller"), &json!({})),
            Some("gateway")
        );
        assert_eq!(
            device_type_for(
                &classed("Entry Control", "Secure Barrier Add-on"),
                &json!({})
            ),
            Some("cover")
        );
        assert_eq!(
            device_type_for(
                &classed("Multilevel Switch", "Motor Control Class C"),
                &json!({})
            ),
            Some("cover")
        );
    }

    #[test]
    fn what_the_node_reports_breaks_the_tie_the_class_cannot() {
        let sensor = classed("Notification Sensor", "Notification Sensor");
        assert_eq!(
            device_type_for(&sensor, &json!({ "open": true })),
            Some("contact_sensor")
        );
        assert_eq!(
            device_type_for(&sensor, &json!({ "motion": false, "tampered": false })),
            Some("motion_sensor")
        );
        assert_eq!(
            device_type_for(&sensor, &json!({ "illuminance": 40 })),
            Some("sensor")
        );
        // A lock reports its door too, and is still a lock.
        let bare = node(json!({ "nodeId": 7 }));
        assert_eq!(
            device_type_for(&bare, &json!({ "locked": true, "open": false })),
            Some("lock")
        );
    }

    #[test]
    fn a_node_that_has_not_been_interviewed_claims_nothing() {
        let bare = node(json!({ "nodeId": 7 }));
        assert_eq!(device_type_for(&bare, &json!({})), None);
    }

    /// Every retired spelling is cleared, along with the raw names of values
    /// that now have a shared one or are not state — and nothing the node
    /// publishes now.
    #[test]
    fn retired_names_are_cleared_and_current_ones_kept() {
        let n = node(json!({
            "nodeId": 3,
            "values": [
                { "commandClass": 38, "endpoint": 0, "property": "currentValue", "value": 50 },
                { "commandClass": 68, "endpoint": 0, "property": "mode", "value": 1 },
                { "commandClass": 102, "endpoint": 0, "property": "targetState", "value": 255 },
                { "commandClass": 99, "endpoint": 0, "property": "userIdStatus", "propertyKey": 1, "value": 1 },
                { "commandClass": 112, "endpoint": 0, "property": 3, "value": 7 },
                { "commandClass": 114, "endpoint": 0, "property": "manufacturerId", "value": 99 }
            ]
        }));
        let t = Translator::new();
        t.learn_node(&n);
        let state = build_state(&n, &t);
        let cleared = retired_state(&n, &t, &state);
        let mut names: Vec<&str> = cleared.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "battery",
                "bolt_status",
                "brightness",
                "cc102_targetstate",
                "cc112_3",
                "cc114_manufacturerid",
                "cc38_currentvalue",
                "cc68_mode",
                "cc99_useridstatus_pk1",
                "co2_ppm",
                "contact_open",
                "door_status",
                "latch_status",
                "location",
                "mode",
                "name",
                "tamper",
                "target_temp",
            ]
        );
        assert!(cleared.values().all(Value::is_null));
        for kept in ["brightness_pct", "on", "fan_mode"] {
            assert!(state.get(kept).is_some(), "{kept}");
            assert!(!cleared.contains_key(kept), "{kept}");
        }
        // NOISE: none of these are state any more.
        for gone in ["cc112_3", "cc114_manufacturerid", "name"] {
            assert!(state.get(gone).is_none(), "{gone}");
        }
    }

    /// M10: a node's name and room are not published as state.
    #[test]
    fn name_and_location_are_not_state() {
        let n = node(json!({
            "nodeId": 4, "name": "Plug - Office AC", "location": "Office",
            "values": [ { "commandClass": 37, "endpoint": 0, "property": "currentValue", "value": true } ]
        }));
        let t = Translator::new();
        t.learn_node(&n);
        let state = build_state(&n, &t);
        assert_eq!(state, json!({ "on": true }));
        assert_eq!(display_name(&n), "Plug - Office AC");
        let unnamed = node(json!({ "nodeId": 4, "label": "ZEN15" }));
        assert_eq!(display_name(&unnamed), "ZEN15");
        assert_eq!(display_name(&node(json!({ "nodeId": 4 }))), "zwave_4");
    }

    #[test]
    fn set_value_carries_the_fade_as_options() {
        let mut w = translator::Write {
            command_class: 38,
            endpoint: 0,
            property: "targetValue".into(),
            property_key: None,
            value: json!(99),
            options: Some(json!({ "transitionDuration": "2s" })),
            attribute: "brightness_pct".into(),
        };
        let msg = set_value_msg(4, &w);
        assert_eq!(msg["command"], "node.set_value");
        assert_eq!(msg["value"], 99);
        assert_eq!(msg["options"], json!({ "transitionDuration": "2s" }));
        assert!(msg["valueId"].get("propertyKey").is_none());
        w.options = None;
        w.property_key = Some(json!(1));
        w.endpoint = 2;
        let msg = set_value_msg(4, &w);
        assert!(msg.get("options").is_none());
        assert_eq!(msg["valueId"]["propertyKey"], 1);
        assert_eq!(msg["valueId"]["endpoint"], 2);
    }

    /// M9: only mains-powered nodes are polled at startup — a lock is not.
    #[test]
    fn only_mains_nodes_are_polled() {
        let mains = node(json!({ "nodeId": 1, "isListening": true }));
        let lock =
            node(json!({ "nodeId": 2, "isListening": false, "isFrequentListening": "1000ms" }));
        let sensor =
            node(json!({ "nodeId": 3, "isListening": false, "isFrequentListening": false }));
        let controller =
            node(json!({ "nodeId": 1, "isListening": true, "isControllerNode": true }));
        assert!(should_poll(&mains));
        assert!(!should_poll(&lock));
        assert!(!should_poll(&sensor));
        assert!(!should_poll(&controller));
    }

    fn val(cc: u32, ep: u32, prop: &str) -> Value {
        json!({ "commandClass": cc, "endpoint": ep, "property": prop })
    }

    #[test]
    fn polls_cover_every_endpoint_and_only_primary_values() {
        let n = node(json!({
            "nodeId": 1, "isListening": true,
            "values": [
                val(37, 1, "currentValue"), val(37, 2, "currentValue"),
                val(37, 0, "targetValue"), val(50, 0, "value"), val(98, 0, "currentMode")
            ]
        }));
        assert_eq!(
            primary_state_values_to_poll(&n),
            vec![
                (37, 1, "currentValue".into()),
                (37, 2, "currentValue".into()),
                (98, 0, "currentMode".into()),
            ]
        );
    }

    #[test]
    fn device_ids_round_trip() {
        assert_eq!(node_device_id(5), "zwave_5");
        assert_eq!(node_of_device("zwave_5"), Some(5));
        assert_eq!(node_of_device("hue_5"), None);
        assert_eq!(node_of_device("zwave_x"), None);
    }

    #[test]
    fn errors_have_kinds() {
        assert_eq!(
            error_code("Z-Wave JS is not connected, so it was not sent"),
            "not_connected"
        );
        assert_eq!(
            error_code("Z-Wave JS did not answer within 25 s"),
            "no_answer"
        );
        assert_eq!(error_code("unknown action \"x\""), "unknown_action");
        assert_eq!(error_code("on: the device did not do it"), "rejected");
    }
}
