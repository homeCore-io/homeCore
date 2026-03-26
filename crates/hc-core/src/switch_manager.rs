//! Virtual switch helper devices — software-only on/off switches for automation rules.
//!
//! Switches appear as first-class devices in the state store (`plugin_id = "core.switch"`).
//! They accept commands on the standard `homecore/devices/{id}/cmd` MQTT topic and emit
//! `DeviceStateChanged` events so the rule engine can trigger on state transitions.
//!
//! # Creating a switch
//!
//! `POST /api/v1/switches` with `{"id": "vacation_mode", "label": "Vacation Mode"}`.
//! This creates a device with `device_id = "switch_vacation_mode"`.
//!
//! # Commands  (PATCH /api/v1/devices/switch_vacation_mode/state)
//!
//! ```json
//! { "command": "on" }
//! { "command": "off" }
//! { "command": "toggle" }
//! ```
//!
//! # Attributes
//!
//! ```json
//! { "on": false }
//! ```
//!
//! # Rule integration
//!
//! ```toml
//! [[triggers]]
//! type      = "DeviceStateChanged"
//! device_id = "switch_vacation_mode"
//! attribute = "on"
//!
//! [[conditions]]
//! type      = "DeviceState"
//! device_id = "switch_vacation_mode"
//! attribute = "on"
//! op        = "eq"
//! value     = true
//! ```

use chrono::Utc;
use hc_state::StateStore;
use hc_types::event::Event;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::EventBus;

pub const SWITCH_PLUGIN_ID: &str = "core.switch";
pub const SWITCH_ID_PREFIX: &str = "switch_";

// ---------------------------------------------------------------------------
// Command payload
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SwitchCommand {
    On,
    Off,
    Toggle,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct SwitchManager {
    bus: EventBus,
    state: StateStore,
}

impl SwitchManager {
    pub fn new(bus: EventBus, state: StateStore) -> Self {
        Self { bus, state }
    }

    /// Listen for commands on the event bus and apply them to switch devices.
    /// Call `tokio::spawn(manager.start())`.
    pub async fn start(self) {
        let mut rx = self.bus.subscribe();
        info!("SwitchManager started");
        loop {
            match rx.recv().await {
                Ok(Event::MqttMessage { topic, payload, .. }) => {
                    if let Some(device_id) = parse_switch_cmd_topic(&topic) {
                        self.handle_cmd(&device_id, &payload).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("SwitchManager lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------------

    async fn handle_cmd(&self, device_id: &str, payload: &[u8]) {
        // Accept both:
        //   {"command": "on" | "off" | "toggle"}  — explicit command form
        //   {"on": true | false}                   — state-patch form (from TUI / curl)
        let cmd: SwitchCommand = if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(on) = v.get("on").and_then(|b| b.as_bool()) {
                if on { SwitchCommand::On } else { SwitchCommand::Off }
            } else {
                match serde_json::from_value(v) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(%device_id, error = %e, "Switch: invalid command payload");
                        return;
                    }
                }
            }
        } else {
            warn!(%device_id, "Switch: command payload is not valid JSON");
            return;
        };
        debug!(%device_id, ?cmd, "Switch command received");

        let mut dev = match self.state.get_device(device_id).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                warn!(%device_id, "Switch: device not found in state store");
                return;
            }
            Err(e) => {
                warn!(%device_id, error = %e, "Switch: failed to read device state");
                return;
            }
        };

        let current_on = dev.attributes.get("on").and_then(|v| v.as_bool()).unwrap_or(false);

        let new_on = match cmd {
            SwitchCommand::On => true,
            SwitchCommand::Off => false,
            SwitchCommand::Toggle => !current_on,
        };

        if new_on == current_on {
            debug!(%device_id, on = new_on, "Switch: state unchanged, skipping");
            return;
        }

        let previous = dev.attributes.clone();
        dev.attributes.insert("on".into(), serde_json::json!(new_on));
        dev.last_seen = Utc::now();

        if let Err(e) = self.state.upsert_device(&dev).await {
            warn!(%device_id, error = %e, "Switch: failed to persist state");
            return;
        }

        let current = dev.attributes;
        let changed: Vec<String> = current.keys()
            .filter(|k| previous.get(*k) != current.get(*k))
            .chain(previous.keys().filter(|k| !current.contains_key(*k)))
            .cloned()
            .collect();
        let _ = self.bus.publish(Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            previous,
            current,
            changed,
        });
    }
}

// ---------------------------------------------------------------------------
// Topic parsing
// ---------------------------------------------------------------------------

fn parse_switch_cmd_topic(topic: &str) -> Option<String> {
    // homecore/devices/switch_{slug}/cmd
    let mut parts = topic.splitn(4, '/');
    let p0 = parts.next()?;
    let p1 = parts.next()?;
    let p2 = parts.next()?;
    let p3 = parts.next()?;
    if p0 == "homecore" && p1 == "devices" && p2.starts_with(SWITCH_ID_PREFIX) && p3 == "cmd" {
        Some(p2.to_string())
    } else {
        None
    }
}
