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
//!
//! Ecosystem-mapped topics:
//! - Any topic matched by the `EcosystemRouter` is translated before processing.
//! - `homecore/devices/{id}/cmd` on a mapped device is relayed to the native
//!   device command topic via the router's outbound path.

use crate::EventBus;
use anyhow::Result;
use chrono::Utc;
use hc_mqtt_client::PublishHandle;
use hc_state::StateStore;
use hc_topic_map::{EcosystemRouter, InboundResult};
use hc_types::device::DeviceState;
use hc_types::event::Event;
use tracing::{debug, info, warn};

pub struct StateBridge {
    bus:     EventBus,
    store:   StateStore,
    router:  Option<EcosystemRouter>,
    publish: Option<PublishHandle>,
}

impl StateBridge {
    pub fn new(bus: EventBus, store: StateStore) -> Self {
        Self { bus, store, router: None, publish: None }
    }

    pub fn with_router(mut self, router: EcosystemRouter) -> Self {
        self.router = Some(router);
        self
    }

    pub fn with_publish(mut self, publish: PublishHandle) -> Self {
        self.publish = Some(publish);
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
        // --- Outbound: relay mapped cmd topics to native device topics ---
        if topic.starts_with("homecore/devices/") && topic.ends_with("/cmd") {
            if let Some(router) = &self.router {
                match router.route_outbound(topic, payload) {
                    Ok(Some(result)) => {
                        debug!(from = topic, to = %result.target_topic, "Relaying cmd to native topic");
                        if let Some(ph) = &self.publish {
                            if let Err(e) = ph.publish(&result.target_topic, result.payload).await {
                                warn!(topic = %result.target_topic, error = %e, "Failed to relay cmd");
                            }
                        } else {
                            warn!("No publish handle — cannot relay cmd to native topic");
                        }
                        return Ok(()); // Fully handled.
                    }
                    Ok(None) => {} // Not a mapped device; fall through.
                    Err(e)   => warn!(topic, error = %e, "Outbound router error"),
                }
            }
        }

        // --- Inbound: try ecosystem router first ---
        if let Some(router) = &self.router {
            match router.route_inbound(topic, payload) {
                Ok(Some(InboundResult::State { device_id, payload: json_payload, partial })) => {
                    return self.handle_state(&device_id, &json_payload, partial).await;
                }
                Ok(Some(InboundResult::Availability { device_id, available })) => {
                    return self.handle_availability(&device_id, available).await;
                }
                Ok(None) => {
                    debug!(topic, "No ecosystem profile match — falling through to canonical handling");
                }
                Err(e)   => warn!(topic, error = %e, "Inbound router error"),
            }
        }

        // --- Canonical HomeCore schema handling ---
        let parts: Vec<&str> = topic.splitn(4, '/').collect();

        // homecore/devices/{id}/state | state/partial | availability | cmd
        if parts.len() >= 4 && parts[0] == "homecore" && parts[1] == "devices" {
            let device_id = parts[2];
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
                _ => {}
            }
        }

        // homecore/plugins/{id}/register
        if parts.len() >= 4
            && parts[0] == "homecore"
            && parts[1] == "plugins"
            && parts[3] == "register"
        {
            let plugin_id = parts[2];
            let _ = self.bus.publish(Event::PluginRegistered {
                timestamp: Utc::now(),
                plugin_id: plugin_id.to_string(),
            });
            info!(plugin_id, "Plugin registered");
            return Ok(());
        }

        debug!(topic, "Topic not handled by any profile or canonical pattern — ignored");
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

        let mut device = self
            .store
            .get_device(device_id)
            .await?
            .unwrap_or_else(|| DeviceState::new(device_id, device_id, "unknown"));

        let previous = device.attributes.clone();

        if partial {
            for (k, v) in &attrs {
                device.attributes.insert(k.clone(), v.clone());
            }
        } else {
            device.attributes = attrs.into_iter().collect();
        }
        device.last_seen = Utc::now();
        device.available = true;

        self.store.upsert_device(&device).await?;

        for (attr, val) in &device.attributes {
            if previous.get(attr) != Some(val) {
                let _ = self.store.append_history(device_id, attr, val).await;
            }
        }

        let current = device.attributes.clone();
        debug!(device_id, "Device state updated");

        let _ = self.bus.publish(Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            previous,
            current,
        });

        Ok(())
    }

    async fn handle_availability(&self, device_id: &str, available: bool) -> Result<()> {
        let mut device = self
            .store
            .get_device(device_id)
            .await?
            .unwrap_or_else(|| DeviceState::new(device_id, device_id, "unknown"));

        device.available = available;
        device.last_seen = Utc::now();
        self.store.upsert_device(&device).await?;

        let _ = self.bus.publish(Event::DeviceAvailabilityChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            available,
        });

        Ok(())
    }
}
