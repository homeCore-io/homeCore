//! Plugin process manager with runtime start/stop/restart support.
//!
//! Replaces the fire-and-forget `plugin_launcher` with a `PluginManager` that
//! retains command channels to each plugin supervisor task, allowing runtime
//! lifecycle control via the REST API.
//!
//! Each `[[plugins]]` entry in `homecore.toml` gets its own supervisor
//! background task.  The supervisor:
//!
//! - Spawns the plugin binary as a child process.
//! - Restarts crashed plugins with exponential backoff (2 s → 60 s max).
//! - Resets backoff when a run lasts ≥ 60 seconds (healthy).
//! - Accepts `Start`, `Stop`, and `Restart` commands via an mpsc channel.
//! - Updates the shared `PluginRecord` in `AppState` on every status change.
//! - Emits `PluginStatusChanged` events on the internal bus.

use chrono::Utc;
use hc_core::EventBus;
use hc_types::event::Event;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{error, info, warn};

use hc_api::{PluginCommand, PluginCommandChannels, PluginRecord};

// ── Constants ──────────────────────────────────────────────────────────────

const MIN_BACKOFF: u64 = 2;
const MAX_BACKOFF: u64 = 60;
/// A run lasting at least this many seconds resets the backoff.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ── Types ──────────────────────────────────────────────────────────────────

/// A resolved plugin entry ready for launching.
#[derive(Clone)]
pub struct PluginProcess {
    pub id: String,
    pub binary: PathBuf,
    pub config: PathBuf,
    pub enabled: bool,
}

/// Spawn supervisors for all configured plugins and register their command
/// channels in the shared `PluginCommandChannels` (accessible from API handlers).
pub async fn spawn_all(
    processes: Vec<PluginProcess>,
    plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    plugin_commands: PluginCommandChannels,
    event_bus: EventBus,
    shutdown: watch::Receiver<bool>,
) {
    for p in processes {
        spawn_one(
            p,
            Arc::clone(&plugins),
            plugin_commands.clone(),
            event_bus.clone(),
            shutdown.clone(),
        )
        .await;
    }
}

/// Register a command channel and spawn a supervisor for a single plugin.
/// Used at boot by [`spawn_all`] and at runtime to activate a freshly-installed
/// plugin without a restart. The caller must have seeded the plugin's
/// `PluginRecord` (supervisor status updates are update-only).
pub async fn spawn_one(
    process: PluginProcess,
    plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    plugin_commands: PluginCommandChannels,
    event_bus: EventBus,
    shutdown: watch::Receiver<bool>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<PluginCommand>(8);
    plugin_commands
        .write()
        .await
        .insert(process.id.clone(), cmd_tx);
    tokio::spawn(supervise(process, cmd_rx, plugins, event_bus, shutdown));
}

// ── Supervisor ─────────────────────────────────────────────────────────────

