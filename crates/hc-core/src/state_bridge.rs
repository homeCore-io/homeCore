//! MQTT → state bridge.
//!
//! Subscribes to the event bus, intercepts `Event::MqttMessage` for the
//! canonical HomeCore topic schema, updates the state store, and re-emits
//! typed `Event::DeviceStateChanged` / `Event::DeviceAvailabilityChanged`
//! events so the rule engine and WebSocket clients see structured data.
//!
//! Topic patterns handled (inbound):
//! - `homecore/devices/{id}/state`          → full state replace
//! - `homecore/devices/{id}/state/partial`  → JSON merge-patch
//! - `homecore/devices/{id}/availability`   → "online" | "offline"
//! - `homecore/plugins/{id}/register`       → plugin registration
//!
//! Topic patterns handled (outbound via reverse topic map):
//! - `homecore/devices/{id}/cmd`            → translated to native device cmd topic

use crate::EventBus;
use anyhow::Result;
use chrono::Utc;
use hc_mqtt_client::PublishHandle;
use hc_state::StateStore;
use hc_topic_map::TopicMapper;
use hc_types::device::DeviceState;
use hc_types::event::Event;
use tracing::{debug, info, warn};

pub struct StateBridge {
    bus: EventBus,
    store: StateStore,
    mapper: Option<TopicMapper>,
    publish: Option<PublishHandle>,
}

impl StateBridge {
    pub fn new(bus: EventBus, store: StateStore) -> Self {
        Self { bus, store, mapper: None, publish: None }
    }

    /// Attach a `TopicMapper` so non-standard device topics are translated
    /// before processing, and HomeCore cmd topics are relayed to native device topics.
    pub fn with_mapper(mut self, mapper: TopicMapper) -> Self {
        self.mapper = Some(mapper);
        self
    }

    /// Attach a publish handle so the bridge can relay translated commands back
    /// to the broker (required for topic-mapped device two-way control).
    pub fn with_publish(mut self, publish: PublishHandle) -> Self {
        self.publish = Some(publish);
        self
    }

    /// Drive the bridge until the event bus closes.  Spawn in a `tokio::task`.
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
                Ok(_) => {} // Other event types pass through unchanged.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("State bridge lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    async fn handle_mqtt(&self, raw_topic: &str, raw_payload: &[u8]) -> Result<()> {
        // Check if this is a cmd topic for a topic-mapped device.
        // If so, translate and republish to the native device command topic.
        if raw_topic.starts_with("homecore/devices/") && raw_topic.ends_with("/cmd") {
            if let Some(mapper) = &self.mapper {
                match mapper.translate_cmd(raw_topic, raw_payload) {
                    Ok(Some(result)) => {
                        debug!(
                            from = raw_topic,
                            to = %result.target_topic,
                            "Relaying cmd to native device topic"
                        );
                        if let Some(ph) = &self.publish {
                            if let Err(e) = ph.publish(&result.target_topic, result.payload).await {
                                warn!(topic = %result.target_topic, error = %e, "Failed to relay cmd");
                            }
                        } else {
                            warn!("No publish handle — cannot relay cmd to native topic");
                        }
                        // A mapped cmd is fully handled; don't process further.
                        return Ok(());
                    }
                    Ok(None) => {} // Not a mapped device; fall through to normal handling.
                    Err(e) => {
                        warn!(topic = raw_topic, error = %e, "Cmd topic mapper error");
                    }
                }
            }
        }

        // Apply topic mapper for inbound state/availability topics.
        let (translated_topic, translated_payload);
        let (topic, payload): (&str, &[u8]) = if let Some(mapper) = &self.mapper {
            match mapper.translate(raw_topic, raw_payload) {
                Ok(Some(result)) => {
                    translated_topic = result.target_topic;
                    translated_payload = result.payload;
                    (translated_topic.as_str(), translated_payload.as_slice())
                }
                Ok(None) => (raw_topic, raw_payload),
                Err(e) => {
                    warn!(topic = raw_topic, error = %e, "Topic mapper error; using original");
                    (raw_topic, raw_payload)
                }
            }
        } else {
            (raw_topic, raw_payload)
        };

        let parts: Vec<&str> = topic.splitn(4, '/').collect();

        // homecore / devices / {id} / state | state/partial | availability
        if parts.len() >= 4 && parts[0] == "homecore" && parts[1] == "devices" {
            let device_id = parts[2];
            let sub = parts[3];

            match sub {
                "state" => self.handle_state(device_id, payload, false).await?,
                "state/partial" => self.handle_state(device_id, payload, true).await?,
                "availability" => self.handle_availability(device_id, payload).await?,
                _ => {}
            }
        }

        // homecore / plugins / {id} / register
        if parts.len() >= 4 && parts[0] == "homecore" && parts[1] == "plugins"
            && parts[3] == "register"
        {
            let plugin_id = parts[2];
            let _ = self.bus.publish(Event::PluginRegistered {
                timestamp: Utc::now(),
                plugin_id: plugin_id.to_string(),
            });
            info!(plugin_id, "Plugin registered");
        }

        Ok(())
    }

    async fn handle_state(&self, device_id: &str, payload: &[u8], partial: bool) -> Result<()> {
        let incoming: serde_json::Value = serde_json::from_slice(payload)?;
        let attrs = match &incoming {
            serde_json::Value::Object(m) => m.clone(),
            _ => {
                warn!(device_id, "State payload is not a JSON object; ignoring");
                return Ok(());
            }
        };

        // Load or create the device record.
        let mut device = self
            .store
            .get_device(device_id)
            .await?
            .unwrap_or_else(|| DeviceState::new(device_id, device_id, "unknown"));

        let previous = device.attributes.clone();

        if partial {
            // JSON merge-patch: only update keys present in payload.
            for (k, v) in &attrs {
                device.attributes.insert(k.clone(), v.clone());
            }
        } else {
            // Full replace.
            device.attributes = attrs.into_iter().collect();
        }
        device.last_seen = Utc::now();
        device.available = true;

        self.store.upsert_device(&device).await?;

        // Append each changed attribute to history.
        for (attr, val) in &device.attributes {
            if previous.get(attr) != Some(val) {
                let _ = self.store.append_history(device_id, attr, val).await;
            }
        }

        let current = device.attributes.clone();
        debug!(device_id, "Device state updated");

        // Emit a typed event for the rule engine.
        let _ = self.bus.publish(Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            previous,
            current,
        });

        Ok(())
    }

    async fn handle_availability(&self, device_id: &str, payload: &[u8]) -> Result<()> {
        let available = matches!(
            std::str::from_utf8(payload).unwrap_or("").trim(),
            "online" | "Online" | "1" | "true"
        );

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
