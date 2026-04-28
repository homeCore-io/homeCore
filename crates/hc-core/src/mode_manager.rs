//! ModeManager — named boolean modes with hot-reload from `config/modes.toml`.
//!
//! # Concept
//!
//! A *mode* is a named boolean device (`plugin_id = "core.mode"`) whose on/off
//! state is driven by a *kind*:
//!
//! - `solar`  — turned on/off automatically by sunrise/sunset (+ configurable offsets)
//! - `manual` — purely user/rule controlled; the manager only creates the device
//!
//! # Built-in solar modes
//!
//! `mode_night` and `mode_day` are always present — written to `modes.toml` on
//! first startup if missing, and rejected by the DELETE API.
//!
//! # Hot-reload
//!
//! `ModeWatcher` monitors `modes.toml` for changes (create/modify).  Any edit
//! — including one made by the API — triggers a debounced reload.
//! New modes get devices created; removed modes get devices deleted from the
//! state store; changed configs (e.g. offsets edited in the file) are applied
//! immediately.
//!
//! # Offset persistence
//!
//! `on_offset_minutes` and `off_offset_minutes` for solar modes live in
//! `modes.toml` as the single source of truth.  When the user PATCHes an
//! offset the manager writes the new value back to the file, which triggers
//! the watcher and a clean reload — no separate redb storage needed.

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use hc_types::device::{extract_change_from_command_payload, DeviceChange};
use hc_types::event::Event;
use hc_types::rule::SunEventType;
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::scheduler::solar_event_time;
use crate::{EventBus, LocationConfig};
use hc_state::StateStore;

// ── Constants ──────────────────────────────────────────────────────────────

pub const MODE_PLUGIN_ID: &str = "core.mode";
pub const MODE_DAY_ID: &str = "mode_day";
pub const MODE_NIGHT_ID: &str = "mode_night";

// ── Config types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Solar,
    Manual,
}

impl ModeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Solar => "solar",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeConfig {
    pub id: String,
    pub name: String,
    pub kind: ModeKind,

    // Solar-only fields — skipped when serialising manual modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_event: Option<SunEventType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_event: Option<SunEventType>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub on_offset_minutes: i32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub off_offset_minutes: i32,
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

// Internal wrapper for TOML array-of-tables serialisation.
#[derive(Debug, Serialize, Deserialize)]
struct ModesFile {
    #[serde(default)]
    modes: Vec<ModeConfig>,
}

// ── File helpers ───────────────────────────────────────────────────────────

fn default_mode_night() -> ModeConfig {
    ModeConfig {
        id: MODE_NIGHT_ID.to_string(),
        name: "Night Mode".to_string(),
        kind: ModeKind::Solar,
        on_event: Some(SunEventType::Sunset),
        off_event: Some(SunEventType::Sunrise),
        on_offset_minutes: 0,
        off_offset_minutes: 0,
    }
}

fn default_mode_day() -> ModeConfig {
    ModeConfig {
        id: MODE_DAY_ID.to_string(),
        name: "Day Mode".to_string(),
        kind: ModeKind::Solar,
        on_event: Some(SunEventType::Sunrise),
        off_event: Some(SunEventType::Sunset),
        on_offset_minutes: 0,
        off_offset_minutes: 0,
    }
}

/// Parse `modes.toml`.  Returns an empty vec when the file does not yet exist.
pub fn load_modes(path: &Path) -> Result<Vec<ModeConfig>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ModesFile =
        toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.modes)
}

/// Write the complete modes list (with header comment) to `modes.toml`.
pub fn write_modes(path: &Path, modes: &[ModeConfig]) -> Result<()> {
    let header = "\
# HomeCore mode definitions.\n\
# This file is managed automatically — API-created modes are appended here.\n\
# Edit directly or use POST /api/v1/modes.\n\n";
    let body = toml::to_string(&ModesFile {
        modes: modes.to_vec(),
    })
    .context("serialising modes to TOML")?;
    std::fs::write(path, format!("{header}{body}"))
        .with_context(|| format!("writing {}", path.display()))
}

/// Ensure `modes.toml` exists and contains the built-in solar modes.
/// Idempotent — safe to call on every startup.
pub fn ensure_default_modes(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let mut modes = load_modes(path).unwrap_or_default();
    let mut changed = false;
    if !modes.iter().any(|m| m.id == MODE_DAY_ID) {
        modes.insert(0, default_mode_day());
        changed = true;
    }
    if !modes.iter().any(|m| m.id == MODE_NIGHT_ID) {
        modes.insert(0, default_mode_night());
        changed = true;
    }
    if changed {
        write_modes(path, &modes)?;
        info!(path = %path.display(), "Ensured built-in solar modes in modes.toml");
    }
    Ok(())
}

