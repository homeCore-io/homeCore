//! Timer helper devices — virtual countdown timers for automation rules.
//!
//! Timers appear as first-class devices in the state store (`plugin_id = "core.glue"`,
//! `device_type = "timer"`).
//! They accept commands on the standard `homecore/devices/{id}/cmd` MQTT topic,
//! and emit `DeviceStateChanged` events on the internal bus so the rule engine
//! can trigger on timer state transitions.
//!
//! # Creating a timer
//!
//! `POST /api/v1/glue` with `{"type": "timer", "id": "garage_close", "name": "Garage close delay"}`.
//! This creates a device with `device_id = "timer_garage_close"`.
//!
//! # Commands  (PATCH /api/v1/devices/timer_garage_close/state)
//!
//! ```json
//! { "command": "start",   "duration_secs": 600, "label": "optional", "repeat": false }
//!   (duration_secs and repeat optional: left out, the timer's own — `default_secs`, `repeat`)
//! { "command": "pause" }
//! { "command": "resume" }
//! { "command": "cancel" }
//! { "command": "restart" }
//! { "command": "extend",  "duration_secs": 300 }
//! ```
//!
//! `extend` adds time to a running or paused timer, as "five more minutes"
//! does; the next cycle of a repeating one is its usual length.
//!
//! # States
//!
//! ```text
//! idle --start--> running --elapsed--> finished
//!                    |                   |
//!                  pause             (repeat: start new cycle)
//!                    v
//!                 paused --resume--> running
//!
//!  cancel from any state -> cancelled
//! ```
//!
//! # Rule integration
//!
//! ```toml
//! [[triggers]]
//! type      = "DeviceStateChanged"
//! device_id = "timer_garage_close"
//! attribute = "state"
//!
//! [[conditions]]
//! type      = "DeviceState"
//! device_id = "timer_garage_close"
//! attribute = "state"
//! op        = "eq"
//! value     = "finished"
//! ```

use chrono::Utc;
use hc_state::StateStore;
use hc_types::device::{extract_change_from_command_payload, DeviceChange};
use hc_types::event::Event;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::EventBus;

pub const TIMER_ID_PREFIX: &str = "timer_";

// ---------------------------------------------------------------------------
// Command payload
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum TimerCommand {
    Start {
        /// This run's length. Left out, the timer's own (`default_secs`).
        #[serde(default)]
        duration_secs: Option<u64>,
        #[serde(default)]
        label: Option<String>,
        /// Left out, the timer's own setting — a start no longer turns
        /// "start again when it finishes" off.
        #[serde(default)]
        repeat: Option<bool>,
    },
    Pause,
    Resume,
    Cancel,
    /// Reset to full duration and restart immediately.
    Restart,
    /// Add time to what's left.
    Extend {
        duration_secs: u64,
    },
}

// ---------------------------------------------------------------------------
// Per-timer task control
// ---------------------------------------------------------------------------

enum TimerCtrl {
    Cancel,
}

