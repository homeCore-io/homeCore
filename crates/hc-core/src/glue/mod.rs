//! Glue Devices — utility devices for automation logic.
//!
//! Unified manager for all virtual/helper device types: timers, switches,
//! counters, input numbers, input selects, input text, buttons, etc.
//!
//! Each glue device:
//! - Registers as `plugin_id = "core.glue"` with `device_type` set to the subtype
//! - Publishes state to `homecore/devices/{id}/state`
//! - Listens for commands on `homecore/devices/{id}/cmd`
//! - Persists state in redb (survives restarts)
//! - Emits `DeviceStateChanged` events (triggers rules like any other device)

pub mod button;
pub mod config;
pub mod counter;
pub mod datetime;
pub mod group;
pub mod number;
pub mod schedule;
pub mod select;
pub mod switch;
pub mod text;
pub mod threshold;
pub mod timer;

use crate::EventBus;
use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use hc_types::event::Event;
use notify::{Event as NotifyEvent, EventKind, RecursiveMode, Watcher};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub const GLUE_PLUGIN_ID: &str = "core.glue";

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Publish a state change event with computed diff.
pub(crate) fn publish_state_change(
    pub_bus: &EventBus,
    device_id: &str,
    device_name: &str,
    previous: HashMap<String, Value>,
    current: HashMap<String, Value>,
    change: DeviceChange,
) {
    let changed: Vec<String> = current
        .keys()
        .filter(|k| previous.get(*k) != current.get(*k))
        .chain(previous.keys().filter(|k| !current.contains_key(*k)))
        .cloned()
        .collect();
    if changed.is_empty() {
        return;
    }
    let _ = pub_bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        device_name: Some(device_name.to_string()),
        previous,
        current,
        changed,
        change,
    });
}

/// Read device from store, apply a state mutation, persist, and emit event.
pub(crate) async fn apply_state_update(
    state: &StateStore,
    pub_bus: &EventBus,
    device_id: &str,
    change: DeviceChange,
    mutate: impl FnOnce(&mut HashMap<String, Value>),
) {
    let mut dev = match state.get_device(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            warn!(%device_id, "Glue: device not found in state store");
            return;
        }
        Err(e) => {
            warn!(%device_id, error = %e, "Glue: failed to read device state");
            return;
        }
    };

    let previous = dev.attributes.clone();
    mutate(&mut dev.attributes);
    dev.last_seen = Utc::now();
    dev.last_change = Some(change.clone());

    if let Err(e) = state.upsert_device(&dev).await {
        warn!(%device_id, error = %e, "Glue: failed to persist state");
        return;
    }

    publish_state_change(
        pub_bus,
        device_id,
        &dev.name,
        previous,
        dev.attributes,
        change,
    );
}

/// Parse a command topic: `homecore/devices/{id}/cmd` → Some(device_id).
fn parse_glue_cmd_topic(topic: &str, prefix: &str) -> Option<String> {
    let mut parts = topic.splitn(4, '/');
    let p0 = parts.next()?;
    let p1 = parts.next()?;
    let p2 = parts.next()?;
    let p3 = parts.next()?;
    if p0 == "homecore" && p1 == "devices" && p2.starts_with(prefix) && p3 == "cmd" {
        Some(p2.to_string())
    } else {
        None
    }
}

// ── GlueManager ──────────────────────────────────────────────────────────────

pub struct GlueManager {
    internal_bus: EventBus,
    pub_bus: EventBus,
    state: StateStore,
    glue_path: Option<PathBuf>,
}

