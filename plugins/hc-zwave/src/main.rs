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

mod bridge;
mod config;
mod inclusion;
mod schema;
mod translator;
mod types;

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

    // Rescan signal — `rescan_nodes` management action pushes onto this
    // channel; the bridge's WS loop sends `start_listening` to zwave-js
    // and republishes all node states from the result. The InclusionHandle
    // also gets a clone so include_node can self-trigger a rescan once
    // the user marks inclusion done.
    let (rescan_tx, rescan_rx) = mpsc::channel::<()>(8);

    // Inclusion / exclusion streaming channels. The handle is cloned
    // into every streaming action closure; the bridge drains the raw
    // control channel and publishes decoded controller events.
    let (inclusion_handle, control_rx, event_tx) = inclusion::new_handle(rescan_tx.clone());

    // Enable management protocol (heartbeat + remote config/log commands,
    // plus streaming include_node + exclude_node actions).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest())
        .with_custom_handler(move |cmd| match cmd["action"].as_str()? {
            "rescan_nodes" => {
                let _ = rescan_tx.try_send(());
                Some(serde_json::json!({ "status": "ok" }))
            }
            _ => None,
        });
    let mgmt = inclusion::register_actions(mgmt, inclusion_handle);

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

    info!(
        config      = %config_path,
        plugin_id   = %cfg.homecore.plugin_id,
        broker_host = %cfg.homecore.broker_host,
        broker_port = cfg.homecore.broker_port,
        server_url  = %cfg.server.url,
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

    // --- Bridge loop (reconnects on WS disconnect) ---
    Bridge {
        config: cfg.clone(),
        publisher,
        cmd_rx,
        control_rx,
        event_tx,
        rescan_rx,
        notices,
    }
    .run()
    .await
}

/// Capability manifest for hc-zwave. Declares the streaming actions
/// (include_node, exclude_node) so the admin UI and hc-mcp can surface
/// them without plugin-specific code.
fn capabilities_manifest() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, ItemOp, RequiresRole};
    use serde_json::json;

    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(), // SDK fills in from plugin_id
        actions: vec![
            Action {
                id: "include_node".into(),
                label: "Include Z-Wave device".into(),
                description: Some(
                    "Put the controller into inclusion mode and add one or more \
                     Z-Wave devices. Reply 'done' when finished, or cancel to \
                     abort. Secure S2 inclusion auto-grants the device's \
                     requested classes; devices requiring DSK PIN entry are \
                     not supported in v1."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "nodes_added": { "type": "array" } })),
                stream: true,
                cancelable: true,
                concurrency: Concurrency::Single,
                item_key: Some("node_id".into()),
                item_operations: Some(vec![ItemOp::Add, ItemOp::Update]),
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(300_000), // 5 min — inclusion windows can be long
            },
            Action {
                id: "exclude_node".into(),
                label: "Exclude Z-Wave device".into(),
                description: Some(
                    "Put the controller into exclusion mode and remove one or \
                     more Z-Wave devices. Reply 'done' when finished, or \
                     cancel to abort."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "nodes_removed": { "type": "array" } })),
                stream: true,
                cancelable: true,
                concurrency: Concurrency::Single,
                item_key: Some("node_id".into()),
                item_operations: Some(vec![ItemOp::Remove]),
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(300_000),
            },
            Action {
                id: "rescan_nodes".into(),
                label: "Rescan nodes".into(),
                description: Some(
                    "Re-fetch every node's full state from zwave-js and \
                     republish to homeCore. Use this after inclusion if a \
                     freshly-added device hasn't appeared yet — typically \
                     because its interview is still running. Safe to invoke \
                     any time; non-destructive."
                        .into(),
                ),
                params: None,
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
        ],
    }
}

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}