struct TimerHandle {
    /// Full configured duration — needed for Restart without re-reading state.
    duration_secs: u64,
    ctrl_tx: mpsc::Sender<TimerCtrl>,
    _join: JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct TimerManager {
    bus: EventBus,
    pub_bus: EventBus,
    state: StateStore,
    handles: Arc<RwLock<HashMap<String, TimerHandle>>>,
}

impl TimerManager {
    pub fn new(bus: EventBus, pub_bus: EventBus, state: StateStore) -> Self {
        Self {
            bus,
            pub_bus,
            state,
            handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Reconstruct running timers from persisted state, then listen for commands.
    /// Call `tokio::spawn(manager.start())`.
    pub async fn start(self) {
        self.reconstruct_timers().await;

        let mut rx = self.bus.subscribe();
        info!("TimerManager started");
        loop {
            match rx.recv().await {
                Ok(Event::MqttMessage { topic, payload, .. }) => {
                    if let Some(device_id) = parse_timer_cmd_topic(&topic) {
                        let payload = crate::glue::action_as_command(&payload);
                        self.handle_cmd(&device_id, &payload).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("TimerManager lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Startup reconstruction
    // -----------------------------------------------------------------------

    async fn reconstruct_timers(&self) {
        let devices = match self.state.list_devices().await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "TimerManager: failed to list devices on startup");
                return;
            }
        };

        for dev in devices {
            let is_timer = dev.plugin_id == "core.glue" && dev.device_id.starts_with("timer_");
            if !is_timer {
                continue;
            }
            let state_str = dev
                .attributes
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("idle");

            match state_str {
                "running" => {
                    let duration_secs = dev
                        .attributes
                        .get("duration_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let started_at = dev
                        .attributes
                        .get("started_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc));
                    let remaining_secs = if let Some(started) = started_at {
                        let elapsed = (Utc::now() - started).num_seconds().max(0) as u64;
                        duration_secs.saturating_sub(elapsed)
                    } else {
                        duration_secs
                    };
                    let repeat = dev
                        .attributes
                        .get("repeat")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    if remaining_secs > 0 {
                        info!(
                            device_id = %dev.device_id,
                            remaining_secs,
                            "TimerManager: reconstructing running timer"
                        );
                        self.spawn_timer_task(
                            &dev.device_id,
                            remaining_secs,
                            duration_secs,
                            repeat,
                        )
                        .await;
                    } else {
                        info!(
                            device_id = %dev.device_id,
                            "TimerManager: timer elapsed while stopped — firing now"
                        );
                        self.set_state(
                            &dev.device_id,
                            "finished",
                            None,
                            DeviceChange::homecore("timer_manager"),
                        )
                        .await;
                    }
                }
                "paused" => {
                    debug!(
                        device_id = %dev.device_id,
                        "TimerManager: paused timer — waiting for resume command"
                    );
                }
                _ => {} // idle / finished / cancelled — nothing to do
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------------

    async fn handle_cmd(&self, device_id: &str, payload: &[u8]) {
        let value: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(%device_id, error = %e, "Timer: invalid command payload");
                return;
            }
        };
        let change = extract_change_from_command_payload(&value).unwrap_or_default();
        let cmd: TimerCommand = match serde_json::from_value(value) {
            Ok(c) => c,
            Err(e) => {
                warn!(%device_id, error = %e, "Timer: invalid command payload");
                return;
            }
        };
        debug!(%device_id, ?cmd, "Timer command received");

        match cmd {
            TimerCommand::Start {
                duration_secs,
                label,
                repeat,
            } => {
                // The timer's own length and repeat, as its helper settings
                // say: `default_secs` (older timers only have `duration_secs`,
                // which was their own until a start overwrote it).
                let attrs = match self.state.get_device(device_id).await {
                    Ok(Some(dev)) => dev.attributes,
                    _ => Default::default(),
                };
                let num = |k: &str| attrs.get(k).and_then(|v| v.as_u64());
                let own_secs = num("default_secs").or_else(|| num("duration_secs"));
                let Some(duration_secs) = duration_secs.or(own_secs).filter(|s| *s > 0) else {
                    warn!(%device_id, "Timer: start with no length, and the timer has none of its own");
                    return;
                };
                let repeat = repeat
                    .or_else(|| attrs.get("repeat").and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                self.cancel_task(device_id).await;
                let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                extra.insert("duration_secs", serde_json::json!(duration_secs));
                extra.insert("remaining_secs", serde_json::json!(duration_secs));
                extra.insert("repeat", serde_json::json!(repeat));
                if let Some(own) = own_secs {
                    extra.insert("default_secs", serde_json::json!(own));
                }
                if let Some(lbl) = label {
                    extra.insert("label", serde_json::json!(lbl));
                }
                self.set_state(device_id, "running", Some(extra), change.clone())
                    .await;
                self.spawn_timer_task(device_id, duration_secs, duration_secs, repeat)
                    .await;
            }

            TimerCommand::Pause => {
                let has_task = self.handles.read().await.contains_key(device_id);
                if !has_task {
                    warn!(%device_id, "Timer: pause requested but timer not running");
                    return;
                }
                // Compute remaining before cancelling the task.
                let remaining_secs = self.compute_remaining(device_id).await;
                self.cancel_task(device_id).await;
                let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                extra.insert("remaining_secs", serde_json::json!(remaining_secs));
                self.set_state(device_id, "paused", Some(extra), change.clone())
                    .await;
            }

            TimerCommand::Resume => {
                let dev = match self.state.get_device(device_id).await {
                    Ok(Some(d)) => d,
                    _ => {
                        warn!(%device_id, "Timer: resume — device not found");
                        return;
                    }
                };
                let state_str = dev
                    .attributes
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if state_str != "paused" {
                    warn!(%device_id, %state_str, "Timer: resume called but timer not paused");
                    return;
                }
                let remaining_secs = dev
                    .attributes
                    .get("remaining_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let duration_secs = dev
                    .attributes
                    .get("duration_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(remaining_secs);
                let repeat = dev
                    .attributes
                    .get("repeat")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if remaining_secs > 0 {
                    // What's left is worked out from when it started and how
                    // long it runs: started as long ago as it has run, so a
                    // timer resumed and then read (or restarted under) agrees.
                    let ran = duration_secs.saturating_sub(remaining_secs);
                    let started = Utc::now() - chrono::Duration::seconds(ran as i64);
                    let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                    extra.insert("started_at", serde_json::json!(started.to_rfc3339()));
                    extra.insert("remaining_secs", serde_json::json!(remaining_secs));
                    self.set_state(device_id, "running", Some(extra), change.clone())
                        .await;
                    self.spawn_timer_task(device_id, remaining_secs, duration_secs, repeat)
                        .await;
                } else {
                    self.set_state(device_id, "finished", None, change.clone())
                        .await;
                }
            }

            TimerCommand::Extend {
                duration_secs: more,
            } => {
                let Ok(Some(dev)) = self.state.get_device(device_id).await else {
                    warn!(%device_id, "Timer: extend — device not found");
                    return;
                };
                let attr_u64 = |k: &str| dev.attributes.get(k).and_then(|v| v.as_u64());
                let state_str = dev
                    .attributes
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let duration_secs = attr_u64("duration_secs").unwrap_or(0);
                let repeat = dev
                    .attributes
                    .get("repeat")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match state_str {
                    "running" => {
                        let left = self.compute_remaining(device_id).await + more;
                        // The usual length, for a repeating timer's next cycle.
                        let usual = {
                            let handles = self.handles.read().await;
                            handles.get(device_id).map(|h| h.duration_secs)
                        }
                        .unwrap_or(duration_secs);
                        self.cancel_task(device_id).await;
                        let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                        // What's left is worked out from when it started and
                        // how long it runs, so the length grows and the start
                        // stays.
                        extra.insert("duration_secs", serde_json::json!(duration_secs + more));
                        extra.insert("remaining_secs", serde_json::json!(left));
                        if let Some(started) = dev.attributes.get("started_at") {
                            extra.insert("started_at", started.clone());
                        }
                        self.set_state(device_id, "running", Some(extra), change.clone())
                            .await;
                        self.spawn_timer_task(device_id, left, usual, repeat).await;
                    }
                    "paused" => {
                        let left = attr_u64("remaining_secs").unwrap_or(0) + more;
                        let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                        extra.insert("duration_secs", serde_json::json!(duration_secs + more));
                        extra.insert("remaining_secs", serde_json::json!(left));
                        self.set_state(device_id, "paused", Some(extra), change.clone())
                            .await;
                    }
                    other => {
                        warn!(%device_id, state = %other, "Timer: extend — not running or paused");
                    }
                }
            }

            TimerCommand::Cancel => {
                self.cancel_task(device_id).await;
                self.set_state(device_id, "cancelled", None, change.clone())
                    .await;
            }

            TimerCommand::Restart => {
                // Use stored duration — no need to accept it again.
                let duration_secs = {
                    let handles = self.handles.read().await;
                    handles.get(device_id).map(|h| h.duration_secs)
                };
                let duration_secs = if let Some(d) = duration_secs {
                    d
                } else {
                    match self.state.get_device(device_id).await {
                        Ok(Some(dev)) => dev
                            .attributes
                            .get("duration_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        _ => {
                            warn!(%device_id, "Timer: restart — device not found");
                            return;
                        }
                    }
                };
                if duration_secs == 0 {
                    warn!(%device_id, "Timer: restart called but no duration configured");
                    return;
                }
                let repeat = match self.state.get_device(device_id).await {
                    Ok(Some(dev)) => dev
                        .attributes
                        .get("repeat")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    _ => false,
                };
                self.cancel_task(device_id).await;
                let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                extra.insert("remaining_secs", serde_json::json!(duration_secs));
                self.set_state(device_id, "running", Some(extra), change)
                    .await;
                self.spawn_timer_task(device_id, duration_secs, duration_secs, repeat)
                    .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Timer task
    // -----------------------------------------------------------------------

    async fn spawn_timer_task(
        &self,
        device_id: &str,
        remaining_secs: u64,
        duration_secs: u64,
        repeat: bool,
    ) {
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<TimerCtrl>(4);
        let id = device_id.to_string();
        let bus = self.pub_bus.clone();
        let state = self.state.clone();
        let handles_ref = Arc::clone(&self.handles);

        let join = tokio::spawn(async move {
            let mut rem = remaining_secs;
            loop {
                debug!(device_id = %id, remaining_secs = rem, "Timer sleeping");
                let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(rem));
                tokio::select! {
                    _ = sleep => {
                        info!(device_id = %id, "Timer finished");
                        fire_timer(&id, &state, &bus).await;
                        if repeat {
                            rem = duration_secs;
                            reset_for_repeat(&id, duration_secs, &state, &bus).await;
                            continue;
                        }
                        handles_ref.write().await.remove(&id);
                        break;
                    }
                    ctrl = ctrl_rx.recv() => {
                        match ctrl {
                            Some(TimerCtrl::Cancel) | None => break,
                        }
                    }
                }
            }
        });

        let handle = TimerHandle {
            duration_secs,
            ctrl_tx,
            _join: join,
        };
        self.handles
            .write()
            .await
            .insert(device_id.to_string(), handle);
    }

    async fn cancel_task(&self, device_id: &str) {
        let mut handles = self.handles.write().await;
        if let Some(h) = handles.remove(device_id) {
            let _ = h.ctrl_tx.send(TimerCtrl::Cancel).await;
        }
    }

    // -----------------------------------------------------------------------
    // State helpers
    // -----------------------------------------------------------------------

    /// Estimate remaining_secs by reading started_at from state store.
    async fn compute_remaining(&self, device_id: &str) -> u64 {
        let Ok(Some(dev)) = self.state.get_device(device_id).await else {
            return 0;
        };
        let duration_secs = dev
            .attributes
            .get("duration_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let started_at = dev
            .attributes
            .get("started_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        if let Some(started) = started_at {
            let elapsed = (Utc::now() - started).num_seconds().max(0) as u64;
            return duration_secs.saturating_sub(elapsed);
        }
        duration_secs
    }

    /// Apply a state transition, optionally merging extra attributes, then emit
    /// `DeviceStateChanged` on the bus.
    async fn set_state(
        &self,
        device_id: &str,
        new_state: &str,
        extra: Option<HashMap<&str, serde_json::Value>>,
        change: DeviceChange,
    ) {
        let Ok(Some(mut dev)) = self.state.get_device(device_id).await else {
            warn!(%device_id, "Timer: device not found in state store");
            return;
        };
        let previous = dev.attributes.clone();
        dev.attributes
            .insert("state".into(), serde_json::json!(new_state));
        if new_state == "running" {
            dev.attributes.insert(
                "started_at".into(),
                serde_json::json!(Utc::now().to_rfc3339()),
            );
        }
        if let Some(extra) = extra {
            for (k, v) in extra {
                dev.attributes.insert(k.into(), v);
            }
        }
        dev.last_seen = Utc::now();
        dev.last_change = Some(change.clone());
        if let Err(e) = self.state.upsert_device(&dev).await {
            warn!(%device_id, error = %e, "Timer: failed to persist state");
            return;
        }
        let device_name = dev.effective_name().to_string();
        let current = dev.attributes;
        let changed: Vec<String> = current
            .keys()
            .filter(|k| previous.get(*k) != current.get(*k))
            .chain(previous.keys().filter(|k| !current.contains_key(*k)))
            .cloned()
            .collect();
        let remaining = current.get("remaining_secs").and_then(|v| v.as_u64());

        let _ = self.pub_bus.publish(Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            device_name: Some(device_name.clone()),
            previous,
            current,
            changed,
            change,
        });

        // Emit a dedicated timer event for the activity stream.
        let _ = self.pub_bus.publish(Event::TimerStateChanged {
            timestamp: Utc::now(),
            timer_id: device_id.to_string(),
            timer_name: None,
            state: new_state.to_string(),
            remaining_secs: remaining,
        });
    }
}

// ---------------------------------------------------------------------------
// Free functions used inside spawned tasks (no &self available)
// ---------------------------------------------------------------------------

async fn fire_timer(device_id: &str, state: &StateStore, bus: &EventBus) {
    let Ok(Some(mut dev)) = state.get_device(device_id).await else {
        return;
    };
    let previous = dev.attributes.clone();
    dev.attributes
        .insert("state".into(), serde_json::json!("finished"));
    dev.attributes
        .insert("remaining_secs".into(), serde_json::json!(0_u64));
    dev.last_seen = Utc::now();
    let change = DeviceChange::homecore("timer_manager");
    dev.last_change = Some(change.clone());
    let _ = state.upsert_device(&dev).await;
    let device_name = dev.effective_name().to_string();
    let current = dev.attributes;
    let changed: Vec<String> = current
        .keys()
        .filter(|k| previous.get(*k) != current.get(*k))
        .chain(previous.keys().filter(|k| !current.contains_key(*k)))
        .cloned()
        .collect();
    let _ = bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        device_name: Some(device_name.clone()),
        previous,
        current,
        changed,
        change,
    });
    let _ = bus.publish(Event::TimerStateChanged {
        timestamp: Utc::now(),
        timer_id: device_id.to_string(),
        timer_name: None,
        state: "finished".to_string(),
        remaining_secs: Some(0),
    });
}

async fn reset_for_repeat(device_id: &str, duration_secs: u64, state: &StateStore, bus: &EventBus) {
    let Ok(Some(mut dev)) = state.get_device(device_id).await else {
        return;
    };
    let previous = dev.attributes.clone();
    dev.attributes
        .insert("state".into(), serde_json::json!("running"));
    dev.attributes.insert(
        "started_at".into(),
        serde_json::json!(Utc::now().to_rfc3339()),
    );
    dev.attributes
        .insert("remaining_secs".into(), serde_json::json!(duration_secs));
    // A cycle someone extended is over: the next is the usual length.
    dev.attributes
        .insert("duration_secs".into(), serde_json::json!(duration_secs));
    dev.last_seen = Utc::now();
    let change = DeviceChange::homecore("timer_manager");
    dev.last_change = Some(change.clone());
    let _ = state.upsert_device(&dev).await;
    let device_name = dev.effective_name().to_string();
    let current = dev.attributes;
    let changed: Vec<String> = current
        .keys()
        .filter(|k| previous.get(*k) != current.get(*k))
        .chain(previous.keys().filter(|k| !current.contains_key(*k)))
        .cloned()
        .collect();
    let _ = bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        device_name: Some(device_name.clone()),
        previous,
        current,
        changed,
        change,
    });
}

// ---------------------------------------------------------------------------
// Topic parsing
// ---------------------------------------------------------------------------

fn parse_timer_cmd_topic(topic: &str) -> Option<String> {
    // homecore/devices/timer_{slug}/cmd
    let mut parts = topic.splitn(4, '/');
    let p0 = parts.next()?;
    let p1 = parts.next()?;
    let p2 = parts.next()?;
    let p3 = parts.next()?;
    if p0 == "homecore" && p1 == "devices" && p2.starts_with(TIMER_ID_PREFIX) && p3 == "cmd" {
        Some(p2.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::device::DeviceState;

    async fn manager() -> TimerManager {
        let tmp = std::env::temp_dir().join(format!("hc_timer_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("temp dir");
        let store = StateStore::open(
            tmp.join("state.redb").to_str().unwrap(),
            tmp.join("history.db").to_str().unwrap(),
        )
        .await
        .expect("store opens");
        let mut dev = DeviceState::new("timer_tea", "Tea", "core.glue");
        dev.device_type = Some("timer".into());
        for (k, v) in [
            ("state", serde_json::json!("idle")),
            ("duration_secs", serde_json::json!(240)),
            ("default_secs", serde_json::json!(240)),
            ("remaining_secs", serde_json::json!(0)),
            ("repeat", serde_json::json!(true)),
        ] {
            dev.attributes.insert(k.into(), v);
        }
        store.upsert_device(&dev).await.unwrap();
        TimerManager::new(EventBus::new(16), EventBus::new(16), store)
    }

    async fn attrs(m: &TimerManager) -> HashMap<String, serde_json::Value> {
        let d = m.state.get_device("timer_tea").await.unwrap().unwrap();
        d.attributes.into_iter().collect()
    }

    #[tokio::test]
    async fn a_start_keeps_the_timers_own_length_and_repeat() {
        let m = manager().await;
        m.handle_cmd("timer_tea", br#"{"command":"start"}"#).await;
        let a = attrs(&m).await;
        assert_eq!(a["state"], "running");
        assert_eq!(a["duration_secs"], 240, "its own length");
        assert_eq!(a["repeat"], true, "a start doesn't turn repeating off");

        // A rule's longer start is this run only.
        m.handle_cmd("timer_tea", br#"{"command":"start","duration_secs":600}"#)
            .await;
        let a = attrs(&m).await;
        assert_eq!(a["duration_secs"], 600);
        assert_eq!(a["default_secs"], 240, "its own length is kept");
        assert_eq!(a["repeat"], true);
        m.cancel_task("timer_tea").await;
    }
}
