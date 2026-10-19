mod actions;
mod bridge;
mod bridge_info;
mod catalog;
mod command;
mod config;
mod controller;
mod discovery;
mod firmware;
mod firmware_check;
mod presets;
mod runtime;
mod schema;
mod state;
mod wled;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::WledConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match WledConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-wled plugin");
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
    plugin_sdk_rs::logging::init_logging(config_path, "hc-wled", "hc_wled=info", &bootstrap.logging)
}

async fn try_start(
    cfg: &WledConfig,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking via the SDK. The path is the
        // same `.published-device-ids.json` the plugin used to manage
        // by hand, so existing snapshots are picked up unchanged.
        .with_device_persistence(published_ids_cache_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    // Conditions for the plugin page, not only the log.
    let notices = client.notices();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // What the bridge, the refresher and the plugin's actions share.
    let rt = runtime::Runtime::new(cfg.clone(), config_path, publisher.clone());
    let rt_for_mgmt = rt.clone();

    // Enable management protocol (heartbeat + remote config/log commands +
    // capability manifest).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(actions::manifest())
        .with_alert_kinds(schema::alert_kinds())
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            // Core merges an action's params into the command itself.
            let params = cmd.clone();
            let rt = rt_for_mgmt.clone();
            // Route each manifest action through a one-shot tokio
            // runtime — the SDK's custom_handler is a sync fn returning
            // Option<Value>, but the WLED HTTP client is async. The
            // runtime is cheap and isolated to this single call.
            let action_for_err = action.clone();
            let result = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(async move { actions::run(&rt, &action, &params).await })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Publish the operator-config JSON Schema so the hc-web editor renders a
    // typed form (rides on the capability manifest).
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    // …and the plugin-authored descriptor the editor renders instead of
    // guessing a form from the schema. Rides the same manifest.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Its own screens — its page, a controller's panel, the automation
    // editor's WLED step: the built `ui/` folder it carries, served from here.
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION")),
        None => mgmt,
    };

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.  Without this, queued publishes block forever once
    // the rumqttc internal buffer (64) fills up.
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
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // An empty device list is the normal state right after install, and until
    // now it looked identical to a healthy plugin: active, zero devices, no
    // explanation. Worth saying out loud, with the container caveat, because
    // mDNS is exactly what a bridge network does not carry.
    if cfg.devices.is_empty() {
        notices.raise(
            PluginNotice::warning(
                "no_devices_configured",
                "No WLED devices are configured, so this plugin publishes nothing.",
            )
            .with_remedy(
                "Run the Discover devices action, which browses mDNS for \
                 _wled._tcp.local. If it finds nothing and homeCore is running in a \
                 container on a bridge network, that is expected — mDNS is multicast \
                 and does not cross the bridge. Add each controller by IP under \
                 Configuration instead.",
            ),
        );
    } else {
        notices.clear("no_devices_configured");
    }

    // Register all devices via DevicePublisher (PluginClient is consumed).
    let schema = schema::schema_json();
    let capabilities = schema::capabilities();
    for dev in &cfg.devices {
        if let Err(e) = publisher
            .register_device_full(
                &dev.hc_id,
                &dev.name,
                // A WLED controller is a light, and said so nowhere: it
                // registered with no `device_type` at all, so every client
                // filtering or ranking by type skipped it.
                Some("light"),
                dev.area.as_deref(),
                Some(capabilities.clone()),
            )
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = publisher
            .register_device_schema_json(&dev.hc_id, &schema)
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish schema");
        }
        if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
            error!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
        }
    }

    // Reconcile against the SDK-tracked set: anything from a prior
    // session that's no longer in `[[devices]]` gets unregistered.
    let live: std::collections::HashSet<String> =
        cfg.devices.iter().map(|d| d.hc_id.clone()).collect();
    if let Err(e) = publisher.reconcile_devices(live).await {
        warn!(error = %e, "reconcile_devices failed");
    }

    // Slow info-poller: per-device task that polls /json/info,
    // /json/nodes, /presets.json every 5 minutes and partial-merges
    // firmware/hardware/wifi/peer attributes onto the existing device.
    // Surfaces the data the manifest's get-actions used to fetch on
    // demand but never had a place to display.
    bridge_info::spawn_per_device(rt.clone());

    bridge::run(rt, cmd_rx).await;
    Ok(())
}

/// Path of the cross-restart device-id snapshot, sibling to
/// `config.toml`. Owned by the SDK's device tracker — see
/// `PluginClient::with_device_persistence`.
fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}
