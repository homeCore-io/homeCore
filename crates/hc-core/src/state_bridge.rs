//! MQTT → state bridge.
//!
//! Subscribes to the event bus, processes `Event::MqttMessage` events, updates
//! the state store, and re-emits typed `Event::DeviceStateChanged` /
//! `Event::DeviceAvailabilityChanged` events for the rule engine and WebSocket
//! clients.
//!
//! Topic patterns handled directly (canonical HomeCore schema):
//! - `homecore/devices/{id}/state`          → full state replace
//! - `homecore/devices/{id}/state/partial`  → JSON merge-patch
//! - `homecore/devices/{id}/availability`   → "online" | "offline"
//! - `homecore/plugins/{id}/register`       → plugin registration
//! - `homecore/plugins/{id}/unregister`     → plugin device retirement
//!
//! Ecosystem-mapped topics:
//! - Any topic matched by the `EcosystemRouter` is translated before processing.
//! - `homecore/devices/{id}/cmd` on a mapped device is relayed to the native
//!   device command topic via the router's outbound path.

use crate::{
    device_naming::{ensure_unique_canonical_name, normalize_name_segment},
    EventBus,
};
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use hc_mqtt_client::PublishHandle;
use hc_state::StateStore;
use hc_topic_map::{
    canonical_device_type_name, DeviceTypeRegistry, EcosystemRouter, InboundResult,
};
use hc_types::device::{
    extract_change_from_command_payload, extract_change_from_state_payload, DeviceChange,
    DeviceState,
};
use hc_types::event::Event;
use hc_types::LogLine;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

pub struct StateBridge {
    bus: EventBus,
    pub_bus: EventBus,
    store: StateStore,
    router: Option<Arc<EcosystemRouter>>,
    publish: Option<PublishHandle>,
    device_types: Option<Arc<DeviceTypeRegistry>>,
    pending_command_changes: DashMap<String, DeviceChange>,
    /// Track which plugins have already emitted a PluginRegistered event
    /// this session, so we only emit once per plugin (not once per device).
    registered_plugins: Mutex<HashSet<String>>,
    /// Track which plugin_ids we've already inspected for SDK-version
    /// mismatch. The check fires once per plugin per session — heartbeats
    /// arrive every 30s and we don't want a periodic warn-spam if a
    /// plugin is on a divergent SDK. Component versioning plan, Phase B.
    seen_sdk_versions: Mutex<HashSet<String>>,
    /// Broadcast sender for the log stream WebSocket — used to inject plugin
    /// log lines received over MQTT into the core's log stream.
    log_tx: Option<broadcast::Sender<LogLine>>,
    /// Ring buffer backing the log stream history replay.
    log_ring: Option<Arc<Mutex<VecDeque<LogLine>>>>,
}

impl StateBridge {
    pub fn new(bus: EventBus, pub_bus: EventBus, store: StateStore) -> Self {
        Self {
            bus,
            pub_bus,
            store,
            router: None,
            publish: None,
            device_types: None,
            pending_command_changes: DashMap::new(),
            registered_plugins: Mutex::new(HashSet::new()),
            seen_sdk_versions: Mutex::new(HashSet::new()),
            log_tx: None,
            log_ring: None,
        }
    }

    pub fn with_router(mut self, router: Arc<EcosystemRouter>) -> Self {
        self.router = Some(router);
        self
    }

    pub fn with_publish(mut self, publish: PublishHandle) -> Self {
        self.publish = Some(publish);
        self
    }

    pub fn with_device_types(mut self, device_types: Arc<DeviceTypeRegistry>) -> Self {
        self.device_types = Some(device_types);
        self
    }

    /// Attach the log stream broadcast channel so plugin logs received over
    /// MQTT are injected into the core's `/logs/stream` WebSocket.
    pub fn with_log_stream(
        mut self,
        tx: broadcast::Sender<LogLine>,
        ring: Arc<Mutex<VecDeque<LogLine>>>,
    ) -> Self {
        self.log_tx = Some(tx);
        self.log_ring = Some(ring);
        self
    }

