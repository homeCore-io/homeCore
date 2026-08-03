//! Thermostat bridge — subscribes to sensors, runs the control loop, publishes
//! state and actuator commands.

use crate::config::{Config, ThermostatEntry};
use crate::control::{aggregate, compute_call_for, lockout_remaining};
use anyhow::Result;
use chrono::{DateTime, Utc};
use plugin_sdk_rs::types::{with_command_change_metadata, DeviceChange};
use plugin_sdk_rs::DevicePublisher;
use rumqttc::{AsyncClient, QoS};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

/// Interpret an actuator's state payload as on/off. Tries common fields in
/// priority order: `on` → `state` → `power`. Returns None if no recognised
/// on/off attribute is present.
pub(crate) fn interpret_actuator_on(payload: &Value) -> Option<bool> {
    if let Some(b) = payload.get("on").and_then(|v| v.as_bool()) {
        return Some(b);
    }
    if let Some(s) = payload.get("state").and_then(|v| v.as_str()) {
        match s.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => return Some(true),
            "off" | "false" | "0" => return Some(false),
            _ => {}
        }
    }
    if let Some(b) = payload.get("power").and_then(|v| v.as_bool()) {
        return Some(b);
    }
    if let Some(s) = payload.get("power").and_then(|v| v.as_str()) {
        match s.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => return Some(true),
            "off" | "false" | "0" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Per-thermostat runtime state — what we've last published.
#[derive(Debug, Clone)]
struct Runtime {
    cfg: ThermostatEntry,
    current_temperature: Option<f64>,
    call_for: String,
    actuator_state: bool,
    actuator_last_change: Option<DateTime<Utc>>,
    pending_call: Option<String>,
    lockout_until: Option<DateTime<Utc>>,
    /// Last actuator publish error — cleared on next successful publish.
    actuator_last_error: Option<ActuatorError>,
    /// True once the bridge has received at least one fresh reading from
    /// every configured sensor since startup (or the thermostat is in
    /// "off" mode / configured with no sensors). Until settled, the
    /// bridge holds — recalculate computes nothing and issues no
    /// actuator commands. The first recalculate after settle issues an
    /// **unconditional** command for the desired state, syncing the
    /// physical actuator to expected even if the cached
    /// `actuator_state` already matches. This fixes THERM-RESTART-1:
    /// after a plugin/appliance restart the cached state is restored
    /// from retained MQTT, but the physical actuator may have drifted
    /// (power blip, manual override). Without the force-issue, the
    /// "transition only" command path would never re-sync.
    settled: bool,
}

#[derive(Debug, Clone)]
struct ActuatorError {
    timestamp: DateTime<Utc>,
    message: String,
}

impl Runtime {
    fn new(cfg: ThermostatEntry) -> Self {
        Self {
            cfg,
            current_temperature: None,
            call_for: "idle".into(),
            actuator_state: false,
            actuator_last_change: None,
            pending_call: None,
            lockout_until: None,
            actuator_last_error: None,
            settled: false,
        }
    }

    fn device_id(&self) -> String {
        format!("thermostat_{}", self.cfg.id)
    }

    /// Build the full state payload published to homecore/devices/{id}/state.
    fn state_payload(&self) -> Value {
        let ids: Vec<&str> = self
            .cfg
            .sensor_device_ids
            .iter()
            .map(|s| s.as_str())
            .collect();
        json!({
            "sensor_ids": ids,
            "sensor_attribute": self.cfg.sensor_attribute,
            "aggregation": self.cfg.aggregation,
            "setpoint": self.cfg.setpoint,
            "hysteresis": self.cfg.hysteresis,
            "mode": self.cfg.mode,
            "actuator_device_id": self.cfg.actuator_device_id,
            "min_on_secs": self.cfg.min_on_secs,
            "min_off_secs": self.cfg.min_off_secs,

            "current_temperature": self.current_temperature,
            "call_for": self.call_for,
            "actuator_state": self.actuator_state,
            "actuator_last_change": self.actuator_last_change.map(|t| t.to_rfc3339()),
            "pending_call": self.pending_call,
            "lockout_until": self.lockout_until.map(|t| t.to_rfc3339()),
            "settled": self.settled,
            "actuator_last_error": self.actuator_last_error.as_ref().map(|e| json!({
                "timestamp": e.timestamp.to_rfc3339(),
                "message": e.message,
            })),
            "last_update": Utc::now().to_rfc3339(),
        })
    }
}

/// A representative state payload, for the schema drift test.
///
/// `state_payload` is the authority on what a thermostat publishes, and the
/// schema has to match it exactly. Exposing one built from a default runtime
/// lets the test compare the two sets rather than trusting a hand-copied list.
#[cfg(test)]
pub fn sample_state_payload() -> Value {
    // Built by deserialising rather than by a hand-written literal, so the
    // fixture picks up the config type's own serde defaults and cannot drift
    // from what a real thermostat entry looks like.
    let cfg: ThermostatEntry = serde_json::from_value(serde_json::json!({
        "id": "t1",
        "name": "Test",
        "sensor_device_ids": ["sensor_a"],
        "setpoint": 21.0,
    }))
    .expect("minimal thermostat entry deserialises");
    Runtime::new(cfg).state_payload()
}

/// Shared plugin state. Wrapped in Arc<Mutex<...>> because commands and state
/// messages arrive via separate callback threads.
pub struct Bridge {
    /// Thermostat_id → Runtime
    thermostats: HashMap<String, Runtime>,
    /// Sensor device_id → last numeric reading (keyed by the configured
    /// `sensor_attribute`).
    sensor_cache: HashMap<String, f64>,
    config_path: String,
    /// Device IDs we're still waiting on for their first post-subscribe state
    /// message during startup sync. `None` after initial sync completes or
    /// times out.
    sync_pending: Option<std::collections::HashSet<String>>,
}

pub struct BridgeHandle {
    inner: Arc<Mutex<Bridge>>,
    publisher: DevicePublisher,
    mqtt: AsyncClient,
    plugin_id: String,
    /// Sync-readable snapshot of thermostat configs — updated whenever the
    /// bridge mutates thermostats. Used by management custom handlers
    /// (`get_thermostats`) which must answer synchronously without blocking
    /// the MQTT event loop.
    snapshot: Arc<std::sync::Mutex<Vec<Value>>>,
    /// Notified whenever a state message drains a pending entry from
    /// `sync_pending`. The initial-sync waiter uses this to exit early once
    /// all expected state has arrived.
    sync_notify: Arc<tokio::sync::Notify>,
}

impl BridgeHandle {
    pub async fn new(
        cfg: &Config,
        publisher: DevicePublisher,
        mqtt: AsyncClient,
        config_path: &str,
        snapshot: Arc<std::sync::Mutex<Vec<Value>>>,
    ) -> Result<Self> {
        let thermostats = cfg
            .thermostats
            .iter()
            .cloned()
            .map(|t| (t.id.clone(), Runtime::new(t)))
            .collect();
        let bridge = Bridge {
            thermostats,
            sensor_cache: HashMap::new(),
            config_path: config_path.to_string(),
            sync_pending: None,
        };
        let handle = Self {
            inner: Arc::new(Mutex::new(bridge)),
            publisher,
            mqtt,
            plugin_id: cfg.homecore.plugin_id.clone(),
            snapshot,
            sync_notify: Arc::new(tokio::sync::Notify::new()),
        };
        handle.refresh_snapshot().await;
        Ok(handle)
    }

    /// Seed the startup-sync tracker with the full set of external and own
    /// device IDs we've subscribed to. Must be called before any subscriber
    /// starts receiving retained messages so we don't miss markings.
    pub async fn begin_initial_sync(&self, expected_ids: Vec<String>) {
        let mut b = self.inner.lock().await;
        b.sync_pending = Some(expected_ids.into_iter().collect());
    }

    /// Block until either all expected state messages have been received or
    /// `max_wait` elapses. Returns the number of IDs that never reported.
    pub async fn wait_for_initial_sync(&self, max_wait: std::time::Duration) -> usize {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let remaining_count = {
                let b = self.inner.lock().await;
                b.sync_pending.as_ref().map(|s| s.len()).unwrap_or(0)
            };
            if remaining_count == 0 {
                break;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let notify = self.sync_notify.clone();
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(left) => break,
            }
        }
        let missed: Vec<String> = {
            let mut b = self.inner.lock().await;
            let missed = b
                .sync_pending
                .as_ref()
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            b.sync_pending = None;
            missed
        };
        if !missed.is_empty() {
            warn!(
                missing = ?missed,
                count = missed.len(),
                "Initial sync: not all subscribed devices reported retained state before timeout"
            );
        }
        missed.len()
    }

    /// Record that we've seen a state message for `device_id`. Drains the
    /// pending set and notifies the waiter when the set becomes empty. No-op
    /// if initial sync has already completed.
    async fn mark_sync_received(&self, device_id: &str) {
        let drained = {
            let mut b = self.inner.lock().await;
            match &mut b.sync_pending {
                Some(set) => set.remove(device_id),
                None => false,
            }
        };
        if drained {
            self.sync_notify.notify_waiters();
        }
    }

    /// Rebuild the sync-readable snapshot after any config mutation.
    async fn refresh_snapshot(&self) {
        let list = self.get_thermostats().await;
        if let Ok(mut snap) = self.snapshot.lock() {
            *snap = list;
        }
    }

    /// Register every configured thermostat with HomeCore. Call after
    /// `run_managed` is spawned.
    pub async fn register_all(&self) -> Result<()> {
        let ids: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats.values().map(|r| r.device_id()).collect()
        };
        let (names, cfgs): (Vec<String>, Vec<ThermostatEntry>) = {
            let b = self.inner.lock().await;
            b.thermostats
                .values()
                .map(|r| (r.cfg.name.clone(), r.cfg.clone()))
                .unzip()
        };
        for (id, name) in ids.iter().zip(names.iter()) {
            self.publisher
                .register_device_full(id, name, Some("thermostat"), None, None)
                .await?;
            // Retained, so a client connecting later still knows what this
            // device's attributes mean and which commands it accepts.
            crate::schema::publish(&self.publisher, id).await?;
            self.publisher.subscribe_commands(id).await?;
            self.publisher.publish_availability(id, true).await?;
        }
        // Also publish initial state for each thermostat.
        for (id, _) in ids.iter().zip(cfgs.iter()) {
            if let Some(payload) = {
                let b = self.inner.lock().await;
                b.thermostats
                    .values()
                    .find(|r| r.device_id() == *id)
                    .map(|r| r.state_payload())
            } {
                self.publisher.publish_state(id, &payload).await?;
            }
        }
        // Reconcile against the SDK-tracked set: anything from a prior
        // session that's no longer in [[thermostats]] gets unregistered.
        let live: std::collections::HashSet<String> = ids.iter().cloned().collect();
        if let Err(e) = self.publisher.reconcile_devices(live).await {
            tracing::warn!(error = %e, "reconcile_devices failed");
        }
        Ok(())
    }

    /// Return the list of device IDs (thermostat_<id>) this plugin owns.
    pub async fn all_own_device_ids(&self) -> Vec<String> {
        let b = self.inner.lock().await;
        b.thermostats.values().map(|r| r.device_id()).collect()
    }

    /// Return the union of all configured sensor IDs across all thermostats.
    pub async fn all_sensor_ids(&self) -> Vec<String> {
        let b = self.inner.lock().await;
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in b.thermostats.values() {
            for id in &r.cfg.sensor_device_ids {
                set.insert(id.clone());
            }
        }
        set.into_iter().collect()
    }

    /// Return the union of all configured actuator device IDs across all
    /// thermostats. Empty actuator_device_id entries are skipped.
    pub async fn all_actuator_ids(&self) -> Vec<String> {
        let b = self.inner.lock().await;
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in b.thermostats.values() {
            if !r.cfg.actuator_device_id.is_empty() {
                set.insert(r.cfg.actuator_device_id.clone());
            }
        }
        set.into_iter().collect()
    }

    /// For an external device_id (neither a thermostat of ours nor an own
    /// retained state), classify whether it's being used as an actuator
    /// and/or a sensor by any configured thermostat. A single id can serve
    /// as both in principle.
    pub async fn classify_external(&self, device_id: &str) -> (bool, bool) {
        let b = self.inner.lock().await;
        let mut is_act = false;
        let mut is_sensor = false;
        for rt in b.thermostats.values() {
            if rt.cfg.actuator_device_id == device_id {
                is_act = true;
            }
            if rt.cfg.sensor_device_ids.iter().any(|s| s == device_id) {
                is_sensor = true;
            }
            if is_act && is_sensor {
                break;
            }
        }
        (is_act, is_sensor)
    }

    /// Incoming state message on one of OUR thermostat device topics. Called
    /// at startup via retained-message replay to restore `actuator_last_change`
    /// (and therefore short-cycle lockout windows) across plugin restarts.
    ///
    /// Safe to be called after we've already started publishing fresh state —
    /// we only apply the retained fields if they weren't already overwritten.
    pub async fn on_own_state_restored(&self, device_id: &str, payload: Value) {
        self.mark_sync_received(device_id).await;
        let Some(therm_id) = device_id.strip_prefix("thermostat_") else {
            return;
        };
        let mut b = self.inner.lock().await;
        let Some(rt) = b.thermostats.get_mut(therm_id) else {
            return;
        };
        // Only restore from retained state if we haven't produced live state yet
        // (i.e. actuator_last_change is None). Otherwise we'd clobber the fresh
        // startup recalc.
        if rt.actuator_last_change.is_some() {
            return;
        }
        if let Some(ts) = payload
            .get("actuator_last_change")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            rt.actuator_last_change = Some(ts.with_timezone(&Utc));
        }
        if let Some(b_) = payload.get("actuator_state").and_then(|v| v.as_bool()) {
            rt.actuator_state = b_;
        }
        if let Some(s) = payload.get("call_for").and_then(|v| v.as_str()) {
            rt.call_for = s.to_string();
        }
        debug!(device_id, "Restored runtime state from retained message");
    }

    /// Incoming state message from an actuator device. Detects drift between
    /// our internal tracking and the observed reality — e.g. an external
    /// command (UI, rule, voice) flipped the actuator behind our back, or a
    /// prior thermostat command was never applied. When drift is detected,
    /// we update internal state to match reality and recalculate, which will
    /// re-issue the correct command if desired state still differs.
    pub async fn on_actuator_state(&self, device_id: &str, payload: Value) {
        self.mark_sync_received(device_id).await;
        let Some(observed) = interpret_actuator_on(&payload) else {
            debug!(device_id, "Actuator state has no recognised on/off field");
            return;
        };

        let drifted: Vec<String> = {
            let mut b = self.inner.lock().await;
            let mut drifted = Vec::new();
            for rt in b.thermostats.values_mut() {
                if rt.cfg.actuator_device_id == device_id && rt.actuator_state != observed {
                    warn!(
                        actuator = device_id,
                        thermostat = %rt.cfg.id,
                        observed,
                        internal = rt.actuator_state,
                        "Actuator state drift detected — updating internal tracking"
                    );
                    rt.actuator_state = observed;
                    // Treat the external transition as a change we observed now;
                    // this keeps short-cycle protection honest against the new
                    // physical state.
                    rt.actuator_last_change = Some(Utc::now());
                    drifted.push(rt.cfg.id.clone());
                }
            }
            drifted
        };

        for id in drifted {
            self.recalculate(&id).await;
        }
    }

    /// Incoming state message from an external sensor device.
    pub async fn on_sensor_state(&self, sensor_id: &str, payload: Value) {
        self.mark_sync_received(sensor_id).await;
        // Extract the configured attribute — the attribute name is per-thermostat,
        // but the common case is "temperature".
        let attrs: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats
                .values()
                .filter(|r| r.cfg.sensor_device_ids.iter().any(|s| s == sensor_id))
                .map(|r| r.cfg.sensor_attribute.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect()
        };
        if attrs.is_empty() {
            return; // no thermostat cares about this sensor
        }

        // Pick the first matching numeric attribute. (Most thermostats will share
        // "temperature"; we update the cache under that attribute name.)
        for attr in attrs {
            if let Some(v) = payload.get(&attr).and_then(|v| v.as_f64()) {
                let mut b = self.inner.lock().await;
                b.sensor_cache.insert(sensor_id.to_string(), v);
                debug!(sensor_id, attr = %attr, value = v, "Sensor update cached");
                break;
            }
        }

        // Recalculate every thermostat that depends on this sensor.
        let affected: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats
                .values()
                .filter(|r| r.cfg.sensor_device_ids.iter().any(|s| s == sensor_id))
                .map(|r| r.cfg.id.clone())
                .collect()
        };
        for id in affected {
            self.recalculate(&id).await;
        }
    }

    /// Incoming command on this plugin's own device topic.
    pub async fn on_device_command(&self, device_id: &str, payload: Value) {
        let Some(therm_id) = device_id.strip_prefix("thermostat_") else {
            return;
        };
        let cmd = payload
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        debug!(device_id, cmd, "Thermostat command");

        // Sensor and actuator set changes need a sub/unsub diff against the
        // union of all thermostats' sensor/actuator sets AFTER the update.
        // Collect those outside the lock so we can await subscribe_state /
        // unsubscribe_state cleanly.
        let mut changed = false;
        let mut sensor_diff: Option<(Vec<String>, Vec<String>)> = None;
        let mut actuator_diff: Option<(Vec<String>, Vec<String>)> = None;
        {
            let mut b = self.inner.lock().await;
            // Snapshot global sensor/actuator unions BEFORE mutation.
            let old_sensor_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            let old_actuator_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();

            let Some(rt) = b.thermostats.get_mut(therm_id) else {
                warn!(device_id, "Unknown thermostat");
                return;
            };
            match cmd {
                "set_setpoint" => {
                    if let Some(v) = payload.get("value").and_then(|v| v.as_f64()) {
                        rt.cfg.setpoint = v;
                        changed = true;
                    }
                }
                "set_mode" => {
                    if let Some(m) = payload.get("value").and_then(|v| v.as_str()) {
                        if matches!(m, "heat" | "cool" | "off") {
                            rt.cfg.mode = m.to_string();
                            changed = true;
                        } else {
                            warn!(device_id, mode = %m, "Unknown mode");
                        }
                    }
                }
                "set_hysteresis" => {
                    if let Some(v) = payload.get("value").and_then(|v| v.as_f64()) {
                        rt.cfg.hysteresis = v.max(0.0);
                        changed = true;
                    }
                }
                "set_aggregation" => {
                    if let Some(v) = payload.get("value").and_then(|v| v.as_str()) {
                        if matches!(v, "average" | "min" | "max") {
                            rt.cfg.aggregation = v.to_string();
                            changed = true;
                        }
                    }
                }
                "set_short_cycle" => {
                    if let Some(v) = payload.get("min_on_secs").and_then(|v| v.as_u64()) {
                        rt.cfg.min_on_secs = v;
                        changed = true;
                    }
                    if let Some(v) = payload.get("min_off_secs").and_then(|v| v.as_u64()) {
                        rt.cfg.min_off_secs = v;
                        changed = true;
                    }
                }
                "set_sensors" => {
                    if let Some(ids) = payload.get("sensor_ids").and_then(|v| v.as_array()) {
                        let new_ids: Vec<String> = ids
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect();
                        rt.cfg.sensor_device_ids = new_ids;
                        changed = true;
                    }
                    if let Some(a) = payload.get("attribute").and_then(|v| v.as_str()) {
                        rt.cfg.sensor_attribute = a.to_string();
                        changed = true;
                    }
                }
                "set_actuator" => {
                    if let Some(aid) = payload.get("device_id").and_then(|v| v.as_str()) {
                        rt.cfg.actuator_device_id = aid.to_string();
                        changed = true;
                    }
                    if let Some(v) = payload.get("on_cmd") {
                        rt.cfg.actuator_on_cmd = Some(v.clone());
                        changed = true;
                    }
                    if let Some(v) = payload.get("off_cmd") {
                        rt.cfg.actuator_off_cmd = Some(v.clone());
                        changed = true;
                    }
                }
                "recalculate" | "" => {}
                other => {
                    warn!(device_id, cmd = %other, "Unknown thermostat command");
                    return;
                }
            }

            // Snapshot NEW global sensor/actuator unions and compute diffs.
            let new_sensor_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            if old_sensor_union != new_sensor_union {
                let added: Vec<String> = new_sensor_union
                    .difference(&old_sensor_union)
                    .cloned()
                    .collect();
                let removed: Vec<String> = old_sensor_union
                    .difference(&new_sensor_union)
                    .cloned()
                    .collect();
                sensor_diff = Some((added, removed));
            }

            let new_actuator_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();
            if old_actuator_union != new_actuator_union {
                let added: Vec<String> = new_actuator_union
                    .difference(&old_actuator_union)
                    .cloned()
                    .collect();
                let removed: Vec<String> = old_actuator_union
                    .difference(&new_actuator_union)
                    .cloned()
                    .collect();
                actuator_diff = Some((added, removed));
            }
        }

        // Apply subscription diffs outside the lock.
        if let Some((added, removed)) = sensor_diff {
            for sid in &added {
                if let Err(e) = self.publisher.subscribe_state(sid).await {
                    warn!(sensor = %sid, error = %e, "set_sensors: subscribe failed");
                }
            }
            for sid in &removed {
                if let Err(e) = self.publisher.unsubscribe_state(sid).await {
                    warn!(sensor = %sid, error = %e, "set_sensors: unsubscribe failed");
                }
            }
        }
        if let Some((added, removed)) = actuator_diff {
            for aid in &added {
                if let Err(e) = self.publisher.subscribe_state(aid).await {
                    warn!(actuator = %aid, error = %e, "set_actuator: subscribe failed");
                }
            }
            for aid in &removed {
                if let Err(e) = self.publisher.unsubscribe_state(aid).await {
                    warn!(actuator = %aid, error = %e, "set_actuator: unsubscribe failed");
                }
            }
        }

        if changed {
            if let Err(e) = self.persist_config().await {
                warn!(error = %e, "Failed to persist config");
            }
        }
        self.recalculate(therm_id).await;
    }

    /// Recalculate a single thermostat and publish updated state / actuator
    /// cmd, using the normal "command-on-transition" rule. Use
    /// [`recalculate_force`] (or pass `force=true` to
    /// [`recalculate_inner`]) for the post-restart re-sync path.
    pub async fn recalculate(&self, therm_id: &str) {
        self.recalculate_inner(therm_id, false).await;
    }

    /// Like [`recalculate`] but issues the actuator command
    /// **unconditionally** for the computed `desired_act`, regardless of
    /// whether the cached `actuator_state` already matches. The
    /// post-restart settle path uses this; the manual `recalculate_all`
    /// management command also takes a `{"force": true}` flag that
    /// routes here.
    ///
    /// Why force exists: after a plugin/appliance restart the cached
    /// `actuator_state` is restored from retained MQTT but the physical
    /// actuator may have drifted (power blip during the restart window,
    /// external command, dead Z-Wave node etc.). The transition-only
    /// path never re-sends a command that matches cached state, so
    /// reality stays out of sync until something causes a transition.
    /// Forced re-sync resolves the drift.
    pub async fn recalculate_force(&self, therm_id: &str) {
        self.recalculate_inner(therm_id, true).await;
    }

    async fn recalculate_inner(&self, therm_id: &str, force: bool) {
        let (publish_cmd, device_id, state_payload) = {
            let mut b = self.inner.lock().await;
            let cache = b.sensor_cache.clone();
            let Some(rt) = b.thermostats.get_mut(therm_id) else {
                return;
            };

            // 1. Gather readings from cache.
            let mut readings: Vec<f64> = Vec::with_capacity(rt.cfg.sensor_device_ids.len());
            for sid in &rt.cfg.sensor_device_ids {
                if let Some(v) = cache.get(sid) {
                    readings.push(*v);
                }
            }

            // Settle gate (THERM-RESTART-1). Until every configured
            // sensor has reported at least once, we hold — no actuator
            // commands, no state-machine updates. As soon as all
            // sensors are seen we run a one-shot force-eval that
            // unconditionally syncs the actuator to desired (joining
            // the same code path as the explicit `force=true` route).
            // Trivial cases that don't need sensor data — mode "off"
            // and zero configured sensors — settle immediately.
            let all_sensors_seen = rt.cfg.mode == "off"
                || rt.cfg.sensor_device_ids.is_empty()
                || rt
                    .cfg
                    .sensor_device_ids
                    .iter()
                    .all(|s| cache.contains_key(s));
            let do_settle_now = !rt.settled && all_sensors_seen;
            // Suppress only when we're pre-settle and the operator
            // hasn't manually forced. A manual `force=true` always
            // runs (so the operator can override the gate when
            // diagnosing or recovering).
            let suppress = !rt.settled && !all_sensors_seen && !force;
            // Force the actuator command on settle OR on explicit op
            // request. Either way, the final publish_cmd branch ignores
            // the `desired_act != actuator_state` check.
            let force_command = force || do_settle_now;

            if suppress {
                // Pre-settle: keep telemetry in sync (the published
                // state will show whatever rt's last full settled
                // values were) but don't act yet.
                (None, rt.device_id(), rt.state_payload())
            } else if readings.is_empty() && rt.cfg.mode != "off" {
                // 2. Stale path.
                if rt.call_for != "stale" {
                    warn!(id = %rt.cfg.id, "No sensor readings available");
                    rt.call_for = "stale".into();
                    rt.current_temperature = None;
                }
                (None, rt.device_id(), rt.state_payload())
            } else {
                let current = aggregate(&readings, &rt.cfg.aggregation);

                // 3. Desired call_for.
                let new_call: &str = if rt.cfg.mode == "off" {
                    "idle"
                } else {
                    match current {
                        Some(t) => compute_call_for(
                            t,
                            rt.cfg.setpoint,
                            rt.cfg.hysteresis,
                            &rt.cfg.mode,
                            &rt.call_for,
                        ),
                        None => "idle",
                    }
                };
                let desired_act = new_call == "heat" || new_call == "cool";

                // 4. Lockout check.
                let now = Utc::now();
                let force_off = rt.cfg.mode == "off" && rt.actuator_state;
                let remaining = if force_off {
                    0
                } else if desired_act != rt.actuator_state || force_command {
                    lockout_remaining(
                        rt.actuator_state,
                        rt.cfg.min_on_secs,
                        rt.cfg.min_off_secs,
                        rt.actuator_last_change,
                        now,
                    )
                } else {
                    0
                };

                let publish_cmd: Option<(String, Value)> = if remaining > 0 {
                    rt.pending_call = Some(new_call.to_string());
                    rt.lockout_until = Some(now + chrono::Duration::seconds(remaining as i64));
                    rt.current_temperature = current;
                    rt.call_for = new_call.to_string();
                    None
                } else if desired_act != rt.actuator_state || force_command {
                    // Default command shape matches the HomeCore Binary Switch
                    // convention (`on` attribute) used by hc-zwave, hc-hue,
                    // hc-wled, etc. Users with non-standard actuators override
                    // via actuator_on_cmd / actuator_off_cmd in config.
                    let payload = if desired_act {
                        rt.cfg
                            .actuator_on_cmd
                            .clone()
                            .unwrap_or_else(|| json!({"on": true}))
                    } else {
                        rt.cfg
                            .actuator_off_cmd
                            .clone()
                            .unwrap_or_else(|| json!({"on": false}))
                    };
                    let was_transition = desired_act != rt.actuator_state;
                    rt.actuator_state = desired_act;
                    rt.actuator_last_change = Some(now);
                    rt.pending_call = None;
                    rt.lockout_until = None;
                    rt.current_temperature = current;
                    rt.call_for = new_call.to_string();
                    if force_command && !was_transition {
                        info!(
                            id = %rt.cfg.id,
                            actuator = %rt.cfg.actuator_device_id,
                            desired = desired_act,
                            do_settle_now,
                            "Force-issuing actuator command (cached state matched but reality may have drifted)"
                        );
                    }
                    if rt.cfg.actuator_device_id.is_empty() {
                        None
                    } else {
                        Some((rt.cfg.actuator_device_id.clone(), payload))
                    }
                } else {
                    rt.pending_call = None;
                    rt.lockout_until = None;
                    rt.current_temperature = current;
                    rt.call_for = new_call.to_string();
                    None
                };

                if do_settle_now {
                    rt.settled = true;
                    info!(
                        id = %rt.cfg.id,
                        current = ?current,
                        call_for = %rt.call_for,
                        sensors = rt.cfg.sensor_device_ids.len(),
                        "Thermostat settled — all sensors reported"
                    );
                }

                (publish_cmd, rt.device_id(), rt.state_payload())
            }
        };

        // 5. Publish updated state.
        if let Err(e) = self
            .publisher
            .publish_state(&device_id, &state_payload)
            .await
        {
            warn!(device_id, error = %e, "Failed to publish thermostat state");
        }

        // 6. Issue actuator command, if any.
        if let Some((actuator_id, payload)) = publish_cmd {
            let change = DeviceChange::homecore("thermostat");
            let payload = with_command_change_metadata(payload, &change);
            let topic = format!("homecore/devices/{actuator_id}/cmd");
            let publish_err: Option<String> = match serde_json::to_vec(&payload) {
                Ok(bytes) => {
                    match self
                        .mqtt
                        .publish(&topic, QoS::AtMostOnce, false, bytes)
                        .await
                    {
                        Ok(()) => {
                            info!(
                                device_id = %device_id,
                                actuator = %actuator_id,
                                "Thermostat published actuator command"
                            );
                            None
                        }
                        Err(e) => {
                            warn!(actuator_id, error = %e, "Failed to publish actuator command");
                            Some(e.to_string())
                        }
                    }
                }
                Err(e) => {
                    warn!(actuator_id, error = %e, "Failed to serialise actuator payload");
                    Some(e.to_string())
                }
            };

            // Record the outcome on the thermostat's runtime state + re-publish
            // device state so the UI sees it.
            let therm_id = device_id
                .strip_prefix("thermostat_")
                .unwrap_or("")
                .to_string();
            if !therm_id.is_empty() {
                let updated_payload = {
                    let mut b = self.inner.lock().await;
                    if let Some(rt) = b.thermostats.get_mut(&therm_id) {
                        rt.actuator_last_error = publish_err.map(|m| ActuatorError {
                            timestamp: Utc::now(),
                            message: m,
                        });
                        Some(rt.state_payload())
                    } else {
                        None
                    }
                };
                if let Some(p) = updated_payload {
                    let _ = self.publisher.publish_state(&device_id, &p).await;
                }
            }
        }
    }

    /// Recalculate every thermostat (startup reconciliation and lockout retry).
    pub async fn recalculate_all(&self) {
        let ids: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats.keys().cloned().collect()
        };
        for id in ids {
            self.recalculate(&id).await;
        }
    }

    /// Force a re-sync of every thermostat — issues actuator commands
    /// unconditionally (regardless of cached actuator_state) for the
    /// computed desired state. Operator-triggered via the
    /// `recalculate_all` management command with `{"force": true}`.
    /// Use case: appliance/plugin restart left the cached actuator
    /// state out of sync with physical reality and a normal
    /// recalculate_all wouldn't issue any commands. THERM-RESTART-1.
    pub async fn recalculate_all_force(&self) {
        let ids: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats.keys().cloned().collect()
        };
        for id in ids {
            self.recalculate_force(&id).await;
        }
    }

    /// Tick — recalculate any thermostats that have a pending_call (i.e. a
    /// short-cycle lockout deferred an actuator command).
    pub async fn tick(&self) {
        let ids: Vec<String> = {
            let b = self.inner.lock().await;
            b.thermostats
                .values()
                .filter(|r| r.pending_call.is_some())
                .map(|r| r.cfg.id.clone())
                .collect()
        };
        for id in ids {
            self.recalculate(&id).await;
        }
    }

    /// Write current in-memory config back to disk so restart is idempotent.
    /// Also refreshes the sync-readable thermostat snapshot.
    async fn persist_config(&self) -> Result<()> {
        let cfg = self.assemble_config().await;
        let toml_str = toml::to_string_pretty(&cfg)?;
        let path = {
            let b = self.inner.lock().await;
            b.config_path.clone()
        };
        // Atomic write: temp + rename
        let tmp = format!("{path}.partial");
        std::fs::write(&tmp, toml_str)?;
        std::fs::rename(&tmp, &path)?;
        debug!(path, "Persisted config to disk");
        self.refresh_snapshot().await;
        Ok(())
    }

    /// Rebuild a Config from current in-memory state (for persistence).
    async fn assemble_config(&self) -> Config {
        // Reload from disk to preserve [plugin] / [logging] sections that aren't
        // owned by runtime, then overwrite [[thermostat]] entries from memory.
        let b = self.inner.lock().await;
        let mut cfg = Config::load(&b.config_path).unwrap_or_else(|_| Config {
            homecore: crate::config::HomecoreSection {
                plugin_id: self.plugin_id.clone(),
                broker_host: "127.0.0.1".into(),
                broker_port: 1883,
                password: String::new(),
                heartbeat_secs: 60,
            },
            logging: Default::default(),
            thermostats: vec![],
        });
        cfg.thermostats = b.thermostats.values().map(|r| r.cfg.clone()).collect();
        cfg.thermostats.sort_by(|a, b| a.id.cmp(&b.id));
        cfg
    }

    /// Add a new thermostat from a runtime command (UI wizard). Persists to
    /// config.toml, registers the device, subscribes any new sensors, and
    /// triggers an initial recalculation.
    pub async fn add_thermostat(&self, entry: ThermostatEntry) -> Result<String> {
        // Validate mode + aggregation before doing anything irreversible.
        if !matches!(entry.mode.as_str(), "heat" | "cool" | "off") {
            return Err(anyhow::anyhow!("invalid mode: {}", entry.mode));
        }
        if !matches!(entry.aggregation.as_str(), "average" | "min" | "max") {
            return Err(anyhow::anyhow!(
                "invalid aggregation: {}",
                entry.aggregation
            ));
        }
        if entry.hysteresis < 0.0 {
            return Err(anyhow::anyhow!("hysteresis must be non-negative"));
        }

        let therm_id = entry.id.clone();
        let device_id = format!("thermostat_{therm_id}");
        let display_name = entry.name.clone();

        let sensor_added: Vec<String>;
        let actuator_added: Vec<String>;
        {
            let mut b = self.inner.lock().await;
            if b.thermostats.contains_key(&therm_id) {
                return Err(anyhow::anyhow!("thermostat already exists: {therm_id}"));
            }
            let old_sensor_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            let old_actuator_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();
            b.thermostats.insert(therm_id.clone(), Runtime::new(entry));
            let new_sensor_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            let new_actuator_union: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();
            sensor_added = new_sensor_union
                .difference(&old_sensor_union)
                .cloned()
                .collect();
            actuator_added = new_actuator_union
                .difference(&old_actuator_union)
                .cloned()
                .collect();
        }

        // Subscribe to new sensors and actuators.
        for sid in &sensor_added {
            if let Err(e) = self.publisher.subscribe_state(sid).await {
                warn!(sensor = %sid, error = %e, "add_thermostat: subscribe failed");
            }
        }
        for aid in &actuator_added {
            if let Err(e) = self.publisher.subscribe_state(aid).await {
                warn!(actuator = %aid, error = %e, "add_thermostat: subscribe actuator failed");
            }
        }

        // Register the new device.
        self.publisher
            .register_device_full(&device_id, &display_name, Some("thermostat"), None, None)
            .await?;
        crate::schema::publish(&self.publisher, &device_id).await?;
        self.publisher.subscribe_commands(&device_id).await?;
        self.publisher
            .publish_availability(&device_id, true)
            .await?;

        // Persist + initial recalc.
        self.persist_config().await?;
        self.recalculate(&therm_id).await;
        Ok(therm_id)
    }

    /// Remove a thermostat. Unsubscribes orphan sensors and actuators (those
    /// no longer referenced by any other thermostat), drops the device,
    /// persists config.
    pub async fn remove_thermostat(&self, therm_id: &str) -> Result<()> {
        let device_id = format!("thermostat_{therm_id}");
        let sensors_removed: Vec<String>;
        let actuators_removed: Vec<String>;
        {
            let mut b = self.inner.lock().await;
            if !b.thermostats.contains_key(therm_id) {
                return Err(anyhow::anyhow!("thermostat not found: {therm_id}"));
            }
            let removed_sensors: std::collections::HashSet<String> = b
                .thermostats
                .get(therm_id)
                .unwrap()
                .cfg
                .sensor_device_ids
                .iter()
                .cloned()
                .collect();
            let this_actuator: Option<String> = {
                let a = &b.thermostats.get(therm_id).unwrap().cfg.actuator_device_id;
                if a.is_empty() {
                    None
                } else {
                    Some(a.clone())
                }
            };
            b.thermostats.remove(therm_id);
            // Anything no longer referenced by remaining thermostats is orphan.
            let still_used_sensors: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            let still_used_actuators: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();
            sensors_removed = removed_sensors
                .difference(&still_used_sensors)
                .cloned()
                .collect();
            actuators_removed = this_actuator
                .filter(|a| !still_used_actuators.contains(a))
                .into_iter()
                .collect();
        }

        for sid in &sensors_removed {
            if let Err(e) = self.publisher.unsubscribe_state(sid).await {
                warn!(sensor = %sid, error = %e, "remove_thermostat: unsubscribe failed");
            }
        }
        for aid in &actuators_removed {
            if let Err(e) = self.publisher.unsubscribe_state(aid).await {
                warn!(actuator = %aid, error = %e, "remove_thermostat: unsubscribe actuator failed");
            }
        }

        // Mark device offline + clear retained state.
        let _ = self.publisher.publish_availability(&device_id, false).await;
        let topic = format!("homecore/devices/{device_id}/state");
        let _ = self
            .mqtt
            .publish(&topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
            .await;

        self.persist_config().await?;
        info!(thermostat = %therm_id, "Removed thermostat");
        Ok(())
    }

    /// Return the current list of thermostat configs as JSON.
    pub async fn get_thermostats(&self) -> Vec<Value> {
        let b = self.inner.lock().await;
        let mut out: Vec<_> = b
            .thermostats
            .values()
            .map(|r| serde_json::to_value(&r.cfg).unwrap_or(Value::Null))
            .collect();
        out.sort_by(|a, b| {
            a.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        });
        out
    }

    /// Reload config from disk. Applies changes to in-memory thermostats and
    /// adjusts MQTT subscriptions to match the new sensor set.
    pub async fn reload_config(&self) -> Result<()> {
        let path = {
            let b = self.inner.lock().await;
            b.config_path.clone()
        };
        let new_cfg = Config::load(&path)?;
        let added_sensors: Vec<String>;
        let removed_sensors: Vec<String>;
        let added_actuators: Vec<String>;
        let removed_actuators: Vec<String>;

        {
            let mut b = self.inner.lock().await;
            let old_sensor_set: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .flat_map(|r| r.cfg.sensor_device_ids.iter().cloned())
                .collect();
            let new_sensor_set: std::collections::HashSet<String> = new_cfg
                .thermostats
                .iter()
                .flat_map(|t| t.sensor_device_ids.iter().cloned())
                .collect();
            added_sensors = new_sensor_set
                .difference(&old_sensor_set)
                .cloned()
                .collect();
            removed_sensors = old_sensor_set
                .difference(&new_sensor_set)
                .cloned()
                .collect();

            let old_actuator_set: std::collections::HashSet<String> = b
                .thermostats
                .values()
                .filter(|r| !r.cfg.actuator_device_id.is_empty())
                .map(|r| r.cfg.actuator_device_id.clone())
                .collect();
            let new_actuator_set: std::collections::HashSet<String> = new_cfg
                .thermostats
                .iter()
                .filter(|t| !t.actuator_device_id.is_empty())
                .map(|t| t.actuator_device_id.clone())
                .collect();
            added_actuators = new_actuator_set
                .difference(&old_actuator_set)
                .cloned()
                .collect();
            removed_actuators = old_actuator_set
                .difference(&new_actuator_set)
                .cloned()
                .collect();

            // Upsert thermostat configs. Preserve runtime state when the
            // thermostat id already exists.
            for entry in new_cfg.thermostats {
                b.thermostats
                    .entry(entry.id.clone())
                    .and_modify(|rt| rt.cfg = entry.clone())
                    .or_insert_with(|| Runtime::new(entry));
            }
        }

        // Apply subscription diffs via the publisher so the shared
        // SubscriptionTracker is updated — subscriptions survive reconnects.
        for sid in &added_sensors {
            if let Err(e) = self.publisher.subscribe_state(sid).await {
                warn!(sensor = %sid, error = %e, "Reload: subscribe_state failed");
            }
        }
        for sid in &removed_sensors {
            if let Err(e) = self.publisher.unsubscribe_state(sid).await {
                warn!(sensor = %sid, error = %e, "Reload: unsubscribe_state failed");
            }
        }
        for aid in &added_actuators {
            if let Err(e) = self.publisher.subscribe_state(aid).await {
                warn!(actuator = %aid, error = %e, "Reload: actuator subscribe failed");
            }
        }
        for aid in &removed_actuators {
            if let Err(e) = self.publisher.unsubscribe_state(aid).await {
                warn!(actuator = %aid, error = %e, "Reload: actuator unsubscribe failed");
            }
        }

        info!(
            added_sensors = added_sensors.len(),
            removed_sensors = removed_sensors.len(),
            added_actuators = added_actuators.len(),
            removed_actuators = removed_actuators.len(),
            "Config reloaded"
        );
        self.refresh_snapshot().await;
        Ok(())
    }
}

