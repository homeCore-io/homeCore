//! Glue device: counter — tracks event counts with increment/decrement/reset.
//!
//! # Attributes
//!
//! ```json
//! { "count": 5, "step": 1, "min": 0, "max": null }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "increment" }
//! { "command": "decrement" }
//! { "command": "reset" }
//! { "command": "set", "value": 10 }
//! ```

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const COUNTER_ID_PREFIX: &str = "counter_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "Counter: invalid JSON");
            return;
        }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    let cmd = match value.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            warn!(%device_id, "Counter: missing command field");
            return;
        }
    };
    debug!(%device_id, %cmd, "Counter command");

    // Read current state for bounds checking.
    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Counter: device not found");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Counter: failed to read state");
            return;
        }
    };

    let current = dev.attributes.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
    let step = dev.attributes.get("step").and_then(|v| v.as_i64()).unwrap_or(1);
    let min = dev.attributes.get("min").and_then(|v| v.as_i64());
    let max = dev.attributes.get("max").and_then(|v| v.as_i64());

    let new_count = match cmd.as_str() {
        "increment" => {
            let v = current + step;
            if let Some(mx) = max {
                v.min(mx)
            } else {
                v
            }
        }
        "decrement" => {
            let v = current - step;
            if let Some(mn) = min {
                v.max(mn)
            } else {
                v
            }
        }
        "reset" => min.unwrap_or(0),
        "set" => {
            let v = value
                .get("value")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let v = if let Some(mn) = min { v.max(mn) } else { v };
            if let Some(mx) = max { v.min(mx) } else { v }
        }
        _ => {
            warn!(%device_id, %cmd, "Counter: unknown command");
            return;
        }
    };

    if new_count == current {
        debug!(%device_id, count = new_count, "Counter: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("count".into(), json!(new_count));
    })
    .await;
}