    /// Drive the bridge until the event bus closes. Spawn in a `tokio::task`.
    pub async fn run(self) {
        let mut rx = self.bus.subscribe();
        info!("State bridge started");
        loop {
            match rx.recv().await {
                Ok(Event::MqttMessage { topic, payload, .. }) => {
                    if let Err(e) = self.handle_mqtt(&topic, &payload).await {
                        warn!(topic, error = %e, "State bridge error");
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("State bridge lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    async fn handle_mqtt(&self, topic: &str, payload: &[u8]) -> Result<()> {
        if let Some(device_id) = parse_cmd_topic(topic) {
            self.record_pending_command_change(device_id, payload);
        }

        // --- Outbound: relay mapped cmd topics to native device topics ---
        if topic.starts_with("homecore/devices/") && topic.ends_with("/cmd") {
            if let Some(router) = &self.router {
                match router.route_outbound(topic, payload) {
                    Ok(Some(results)) => {
                        for result in results {
                            debug!(from = topic, to = %result.target_topic, "Relaying cmd to native topic");
                            if let Some(ph) = &self.publish {
                                if let Err(e) =
                                    ph.publish(&result.target_topic, result.payload).await
                                {
                                    warn!(topic = %result.target_topic, error = %e, "Failed to relay cmd");
                                }
                            } else {
                                warn!("No publish handle — cannot relay cmd to native topic");
                            }
                        }
                        return Ok(()); // Fully handled.
                    }
                    Ok(None) => {} // Not a mapped device; fall through.
                    Err(e) => warn!(topic, error = %e, "Outbound router error"),
                }
            }
        }

        // --- Inbound: try ecosystem router first ---
        if let Some(router) = &self.router {
            match router.route_inbound(topic, payload) {
                Ok(Some(InboundResult::State {
                    device_id,
                    payload: json_payload,
                    partial,
                })) => {
                    return self.handle_state(&device_id, &json_payload, partial).await;
                }
                Ok(Some(InboundResult::Availability {
                    device_id,
                    available,
                })) => {
                    return self.handle_availability(&device_id, available).await;
                }
                Ok(None) => {
                    debug!(
                        topic,
                        "No ecosystem profile match — falling through to canonical handling"
                    );
                }
                Err(e) => warn!(topic, error = %e, "Inbound router error"),
            }
        }

        // --- Canonical HomeCore schema handling ---
        // NOTE: use `split` (unlimited) rather than `splitn(4, ...)`, since
        // 5-part topics like `homecore/plugins/{id}/manage/response` and
        // `/manage/cmd` need parts[4] to match correctly.
        let parts: Vec<&str> = topic.split('/').collect();

        // homecore/devices/{id}/state | state/partial | availability | schema | cmd
        if parts.len() >= 4 && parts[0] == "homecore" && parts[1] == "devices" {
            let device_id = parts[2];
            if payload.is_empty() {
                debug!(
                    device_id,
                    topic, "Ignoring empty payload for canonical device topic"
                );
                return Ok(());
            }
            // Important: `parts` came from `topic.split('/')`, so
            // `homecore/devices/{id}/state/partial` splits into FIVE parts
            // (`state` and `partial` separately). Earlier code matched
            // `parts[3]` against the literal `"state/partial"`, which never
            // hit — every partial publish was silently routed through the
            // full-replace `"state"` branch. That wiped device.attributes
            // on every per-attribute partial, manifesting as devices with
            // only the most-recent single attribute.
            let tail = if parts.len() >= 5 {
                Some(parts[4])
            } else {
                None
            };
            match (parts[3], tail) {
                ("state", None) => {
                    let json: serde_json::Value = serde_json::from_slice(payload)?;
                    return self.handle_state(device_id, &json, false).await;
                }
                ("state", Some("partial")) => {
                    let json: serde_json::Value = serde_json::from_slice(payload)?;
                    return self.handle_state(device_id, &json, true).await;
                }
                ("availability", None) => {
                    let available = matches!(
                        std::str::from_utf8(payload).unwrap_or("").trim(),
                        "online" | "Online" | "1" | "true"
                    );
                    return self.handle_availability(device_id, available).await;
                }
                ("schema", None) => {
                    return self.handle_device_schema(device_id, payload).await;
                }
                _ => {}
            }
        }

        // homecore/plugins/{id}/manage/response
        if parts.len() >= 5
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && parts[3] == "manage"
            && parts[4] == "response"
        {
            if let Ok(resp) = serde_json::from_slice::<serde_json::Value>(payload) {
                let _ = self.pub_bus.publish(Event::Custom {
                    timestamp: Utc::now(),
                    event_type: "plugin_management_response".to_string(),
                    payload: resp,
                });
            }
            return Ok(());
        }

        // homecore/plugins/{id}/logs — forward plugin logs to the log stream
        if parts.len() >= 4 && parts[0] == "homecore" && parts[1] == "plugins" && parts[3] == "logs"
        {
            if let Ok(mut line) = serde_json::from_slice::<LogLine>(payload) {
                // The topic already says who sent this; it used to be thrown
                // away, leaving the tracing target as the only handle on a
                // plugin's logs — and that names the *module*, so a filter
                // built from it keeps hc_caseta's lines and loses the SDK's
                // and rumqttc's from the same process.
                line = line.with_plugin_id(parts[2]);
                if let Some(ref tx) = self.log_tx {
                    // Push into ring buffer for late subscribers.
                    if let Some(ref ring) = self.log_ring {
                        if let Ok(mut r) = ring.lock() {
                            if r.len() >= r.capacity() {
                                r.pop_front();
                            }
                            r.push_back(line.clone());
                        }
                    }
                    let _ = tx.send(line);
                }
            }
            return Ok(());
        }

        // homecore/plugins/{id}/capabilities
        if parts.len() >= 4
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && parts[3] == "capabilities"
        {
            let plugin_id = parts[2];
            // Empty retained payload = "clear manifest"; ignore.
            if payload.is_empty() {
                return Ok(());
            }
            match serde_json::from_slice::<hc_types::Capabilities>(payload) {
                Ok(caps) => {
                    // `config_schema` and `config_descriptor` ride on the manifest
                    // JSON but are not part of the frozen `Capabilities` type;
                    // pull them from the raw payload.
                    let raw = serde_json::from_slice::<serde_json::Value>(payload).ok();
                    let pick = |key: &str| {
                        raw.as_ref()
                            .and_then(|v| v.get(key).cloned())
                            .filter(|v| !v.is_null())
                    };
                    let config_schema = pick("config_schema");
                    let config_descriptor = pick("config_descriptor");

                    let widgets = parse_plugin_widgets(
                        raw.as_ref().and_then(|v| v.get("widgets")),
                        plugin_id,
                    );
                    let _ = self.pub_bus.publish(Event::PluginCapabilities {
                        timestamp: Utc::now(),
                        plugin_id: plugin_id.to_string(),
                        capabilities: caps,
                        widgets,
                        config_schema,
                        config_descriptor,
                    });
                }
                Err(e) => warn!(
                    plugin_id,
                    error = %e,
                    "Discarding malformed plugin capability manifest"
                ),
            }
            return Ok(());
        }

        // homecore/plugins/{id}/heartbeat
        if parts.len() >= 4
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && parts[3] == "heartbeat"
        {
            let plugin_id = parts[2];
            if let Ok(hb) = serde_json::from_slice::<serde_json::Value>(payload) {
                let sdk_version = hb["sdk_version"].as_str().map(str::to_string);
                let protocol_version = hb["protocol_version"].as_str();

                // First-heartbeat-per-plugin protocol check. Warn-only for
                // v0.1.x — refusing on mismatch locks operators out of
                // recoverable states (core upgraded, plugin not yet rebuilt).
                {
                    let mut seen = self.seen_sdk_versions.lock().unwrap();
                    if seen.insert(plugin_id.to_string()) {
                        match check_protocol(protocol_version, hc_types::PROTOCOL_VERSION) {
                            ProtocolCheck::Mismatch => warn!(
                                plugin_id,
                                plugin_protocol_version = protocol_version.unwrap_or("?"),
                                core_protocol_version = hc_types::PROTOCOL_VERSION,
                                plugin_sdk_version = sdk_version.as_deref().unwrap_or("?"),
                                "Plugin was built against a different hc-types wire protocol — \
                                 protocol changes may not be visible. Rebuild the plugin against \
                                 an SDK matching this core if rules or device events misbehave."
                            ),
                            ProtocolCheck::Match => debug!(
                                plugin_id,
                                plugin_protocol_version = protocol_version.unwrap_or("?"),
                                "Plugin protocol version matches core"
                            ),
                            ProtocolCheck::Unknown => debug!(
                                plugin_id,
                                plugin_sdk_version = sdk_version.as_deref().unwrap_or("?"),
                                "Plugin heartbeat carries no protocol_version — built against an \
                                 SDK older than 0.3.9, so compatibility cannot be determined"
                            ),
                        }
                    }
                }

                let notices = parse_plugin_notices(hb.get("notices"), plugin_id);

                let _ = self.pub_bus.publish(Event::PluginHeartbeat {
                    timestamp: Utc::now(),
                    plugin_id: plugin_id.to_string(),
                    version: hb["version"].as_str().map(str::to_string),
                    sdk_version,
                    uptime_secs: hb["uptime_secs"].as_u64(),
                    device_count: hb["device_count"].as_u64().map(|n| n as u32),
                    notices,
                });
            }
            return Ok(());
        }

        // homecore/plugins/{id}/register | unregister
        if parts.len() >= 4
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && (parts[3] == "register" || parts[3] == "unregister")
        {
            let plugin_id = parts[2];
            if parts[3] == "register" {
                // Only emit PluginRegistered once per plugin per session —
                // plugins send one registration message per device, which
                // would flood the event stream with duplicates.
                {
                    let mut seen = self.registered_plugins.lock().unwrap();
                    if seen.insert(plugin_id.to_string()) {
                        let _ = self.pub_bus.publish(Event::PluginRegistered {
                            timestamp: Utc::now(),
                            plugin_id: plugin_id.to_string(),
                        });
                    }
                }
                if let Err(e) = self.handle_device_registration(plugin_id, payload).await {
                    warn!(plugin_id, error = %e, "Device registration upsert failed");
                }
            } else if let Err(e) = self.handle_device_unregistration(plugin_id, payload).await {
                warn!(plugin_id, error = %e, "Device unregister failed");
            }
            return Ok(());
        }

        debug!(
            topic,
            "Topic not handled by any profile or canonical pattern — ignored"
        );
        Ok(())
    }

    async fn handle_state(
        &self,
        device_id: &str,
        incoming: &serde_json::Value,
        partial: bool,
    ) -> Result<()> {
        let attrs = match incoming.as_object() {
            Some(m) => m.clone(),
            None => {
                warn!(device_id, "State payload is not a JSON object; ignoring");
                return Ok(());
            }
        };

        let mut device = self.store.get_device(device_id).await?.unwrap_or_else(|| {
            // Derive plugin_id from the device_id prefix convention:
            // "shelly_abc" → "shelly", "tasmota_abc" → "tasmota", etc.
            let plugin_id = device_id.split('_').next().unwrap_or("unknown");
            DeviceState::new(device_id, device_id, plugin_id)
        });

        let previous = device.attributes.clone();
        let previous_name = device.name.clone();
        let change = self.resolve_state_change(device_id, incoming);
        let mut attrs = attrs;
        attrs.remove("_hc");

        // Extract "name" before attrs is potentially consumed by into_iter().
        let incoming_name: Option<String> = attrs
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if partial {
            apply_partial_merge_patch(&mut device.attributes, &attrs);
        } else {
            device.attributes = attrs.into_iter().collect();
        }
        device.last_seen = Utc::now();
        device.available = true;
        device.last_change = Some(change.clone());

        // Sync display name from the "name" attribute when it arrives in a state
        // update (e.g. from ZwaveJS UI nodeInfo). Keeps device.name in sync with
        // the node name set in the ecosystem controller without a full registration.
        if let Some(new_name) = incoming_name {
            device.name = new_name;
        }

        if device.canonical_name.is_none() {
            let devices = self.store.list_devices().await?;
            device.canonical_name = Some(ensure_unique_canonical_name(&device, &devices));
        }

        self.store.upsert_device(&device).await?;

        // Fire DeviceNameChanged if the name attribute caused a rename.
        if device.name != previous_name {
            info!(
                device_id,
                previous_name = %previous_name,
                current_name  = %device.name,
                "Device name changed via state attribute"
            );
            let _ = self.pub_bus.publish(Event::DeviceNameChanged {
                timestamp: Utc::now(),
                device_id: device_id.to_string(),
                previous_name,
                current_name: device.name.clone(),
            });
        }

        let current = device.attributes.clone();
        debug!(device_id, "Device state updated");

        // Compute which attribute keys actually changed (added, updated, or removed).
        let mut changed: Vec<String> = current
            .keys()
            .filter(|k| previous.get(*k) != current.get(*k))
            .cloned()
            .collect();
        for k in previous.keys() {
            if !current.contains_key(k) && !changed.contains(k) {
                changed.push(k.clone());
            }
        }

        let history_entries: Vec<(String, Value)> = current
            .iter()
            .filter(|(attr, val)| previous.get(*attr) != Some(*val))
            .map(|(attr, val)| (attr.clone(), val.clone()))
            .collect();

        // Only publish if at least one attribute value actually changed.
        if !changed.is_empty() {
            let _ = self.pub_bus.publish(Event::DeviceStateChanged {
                timestamp: Utc::now(),
                device_id: device_id.to_string(),
                device_name: Some(device.effective_name().to_string()),
                previous,
                current,
                changed,
                change,
            });
        }

        Self::persist_history_async(self.store.clone(), device_id.to_string(), history_entries);

        Ok(())
    }

    /// Parse a plugin registration payload and upsert the device record.
    ///
    /// If the device already exists and the name has changed, the stored name is
    /// updated and a [`Event::DeviceNameChanged`] event is emitted so that API
    /// clients and the WebSocket stream are notified immediately.
    ///
    /// This is the single point where registration is treated as an upsert —
    /// both new registrations and re-registrations (e.g. after a source rename)
    /// go through here.
    async fn handle_device_registration(&self, plugin_id: &str, payload: &[u8]) -> Result<()> {
        let json: serde_json::Value = serde_json::from_slice(payload)?;

        // Both old-style (capabilities) and new-style (device_type) payloads
        // carry these common fields.
        let device_id = json["device_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("registration missing device_id"))?;
        let new_name = json["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("registration missing name"))?;
        // Canonicalize the area exactly as `device_type` is canonicalized below.
        //
        // `device.area` holds a normalized slug (`living_room`); the UI renders a
        // pretty label from it, and `derive_areas_from_devices`,
        // `set_area_devices`, and `area_id_from_name` all key on the normalized
        // form. Plugins, though, report whatever the upstream system calls the
        // room — Z-Wave JS says "Living Room" — and that string used to be stored
        // verbatim. The same room then existed twice, as `Living Room` and
        // `living_room`, and anything grouping devices by the raw string split it
        // in two: a duplicate room appeared, and the devices landed in neither.
        //
        // Normalizing here means plugins can keep reporting the upstream label
        // and core owns the canonical form — which is the whole point of having
        // one.
        let area = json["area"]
            .as_str()
            .map(normalize_name_segment)
            .filter(|a| !a.is_empty());
        if let Some(raw) = json["area"].as_str() {
            if let Some(canonical) = area.as_deref() {
                if raw != canonical {
                    debug!(
                        device_id,
                        raw_area = raw,
                        canonical_area = canonical,
                        "Normalized plugin-reported area"
                    );
                }
            }
        }
        // Hardware identity, when the upstream system knows it. Absent leaves
        // whatever is already stored alone: a plugin that has not been taught
        // to report these yet must not blank out what an earlier version, or
        // another code path, already learned.
        let manufacturer = hardware_field(&json, "manufacturer");
        let model = hardware_field(&json, "model");
        let sw_version = hardware_field(&json, "sw_version");
        // A device cannot sit behind itself. A plugin that says so has a bug,
        // and storing it would make anything walking parents loop forever.
        let parent_device_id = hardware_field(&json, "parent_device_id").filter(|p| p != device_id);

        let raw_device_type = json["device_type"].as_str().map(str::to_string);
        let device_type = raw_device_type.as_deref().map(canonical_device_type_name);

        if let (Some(raw), Some(canonical)) = (raw_device_type.as_deref(), device_type.as_deref()) {
            if raw != canonical {
                info!(
                    device_id,
                    raw_device_type = raw,
                    canonical_device_type = canonical,
                    "Normalized device_type alias"
                );
            }
        }

        match self.store.get_device(device_id).await? {
            Some(mut existing) => {
                let previous_name = existing.name.clone();

                // Always keep metadata in sync with what the plugin reports.
                existing.plugin_id = plugin_id.to_string();
                if area.is_some() {
                    existing.area = area;
                }
                if let Some(dt) = device_type.as_ref() {
                    existing.device_type = Some(dt.clone());
                }
                if manufacturer.is_some() {
                    existing.manufacturer = manufacturer.clone();
                }
                if model.is_some() {
                    existing.model = model.clone();
                }
                // Firmware changes under a device that is otherwise the same
                // one, so this is the field most worth keeping current.
                if sw_version.is_some() {
                    existing.sw_version = sw_version.clone();
                }
                if parent_device_id.is_some() {
                    existing.parent_device_id = parent_device_id.clone();
                }
                existing.name = new_name.to_string();
                if existing.canonical_name.is_none() {
                    let devices = self.store.list_devices().await?;
                    existing.canonical_name =
                        Some(ensure_unique_canonical_name(&existing, &devices));
                }

                // Always persist — ensures name/plugin_id/area are correct even
                // when the device was auto-created from a retained state message
                // before registration arrived.
                self.store.upsert_device(&existing).await?;

                if previous_name != new_name {
                    info!(
                        device_id,
                        previous_name = %previous_name,
                        current_name  = %new_name,
                        "Device name changed"
                    );
                    let _ = self.pub_bus.publish(Event::DeviceNameChanged {
                        timestamp: Utc::now(),
                        device_id: device_id.to_string(),
                        previous_name,
                        current_name: new_name.to_string(),
                    });
                }
            }
            None => {
                // First registration — create the device record.
                let mut device = DeviceState::new(device_id, new_name, plugin_id);
                device.area = area;
                device.device_type = device_type.clone();
                device.manufacturer = manufacturer.clone();
                device.model = model.clone();
                device.sw_version = sw_version.clone();
                device.parent_device_id = parent_device_id.clone();
                let devices = self.store.list_devices().await?;
                device.canonical_name = Some(ensure_unique_canonical_name(&device, &devices));
                self.store.upsert_device(&device).await?;
                info!(device_id, name = new_name, plugin_id, "Device registered");
            }
        }

        if let Some(device_type) = device_type.as_deref() {
            if let Some(registry) = &self.device_types {
                match registry.get_device_schema(device_type) {
                    Some(schema) => {
                        self.store.upsert_device_schema(device_id, &schema).await?;
                        debug!(device_id, device_type, "Typed device schema stored");
                    }
                    None => {
                        warn!(
                            device_id,
                            device_type, "Unknown device_type; no schema resolved"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_device_schema(&self, device_id: &str, payload: &[u8]) -> Result<()> {
        let schema: hc_types::DeviceSchema = serde_json::from_slice(payload)?;
        self.store.upsert_device_schema(device_id, &schema).await?;
        debug!(device_id, "Device schema stored");
        Ok(())
    }

    async fn handle_device_unregistration(&self, plugin_id: &str, payload: &[u8]) -> Result<()> {
        let json: serde_json::Value = serde_json::from_slice(payload)?;
        let device_id = json["device_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("unregister missing device_id"))?;

        if let Some(existing) = self.store.get_device(device_id).await? {
            if existing.plugin_id != plugin_id {
                warn!(
                    device_id,
                    claimed_plugin_id = plugin_id,
                    actual_plugin_id = %existing.plugin_id,
                    "Ignoring unregister for device owned by another plugin"
                );
                return Ok(());
            }
        }

        let device_removed = self.store.delete_device(device_id).await?;
        let schema_removed = self.store.delete_device_schema(device_id).await?;

        if device_removed || schema_removed {
            let _ = self.pub_bus.publish(Event::Custom {
                timestamp: Utc::now(),
                event_type: "device_deleted".to_string(),
                payload: serde_json::json!({
                    "device_id": device_id,
                    "plugin_id": plugin_id,
                    "source": "plugin_unregister",
                }),
            });
            info!(
                device_id,
                plugin_id, device_removed, schema_removed, "Device unregistered"
            );
        } else {
            debug!(
                device_id,
                plugin_id, "Unregister ignored for unknown device"
            );
        }

        Ok(())
    }

    async fn handle_availability(&self, device_id: &str, available: bool) -> Result<()> {
        let mut device = self.store.get_device(device_id).await?.unwrap_or_else(|| {
            let plugin_id = device_id.split('_').next().unwrap_or("unknown");
            DeviceState::new(device_id, device_id, plugin_id)
        });

        let changed = device.available != available;
        device.available = available;
        device.last_seen = Utc::now();
        if device.canonical_name.is_none() {
            let devices = self.store.list_devices().await?;
            device.canonical_name = Some(ensure_unique_canonical_name(&device, &devices));
        }
        self.store.upsert_device(&device).await?;

        // Only emit an event when availability actually changes — plugins
        // re-publish availability on every refresh cycle, and emitting an
        // event each time floods the activity stream with no-op updates.
        if changed {
            let _ = self.pub_bus.publish(Event::DeviceAvailabilityChanged {
                timestamp: Utc::now(),
                device_id: device_id.to_string(),
                device_name: Some(device.effective_name().to_string()),
                available,
            });
        }

        Ok(())
    }
}

impl StateBridge {
    fn persist_history_async(store: StateStore, device_id: String, entries: Vec<(String, Value)>) {
        if entries.is_empty() {
            return;
        }

        tokio::spawn(async move {
            for (attribute, value) in entries {
                if let Err(error) = store.append_history(&device_id, &attribute, &value).await {
                    warn!(device_id, attribute, %error, "Failed to append state history");
                }
            }
        });
    }

    fn record_pending_command_change(&self, device_id: &str, payload: &[u8]) {
        let Ok(command) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return;
        };
        let Some(change) = extract_change_from_command_payload(&command) else {
            return;
        };
        self.pending_command_changes
            .insert(device_id.to_string(), change);
    }

    fn resolve_state_change(&self, device_id: &str, incoming: &serde_json::Value) -> DeviceChange {
        // Take ownership of any pending command provenance up front so we do
        // not hold a DashMap guard while deciding whether to discard it.
        if let Some((_, pending)) = self.pending_command_changes.remove(device_id) {
            if (Utc::now() - pending.changed_at).num_seconds() <= 5 {
                if let Some(explicit) = extract_change_from_state_payload(incoming) {
                    if is_generic_plugin_external_change(&explicit) {
                        return pending;
                    }
                    return explicit;
                }

                return pending;
            }
        }

        if let Some(change) = extract_change_from_state_payload(incoming) {
            return change;
        }

        DeviceChange::unknown()
    }
}

fn is_generic_plugin_external_change(change: &DeviceChange) -> bool {
    change.kind == hc_types::device::DeviceChangeKind::External
        && change.correlation_id.is_none()
        && change.actor_id.is_none()
        && change.actor_name.is_none()
}

/// Decode the `notices` array from a plugin heartbeat.
///
/// Tolerant on purpose, in two directions. A missing or non-array field yields
/// an empty list — that is every plugin built against an SDK without notices,
/// and it is indistinguishable from "nothing to report", which is the correct
/// reading. And a single malformed entry is dropped rather than failing the
/// batch: notices are advisory, while the heartbeat carrying them is how core
/// knows the plugin is alive. Refusing the whole beat over a bad diagnostic
/// would mark a healthy plugin offline — the diagnostic causing an outage it
/// was meant to describe.
fn parse_plugin_notices(
    raw: Option<&serde_json::Value>,
    plugin_id: &str,
) -> Vec<hc_types::PluginNotice> {
    let Some(serde_json::Value::Array(items)) = raw else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            serde_json::from_value::<hc_types::PluginNotice>(v.clone())
                .map_err(
                    |e| warn!(plugin_id, error = %e, "Discarding an unparseable plugin notice"),
                )
                .ok()
        })
        .collect()
}

/// Decode the `widgets` array from a plugin's capability manifest.
///
/// Tolerant in the same two directions as [`parse_plugin_notices`], and for a
/// reason of its own. A missing or non-array field is every plugin built
/// against an SDK without widgets, and is indistinguishable from "contributes
/// none". A single bad entry is dropped rather than failing the batch: reading
/// the array as `Vec<WidgetDescriptor>` in one go would let one typo take every
/// other card down with it, silently, and a plugin shipping four cards and one
/// mistake should serve four.
///
/// The *reason* is logged against the widget id, because a rejection is the
/// whole diagnostic a plugin author gets — core will not draw the card and
/// nothing else will tell them why.
///
/// Validating here rather than at every reader is what makes
/// [`hc_types::widget_descriptor::WidgetDescriptor`] worth having typed: a
/// descriptor that reaches `PluginRecord` has already been checked, so no
/// client has to wonder whether this one can be drawn.
fn parse_plugin_widgets(
    raw: Option<&serde_json::Value>,
    plugin_id: &str,
) -> Vec<hc_types::widget_descriptor::WidgetDescriptor> {
    let Some(serde_json::Value::Array(items)) = raw else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let widget_id = item
                .get("widget_id")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            let descriptor =
                serde_json::from_value::<hc_types::widget_descriptor::WidgetDescriptor>(
                    item.clone(),
                )
                .map_err(|e| {
                    warn!(plugin_id, widget_id, error = %e,
                          "Discarding a plugin widget core could not read")
                })
                .ok()?;
            match hc_types::widget_descriptor::validate(&descriptor) {
                Ok(()) => Some(descriptor),
                Err(reason) => {
                    warn!(
                        plugin_id,
                        widget_id, reason, "Discarding a plugin widget some client could not draw"
                    );
                    None
                }
            }
        })
        .collect()
}
/// Outcome of comparing a plugin's wire protocol against core's.
#[derive(Debug, PartialEq, Eq)]
enum ProtocolCheck {
    /// Same protocol — safe to talk.
    Match,
    /// Divergent protocol; the plugin may not see everything core sends.
    Mismatch,
    /// The plugin did not say, so there is nothing to compare.
    Unknown,
}

/// Decide whether a plugin's wire protocol matches core's.
///
/// **Both sides must be the same version line.** This used to compare the
/// plugin's `plugin-sdk-rs` version against `hc_types::PROTOCOL_VERSION` —
/// two crates versioned independently, so 0.3.x was measured against 0.1.x and,
/// with MINOR breaking below 1.0, the check could never pass. It warned on every
/// heartbeat from every plugin, including ones working perfectly, which is worse
/// than not checking: a warning that is always on carries no information and
/// trains you to ignore the one that matters.
///
/// The plugin now reports the `hc-types` version it was compiled against, which
/// is the thing that actually defines the wire format, and core compares that
/// against its own.
///
/// `None` means the plugin is on an SDK older than 0.3.9 and did not send the
/// field. That is [`ProtocolCheck::Unknown`], not a mismatch — we genuinely
/// cannot tell, and guessing "broken" would recreate the false positive this
/// replaces.
fn check_protocol(plugin: Option<&str>, core: &str) -> ProtocolCheck {
    match plugin {
        None => ProtocolCheck::Unknown,
        Some(v) if sdk_versions_compatible(v, core) => ProtocolCheck::Match,
        Some(_) => ProtocolCheck::Mismatch,
    }
}

/// Compare two SemVer-shaped strings for wire-protocol compatibility.
///
/// Pre-1.0 (`0.x.y`) treats MINOR as the breaking position — a 0.1.x →
/// 0.2.x bump is a wire-protocol change, but 0.1.2 → 0.1.5 stays
/// compatible. Once on 1.0+, MAJOR is the breaking position.
///
/// Returns `true` if the two versions can talk to each other safely.
/// Unparseable versions return `true` (don't refuse on garbage — the
/// caller already treats this as warn-only). Component versioning Phase B.
fn sdk_versions_compatible(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64)> {
        let mut parts = s.split('.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next()?.parse().ok()?;
        Some((major, minor))
    };
    let Some((a_major, a_minor)) = parse(a) else {
        return true;
    };
    let Some((b_major, b_minor)) = parse(b) else {
        return true;
    };
    if a_major == 0 || b_major == 0 {
        // 0.x: minor is the breaking position. Both must match major + minor.
        a_major == b_major && a_minor == b_minor
    } else {
        // 1.0+: only major matters.
        a_major == b_major
    }
}

fn parse_cmd_topic(topic: &str) -> Option<&str> {
    let parts: Vec<&str> = topic.splitn(4, '/').collect();
    if parts.len() == 4 && parts[0] == "homecore" && parts[1] == "devices" && parts[3] == "cmd" {
        return Some(parts[2]);
    }
    None
}

fn apply_partial_merge_patch(
    target: &mut HashMap<String, Value>,
    patch: &serde_json::Map<String, Value>,
) {
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// One hardware-identity field from a registration payload.
///
/// Absent, null, empty and whitespace all mean *the plugin did not say*, and
/// the caller leaves whatever core already stored alone. A plugin that learns
/// its firmware only after the first poll re-registers with it later; treating
/// "" as an answer would blank the field it is about to fill.
fn hardware_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_partial_merge_patch, check_protocol, is_generic_plugin_external_change,
        parse_plugin_notices, parse_plugin_widgets, sdk_versions_compatible, ProtocolCheck,
    };
    use hc_types::device::{DeviceChange, DeviceChangeKind};
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn notices_absent_or_wrong_shape_yields_empty() {
        // Every plugin on an SDK without notices lands here. "No field" and
        // "nothing to report" must be the same thing, with no warning noise.
        assert!(parse_plugin_notices(None, "p").is_empty());
        assert!(parse_plugin_notices(Some(&json!("not-an-array")), "p").is_empty());
        assert!(parse_plugin_notices(Some(&json!({})), "p").is_empty());
        assert!(parse_plugin_notices(Some(&json!([])), "p").is_empty());
    }

    #[test]
    fn notices_decode_with_and_without_remedy() {
        let got = parse_plugin_notices(
            Some(&json!([
                {"level": "warning", "code": "a", "message": "m1", "remedy": "do x"},
                {"level": "error",   "code": "b", "message": "m2"}
            ])),
            "p",
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].code, "a");
        assert_eq!(got[0].remedy.as_deref(), Some("do x"));
        assert_eq!(got[1].level, hc_types::NoticeLevel::Error);
        assert!(got[1].remedy.is_none());
    }

    #[test]
    fn one_bad_notice_does_not_discard_the_others() {
        // The heartbeat is how core knows the plugin is alive. Failing the
        // batch over a malformed diagnostic would mark a healthy plugin
        // offline — the notice causing the outage it was meant to report.
        let got = parse_plugin_notices(
            Some(&json!([
                {"level": "warning", "code": "good", "message": "m"},
                {"level": "nonsense", "code": "bad", "message": "m"},
                {"code": "missing-level", "message": "m"},
                {"level": "info", "code": "also_good", "message": "m"}
            ])),
            "p",
        );
        let codes: Vec<&str> = got.iter().map(|n| n.code.as_str()).collect();
        assert_eq!(codes, vec!["good", "also_good"]);
    }

    #[test]
    fn widgets_absent_or_wrong_shape_yields_empty() {
        // Every plugin on an SDK without widgets lands here. "No field" and
        // "contributes none" must be the same thing, with no warning noise.
        assert!(parse_plugin_widgets(None, "p").is_empty());
        assert!(parse_plugin_widgets(Some(&json!("not-an-array")), "p").is_empty());
        assert!(parse_plugin_widgets(Some(&json!({})), "p").is_empty());
        assert!(parse_plugin_widgets(Some(&json!([])), "p").is_empty());
    }

    #[test]
    fn one_unrenderable_widget_does_not_discard_the_others() {
        // The manifest is how core learns everything about a plugin. Failing
        // the batch over one bad card would cost the plugin every card it got
        // right — and the author would see a plugin that contributes nothing
        // rather than a card with a typo in it.
        let got = parse_plugin_widgets(
            Some(&json!([
                {
                    "widget_id": "good",
                    "title": "Good",
                    "render": {"kind": "gauge", "value": "flow"}
                },
                {
                    // Code with no render: the portability guarantee.
                    "widget_id": "web_only",
                    "title": "Web only",
                    "code": {"entry": "x.html"}
                },
                {
                    // An element nothing knows how to draw.
                    "widget_id": "unknown_kind",
                    "title": "Unknown",
                    "render": {"kind": "sparkline"}
                },
                {
                    // Not a descriptor at all — decode fails before validation.
                    "widget_id": "malformed",
                    "render": {"kind": "gauge", "value": "flow"}
                },
                {
                    "widget_id": "also_good",
                    "title": "Also good",
                    "render": {"kind": "row", "children": [
                        {"kind": "text", "content": "Flow"}
                    ]}
                }
            ])),
            "p",
        );
        let ids: Vec<&str> = got.iter().map(|w| w.widget_id.as_str()).collect();
        assert_eq!(ids, vec!["good", "also_good"]);
    }

    #[test]
    fn a_widget_reaching_a_record_has_already_been_validated() {
        // The property that makes the descriptor worth typing: no client has to
        // re-check one, because an invalid one never got this far.
        let got = parse_plugin_widgets(
            Some(&json!([{
                "widget_id": "boiler_flow",
                "title": "Boiler flow",
                "bindings": [{
                    "name": "flow",
                    "device": "{{config.device_id}}",
                    "key": "flow_lpm",
                    "in_from": 0.0, "in_to": 30.0, "out_from": 0.0, "out_to": 1.0
                }],
                "render": {"kind": "gauge", "value": "flow"}
            }])),
            "p",
        );
        assert_eq!(got.len(), 1);
        assert!(hc_types::widget_descriptor::validate(&got[0]).is_ok());
    }

    #[test]
    fn protocol_check_compares_like_against_like() {
        // The bug this replaces: the plugin's plugin-sdk-rs version (0.3.x) was
        // compared against hc_types::PROTOCOL_VERSION (0.1.x). Independent
        // version lines, MINOR breaking below 1.0 — so it never passed, and
        // warned on every heartbeat from every plugin, including hc-caseta
        // while it drove nine devices correctly.
        assert_eq!(check_protocol(Some("0.1.5"), "0.1.5"), ProtocolCheck::Match);
        assert_eq!(check_protocol(Some("0.1.2"), "0.1.5"), ProtocolCheck::Match);
        // An SDK version in the old namespace is a genuine mismatch now, not a
        // permanent false alarm.
        assert_eq!(
            check_protocol(Some("0.3.8"), "0.1.5"),
            ProtocolCheck::Mismatch
        );
        assert_eq!(
            check_protocol(Some("0.2.0"), "0.1.5"),
            ProtocolCheck::Mismatch
        );
    }

    #[test]
    fn protocol_check_is_silent_when_the_plugin_did_not_say() {
        // Every plugin on an SDK older than 0.3.9. We cannot tell, and calling
        // that "broken" would recreate the false positive being removed.
        assert_eq!(check_protocol(None, "0.1.5"), ProtocolCheck::Unknown);
    }

    #[test]
    fn protocol_check_tolerates_garbage() {
        // sdk_versions_compatible returns true on unparseable input by design —
        // do not refuse on garbage. Confirm that path lands on Match, not a
        // spurious warning.
        assert_eq!(
            check_protocol(Some("not-a-version"), "0.1.5"),
            ProtocolCheck::Match
        );
        assert_eq!(check_protocol(Some(""), "0.1.5"), ProtocolCheck::Match);
    }

    #[test]
    fn sdk_compat_pre_1_0_minor_is_breaking() {
        assert!(sdk_versions_compatible("0.1.2", "0.1.5")); // patch ok
        assert!(sdk_versions_compatible("0.1.0", "0.1.0")); // identical
        assert!(!sdk_versions_compatible("0.1.2", "0.2.0")); // minor breaks
        assert!(!sdk_versions_compatible("0.1.2", "0.0.9")); // minor breaks
    }

    #[test]
    fn sdk_compat_post_1_0_only_major_matters() {
        assert!(sdk_versions_compatible("1.4.2", "1.7.0")); // minor ok at 1.x
        assert!(!sdk_versions_compatible("1.4.2", "2.0.0")); // major breaks
        assert!(!sdk_versions_compatible("1.4.2", "0.9.0")); // major breaks (one is 0.x)
    }

    #[test]
    fn sdk_compat_unparseable_is_lenient() {
        // Don't refuse on garbage — caller treats this as warn-only,
        // and we'd rather not fire spurious warnings on malformed input.
        assert!(sdk_versions_compatible("garbage", "0.1.2"));
        assert!(sdk_versions_compatible("0.1.2", ""));
        assert!(sdk_versions_compatible("", ""));
    }

    #[test]
    fn partial_merge_patch_removes_null_fields() {
        let mut target = HashMap::new();
        target.insert("motion".to_string(), json!(true));
        target.insert("temperature".to_string(), json!(72.5));
        target.insert("legacy".to_string(), json!("stale"));

        let mut patch = serde_json::Map::new();
        patch.insert("temperature".to_string(), json!(70.0));
        patch.insert("legacy".to_string(), serde_json::Value::Null);
        patch.insert("illuminance".to_string(), json!(145.0));

        apply_partial_merge_patch(&mut target, &patch);

        assert_eq!(target.get("motion"), Some(&json!(true)));
        assert_eq!(target.get("temperature"), Some(&json!(70.0)));
        assert_eq!(target.get("illuminance"), Some(&json!(145.0)));
        assert!(!target.contains_key("legacy"));
    }

    #[test]
    fn generic_external_plugin_change_is_detected() {
        let change = DeviceChange::external("plugin.hue");
        assert!(is_generic_plugin_external_change(&change));
    }

    #[test]
    fn homecore_change_is_not_treated_as_generic_external() {
        let change = DeviceChange {
            kind: DeviceChangeKind::Homecore,
            ..DeviceChange::unknown()
        };
        assert!(!is_generic_plugin_external_change(&change));
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::hardware_field;
    use serde_json::json;

    /// Absent must not read as "clear it". A plugin learns manufacturer at
    /// discovery and firmware only after the first poll, so most registrations
    /// carry some fields and not others — and the missing ones are silence, not
    /// an instruction.
    #[test]
    fn absent_null_and_blank_are_all_silence() {
        let payload = json!({
            "manufacturer": "Acme",
            "model": null,
            "sw_version": "   ",
        });
        assert_eq!(
            hardware_field(&payload, "manufacturer").as_deref(),
            Some("Acme")
        );
        assert!(hardware_field(&payload, "model").is_none());
        assert!(hardware_field(&payload, "sw_version").is_none());
        assert!(hardware_field(&payload, "missing").is_none());
    }

    /// Trimmed, because these are labels an operator reads beside each other
    /// and a stray space makes two identical models look like two models.
    #[test]
    fn values_are_trimmed() {
        let payload = json!({ "model": "  A1  " });
        assert_eq!(hardware_field(&payload, "model").as_deref(), Some("A1"));
    }

    /// A device cannot sit behind itself, and storing it would make anything
    /// walking parents loop forever. The guard lives at the parse, so no
    /// consumer has to remember it.
    #[test]
    fn a_device_cannot_be_its_own_parent() {
        let payload = json!({ "parent_device_id": "dev1" });
        let parsed = hardware_field(&payload, "parent_device_id").filter(|p| p != "dev1");
        assert!(parsed.is_none());
        let other = json!({ "parent_device_id": "bridge1" });
        assert_eq!(
            hardware_field(&other, "parent_device_id")
                .filter(|p| p != "dev1")
                .as_deref(),
            Some("bridge1")
        );
    }

    /// A number is not a version string. Anything non-string is silence rather
    /// than a stringified surprise.
    #[test]
    fn a_non_string_is_silence() {
        let payload = json!({ "sw_version": 2 });
        assert!(hardware_field(&payload, "sw_version").is_none());
    }
}