/// Append a new mode entry to `modes.toml`.
/// Returns an error if a mode with the same id already exists.
pub fn append_mode(path: &Path, mode: ModeConfig) -> Result<()> {
    let mut modes = load_modes(path).unwrap_or_default();
    if modes.iter().any(|m| m.id == mode.id) {
        anyhow::bail!("mode '{}' already exists in modes.toml", mode.id);
    }
    modes.push(mode);
    write_modes(path, &modes)
}

/// Remove a mode entry from `modes.toml`.
pub fn remove_mode(path: &Path, id: &str) -> Result<()> {
    let mut modes = load_modes(path).unwrap_or_default();
    let before = modes.len();
    modes.retain(|m| m.id != id);
    if modes.len() == before {
        anyhow::bail!("mode '{}' not found in modes.toml", id);
    }
    write_modes(path, &modes)
}

// ── Solar helpers ──────────────────────────────────────────────────────────

/// Whether a solar mode is currently active based on local clock.
///
/// Handles overnight windows (e.g. sunset → sunrise wraps midnight).
fn solar_mode_is_on(
    lat: f64,
    lon: f64,
    today: NaiveDate,
    on_ev: SunEventType,
    off_ev: SunEventType,
    on_off: i32,
    off_off: i32,
) -> Option<bool> {
    let on_t = solar_event_time(lat, lon, today, on_ev, on_off)?;
    let off_t = solar_event_time(lat, lon, today, off_ev, off_off)?;
    let now = Local::now().time();
    // Overnight window (sunset → sunrise): on_t is later in the day than off_t.
    Some(if on_t > off_t {
        now >= on_t || now < off_t
    } else {
        now >= on_t && now < off_t
    })
}