impl GlueManager {
    pub fn new(internal_bus: EventBus, pub_bus: EventBus, state: StateStore) -> Self {
        Self {
            internal_bus,
            pub_bus,
            state,
            glue_path: None,
        }
    }

    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.glue_path = Some(path);
        self
    }

    /// Drive the glue device event loop. Dispatches commands to type-specific handlers
    /// and recalculates reactive devices (groups, thresholds) on state changes.
    pub async fn start(self) {
        let mut rx = self.internal_bus.subscribe();

        // Schedule tick — check schedule devices every 30 seconds.
        let mut schedule_tick = tokio::time::interval(std::time::Duration::from_secs(30));
        schedule_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Hot-reload watcher for glue.toml.
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(4);
        let _watcher = self.glue_path.as_ref().and_then(|path| {
            let parent = path.parent().unwrap_or(path).to_path_buf();
            let tx = reload_tx.clone();
            let filename = path.file_name().map(|f| f.to_os_string());
            let mut watcher =
                notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
                    let Ok(event) = res else { return };
                    let relevant =
                        matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                            && filename
                                .as_ref()
                                .map(|f| event.paths.iter().any(|p| p.file_name() == Some(f)))
                                .unwrap_or(false);
                    if relevant {
                        let _ = tx.blocking_send(());
                    }
                })
                .map_err(|e| warn!(error = %e, "GlueManager: watcher failed"))
                .ok()?;
            watcher
                .watch(&parent, RecursiveMode::NonRecursive)
                .map_err(|e| warn!(error = %e, "GlueManager: watch failed"))
                .ok()?;
            info!(dir = %parent.display(), "Glue hot-reload watcher active");
            Some(watcher)
        });

        info!("GlueManager started");
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Ok(Event::MqttMessage { topic, payload, .. }) => {
                            // Command dispatch by device ID prefix.
                            // Timer: handled by TimerManager (complex async countdown tasks).
                            // Switch: handled here (migrated from SwitchManager).
                            if let Some(device_id) = parse_glue_cmd_topic(&topic, switch::SWITCH_ID_PREFIX) {
                                switch::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, counter::COUNTER_ID_PREFIX) {
                                counter::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, number::NUMBER_ID_PREFIX) {
                                number::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, select::SELECT_ID_PREFIX) {
                                select::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, text::TEXT_ID_PREFIX) {
                                text::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, button::BUTTON_ID_PREFIX) {
                                button::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, datetime::DATETIME_ID_PREFIX) {
                                datetime::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            } else if let Some(device_id) = parse_glue_cmd_topic(&topic, group::GROUP_ID_PREFIX) {
                                group::handle_cmd(&self.state, &self.pub_bus, &device_id, &payload).await;
                            }
                        }
                        Ok(Event::DeviceStateChanged { device_id, .. }) => {
                            // Reactive recalculation: when any device's state changes,
                            // check if it's a member of a group or source of a threshold.
                            // Skip if the changed device is itself a group/threshold
                            // (avoid infinite loops).
                            if !device_id.starts_with(group::GROUP_ID_PREFIX)
                                && !device_id.starts_with(threshold::THRESHOLD_ID_PREFIX)
                            {
                                self.recalculate_dependents(&device_id).await;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("GlueManager lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = schedule_tick.tick() => {
                    self.tick_schedules().await;
                }
                _ = reload_rx.recv() => {
                    // Debounce: wait 200ms then drain any extra signals.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    while reload_rx.try_recv().is_ok() {}
                    if let Some(ref path) = self.glue_path {
                        info!("glue.toml changed — reloading");
                        if let Err(e) = config::load_glue_config(path, &self.state).await {
                            warn!(error = %e, "Glue hot-reload failed");
                        }
                    }
                }
            }
        }
    }

    /// Recalculate groups and thresholds that depend on a changed device.
    async fn recalculate_dependents(&self, changed_device_id: &str) {
        // Scan for group and threshold devices that reference this device.
        // This is a linear scan — acceptable for small numbers of glue devices.
        let devices = match self.state.list_devices().await {
            Ok(d) => d,
            Err(_) => return,
        };

        for dev in &devices {
            if dev.device_id.starts_with(group::GROUP_ID_PREFIX) {
                let is_member = dev
                    .attributes
                    .get("member_ids")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|v| v.as_str() == Some(changed_device_id)))
                    .unwrap_or(false);
                if is_member {
                    group::recalculate(&self.state, &self.pub_bus, &dev.device_id).await;
                }
            } else if dev.device_id.starts_with(threshold::THRESHOLD_ID_PREFIX) {
                let is_source = dev
                    .attributes
                    .get("source_device_id")
                    .and_then(|v| v.as_str())
                    == Some(changed_device_id);
                if is_source {
                    threshold::recalculate(&self.state, &self.pub_bus, &dev.device_id).await;
                }
            }
        }
    }

    /// Tick all schedule devices to update their active state.
    async fn tick_schedules(&self) {
        let devices = match self.state.list_devices().await {
            Ok(d) => d,
            Err(_) => return,
        };
        for dev in &devices {
            if dev.device_id.starts_with(schedule::SCHEDULE_ID_PREFIX) {
                schedule::recalculate(&self.state, &self.pub_bus, &dev.device_id).await;
            }
        }
    }
}

// ── Migration ────────────────────────────────────────────────────────────────

/// Migrate legacy `core.switch` devices to `core.glue` plugin_id, and backfill
/// missing `device_type` for any glue/timer devices.
/// Called once on startup. Idempotent — skips devices already migrated.
pub async fn migrate_legacy_plugin_ids(store: &StateStore) {
    let devices = match store.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "Glue migration: failed to list devices");
            return;
        }
    };

    let mut migrated = 0u32;
    for mut dev in devices {
        let mut changed = false;

        // Migrate legacy plugin_id
        if dev.plugin_id == "core.switch" {
            dev.plugin_id = "core.glue".to_string();
            dev.device_type = Some("switch".to_string());
            changed = true;
        }

        // Backfill missing device_type from device_id prefix
        if dev.device_type.is_none()
            && (dev.plugin_id == "core.glue" || dev.plugin_id == "core.timer")
        {
            let prefixes = &[
                "switch_",
                "timer_",
                "counter_",
                "number_",
                "select_",
                "text_",
                "button_",
                "datetime_",
                "group_",
                "threshold_",
                "schedule_",
            ];
            for prefix in prefixes {
                if dev.device_id.starts_with(prefix) {
                    dev.device_type = Some(prefix.trim_end_matches('_').to_string());
                    changed = true;
                    break;
                }
            }
        }

        if !changed {
            continue;
        }

        if let Err(e) = store.upsert_device(&dev).await {
            warn!(device_id = %dev.device_id, error = %e, "Glue migration: failed to update device");
        } else {
            migrated += 1;
        }
    }

    if migrated > 0 {
        info!(
            migrated,
            "Glue migration: updated legacy devices (plugin_id / device_type)"
        );
    }
}
