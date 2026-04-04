//! Glue device: group — combines multiple devices into a single entity.
//!
//! # Attributes
//!
//! ```json
//! {
//!   "on": true,
//!   "member_ids": ["switch_a", "switch_b", "switch_c"],
//!   "attribute": "on",
//!   "mode": "any",
//!   "member_count": 3,
//!   "active_count": 2
//! }
//! ```
//!
//! `mode`:
//! - `"any"` — `on = true` if ANY member's attribute is truthy (default)
//! - `"all"` — `on = true` only if ALL members' attribute is truthy
//!
//! # Commands
//!
//! ```json
//! { "command": "recalculate" }
//! ```
//!
//! Groups also auto-recalculate when any member device's state changes.

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use serde_json::json;
use tracing::{debug, warn};

pub const GROUP_ID_PREFIX: &str = "group_";

/// Check if a JSON value is "truthy" for group membership evaluation.
fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        serde_json::Value::String(s) => {
            matches!(s.as_str(), "true" | "on" | "open" | "online" | "active" | "running" | "locked")
        }
        _ => false,
    }
}

/// Recalculate a group device's `on` state from its member devices.
pub async fn recalculate(state: &StateStore, pub_bus: &EventBus, device_id: &str) {
    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => { warn!(%device_id, error = %e, "Group: read failed"); return; }
    };

    let member_ids: Vec<String> = dev.attributes.get("member_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let attribute = dev.attributes.get("attribute")
        .and_then(|v| v.as_str())
        .unwrap_or("on");

    let mode = dev.attributes.get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("any");

    let mut active_count: u64 = 0;
    let member_count = member_ids.len() as u64;

    for mid in &member_ids {
        if let Ok(Some(member)) = state.get_device(mid).await {
            if let Some(val) = member.attributes.get(attribute) {
                if is_truthy(val) {
                    active_count += 1;
                }
            }
        }
    }

    let new_on = match mode {
        "all" => active_count == member_count && member_count > 0,
        _ => active_count > 0, // "any" is default
    };

    let change = DeviceChange::homecore("group_recalculate");

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("on".into(), json!(new_on));
        attrs.insert("active_count".into(), json!(active_count));
        attrs.insert("member_count".into(), json!(member_count));
    }).await;
}

/// Check if a device_id is a member of a group device.
pub async fn is_member(state: &StateStore, group_device_id: &str, member_device_id: &str) -> bool {
    if let Ok(Some(dev)) = state.get_device(group_device_id).await {
        dev.attributes.get("member_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(member_device_id)))
            .unwrap_or(false)
    } else {
        false
    }
}

/// Handle explicit commands (only "recalculate" for now).
pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => { warn!(%device_id, "Group: invalid JSON"); return; }
    };
    let cmd = value.get("command").and_then(|v| v.as_str()).unwrap_or("recalculate");
    debug!(%device_id, %cmd, "Group command");
    recalculate(state, pub_bus, device_id).await;
}
