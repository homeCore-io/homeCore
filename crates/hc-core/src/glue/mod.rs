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
pub mod schema;
pub mod select;
pub mod switch;
pub mod text;
pub mod threshold;
pub mod timer;

use crate::EventBus;
use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::{DeviceChange, DeviceState};
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

        // Heal switches written by `POST /switches` before it was fixed: it set
        // device_type = "virtual_switch", which matches neither the glue
        // convention nor the `list_switches` filter, so those switches existed
        // in the store but `GET /switches` could never return them.  Scoped to
        // core.glue on purpose — "virtual_switch" is a legitimate device_type
        // for a plugin-supplied device (see hc-topic-map's device_types).
        if dev.plugin_id == "core.glue" && dev.device_type.as_deref() == Some("virtual_switch") {
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

/// Give core's own devices the schema they never had.
///
/// Write the schema for a core-owned device, unless it already has one.
///
/// Cheap enough to call on any path that writes a device: a device that has
/// its schema costs one read, and a device whose type we do not model costs
/// nothing at all.
pub async fn ensure_device_schema(store: &StateStore, dev: &DeviceState) {
    if matches!(store.get_device_schema(&dev.device_id).await, Ok(Some(_))) {
        return;
    }
    let Some(schema) = schema::for_device(&dev.plugin_id, dev.device_type.as_deref()) else {
        return;
    };
    if let Err(e) = store.upsert_device_schema(&dev.device_id, &schema).await {
        warn!(device_id = %dev.device_id, error = %e, "Glue schema: failed to persist");
    }
}

/// Create or update a core-owned device together with its schema.
///
/// Every creation path goes through here so the two never drift apart.
/// [`publish_core_device_schemas`] only sees devices that already exist when
/// it runs, so a device created after it — a timer added through the API, or
/// one seeded from config while the sweep was already listing — would
/// otherwise stay schema-less until the next restart, and clients would infer
/// its attributes: a timer's `state` became a text box wanting `"finished"`
/// with the quotes.
pub async fn upsert_device_with_schema(
    store: &StateStore,
    dev: &DeviceState,
) -> anyhow::Result<()> {
    store.upsert_device(dev).await?;
    ensure_device_schema(store, dev).await;
    Ok(())
}

/// Glue devices are created directly in the state store rather than registered
/// over MQTT, so the schema-publishing path plugins use never applied to them
/// and clients were left inferring every attribute. Runs on startup beside
/// [`migrate_legacy_plugin_ids`], and is idempotent: the schema is derived
/// from the device type, so re-running writes the same bytes.
pub async fn publish_core_device_schemas(store: &StateStore) {
    let devices = match store.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "Glue schemas: failed to list devices");
            return;
        }
    };

    let mut written = 0u32;
    for dev in devices {
        let Some(schema) = schema::for_device(&dev.plugin_id, dev.device_type.as_deref()) else {
            continue;
        };

        if let Err(e) = store.upsert_device_schema(&dev.device_id, &schema).await {
            warn!(device_id = %dev.device_id, error = %e,
                  "Glue schemas: failed to persist");
        } else {
            written += 1;
        }
    }

    if written > 0 {
        info!(written, "Glue schemas: published for core-owned devices");
    }
}

/// Evaluate every group once at startup.
///
/// A group recalculates when a member changes. One created before that
/// evaluation existed — or whose members happened not to move while the hub
/// was down — sits at `active_count: 0, member_count: 0`, which reads as
/// "nothing matches" when it may be fully satisfied. A group of doors that are
/// all shut looked wrong until someone opened one.
///
/// Cheap and idempotent: it reads each member and writes the counts it derives.
pub async fn recalculate_all_groups(state: &StateStore, pub_bus: &EventBus) {
    let devices = match state.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "Group startup recalculation: failed to list devices");
            return;
        }
    };

    let mut done = 0u32;
    for dev in devices {
        if dev.plugin_id != GLUE_PLUGIN_ID || dev.device_type.as_deref() != Some("group") {
            continue;
        }
        group::recalculate(state, pub_bus, &dev.device_id).await;
        done += 1;
    }

    if done > 0 {
        info!(groups = done, "Groups recalculated at startup");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> StateStore {
        let tmp = std::env::temp_dir().join(format!("hc_glue_schema_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("temp dir");
        StateStore::open(
            tmp.join("state.redb").to_str().unwrap(),
            tmp.join("history.db").to_str().unwrap(),
        )
        .await
        .expect("store opens")
    }

    fn glue(device_id: &str, device_type: &str) -> DeviceState {
        let mut dev = DeviceState::new(device_id, device_id, GLUE_PLUGIN_ID);
        dev.device_type = Some(device_type.to_string());
        dev
    }

    #[tokio::test]
    async fn a_timer_created_after_the_startup_sweep_still_gets_its_schema() {
        let store = store().await;
        // The sweep runs against an empty store, as it does when it wins the
        // race with the GlueManager.
        publish_core_device_schemas(&store).await;

        upsert_device_with_schema(&store, &glue("timer_kettle", "timer"))
            .await
            .expect("timer is written");

        let schema = store
            .get_device_schema("timer_kettle")
            .await
            .expect("schema reads")
            .expect("a timer created later still has a schema");
        assert!(
            schema.attributes.contains_key("state"),
            "a timer declares the state it reports"
        );
    }

    #[tokio::test]
    async fn a_mode_is_core_owned_too() {
        let store = store().await;
        let dev = DeviceState::new("mode_night", "Night", "core.mode");
        upsert_device_with_schema(&store, &dev)
            .await
            .expect("mode is written");
        assert!(store
            .get_device_schema("mode_night")
            .await
            .expect("schema reads")
            .is_some());
    }

    #[tokio::test]
    async fn a_plugin_device_is_left_to_publish_its_own() {
        let store = store().await;
        let dev = DeviceState::new("lutron_9", "Sconce", "plugin.lutron");
        upsert_device_with_schema(&store, &dev)
            .await
            .expect("device is written");
        assert!(
            store
                .get_device_schema("lutron_9")
                .await
                .expect("schema reads")
                .is_none(),
            "core must not invent a schema for a device it does not own"
        );
    }

    #[tokio::test]
    async fn a_published_schema_is_not_overwritten() {
        let store = store().await;
        let dev = glue("switch_porch", "switch");
        store.upsert_device(&dev).await.expect("device is written");

        let mut mine = hc_types::DeviceSchema::default();
        mine.attributes
            .insert("on".into(), hc_types::AttributeSchema::default());
        store
            .upsert_device_schema("switch_porch", &mine)
            .await
            .expect("schema is written");

        ensure_device_schema(&store, &dev).await;

        let after = store
            .get_device_schema("switch_porch")
            .await
            .expect("schema reads")
            .expect("schema survives");
        assert_eq!(
            after.attributes.len(),
            1,
            "an existing schema is left as it was found"
        );
    }
}
