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
//! - `"any"` — `on = true` if ANY member matches (default)
//! - `"all"` — `on = true` only if ALL members match
//!
//! `expect` — the value a member has to hold to count as matching. Defaults to
//! `true`, which is what every group written before it existed meant. Without
//! it a group could only ask "are any of these ON", so "all deck doors CLOSED"
//! — an ordinary thing to want — could not be expressed at all.
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
            matches!(
                s.as_str(),
                "true" | "on" | "open" | "online" | "active" | "running" | "locked"
            )
        }
        _ => false,
    }
}

/// Recalculate a group device's `on` state from its member devices.
pub async fn recalculate(state: &StateStore, pub_bus: &EventBus, device_id: &str) {
    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => {
            warn!(%device_id, error = %e, "Group: read failed");
            return;
        }
    };

    let member_ids: Vec<String> = dev
        .attributes
        .get("member_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let attribute = dev
        .attributes
        .get("attribute")
        .and_then(|v| v.as_str())
        .unwrap_or("on");

    let mode = dev
        .attributes
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("any");

    // The state a member must be in to count. `true` keeps every group that
    // predates this field meaning exactly what it meant.
    let expect = dev
        .attributes
        .get("expect")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut active_count: u64 = 0;
    let member_count = member_ids.len() as u64;

    // One read, then resolve each member against it. A member is stored the
    // way rules store a device reference — a raw `device_id` OR a
    // `canonical_name` — and `get_device` only knows the first. A group whose
    // members were picked in the UI (which prefers the canonical form, because
    // it survives the device being replaced) resolved NOTHING and sat at
    // `active_count: 0` forever, looking like a group whose members were all
    // in the wrong state.
    let devices = state.list_devices().await.unwrap_or_default();

    for mid in &member_ids {
        let member = devices.iter().find(|d| {
            d.device_id == *mid || d.canonical_name.as_deref() == Some(mid.as_str())
        });
        if let Some(member) = member {
            if let Some(val) = member.attributes.get(attribute) {
                if is_truthy(val) == expect {
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
    })
    .await;
}

/// Check if a device_id is a member of a group device.
pub async fn is_member(state: &StateStore, group_device_id: &str, member_device_id: &str) -> bool {
    let Ok(Some(dev)) = state.get_device(group_device_id).await else {
        return false;
    };
    let Some(members) = dev.attributes.get("member_ids").and_then(|v| v.as_array()) else {
        return false;
    };

    if members.iter().any(|v| v.as_str() == Some(member_device_id)) {
        return true;
    }

    // The changed device arrives as a `device_id`, but the member may be
    // recorded under its canonical name. Matching only the raw id meant a
    // group built through the UI never auto-recalculated: the member changed,
    // nothing matched, and the group stayed as it was.
    let Ok(Some(changed)) = state.get_device(member_device_id).await else {
        return false;
    };
    let Some(canonical) = changed.canonical_name.as_deref() else {
        return false;
    };
    members.iter().any(|v| v.as_str() == Some(canonical))
}

/// Handle explicit commands (only "recalculate" for now).
pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            warn!(%device_id, "Group: invalid JSON");
            return;
        }
    };
    let cmd = value
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("recalculate");
    debug!(%device_id, %cmd, "Group command");
    recalculate(state, pub_bus, device_id).await;
}
