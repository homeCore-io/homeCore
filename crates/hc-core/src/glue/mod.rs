//! Glue Devices — utility devices for automation logic.
//!
//! Unified manager for all virtual/helper device types: timers, switches,
//! counters, input numbers, input selects, input text, buttons, etc.
//!
//! Each glue device:
//! - Registers as `plugin_id = "core.glue"` with `device_type` set to the subtype
//! - Publishes state to `homecore/devices/{id}/state`
//! - Listens for commands on `homecore/devices/{id}/cmd`
//! - Persists state in redb (survives restarts)
//! - Emits `DeviceStateChanged` events (triggers rules like any other device)

pub mod button;
pub mod counter;
pub mod datetime;
pub mod number;
pub mod select;
pub mod switch;
pub mod text;
pub mod timer;

use crate::EventBus;
use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::{DeviceChange};
use hc_types::event::Event;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{info, warn};

pub const GLUE_PLUGIN_ID: &str = "core.glue";

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Publish a state change event with computed diff.
pub(crate) fn publish_state_change(
    pub_bus: &EventBus,
    device_id: &str,
    device_name: &str,
    previous: HashMap<String, Value>,
    current: HashMap<String, Value>,
    change: DeviceChange,
) {
    let changed: Vec<String> = current
        .keys()
        .filter(|k| previous.get(*k) != current.get(*k))
        .chain(previous.keys().filter(|k| !current.contains_key(*k)))
        .cloned()
        .collect();
    if changed.is_empty() {
        return;
    }
    let _ = pub_bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        device_name: Some(device_name.to_string()),
        previous,
        current,
        changed,
        change,
    });
}

/// Read device from store, apply a state mutation, persist, and emit event.
pub(crate) async fn apply_state_update(
    state: &StateStore,
    pub_bus: &EventBus,
    device_id: &str,
    change: DeviceChange,
    mutate: impl FnOnce(&mut HashMap<String, Value>),
) {
    let mut dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Glue: device not found in state store");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Glue: failed to read device state");
            return;
        }
    };

    let previous = dev.attributes.clone();
    mutate(&mut dev.attributes);
    dev.last_seen = Utc::now();
    dev.last_change = Some(change.clone());

    if let Err(e) = state.upsert_device(&dev).await {
        warn!(%device_id, error = %e, "Glue: failed to persist state");
        return;
    }

    publish_state_change(pub_bus, device_id, &dev.name, previous, dev.attributes, change);
}

/// Parse a command topic: `homecore/devices/{id}/cmd` → Some(device_id).
fn parse_glue_cmd_topic(topic: &str, prefix: &str) -> Option<String> {
    let mut parts = topic.splitn(4, '/');
    let p0 = parts.next()?;
    let p1 = parts.next()?;
    let p2 = parts.next()?;
    let p3 = parts.next()?;
    if p0 == "homecore" && p1 == "devices" && p2.starts_with(prefix) && p3 == "cmd" {
        Some(p2.to_string())
    } else {
        None
    }
}

// ── GlueManager ──────────────────────────────────────────────────────────────

pub struct GlueManager {
    internal_bus: EventBus,
    pub_bus: EventBus,
    state: StateStore,
}

impl GlueManager {
    pub fn new(internal_bus: EventBus, pub_bus: EventBus, state: StateStore) -> Self {
        Self {
            internal_bus,
            pub_bus,
            state,
        }
    }

    /// Drive the glue device event loop. Dispatches commands to type-specific handlers.
    pub async fn start(self) {
        let mut rx = self.internal_bus.subscribe();
        info!("GlueManager started (counter, number, select, text, button, datetime)");
        loop {
            match rx.recv().await {
                Ok(Event::MqttMessage { topic, payload, .. }) => {
                    // Timer: timer_* — handled by TimerManager (not here)
                    // Switch: switch_* — handled by SwitchManager (not here)
                    if let Some(device_id) = parse_glue_cmd_topic(&topic, counter::COUNTER_ID_PREFIX) {
                        counter::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    } else if let Some(device_id) = parse_glue_cmd_topic(&topic, number::NUMBER_ID_PREFIX) {
                        number::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    } else if let Some(device_id) = parse_glue_cmd_topic(&topic, select::SELECT_ID_PREFIX) {
                        select::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    } else if let Some(device_id) = parse_glue_cmd_topic(&topic, text::TEXT_ID_PREFIX) {
                        text::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    } else if let Some(device_id) = parse_glue_cmd_topic(&topic, button::BUTTON_ID_PREFIX) {
                        button::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    } else if let Some(device_id) = parse_glue_cmd_topic(&topic, datetime::DATETIME_ID_PREFIX) {
                        datetime::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("GlueManager lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