/// Find the soonest upcoming solar transition across all solar modes.
///
/// Checks today and tomorrow for each event so transitions after midnight
/// (e.g. sunrise tomorrow when it is currently 23:00) are found correctly.
fn next_solar_transition(
    modes: &[ModeConfig],
    lat: f64,
    lon: f64,
) -> Option<(chrono::DateTime<Local>, String, bool)> {
    let now = Local::now();
    let mut candidates: Vec<(chrono::DateTime<Local>, String, bool)> = Vec::new();

    for mode in modes {
        if mode.kind != ModeKind::Solar {
            continue;
        }
        let (Some(on_ev), Some(off_ev)) = (mode.on_event, mode.off_event) else {
            continue;
        };

        for days_ahead in 0i64..=1 {
            let date = now.date_naive() + chrono::Duration::days(days_ahead);
            for (ev, offset, new_on) in [
                (on_ev, mode.on_offset_minutes, true),
                (off_ev, mode.off_offset_minutes, false),
            ] {
                if let Some(t) = solar_event_time(lat, lon, date, ev, offset) {
                    let naive = NaiveDateTime::new(date, t);
                    if let Some(local_dt) = Local.from_local_datetime(&naive).latest() {
                        if local_dt > now {
                            candidates.push((local_dt, mode.id.clone(), new_on));
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by_key(|(dt, _, _)| *dt);
    candidates.into_iter().next()
}

// ── ModeWatcher ────────────────────────────────────────────────────────────

/// Watches `modes.toml` for file-system changes and notifies the manager.
pub struct ModeWatcher {
    _watcher: RecommendedWatcher,
}

impl ModeWatcher {
    pub fn start(path: PathBuf, tx: mpsc::Sender<()>) -> Result<Self> {
        let parent = path.parent().unwrap_or(&path).to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
            let Ok(event) = res else { return };
            let relevant = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                && event.paths.iter().any(|p| p.ends_with("modes.toml"));
            if relevant {
                let _ = tx.blocking_send(());
            }
        })?;
        watcher.watch(&parent, RecursiveMode::NonRecursive)?;
        info!(dir = %parent.display(), "Mode hot-reload watcher active");
        Ok(Self { _watcher: watcher })
    }
}

// ── ModeManager ────────────────────────────────────────────────────────────

pub struct ModeManager {
    bus: EventBus,
    pub_bus: EventBus,
    state: StateStore,
    location: LocationConfig,
    modes_path: PathBuf,
    startup_delay_secs: u64,
}

impl ModeManager {
    pub fn new(
        bus: EventBus,
        pub_bus: EventBus,
        state: StateStore,
        location: LocationConfig,
        modes_path: PathBuf,
        startup_delay_secs: u64,
    ) -> Self {
        Self {
            bus,
            pub_bus,
            state,
            location,
            modes_path,
            startup_delay_secs,
        }
    }

    pub async fn start(self) {
        if let Err(e) = ensure_default_modes(&self.modes_path) {
            warn!(error = %e, "ModeManager: failed to ensure default modes");
        }

        let mut modes = match load_modes(&self.modes_path) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "ModeManager: failed to load modes; using empty set");
                vec![]
            }
        };

        // Wait for plugins to connect and subscribe before publishing initial
        // mode states.  Without this delay, commands triggered by the initial
        // state (e.g. "mode_night on → turn on wled_deck") are published before
        // the plugin has subscribed to its cmd topic and are silently dropped.
        if self.startup_delay_secs > 0 {
            info!(
                secs = self.startup_delay_secs,
                "ModeManager: waiting for plugins to initialise"
            );
            tokio::time::sleep(Duration::from_secs(self.startup_delay_secs)).await;
        }

        // Set correct on/off state after plugins are ready.
        self.apply_initial_states(&modes).await;

        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(4);
        let _watcher = ModeWatcher::start(self.modes_path.clone(), reload_tx)
            .map_err(|e| warn!(error = %e, "ModeManager: watcher failed to start"))
            .ok();

        let mut bus_rx = self.bus.subscribe();
        info!("ModeManager started");

        loop {
            let (sleep_dur, next_transition) = self.compute_sleep(&modes);

            tokio::select! {
                // ── Solar transition ──────────────────────────────────────
                _ = tokio::time::sleep(sleep_dur) => {
                    if let Some((_, ref mode_id, new_on)) = next_transition {
                        self.flip_mode(mode_id, new_on, &modes).await;
                    }
                    // Around midnight: refresh solar times for the new day.
                    if Local::now().hour() == 0 {
                        self.apply_initial_states(&modes).await;
                    }
                }

                // ── modes.toml changed on disk ────────────────────────────
                _ = reload_rx.recv() => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    while reload_rx.try_recv().is_ok() {} // drain extras
                    match load_modes(&self.modes_path) {
                        Ok(new_modes) => {
                            self.handle_reload(&modes, &new_modes).await;
                            modes = new_modes;
                        }
                        Err(e) => warn!(error = %e, "ModeManager: reload parse error — keeping existing"),
                    }
                }

                // ── Event bus: offset-change commands ─────────────────────
                event = bus_rx.recv() => {
                    match event {
                        Ok(Event::MqttMessage { topic, payload, .. }) => {
                            if let Some(mode_id) = parse_mode_cmd_topic(&topic) {
                                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                                    self.handle_mode_cmd(mode_id, &v, &mut modes).await;
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!("ModeManager lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    // ── Private helpers ────────────────────────────────────────────────────

    fn compute_sleep(
        &self,
        modes: &[ModeConfig],
    ) -> (Duration, Option<(chrono::DateTime<Local>, String, bool)>) {
        let next = next_solar_transition(modes, self.location.latitude, self.location.longitude);
        let duration = next
            .as_ref()
            .and_then(|(dt, _, _)| (*dt - Local::now()).to_std().ok())
            .unwrap_or(Duration::from_secs(3600));
        (duration, next)
    }

    /// Determine and write the correct on/off state for every mode right now.
    async fn apply_initial_states(&self, modes: &[ModeConfig]) {
        let today = Local::now().date_naive();
        let lat = self.location.latitude;
        let lon = self.location.longitude;

        for mode in modes {
            match mode.kind {
                ModeKind::Solar => {
                    let (Some(on_ev), Some(off_ev)) = (mode.on_event, mode.off_event) else {
                        continue;
                    };
                    let on = solar_mode_is_on(
                        lat,
                        lon,
                        today,
                        on_ev,
                        off_ev,
                        mode.on_offset_minutes,
                        mode.off_offset_minutes,
                    )
                    .unwrap_or(false);
                    self.write_mode_state(mode, on, None).await;
                }
                ModeKind::Manual => {
                    // Only initialise if device doesn't exist yet.
                    if let Ok(None) = self.state.get_device(&mode.id).await {
                        self.write_mode_state(mode, false, None).await;
                    }
                }
            }
        }
    }

    async fn flip_mode(&self, mode_id: &str, new_on: bool, modes: &[ModeConfig]) {
        if let Some(mode) = modes.iter().find(|m| m.id == mode_id) {
            info!(mode_id, on = new_on, "ModeManager: solar transition");
            self.write_mode_state(mode, new_on, None).await;
        }
    }

    /// Persist the device state for a mode and publish `DeviceStateChanged`.
    async fn write_mode_state(&self, mode: &ModeConfig, on: bool, change: Option<DeviceChange>) {
        let today = Local::now().date_naive();
        let lat = self.location.latitude;
        let lon = self.location.longitude;

        let mut dev = match self.state.get_device(&mode.id).await {
            Ok(Some(d)) => d,
            _ => hc_types::device::DeviceState::new(&mode.id, &mode.name, MODE_PLUGIN_ID),
        };

        let previous = dev.attributes.clone();
        dev.attributes.insert("on".into(), json!(on));
        dev.attributes
            .insert("kind".into(), json!(mode.kind.as_str()));

        if mode.kind == ModeKind::Solar {
            dev.attributes
                .insert("on_offset_minutes".into(), json!(mode.on_offset_minutes));
            dev.attributes
                .insert("off_offset_minutes".into(), json!(mode.off_offset_minutes));

            if let (Some(on_ev), Some(off_ev)) = (mode.on_event, mode.off_event) {
                // Base times (no offset) — for human reference in TUI.
                if let Some(t) = solar_event_time(lat, lon, today, off_ev, 0) {
                    let key = if off_ev == SunEventType::Sunrise {
                        "sunrise_today"
                    } else {
                        "sunset_today"
                    };
                    dev.attributes.insert(key.into(), json!(fmt_time(t)));
                }
                if let Some(t) = solar_event_time(lat, lon, today, on_ev, 0) {
                    let key = if on_ev == SunEventType::Sunset {
                        "sunset_today"
                    } else {
                        "sunrise_today"
                    };
                    dev.attributes.insert(key.into(), json!(fmt_time(t)));
                }
                // Effective times (with offset applied) — what the scheduler actually uses.
                if let Some(t) = solar_event_time(lat, lon, today, on_ev, mode.on_offset_minutes) {
                    dev.attributes
                        .insert("effective_on".into(), json!(fmt_time(t)));
                }
                if let Some(t) = solar_event_time(lat, lon, today, off_ev, mode.off_offset_minutes)
                {
                    dev.attributes
                        .insert("effective_off".into(), json!(fmt_time(t)));
                }
            }
        }

        dev.last_seen = chrono::Utc::now();
        dev.available = true;
        let change = change.unwrap_or_else(|| DeviceChange::homecore("mode_manager"));
        dev.last_change = Some(change.clone());

        if let Err(e) = self.state.upsert_device(&dev).await {
            warn!(device_id = %mode.id, error = %e, "ModeManager: failed to persist device state");
            return;
        }

        let current = dev.attributes;
        let changed: Vec<String> = current
            .keys()
            .filter(|k| previous.get(*k) != current.get(*k))
            .chain(previous.keys().filter(|k| !current.contains_key(*k)))
            .cloned()
            .collect();

        // Fire mode_changed events so Trigger::ModeChanged rules can react.
        if changed.contains(&"on".to_string()) {
            // First-class event for activity stream and filtering.
            let _ = self.pub_bus.publish(Event::ModeChanged {
                timestamp: chrono::Utc::now(),
                mode_id: mode.id.clone(),
                mode_name: mode.name.clone(),
                on,
            });
            // Keep the Custom event for backward compatibility with existing
            // Trigger::ModeChanged rules that match on event_type == "mode_changed".
            let _ = self.pub_bus.publish(Event::Custom {
                timestamp: chrono::Utc::now(),
                event_type: "mode_changed".to_string(),
                payload: json!({ "mode_id": mode.id, "on": on }),
            });
        }

        let _ = self.pub_bus.publish(Event::DeviceStateChanged {
            timestamp: chrono::Utc::now(),
            device_id: mode.id.clone(),
            device_name: Some(mode.name.clone()),
            previous,
            current,
            changed,
            change,
        });
    }

    /// Handle commands arriving on the event bus.
    ///
    /// For **solar** modes: handles `on_offset_minutes` / `off_offset_minutes`
    /// and writes back to `modes.toml`.
    ///
    /// For **manual** modes: handles `{ "command": "on|off|toggle" }` and
    /// `{ "on": bool }` payloads to change mode state directly.
    async fn handle_mode_cmd(
        &self,
        mode_id: &str,
        cmd: &serde_json::Value,
        modes: &mut [ModeConfig],
    ) {
        let Some(mode_idx) = modes.iter().position(|m| m.id == mode_id) else {
            return;
        };
        let mode = &modes[mode_idx];

        // ── Manual mode: on / off / toggle ────────────────────────────────
        if mode.kind == ModeKind::Manual {
            // Determine desired new state.
            let current_on = self
                .state
                .get_device(mode_id)
                .await
                .ok()
                .flatten()
                .and_then(|d| d.attributes.get("on").and_then(|v| v.as_bool()))
                .unwrap_or(false);

            let new_on = if let Some(command) = cmd.get("command").and_then(|v| v.as_str()) {
                match command {
                    "on" => Some(true),
                    "off" => Some(false),
                    "toggle" => Some(!current_on),
                    _ => None,
                }
            } else {
                cmd.get("on").and_then(|v| v.as_bool())
            };

            if let Some(on) = new_on {
                info!(mode_id, on, "ModeManager: manual mode command");
                let mode_cfg = modes[mode_idx].clone();
                let change = extract_change_from_command_payload(cmd).unwrap_or_default();
                self.write_mode_state(&mode_cfg, on, Some(change)).await;
            }
            return;
        }

        // ── Solar mode: offset changes ────────────────────────────────────
        let mode = &mut modes[mode_idx];
        let mut changed = false;
        if let Some(v) = cmd.get("on_offset_minutes").and_then(|v| v.as_i64()) {
            mode.on_offset_minutes = v as i32;
            changed = true;
        }
        if let Some(v) = cmd.get("off_offset_minutes").and_then(|v| v.as_i64()) {
            mode.off_offset_minutes = v as i32;
            changed = true;
        }

        if changed {
            info!(
                mode_id,
                on_off = mode.on_offset_minutes,
                off_off = mode.off_offset_minutes,
                "ModeManager: offset updated"
            );
            // Write back to modes.toml — watcher will reload and reschedule.
            if let Err(e) = write_modes(&self.modes_path, modes) {
                warn!(error = %e, "ModeManager: failed to write updated offsets to modes.toml");
            }
        }
    }

    /// Reconcile running modes against a freshly loaded config.
    async fn handle_reload(&self, old: &[ModeConfig], new: &[ModeConfig]) {
        // Deleted modes → remove device from state store.
        for old_mode in old {
            if !new.iter().any(|m| m.id == old_mode.id) {
                info!(mode_id = %old_mode.id, "ModeManager: mode removed");
                if let Err(e) = self.state.delete_device(&old_mode.id).await {
                    warn!(mode_id = %old_mode.id, error = %e,
                        "ModeManager: failed to delete removed mode device");
                }
            }
        }

        // New or changed modes → recompute and apply state.
        let today = Local::now().date_naive();
        let lat = self.location.latitude;
        let lon = self.location.longitude;

        for new_mode in new {
            let existed = old.iter().any(|m| m.id == new_mode.id);
            let changed = old.iter().any(|m| m.id == new_mode.id && m != new_mode);
            if !existed || changed {
                let on = match new_mode.kind {
                    ModeKind::Solar => {
                        if let (Some(on_ev), Some(off_ev)) = (new_mode.on_event, new_mode.off_event)
                        {
                            solar_mode_is_on(
                                lat,
                                lon,
                                today,
                                on_ev,
                                off_ev,
                                new_mode.on_offset_minutes,
                                new_mode.off_offset_minutes,
                            )
                            .unwrap_or(false)
                        } else {
                            false
                        }
                    }
                    ModeKind::Manual => {
                        // For a new manual mode start off; preserve existing state if just config changed.
                        if existed {
                            self.state
                                .get_device(&new_mode.id)
                                .await
                                .ok()
                                .flatten()
                                .and_then(|d| d.attributes.get("on").and_then(|v| v.as_bool()))
                                .unwrap_or(false)
                        } else {
                            false
                        }
                    }
                };
                self.write_mode_state(new_mode, on, None).await;
            }
        }
    }
}

// ── Topic parsing ──────────────────────────────────────────────────────────

fn parse_mode_cmd_topic(topic: &str) -> Option<&str> {
    // homecore/devices/mode_{id}/cmd
    let rest = topic.strip_prefix("homecore/devices/")?;
    let id = rest.strip_suffix("/cmd")?;
    if id.starts_with("mode_") {
        Some(id)
    } else {
        None
    }
}

// ── Formatting ─────────────────────────────────────────────────────────────

fn fmt_time(t: chrono::NaiveTime) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}
