//! Battery low/recovered watcher.
//!
//! Subscribes to `DeviceStateChanged` events on the public bus, watches for
//! battery-attribute changes, and consults the persisted `BatteryStore`
//! latch to decide whether the change crosses the configured low/recover
//! thresholds. Edge transitions are republished as `DeviceBatteryLow` /
//! `DeviceBatteryRecovered` events (used by the matching new triggers in
//! the rule engine) and, optionally, fan out through `hc-notify` directly
//! when `notify_channel` is configured.

use chrono::Utc;
use hc_notify::NotificationService;
use hc_state::{BatteryEdge, StateStore};
use hc_types::event::Event;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::EventBus;

/// Runtime-mutable config for the battery watcher. Held behind a
/// `watch::Receiver` so changes (eventually delivered via a PATCH on the
/// settings REST endpoint) are picked up on the next event without a
/// restart.
#[derive(Debug, Clone, PartialEq)]
pub struct BatteryConfig {
    /// Battery percentage at or below which the latch engages. Default 20.
    pub threshold_pct: f64,
    /// Recovery band added to the threshold to clear the latch. Default 5.
    /// Recovery fires at `threshold_pct + recover_band_pct`.
    pub recover_band_pct: f64,
    /// If set, the watcher fires hc-notify directly on each low edge
    /// alongside the event. None disables the shortcut.
    pub notify_channel: Option<String>,
    /// When true (and `notify_channel` is set), recovery edges also notify.
    pub notify_on_recovered: bool,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            threshold_pct: 20.0,
            recover_band_pct: 5.0,
            notify_channel: None,
            notify_on_recovered: false,
        }
    }
}

/// Spawn the watcher loop. The returned receiver count grows by one for
/// the watcher subscription on `pub_bus`.
pub fn spawn(
    pub_bus: EventBus,
    state: StateStore,
    notify: Option<Arc<NotificationService>>,
    config: watch::Receiver<BatteryConfig>,
) {
    tokio::spawn(run(pub_bus, state, notify, config));
}

async fn run(
    pub_bus: EventBus,
    state: StateStore,
    notify: Option<Arc<NotificationService>>,
    config: watch::Receiver<BatteryConfig>,
) {
    info!("Battery watcher starting");
    let mut rx = pub_bus.subscribe();
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "Battery watcher lagged behind event bus");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("Battery watcher: event bus closed, stopping");
                return;
            }
        };

        let Event::DeviceStateChanged {
            device_id,
            device_name,
            current,
            changed,
            ..
        } = event
        else {
            continue;
        };

        // Cheap pre-filter: skip events whose change set didn't touch a
        // battery-related attribute. Plugins normalize on `battery`
        // (0–100) but we accept the alternates for safety.
        let touched_battery = changed
            .iter()
            .any(|k| matches!(k.as_str(), "battery" | "battery_pct" | "battery_level"));
        if !touched_battery {
            continue;
        }

        let Some(pct) = current
            .get("battery_pct")
            .or_else(|| current.get("battery"))
            .or_else(|| current.get("battery_level"))
            .and_then(extract_pct)
        else {
            continue;
        };

        let cfg = config.borrow().clone();

        let edge = match state
            .evaluate_battery(&device_id, pct, cfg.threshold_pct, cfg.recover_band_pct)
            .await
        {
            Ok(Some(edge)) => edge,
            Ok(None) => continue,
            Err(e) => {
                warn!(device_id = %device_id, error = %e, "Battery latch evaluation failed");
                continue;
            }
        };

        debug!(
            device_id = %device_id,
            battery_pct = pct,
            edge = ?edge,
            "Battery edge"
        );

        let event = match edge {
            BatteryEdge::WentLow => Event::DeviceBatteryLow {
                timestamp: Utc::now(),
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                battery_pct: pct,
                threshold_pct: cfg.threshold_pct,
            },
            BatteryEdge::Recovered => Event::DeviceBatteryRecovered {
                timestamp: Utc::now(),
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                battery_pct: pct,
            },
        };
        if let Err(e) = pub_bus.publish(event) {
            warn!(error = %e, "Failed to publish battery event");
        }

        // Optional notify shortcut.
        if let (Some(channel), Some(svc)) = (cfg.notify_channel.as_deref(), notify.as_ref()) {
            let send = match edge {
                BatteryEdge::WentLow => true,
                BatteryEdge::Recovered => cfg.notify_on_recovered,
            };
            if send {
                let display_name = device_name.as_deref().unwrap_or(&device_id);
                let (title, message) = match edge {
                    BatteryEdge::WentLow => (
                        "Battery low".to_string(),
                        format!("Battery low: {display_name} at {pct:.0}%"),
                    ),
                    BatteryEdge::Recovered => (
                        "Battery recovered".to_string(),
                        format!("Battery recovered: {display_name} at {pct:.0}%"),
                    ),
                };
                if let Err(e) = svc.notify(channel, &title, &message).await {
                    warn!(channel = %channel, error = %e, "Battery notify shortcut failed");
                }
            }
        }
    }
}

fn extract_pct(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_u64().map(|n| n as f64))
}
