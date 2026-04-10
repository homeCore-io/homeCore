//! Glue device: input datetime — stored date/time value for alarms and schedules.
//!
//! # Attributes
//!
//! ```json
//! { "value": "2026-04-04T08:00:00", "has_date": true, "has_time": true }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "set", "value": "2026-04-04T08:00:00" }
//! { "command": "set", "value": "08:00:00" }
//! { "command": "set", "value": "2026-04-04" }
//! { "command": "clear" }
//! ```

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const DATETIME_ID_PREFIX: &str = "datetime_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "DateTime: invalid JSON");
            return;
        }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    let cmd = match value.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            if value.get("value").is_some() {
                "set".to_string()
            } else {
                warn!(%device_id, "DateTime: missing command");
                return;
            }
        }
    };

    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "DateTime: device not found");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "DateTime: read failed");
            return;
        }
    };

    let current = dev
        .attributes
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let new_val = match cmd.as_str() {
        "set" => value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "clear" => String::new(),
        _ => {
            warn!(%device_id, %cmd, "DateTime: unknown command");
            return;
        }
    };

    if new_val == current {
        debug!(%device_id, "DateTime: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("value".into(), json!(new_val));
    })
    .await;
}
