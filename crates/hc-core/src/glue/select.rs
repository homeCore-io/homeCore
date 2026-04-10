//! Glue device: input select — dropdown state machine with predefined options.
//!
//! # Attributes
//!
//! ```json
//! { "selected": "Home", "options": ["Home", "Away", "Vacation", "Guest"] }
//! ```
//!
//! # Commands
//!
//! ```json
//! { "command": "select", "option": "Away" }
//! { "command": "next" }
//! { "command": "previous" }
//! ```

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use serde_json::{json, Value};
use tracing::{debug, warn};

pub const SELECT_ID_PREFIX: &str = "select_";

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "Select: invalid JSON");
            return;
        }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    let cmd = match value.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            if value.get("option").is_some() || value.get("selected").is_some() {
                "select".to_string()
            } else {
                warn!(%device_id, "Select: missing command");
                return;
            }
        }
    };

    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Select: device not found");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Select: read failed");
            return;
        }
    };

    let current = dev
        .attributes
        .get("selected")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let options: Vec<String> = dev
        .attributes
        .get("options")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if options.is_empty() {
        warn!(%device_id, "Select: no options configured");
        return;
    }

    let current_idx = options.iter().position(|o| o == &current);

    let new_val = match cmd.as_str() {
        "select" => {
            let opt = value
                .get("option")
                .or_else(|| value.get("selected"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !options.iter().any(|o| o == opt) {
                warn!(%device_id, option = opt, "Select: invalid option");
                return;
            }
            opt.to_string()
        }
        "next" => {
            let idx = current_idx.map(|i| (i + 1) % options.len()).unwrap_or(0);
            options[idx].clone()
        }
        "previous" => {
            let idx = current_idx
                .map(|i| if i == 0 { options.len() - 1 } else { i - 1 })
                .unwrap_or(0);
            options[idx].clone()
        }
        _ => {
            warn!(%device_id, %cmd, "Select: unknown command");
            return;
        }
    };

    if new_val == current {
        debug!(%device_id, "Select: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("selected".into(), json!(new_val));
    })
    .await;
}
