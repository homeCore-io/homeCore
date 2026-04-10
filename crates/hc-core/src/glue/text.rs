//! Glue device: input text — stored string value.
//!
//! # Attributes
//!
//! ```json
//! { "value": "hello", "max_length": 255 }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "set", "value": "new text" }
//! { "command": "clear" }
//! ```

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const TEXT_ID_PREFIX: &str = "text_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "Text: invalid JSON");
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
                warn!(%device_id, "Text: missing command");
                return;
            }
        }
    };

    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Text: device not found");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Text: read failed");
            return;
        }
    };

    let current = dev
        .attributes
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let max_length = dev
        .attributes
        .get("max_length")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    let new_val = match cmd.as_str() {
        "set" => {
            let mut s = value
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(max) = max_length {
                s.truncate(max);
            }
            s
        }
        "clear" => String::new(),
        _ => {
            warn!(%device_id, %cmd, "Text: unknown command");
            return;
        }
    };

    if new_val == current {
        debug!(%device_id, "Text: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("value".into(), json!(new_val));
    })
    .await;
}
