//! Glue device: button — stateless trigger that fires an event when pressed.
//!
//! Unlike switches, buttons have no persistent on/off state. Pressing a button
//! updates `last_pressed` and emits a `DeviceStateChanged` event that rules
//! can trigger on.
//!
//! # Attributes
//!
//! ```json
//! { "last_pressed": "2026-04-04T15:30:00Z" }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "press" }
//! ```

use super::apply_state_update;
use crate::EventBus;
use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const BUTTON_ID_PREFIX: &str = "button_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => { warn!(%device_id, "Button: invalid JSON"); return; }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    let cmd = value.get("command").and_then(|v| v.as_str()).unwrap_or("press");
    if cmd != "press" {
        warn!(%device_id, %cmd, "Button: unknown command (only 'press' supported)");
        return;
    }

    debug!(%device_id, "Button pressed");

    // Always update — buttons are stateless, every press is a new event.
    let now = Utc::now().to_rfc3339();
    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("last_pressed".into(), json!(now));
    }).await;
}
