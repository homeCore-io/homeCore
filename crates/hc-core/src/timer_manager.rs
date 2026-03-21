//! Timer helper devices — virtual countdown timers for automation rules.
//!
//! Timers appear as first-class devices in the state store (`plugin_id = "core.timer"`).
//! They accept commands on the standard `homecore/devices/{id}/cmd` MQTT topic,
//! and emit `DeviceStateChanged` events on the internal bus so the rule engine
//! can trigger on timer state transitions.
//!
//! # Creating a timer
//!
//! `POST /api/v1/timers` with `{"id": "garage_close", "label": "Garage close delay"}`.
//! This creates a device with `device_id = "timer_garage_close"`.
//!
//! # Commands  (PATCH /api/v1/devices/timer_garage_close/state)
//!
//! ```json
//! { "command": "start",   "duration_ms": 600000, "label": "optional", "repeat": false }
//! { "command": "pause" }
//! { "command": "resume" }
//! { "command": "cancel" }
//! { "command": "restart" }
//! ```
//!
//! # States
//!
//! ```
//! idle ──start──► running ──elapsed──► fired
//!                    │                   │
//!                  pause             (repeat: start new cycle)
//!                    ▼
//!                 paused ──resume──► running
//!
//!  cancel from any state → cancelled
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
//! value     = "fired"
//! ```

use chrono::Utc;
use hc_state::StateStore;
use hc_types::event::Event;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::EventBus;

pub const TIMER_PLUGIN_ID: &str = "core.timer";
pub const TIMER_ID_PREFIX: &str = "timer_";

// ---------------------------------------------------------------------------
// Command payload
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum TimerCommand {
    Start {
        duration_ms: u64,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        repeat: Option<bool>,
    },
    Pause,
    Resume,
    Cancel,
    /// Reset to full duration and restart immediately.
    Restart,
}

// ---------------------------------------------------------------------------
// Per-timer task control
// ---------------------------------------------------------------------------

enum TimerCtrl {
    Cancel,
}