async fn supervise(
    entry: PluginProcess,
    mut cmd_rx: mpsc::Receiver<PluginCommand>,
    plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    event_bus: EventBus,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff_secs: u64 = MIN_BACKOFF;
    let mut running = entry.enabled;

    // If not enabled, wait for a Start command.
    if !running {
        set_status(&plugins, &event_bus, &entry.id, "stopped").await;
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(PluginCommand::Start | PluginCommand::Restart) => {
                            running = true;
                            break;
                        }
                        Some(PluginCommand::Stop) => {} // already stopped
                        None => return, // channel closed
                    }
                }
                _ = wait_for_shutdown(&mut shutdown) => return,
            }
        }
    }

    // Main supervisor loop — exits via break/return, not by mutating `running`.
    #[allow(clippy::while_immutable_condition)]
    while running {
        if shutdown_requested(&shutdown) {
            info!(plugin_id = %entry.id, "Plugin supervisor stopping for shutdown");
            break;
        }

        set_status(&plugins, &event_bus, &entry.id, "starting").await;
        record_restart(&plugins, &entry.id).await;

        // Read binary/config from the plugin's record so an upgrade (which
        // rewrites the record + sends Restart) launches the NEW binary without
        // having to replace this supervisor. Falls back to the spawn-time entry.
        let (binary, config) = {
            let map = plugins.read().await;
            match map.get(&entry.id) {
                Some(r) => (
                    r.binary_path
                        .clone()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| entry.binary.clone()),
                    r.config_path
                        .clone()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| entry.config.clone()),
                ),
                None => (entry.binary.clone(), entry.config.clone()),
            }
        };

        info!(
            plugin_id = %entry.id,
            binary    = %binary.display(),
            config    = %config.display(),
            "Launching plugin"
        );

        let started_at = Instant::now();

        let mut child = match Command::new(&binary)
            .arg(&config)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                error!(
                    plugin_id = %entry.id,
                    binary    = %entry.binary.display(),
                    error     = %e,
                    "Failed to spawn plugin — retrying in {backoff_secs} s"
                );
                set_status(&plugins, &event_bus, &entry.id, "offline").await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                if !backoff_sleep(backoff_secs, &mut cmd_rx, &mut shutdown).await {
                    break;
                }
                continue;
            }
        };

        // The plugin is now running.  PluginRegistered event (from MQTT
        // registration) will set status to "active" via the AppState background
        // task.  We just track uptime here.
        {
            let mut map = plugins.write().await;
            if let Some(rec) = map.get_mut(&entry.id) {
                rec.uptime_started = Some(Utc::now());
            }
        }

        // Wait for exit, command, or shutdown.
        let mut stopped_by_command = false;
        let uptime = tokio::select! {
            result = child.wait() => {
                let uptime = started_at.elapsed();
                match result {
                    Ok(status) if status.success() => {
                        info!(plugin_id = %entry.id, uptime_secs = uptime.as_secs(), "Plugin exited cleanly");
                    }
                    Ok(status) => {
                        warn!(plugin_id = %entry.id, code = ?status.code(), uptime_secs = uptime.as_secs(), "Plugin exited with error");
                    }
                    Err(e) => {
                        error!(plugin_id = %entry.id, error = %e, "wait() failed for plugin");
                    }
                }
                uptime
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(PluginCommand::Stop) => {
                        info!(plugin_id = %entry.id, "Stop command received — killing plugin");
                        kill_child(&mut child, &entry.id).await;
                        stopped_by_command = true;
                        started_at.elapsed()
                    }
                    Some(PluginCommand::Restart) => {
                        info!(plugin_id = %entry.id, "Restart command received — killing plugin");
                        kill_child(&mut child, &entry.id).await;
                        backoff_secs = MIN_BACKOFF;
                        started_at.elapsed()
                    }
                    Some(PluginCommand::Start) => {
                        // Already running — ignore.
                        continue;
                    }
                    None => {
                        kill_child(&mut child, &entry.id).await;
                        return;
                    }
                }
            }
            _ = wait_for_shutdown(&mut shutdown) => {
                info!(plugin_id = %entry.id, "Shutdown requested — stopping plugin");
                kill_child(&mut child, &entry.id).await;
                set_status(&plugins, &event_bus, &entry.id, "stopped").await;
                return;
            }
        };

        if stopped_by_command {
            set_status(&plugins, &event_bus, &entry.id, "stopped").await;
            // Wait for a Start or Restart command before resuming.
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(PluginCommand::Start | PluginCommand::Restart) => {
                                backoff_secs = MIN_BACKOFF;
                                break;
                            }
                            Some(PluginCommand::Stop) => {} // already stopped
                            None => return,
                        }
                    }
                    _ = wait_for_shutdown(&mut shutdown) => return,
                }
            }
            continue;
        }

        // Process exited on its own — restart with backoff.
        set_status(&plugins, &event_bus, &entry.id, "offline").await;

        if uptime >= HEALTHY_UPTIME {
            backoff_secs = MIN_BACKOFF;
        } else {
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
        }

        warn!(plugin_id = %entry.id, backoff_secs, "Plugin will restart after backoff");
        if !backoff_sleep(backoff_secs, &mut cmd_rx, &mut shutdown).await {
            break;
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

async fn kill_child(child: &mut tokio::process::Child, plugin_id: &str) {
    if let Err(e) = child.start_kill() {
        warn!(plugin_id, error = %e, "Failed to send kill to plugin");
    }
    match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!(plugin_id, error = %e, "Plugin wait() failed during kill"),
        Err(_) => warn!(plugin_id, "Plugin did not exit after kill signal"),
    }
}

/// Sleep for `secs`, but wake early on command or shutdown.
/// Returns `true` if the supervisor should continue, `false` to exit.
async fn backoff_sleep(
    secs: u64,
    cmd_rx: &mut mpsc::Receiver<PluginCommand>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => true,
        cmd = cmd_rx.recv() => {
            match cmd {
                Some(PluginCommand::Start | PluginCommand::Restart) => true,
                Some(PluginCommand::Stop) => true, // will re-check in main loop
                None => false,
            }
        }
        _ = wait_for_shutdown(shutdown) => false,
    }
}

async fn set_status(
    plugins: &Arc<RwLock<HashMap<String, PluginRecord>>>,
    event_bus: &EventBus,
    plugin_id: &str,
    new_status: &str,
) {
    let previous_status;
    {
        let mut map = plugins.write().await;
        if let Some(rec) = map.get_mut(plugin_id) {
            previous_status = rec.status.clone();
            if previous_status == new_status {
                return;
            }
            rec.status = new_status.to_string();
        } else {
            return;
        }
    }
    let _ = event_bus.publish(Event::PluginStatusChanged {
        timestamp: Utc::now(),
        plugin_id: plugin_id.to_string(),
        status: new_status.to_string(),
        previous_status,
    });
}

async fn record_restart(plugins: &Arc<RwLock<HashMap<String, PluginRecord>>>, plugin_id: &str) {
    let mut map = plugins.write().await;
    if let Some(rec) = map.get_mut(plugin_id) {
        rec.restart_count += 1;
        rec.last_restart = Some(Utc::now());
    }
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if shutdown_requested(shutdown) {
        return;
    }
    loop {
        if shutdown.changed().await.is_err() {
            return;
        }
        if shutdown_requested(shutdown) {
            return;
        }
    }
}
