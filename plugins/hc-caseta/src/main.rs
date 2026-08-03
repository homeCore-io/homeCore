mod bridge;
mod config;
mod devices;
mod import;
mod lip;
mod schema;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::Config;
use devices::{DeviceEntry, SceneEntry};

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

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-caseta plugin");
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
// Logging initialisation
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
        "hc-caseta",
        "hc_caseta=info",
        &bootstrap.logging,
    )
}

// ---------------------------------------------------------------------------
// Startup — retried up to MAX_ATTEMPTS on failure
// ---------------------------------------------------------------------------

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- Plugin SDK connection --------------------------------------------------
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
    // Conditions for the plugin page, not only the log. Taken before run()
    // consumes the client.
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

    // Backs the descriptor's `import` field. The plugin parses, because only
    // it knows Lutron's export format; the editor writes, because config is
    // core-owned. Rows land unsaved so they can be reviewed first.
    let mgmt = mgmt.with_streaming_action(plugin_sdk_rs::StreamingAction::new(
        "import_integration_report",
        |ctx: plugin_sdk_rs::StreamContext, params: serde_json::Value| async move {
            let text = params
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            match import::parse_integration_report(text) {
                Ok(out) => {
                    let summary = out.summary();
                    info!(
                        devices = out.devices.len(),
                        scenes = out.scenes.len(),
                        "Parsed integration report"
                    );
                    ctx.complete(serde_json::json!({
                        "devices": out.devices,
                        "scenes": out.scenes,
                        "summary": summary,
                    }))
                    .await
                }
                Err(e) => ctx.error(e.to_string()).await,
            }
        },
    ));

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.
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

    // Brief yield to let the eventloop connect before publishing.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Build device registry --------------------------------------------------
    // A row with no `kind` is one the operator imported but has not classified.
    // Skipping it keeps the rest of the house working — the alternative, a hard
    // parse failure, took the whole plugin offline on the first save after an
    // import.
    let devices: Vec<DeviceEntry> = cfg
        .devices
        .iter()
        .filter_map(|d| {
            let entry = DeviceEntry::new(d.clone());
            if entry.is_none() {
                warn!(
                    integration_id = d.integration_id,
                    name = %d.name,
                    "Skipping device with no kind set — choose one in Devices to enable it"
                );
            }
            entry
        })
        .collect();

    let scenes: Vec<SceneEntry> = cfg
        .scenes
        .iter()
        .map(|s| SceneEntry::new(s.clone()))
        .collect();

    let live: std::collections::HashSet<String> = devices
        .iter()
        .map(|d| d.hc_id.clone())
        .chain(scenes.iter().map(|s| s.hc_id.clone()))
        .collect();

    // --- Register all devices with HomeCore ------------------------------------
    for dev in &devices {
        if let Err(e) = publisher
            .register_device_full(
                &dev.hc_id,
                &dev.config.name,
                Some(dev.homecore_device_type()),
                dev.config.area.as_deref(),
                None,
            )
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
            error!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
        }
        if let Err(e) = publisher.publish_availability(&dev.hc_id, true).await {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
        }
        // A Pico's buttons, which the integration report has always known.
        if let Some(schema) = crate::schema::device_schema_json(&dev.config) {
            if let Err(e) = publisher
                .register_device_schema_json(&dev.hc_id, &schema)
                .await
            {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish device schema");
            }
        }
        if let Some(cat) = crate::schema::button_catalogue(&dev.config) {
            if let Err(e) = publisher.publish_state_partial(&dev.hc_id, &cat).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish button catalogue");
            }
        }
    }

    // --- Register scenes with HomeCore -----------------------------------------
    for scene in &scenes {
        if let Err(e) = publisher
            .register_device_full(
                &scene.hc_id,
                &scene.config.name,
                Some("scene"),
                scene.config.area.as_deref(),
                None,
            )
            .await
        {
            warn!(hc_id = %scene.hc_id, error = %e, "Failed to register scene");
        }
        if let Err(e) = publisher.subscribe_commands(&scene.hc_id).await {
            error!(hc_id = %scene.hc_id, error = %e, "Failed to subscribe scene commands");
        }
        if let Err(e) = publisher.publish_availability(&scene.hc_id, true).await {
            warn!(hc_id = %scene.hc_id, error = %e, "Failed to publish scene availability");
        }
    }

    info!(
        devices = devices.len(),
        scenes = scenes.len(),
        "All devices and scenes registered with HomeCore"
    );

    // Reconcile against the SDK-tracked set: anything from a prior
    // session that's no longer in [[devices]] / [[scenes]] gets unregistered.
    if let Err(e) = publisher.reconcile_devices(live).await {
        warn!(error = %e, "reconcile_devices failed");
    }

    // --- Build and run bridge ---------------------------------------------------
    if cfg.caseta.host.trim().is_empty() {
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "No Smart Bridge PRO address is set, so this plugin cannot connect to \
                 anything.",
            )
            .with_remedy(
                "Set the bridge's IP under Configuration, with the telnet integration \
                 username and password. Note this needs the Smart Bridge **PRO** — the \
                 standard bridge has no telnet integration to connect to.",
            ),
        );
    }

    let mut bridge = bridge::Bridge::new(devices, scenes, publisher, cfg.caseta.clone(), notices);
    bridge.run(cmd_rx).await;
    Ok(())
}

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}