struct TimerHandle {
    /// Full configured duration — needed for Restart without re-reading state.
    duration_ms: u64,
    ctrl_tx: mpsc::Sender<TimerCtrl>,
    _join: JoinHandle<()>,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct TimerManager {
    bus: EventBus,
    state: StateStore,
    handles: Arc<RwLock<HashMap<String, TimerHandle>>>,
}

impl TimerManager {
    pub fn new(bus: EventBus, state: StateStore) -> Self {
        Self {
            bus,
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
            if dev.plugin_id != TIMER_PLUGIN_ID {
                continue;
            }
            let state_str = dev
                .attributes
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("idle");

            match state_str {
                "running" => {
                    let duration_ms = dev
                        .attributes
                        .get("duration_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let started_at = dev
                        .attributes
                        .get("started_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc));
                    let remaining_ms = if let Some(started) = started_at {
                        let elapsed =
                            (Utc::now() - started).num_milliseconds().max(0) as u64;
                        duration_ms.saturating_sub(elapsed)
                    } else {
                        duration_ms
                    };
                    let repeat = dev
                        .attributes
                        .get("repeat")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    if remaining_ms > 0 {
                        info!(
                            device_id = %dev.device_id,
                            remaining_ms,
                            "TimerManager: reconstructing running timer"
                        );
                        self.spawn_timer_task(&dev.device_id, remaining_ms, duration_ms, repeat)
                            .await;
                    } else {
                        info!(
                            device_id = %dev.device_id,
                            "TimerManager: timer elapsed while stopped — firing now"
                        );
                        self.set_state(&dev.device_id, "fired", None).await;
                    }
                }
                "paused" => {
                    debug!(
                        device_id = %dev.device_id,
                        "TimerManager: paused timer — waiting for resume command"
                    );
                }
                _ => {} // idle / fired / cancelled — nothing to do
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------------

    async fn handle_cmd(&self, device_id: &str, payload: &[u8]) {
        let cmd: TimerCommand = match serde_json::from_slice(payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(%device_id, error = %e, "Timer: invalid command payload");
                return;
            }
        };
        debug!(%device_id, ?cmd, "Timer command received");

        match cmd {
            TimerCommand::Start { duration_ms, label, repeat } => {
                self.cancel_task(device_id).await;
                let repeat = repeat.unwrap_or(false);
                let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                extra.insert("duration_ms", serde_json::json!(duration_ms));
                extra.insert("remaining_ms", serde_json::json!(duration_ms));
                extra.insert("repeat", serde_json::json!(repeat));
                if let Some(lbl) = label {
                    extra.insert("label", serde_json::json!(lbl));
                }
                self.set_state(device_id, "running", Some(extra)).await;
                self.spawn_timer_task(device_id, duration_ms, duration_ms, repeat)
                    .await;
            }

            TimerCommand::Pause => {
                let has_task = self.handles.read().await.contains_key(device_id);
                if !has_task {
                    warn!(%device_id, "Timer: pause requested but timer not running");
                    return;
                }
                // Compute remaining before cancelling the task.
                let remaining_ms = self.compute_remaining(device_id).await;
                self.cancel_task(device_id).await;
                let mut extra: HashMap<&str, serde_json::Value> = HashMap::new();
                extra.insert("remaining_ms", serde_json::json!(remaining_ms));
                self.set_state(device_id, "paused", Some(extra)).await;
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
                let remaining_ms = dev
                    .attributes
                    .get("remaining_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let duration_ms = dev
                    .attributes
                    .get("duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(remaining_ms);
                let repeat = dev
                    .attributes
                    .get("repeat")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if remaining_ms > 0 {
                    self.set_state(device_id, "running", None).await;
                    self.spawn_timer_task(device_id, remaining_ms, duration_ms, repeat)
                        .await;
                } else {
                    self.set_state(device_id, "fired", None).await;
                }
            }

            TimerCommand::Cancel => {
                self.cancel_task(device_id).await;
                self.set_state(device_id, "cancelled", None).await;
            }

            TimerCommand::Restart => {
                // Use stored duration — no need to accept it again.
                let duration_ms = {
                    let handles = self.handles.read().await;
                    handles.get(device_id).map(|h| h.duration_ms)
                };
                let duration_ms = if let Some(d) = duration_ms {
                    d
                } else {
                    match self.state.get_device(device_id).await {
                        Ok(Some(dev)) => dev
                            .attributes
                            .get("duration_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                        _ => {
                            warn!(%device_id, "Timer: restart — device not found");
                            return;
                        }
                    }
                };
                if duration_ms == 0 {
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
                extra.insert("remaining_ms", serde_json::json!(duration_ms));
                self.set_state(device_id, "running", Some(extra)).await;
                self.spawn_timer_task(device_id, duration_ms, duration_ms, repeat)
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
        remaining_ms: u64,
        duration_ms: u64,
        repeat: bool,
    ) {
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<TimerCtrl>(4);
        let id = device_id.to_string();
        let bus = self.bus.clone();
        let state = self.state.clone();
        let handles_ref = Arc::clone(&self.handles);

        let join = tokio::spawn(async move {
            let mut rem = remaining_ms;
            loop {
                debug!(device_id = %id, remaining_ms = rem, "Timer sleeping");
                let sleep = tokio::time::sleep(tokio::time::Duration::from_millis(rem));
                tokio::select! {
                    _ = sleep => {
                        info!(device_id = %id, "Timer fired");
                        fire_timer(&id, &state, &bus).await;
                        if repeat {
                            // Reset for next cycle.
                            rem = duration_ms;
                            reset_for_repeat(&id, duration_ms, &state, &bus).await;
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

        let handle = TimerHandle { duration_ms, ctrl_tx, _join: join };
        self.handles.write().await.insert(device_id.to_string(), handle);
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

    /// Estimate remaining_ms by reading started_at from state store.
    async fn compute_remaining(&self, device_id: &str) -> u64 {
        let Ok(Some(dev)) = self.state.get_device(device_id).await else {
            return 0;
        };
        let duration_ms = dev
            .attributes
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let started_at = dev
            .attributes
            .get("started_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        if let Some(started) = started_at {
            let elapsed = (Utc::now() - started).num_milliseconds().max(0) as u64;
            return duration_ms.saturating_sub(elapsed);
        }
        duration_ms
    }

    /// Apply a state transition, optionally merging extra attributes, then emit
    /// `DeviceStateChanged` on the bus.
    async fn set_state(
        &self,
        device_id: &str,
        new_state: &str,
        extra: Option<HashMap<&str, serde_json::Value>>,
    ) {
        let Ok(Some(mut dev)) = self.state.get_device(device_id).await else {
            warn!(%device_id, "Timer: device not found in state store");
            return;
        };
        let previous = dev.attributes.clone();
        dev.attributes.insert("state".into(), serde_json::json!(new_state));
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
        if let Err(e) = self.state.upsert_device(&dev).await {
            warn!(%device_id, error = %e, "Timer: failed to persist state");
            return;
        }
        let _ = self.bus.publish(Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            previous,
            current: dev.attributes,
        });
    }
}

// ---------------------------------------------------------------------------
// Free functions used inside spawned tasks (no &self available)
// ---------------------------------------------------------------------------

async fn fire_timer(device_id: &str, state: &StateStore, bus: &EventBus) {
    let Ok(Some(mut dev)) = state.get_device(device_id).await else { return };
    let previous = dev.attributes.clone();
    dev.attributes.insert("state".into(), serde_json::json!("fired"));
    dev.attributes.insert("remaining_ms".into(), serde_json::json!(0_u64));
    dev.last_seen = Utc::now();
    let _ = state.upsert_device(&dev).await;
    let _ = bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        previous,
        current: dev.attributes,
    });
}

async fn reset_for_repeat(
    device_id: &str,
    duration_ms: u64,
    state: &StateStore,
    bus: &EventBus,
) {
    let Ok(Some(mut dev)) = state.get_device(device_id).await else { return };
    let previous = dev.attributes.clone();
    dev.attributes.insert("state".into(), serde_json::json!("running"));
    dev.attributes.insert(
        "started_at".into(),
        serde_json::json!(Utc::now().to_rfc3339()),
    );
    dev.attributes.insert("remaining_ms".into(), serde_json::json!(duration_ms));
    dev.last_seen = Utc::now();
    let _ = state.upsert_device(&dev).await;
    let _ = bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        previous,
        current: dev.attributes,
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
