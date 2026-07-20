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
            if let Ok(line) = serde_json::from_slice::<LogLine>(payload) {
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
                    let _ = self.pub_bus.publish(Event::PluginCapabilities {
                        timestamp: Utc::now(),
                        plugin_id: plugin_id.to_string(),
                        capabilities: caps,
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

                // First-heartbeat-per-plugin: log + check SDK compat.
                // Component versioning plan, Phase B. Warn-only for v0.1.x —
                // refusing on mismatch locks operators out of recoverable
                // states (core upgraded, plugin not yet rebuilt).
                {
                    let mut seen = self.seen_sdk_versions.lock().unwrap();
                    if seen.insert(plugin_id.to_string()) {
                        match sdk_version.as_deref() {
                            Some(v) => {
                                if !sdk_versions_compatible(v, hc_types::PROTOCOL_VERSION) {
                                    warn!(
                                        plugin_id,
                                        plugin_sdk_version = v,
                                        core_compat_version = hc_types::PROTOCOL_VERSION,
                                        "Plugin SDK version differs from core's expected SDK \
                                         major/minor — protocol changes may not be visible. \
                                         Rebuild the plugin against a matching SDK if rules \
                                         or device events misbehave."
                                    );
                                } else {
                                    debug!(
                                        plugin_id,
                                        plugin_sdk_version = v,
                                        "Plugin SDK version matches core (compat check passed)"
                                    );
                                }
                            }
                            None => {
                                debug!(
                                    plugin_id,
                                    "Plugin heartbeat carries no sdk_version field — \
                                     plugin built against pre-Phase-B SDK (≤ 0.1.2)"
                                );
                            }
                        }
                    }
                }

                let _ = self.pub_bus.publish(Event::PluginHeartbeat {
                    timestamp: Utc::now(),
                    plugin_id: plugin_id.to_string(),
                    version: hb["version"].as_str().map(str::to_string),
                    sdk_version,
                    uptime_secs: hb["uptime_secs"].as_u64(),
                    device_count: hb["device_count"].as_u64().map(|n| n as u32),
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

/// Compare two SemVer-shaped strings for SDK protocol compatibility.
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

#[cfg(test)]
mod tests {
    use super::{
        apply_partial_merge_patch, is_generic_plugin_external_change, sdk_versions_compatible,
    };
    use hc_types::device::{DeviceChange, DeviceChangeKind};
    use serde_json::json;
    use std::collections::HashMap;

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
