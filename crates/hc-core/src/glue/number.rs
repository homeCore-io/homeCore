//! Glue device: input number — user-adjustable numeric value with min/max/step.
//!
//! # Attributes
//!
//! ```json
//! { "value": 50.0, "min": 0.0, "max": 100.0, "step": 1.0, "unit": "%" }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "set", "value": 75.0 }
//! { "command": "increment" }
//! { "command": "decrement" }
//! ```

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const NUMBER_ID_PREFIX: &str = "number_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "Number: invalid JSON");
            return;
        }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    let cmd = match value.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            // Accept bare {"value": N} as a set command.
            if value.get("value").is_some() {
                "set".to_string()
            } else {
                warn!(%device_id, "Number: missing command");
                return;
            }
        }
    };

    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Number: device not found");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Number: read failed");
            return;
        }
    };

    let current = dev
        .attributes
        .get("value")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let step = dev
        .attributes
        .get("step")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let min = dev.attributes.get("min").and_then(|v| v.as_f64());
    let max = dev.attributes.get("max").and_then(|v| v.as_f64());

    let clamp = |v: f64| -> f64 {
        let v = if let Some(mn) = min { v.max(mn) } else { v };
        if let Some(mx) = max {
            v.min(mx)
        } else {
            v
        }
    };

    let new_val = match cmd.as_str() {
        "set" => clamp(
            value
                .get("value")
                .and_then(|v| v.as_f64())
                .unwrap_or(current),
        ),
        "increment" => clamp(current + step),
        "decrement" => clamp(current - step),
        _ => {
            warn!(%device_id, %cmd, "Number: unknown command");
            return;
        }
    };

    if (new_val - current).abs() < f64::EPSILON {
        debug!(%device_id, value = new_val, "Number: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("value".into(), json!(new_val));
    })
    .await;
}
