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

use crate::{device_naming::ensure_unique_canonical_name, EventBus};
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
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct StateBridge {
    bus: EventBus,
    pub_bus: EventBus,
    store: StateStore,
    router: Option<Arc<EcosystemRouter>>,
    publish: Option<PublishHandle>,
    device_types: Option<Arc<DeviceTypeRegistry>>,
    pending_command_changes: DashMap<String, DeviceChange>,
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
        let parts: Vec<&str> = topic.splitn(4, '/').collect();

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
            match parts[3] {
                "state" => {
                    let json: serde_json::Value = serde_json::from_slice(payload)?;
                    return self.handle_state(device_id, &json, false).await;
                }
                "state/partial" => {
                    let json: serde_json::Value = serde_json::from_slice(payload)?;
                    return self.handle_state(device_id, &json, true).await;
                }
                "availability" => {
                    let available = matches!(
                        std::str::from_utf8(payload).unwrap_or("").trim(),
                        "online" | "Online" | "1" | "true"
                    );
                    return self.handle_availability(device_id, available).await;
                }
                "schema" => {
                    return self.handle_device_schema(device_id, payload).await;
                }
                _ => {}
            }
        }

        // homecore/plugins/{id}/register | unregister
        if parts.len() >= 4
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && (parts[3] == "register" || parts[3] == "unregister")
        {
            let plugin_id = parts[2];
            if parts[3] == "register" {
                let _ = self.pub_bus.publish(Event::PluginRegistered {
                    timestamp: Utc::now(),
                    plugin_id: plugin_id.to_string(),
                });
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
        let area = json["area"].as_str().map(str::to_string);
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

        device.available = available;
        device.last_seen = Utc::now();
        if device.canonical_name.is_none() {
            let devices = self.store.list_devices().await?;
            device.canonical_name = Some(ensure_unique_canonical_name(&device, &devices));
        }
        self.store.upsert_device(&device).await?;

        let _ = self.pub_bus.publish(Event::DeviceAvailabilityChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            available,
        });

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
    use super::{apply_partial_merge_patch, is_generic_plugin_external_change};
    use hc_types::device::{DeviceChange, DeviceChangeKind};
    use serde_json::json;
    use std::collections::HashMap;

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
