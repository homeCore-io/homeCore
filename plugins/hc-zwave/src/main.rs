//! `hc-zwave` — HomeCore plugin for Z-Wave via the zwave-js-server WebSocket API.
//!
//! Connects directly to a running `zwave-js-server` (bundled inside ZwaveJS UI or
//! standalone), receives live Z-Wave events, and publishes canonical device state
//! to the HomeCore MQTT broker.  Outbound commands (`homecore/devices/zwave_+/cmd`)
//! are translated to `node.set_value` WebSocket calls.
//!
//! ## Usage
//!
//! ```sh
//! hc-zwave [config/config.toml]
//! ```

mod actions;
mod battery;
mod bridge;
mod config;
mod convert;
mod inclusion;
mod lock;
mod network;
mod schema;
mod translator;
mod types;
mod zwclient;

use anyhow::Result;
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
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-zwave plugin");
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
// Logging: stderr (filtered) + rotating compressed file in logs/
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
    plugin_sdk_rs::logging::init_logging(
        config_path,
        "hc-zwave",
        "hc_zwave=info",
        &bootstrap.logging,
    )
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
        // Which devices this plugin registered, kept across restarts. Nothing
        // is removed by difference any more (see `bridge`); the record stays
        // for the SDK's own bookkeeping.
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

    let shared = bridge::Shared::new(cfg.clone(), config_path, publisher, notices.clone());

    let for_handler = shared.clone();
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(actions::manifest())
        .with_alert_kinds(actions::alert_kinds())
        .with_custom_handler(move |cmd| actions::handle(&for_handler, cmd));
    let mgmt = inclusion::register_actions(mgmt, shared.clone());

    // Publish the operator-config JSON Schema so the editor renders a typed
    // form (rides on the capability manifest)…
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    // …and the plugin-authored descriptor the editor renders instead.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Its own screens: the built `ui/` folder it carries, served from here.
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION")),
        None => mgmt,
    };

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    // Handed straight to the bridge, which sends it now or
                    // says it couldn't — never later.
                    let _ = cmd_tx.try_send((device_id, payload));
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

    info!(
        config      = %config_path,
        plugin_id   = %cfg.homecore.plugin_id,
        broker_host = %cfg.homecore.broker_host,
        broker_port = cfg.homecore.broker_port,
        server_url  = %network::redact_url(&cfg.server.url),
        "hc-zwave connected",
    );

    if cfg.server.url.trim().is_empty() {
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "No zwave-js-server URL is set, so this plugin has nothing to connect to.",
            )
            .with_remedy(
                "Set [server].url to the zwave-js-server WebSocket address, e.g. \
                 ws://192.168.1.10:3000. That server is a separate service and must be \
                 running before this plugin can see any devices.",
            ),
        );
    }

    bridge::run(shared, cmd_rx).await;
    Ok(())
}

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}
