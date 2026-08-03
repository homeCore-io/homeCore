//! `hc-isy` — HomeCore plugin for Universal Devices ISY/IoX controllers.
//!
//! Connects to an ISY994i, eisy, or Polisy controller via HTTP REST and
//! WebSocket, registers all nodes as HomeCore devices, and bridges real-time
//! state changes bidirectionally between ISY and the HomeCore MQTT bus.
//!
//! ## Supported device types
//!
//! | ISY category / UOM | HomeCore type |
//! |---|---|
//! | Insteon dimmer (UOM 51, cat 1) | `light` |
//! | Insteon relay / Z-Wave switch (UOM 78) | `switch` |
//! | Door/window sensors | `contact_sensor` |
//! | Motion sensors | `motion_sensor` |
//! | Moisture / leak sensors | `water_sensor` |
//! | Unknown binary sensors | `binary_sensor` |
//! | Temperature, humidity, power, … | `sensor` |
//! | Deadbolt / Z-Wave lock (UOM 11) | `lock` |
//! | Garage door / shade / motor (UOM 97, cat 14) | `cover` |
//! | Insteon FanLinc (cat 1.46) | `fan` |
//! | Insteon thermostat (cat 5) | `thermostat` |
//! | ISY scenes / node groups | `scene` |
//!
//! ## Usage
//!
//! ```sh
//! hc-isy [config/config.toml]
//! ```

mod bridge;
mod config;
mod device;
mod isy;
mod schema;

use anyhow::Result;
use bridge::Bridge;
use config::Config;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-isy plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(
                        error = %e,
                        attempt,
                        "Startup failed; retrying in {RETRY_DELAY_SECS} s"
                    );
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
// Logging: stderr (RUST_LOG filtered) + rotating compressed file in logs/
// ---------------------------------------------------------------------------

fn init_logging(
    config_path: &str,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    plugin_sdk_rs::logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: plugin_sdk_rs::logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    plugin_sdk_rs::logging::init_logging(config_path, "hc-isy", "hc_isy=info", &bootstrap.logging)
}

// ---------------------------------------------------------------------------
// Startup — everything that can fail (retried up to MAX_ATTEMPTS times)
// ---------------------------------------------------------------------------

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- HomeCore MQTT (via SDK) ----------------------------------------------
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking via the SDK. Same path the
        // plugin used to manage by hand, so existing snapshots are
        // picked up unchanged.
        .with_device_persistence(published_ids_cache_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    // Conditions for the plugin page, not only the log.
    let notices = client.notices();
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // Enable management protocol (heartbeat + remote config/log commands).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?;

    // Publish the operator-config JSON Schema so the hc-web editor renders a
    // typed form (rides on the capability manifest).
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };

    // …and the plugin-authored descriptor the editor renders instead of
    // guessing a form from the schema. Rides the same manifest.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.  Without this, queued publishes block forever once
    // the rumqttc internal buffer fills up.
    let cmd_tx_clone = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    let _ = cmd_tx_clone.try_send((device_id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    // Brief yield to let the eventloop connect before we start publishing.
    tokio::time::sleep(Duration::from_millis(100)).await;

    tracing::info!(
        config      = %config_path,
        plugin_id   = %cfg.homecore.plugin_id,
        broker_host = %cfg.homecore.broker_host,
        broker_port = cfg.homecore.broker_port,
        isy_host    = %cfg.isy.host,
        isy_port    = cfg.isy.port,
        isy_tls     = cfg.isy.tls,
        "hc-isy starting",
    );

    // --- Bridge event loop (runs until error / shutdown) ----------------------
    if cfg.isy.host.trim().is_empty() {
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "No ISY/IoX controller address is set, so this plugin cannot connect to \
                 anything.",
            )
            .with_remedy(
                "Set [isy].host to the controller's address, with a username and password \
                 for an account that can use its REST interface.",
            ),
        );
    }

    let bridge = Bridge {
        config: cfg.clone(),
        publisher,
        cmd_rx,
        notices,
    };

    bridge.run().await
}

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}
