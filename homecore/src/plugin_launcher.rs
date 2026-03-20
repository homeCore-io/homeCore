//! Plugin process manager.
//!
//! Each `[[plugins]]` entry in `homecore.toml` is a standalone binary that
//! HomeCore spawns as a child process.  This module:
//!
//! - Spawns each enabled plugin after the broker is ready.
//! - Monitors each process in a dedicated background task.
//! - Restarts crashed plugins with exponential backoff (2 s → 4 → 8 → … → 60 s).
//! - Resets the backoff counter if a plugin ran for at least 60 seconds before
//!   exiting (considered a healthy run that ended unexpectedly).
//!
//! Plugin stdout/stderr is inherited so plugin log lines appear in the same
//! terminal as HomeCore output.
//!
//! Shutdown: Ctrl-C sends SIGINT to the entire process group on Linux/macOS,
//! which also terminates child processes.  No explicit kill logic is needed
//! for interactive use.

use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{error, info, warn};

/// A resolved plugin entry ready for launching.
pub struct PluginProcess {
    /// Human-readable ID used in log messages (e.g. "plugin.yolink").
    pub id: String,
    /// Absolute path to the plugin binary.
    pub binary: PathBuf,
    /// Absolute path to the plugin's config file (passed as first argument).
    pub config: PathBuf,
}

/// Spawn all plugins.  Each is managed by its own background task; this
/// function returns immediately.
pub fn spawn_all(plugins: Vec<PluginProcess>) {
    for p in plugins {
        tokio::spawn(supervise(p));
    }
}

/// Supervisor loop for a single plugin.  Runs forever, restarting the process
/// after each exit.
async fn supervise(entry: PluginProcess) {
    const MIN_BACKOFF: u64 = 2;
    const MAX_BACKOFF: u64 = 60;
    /// A run lasting at least this many seconds resets the backoff.
    const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

    let mut backoff_secs: u64 = MIN_BACKOFF;

    loop {
        info!(
            plugin_id = %entry.id,
            binary    = %entry.binary.display(),
            config    = %entry.config.display(),
            "Launching plugin"
        );

        let started_at = Instant::now();

        let mut child = match Command::new(&entry.binary)
            .arg(&entry.config)
            // Inherit stdout/stderr so plugin logs are visible in the same terminal.
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
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        // Wait for the process to finish.
        let uptime = match child.wait().await {
            Ok(status) => {
                let uptime = started_at.elapsed();
                if status.success() {
                    info!(
                        plugin_id   = %entry.id,
                        uptime_secs = uptime.as_secs(),
                        "Plugin exited cleanly"
                    );
                } else {
                    warn!(
                        plugin_id   = %entry.id,
                        code        = ?status.code(),
                        uptime_secs = uptime.as_secs(),
                        "Plugin exited with error status"
                    );
                }
                uptime
            }
            Err(e) => {
                error!(plugin_id = %entry.id, error = %e, "wait() failed for plugin");
                Duration::ZERO
            }
        };

        // Reset backoff for healthy long-running processes; escalate for crashes.
        if uptime >= HEALTHY_UPTIME {
            backoff_secs = MIN_BACKOFF;
        } else {
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
        }

        warn!(
            plugin_id   = %entry.id,
            backoff_secs,
            "Plugin will restart after backoff"
        );
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
    }
}
