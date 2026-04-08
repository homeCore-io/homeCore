//! Glue device: threshold — binary sensor that tracks when a numeric value crosses a boundary.
//!
//! # Attributes
//!
//! ```json
//! {
//!   "above": false,
//!   "source_device_id": "sensor_temp",
//!   "source_attribute": "temperature",
//!   "threshold": 75.0,
//!   "hysteresis": 2.0
//! }
//! ```
//!
//! `above` flips to `true` when source value > threshold + hysteresis/2,
//! and back to `false` when source value < threshold - hysteresis/2.
//! Hysteresis prevents rapid toggling around the threshold.

use super::apply_state_update;
use crate::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use serde_json::json;
use tracing::warn;

pub const THRESHOLD_ID_PREFIX: &str = "threshold_";

/// Recalculate a threshold device's `above` state from its source device.
pub async fn recalculate(state: &StateStore, pub_bus: &EventBus, device_id: &str) {
    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => { warn!(%device_id, error = %e, "Threshold: read failed"); return; }
    };

    let source_id = match dev.attributes.get("source_device_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => { warn!(%device_id, "Threshold: no source_device_id"); return; }
    };
    let source_attr = dev.attributes.get("source_attribute")
        .and_then(|v| v.as_str())
        .unwrap_or("value");
    let threshold = dev.attributes.get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let hysteresis = dev.attributes.get("hysteresis")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let currently_above = dev.attributes.get("above")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let source_val = match state.get_device(&source_id).await {
        Ok(Some(d)) => d.attributes.get(source_attr).and_then(|v| v.as_f64()),
        _ => None,
    };

    let Some(val) = source_val else { return; };

    let half_hyst = hysteresis / 2.0;
    let new_above = if currently_above {
        // Currently above — only flip to false when below lower bound.
        val >= (threshold - half_hyst)
    } else {
        // Currently below — only flip to true when above upper bound.
        val > (threshold + half_hyst)
    };

    if new_above == currently_above { return; }

    let change = DeviceChange::homecore("threshold_recalculate");
    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("above".into(), json!(new_above));
        attrs.insert("source_value".into(), json!(val));
    }).await;
}

/// Check if a device_id is the source of a threshold device.
pub async fn is_source(state: &StateStore, threshold_device_id: &str, source_device_id: &str) -> bool {
    if let Ok(Some(dev)) = state.get_device(threshold_device_id).await {
        dev.attributes.get("source_device_id")
            .and_then(|v| v.as_str()) == Some(source_device_id)
    } else {
        false
    }
}