// ── Helper: build the per-plugin bridge command channel ──────────────────────

/// Open a one-shot channel used by management handlers to trigger bridge
/// actions (recalculate_all, reload_config) from synchronous callback context.
pub enum BridgeTask {
    RecalculateAll,
    /// Force-recalculate every thermostat — issues actuator commands
    /// regardless of whether cached state already matches desired.
    /// Sent by the `recalculate_all` management command when invoked
    /// with `{"force": true}`. THERM-RESTART-1 recovery path.
    RecalculateAllForce,
    ReloadConfig,
    AddThermostat {
        entry: Box<ThermostatEntry>,
    },
    RemoveThermostat {
        id: String,
    },
}

pub fn bridge_task_channel() -> (mpsc::Sender<BridgeTask>, mpsc::Receiver<BridgeTask>) {
    mpsc::channel(16)
}

#[cfg(test)]
mod tests {
    use super::interpret_actuator_on;
    use serde_json::json;

    #[test]
    fn interprets_on_bool() {
        assert_eq!(interpret_actuator_on(&json!({"on": true})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"on": false})), Some(false));
    }

    #[test]
    fn interprets_state_string() {
        assert_eq!(interpret_actuator_on(&json!({"state": "on"})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"state": "ON"})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"state": "off"})), Some(false));
        assert_eq!(interpret_actuator_on(&json!({"state": "OFF"})), Some(false));
    }

    #[test]
    fn interprets_power_variants() {
        assert_eq!(interpret_actuator_on(&json!({"power": true})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"power": "on"})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"power": "off"})), Some(false));
    }

    #[test]
    fn on_takes_priority_over_state() {
        // If both are present, `on` wins.
        let v = json!({"on": false, "state": "on"});
        assert_eq!(interpret_actuator_on(&v), Some(false));
    }

    #[test]
    fn unrecognised_payload_returns_none() {
        assert_eq!(interpret_actuator_on(&json!({})), None);
        assert_eq!(interpret_actuator_on(&json!({"brightness": 128})), None);
        assert_eq!(interpret_actuator_on(&json!({"state": "dimmed"})), None);
    }
}
