//! Glue device: schedule — weekly time blocks that produce an on/off state.
//!
//! # Attributes
//!
//! ```json
//! {
//!   "active": false,
//!   "blocks": [
//!     { "days": ["Mon","Tue","Wed","Thu","Fri"], "start": "08:00", "end": "17:00" },
//!     { "days": ["Sat"], "start": "10:00", "end": "14:00" }
//!   ]
//! }
//! ```
//!
//! The schedule is evaluated every minute by the GlueManager tick.
//! `active` is `true` if the current time falls within any block.

use super::apply_state_update;
use crate::EventBus;
use chrono::{Datelike, Local, Timelike, Weekday};
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use serde_json::json;
use tracing::warn;

pub const SCHEDULE_ID_PREFIX: &str = "schedule_";

fn weekday_name(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split(':');
    let h = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    Some((h, m))
}

/// Recalculate a schedule device's `active` state from the current time.
pub async fn recalculate(state: &StateStore, pub_bus: &EventBus, device_id: &str) {
    let dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => {
            warn!(%device_id, error = %e, "Schedule: read failed");
            return;
        }
    };

    let blocks = dev
        .attributes
        .get("blocks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let now = Local::now();
    let today = weekday_name(now.weekday());
    let now_minutes = now.hour() * 60 + now.minute();

    let mut is_active = false;
    for block in &blocks {
        let days = block["days"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        if !days.contains(&today) {
            continue;
        }

        let start = block["start"].as_str().and_then(parse_hm);
        let end = block["end"].as_str().and_then(parse_hm);

        if let (Some((sh, sm)), Some((eh, em))) = (start, end) {
            let start_min = sh * 60 + sm;
            let end_min = eh * 60 + em;

            if start_min <= end_min {
                // Normal range: 08:00 - 17:00
                if now_minutes >= start_min && now_minutes < end_min {
                    is_active = true;
                    break;
                }
            } else {
                // Overnight: 22:00 - 06:00
                if now_minutes >= start_min || now_minutes < end_min {
                    is_active = true;
                    break;
                }
            }
        }
    }

    let currently_active = dev
        .attributes
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_active == currently_active {
        return;
    }

    let change = DeviceChange::homecore("schedule_tick");
    apply_state_update(state, pub_bus, device_id, change, |attrs| {
        attrs.insert("active".into(), json!(is_active));
    })
    .await;
}
