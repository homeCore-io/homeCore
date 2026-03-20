mod bridge;
mod config;
mod discovery;
mod homecore;
mod speaker;

use anyhow::Result;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use config::SonosConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let _log_guard = init_logging(&config_path);

    let cfg = match SonosConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-sonos plugin");
        match try_start(&cfg).await {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn init_logging(config_path: &str) -> tracing_appender::non_blocking::WorkerGuard {
    let log_dir = std::path::Path::new(config_path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(&log_dir, "hc-sonos.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "hc_sonos=info".parse().unwrap());
    let file_filter = EnvFilter::new("debug");

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async fn try_start(cfg: &SonosConfig) -> Result<()> {
    // ── HomeCore MQTT ─────────────────────────────────────────────────────
    let hc_client = homecore::HomecoreClient::connect(&cfg.homecore).await?;
    let publisher = hc_client.publisher();
    let (hc_tx, hc_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // ── Discovery channel ─────────────────────────────────────────────────
    let (discovery_tx, discovery_rx) = mpsc::channel::<sonor::Speaker>(32);

    // ── Spawn HomeCore MQTT event loop ────────────────────────────────────
    tokio::spawn(hc_client.run(hc_tx));

    // ── Spawn discovery task ──────────────────────────────────────────────
    discovery::spawn(
        Duration::from_secs(cfg.sonos.discovery_interval_secs),
        Duration::from_secs(cfg.sonos.discovery_timeout_secs),
        cfg.sonos.manual_hosts.clone(),
        discovery_tx,
    );

    info!(
        discovery_interval_secs = cfg.sonos.discovery_interval_secs,
        poll_interval_secs      = cfg.sonos.poll_interval_secs,
        manual_hosts            = cfg.sonos.manual_hosts.len(),
        "hc-sonos started"
    );

    // ── Run bridge (blocks until HomeCore channel closes) ─────────────────
    let bridge = bridge::Bridge::new(cfg, publisher);
    bridge.run(discovery_rx, hc_rx).await;

    Ok(())
}
