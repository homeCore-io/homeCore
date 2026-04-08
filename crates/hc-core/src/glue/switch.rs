//! Glue device: virtual on/off switch.

use super::apply_state_update;
use hc_state::StateStore;
use hc_types::device::extract_change_from_command_payload;
use crate::EventBus;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

pub const SWITCH_ID_PREFIX: &str = "switch_";

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SwitchCommand {
    On,
    Off,
    Toggle,
}

pub async fn handle_cmd(state: &StateStore, pub_bus: &EventBus, device_id: &str, payload: &[u8]) {
    let value = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(v) => v,
        Err(_) => { warn!(%device_id, "Switch: invalid JSON"); return; }
    };
    let change = extract_change_from_command_payload(&value).unwrap_or_default();

    // Accept both {"command":"on"} and {"on":true} forms.
    let cmd: SwitchCommand = if let Some(on) = value.get("on").and_then(|b| b.as_bool()) {
        if on { SwitchCommand::On } else { SwitchCommand::Off }
    } else {
        match serde_json::from_value(value) {
            Ok(c) => c,
            Err(e) => { warn!(%device_id, error = %e, "Switch: invalid command"); return; }
        }
    };
    debug!(%device_id, ?cmd, "Switch command");

    // Read current state to compute toggle and detect no-change.
    let current_on = state
        .get_device(device_id)
        .await
        .ok()
        .flatten()
        .and_then(|d| d.attributes.get("on").and_then(|v| v.as_bool()))
        .unwrap_or(false);

    let new_on = match cmd {
        SwitchCommand::On => true,
        SwitchCommand::Off => false,
        SwitchCommand::Toggle => !current_on,
    };

    if new_on == current_on {
        debug!(%device_id, on = new_on, "Switch: unchanged");
        return;
    }

    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("on".into(), json!(new_on));
    })
    .await;
}
