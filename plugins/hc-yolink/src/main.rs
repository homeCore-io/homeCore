mod auth;
mod bridge;
mod config;
mod devices;
mod schema;
mod yolink;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info, warn};

use config::{Config, Endpoints};
use devices::DeviceKind;
use yolink::{api::YolinkApi, mqtt::YolinkMqtt, types::YolinkReport};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-yolink plugin");
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
        "hc-yolink",
        "hc_yolink=info",
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
    // --- HomeCore MQTT (via SDK) — brought up FIRST ----------------------------
    // Connect to the broker and enable the management protocol (which carries
    // the operator-config schema) BEFORE any YoLink endpoint resolution / cloud
    // auth.  This lets the plugin install and serve its schema on a minimal
    // `[homecore]`-only config, so the operator can enter credentials in the
    // Studio and then restart — exactly like hc-caseta comes up before it
    // touches its hardware bridge.
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking so `bridge::sync_inventory`'s
        // reconcile_devices call can clean up zombies from prior
        // sessions (devices removed from YoLink while the plugin was
        // offline). Snapshot lives next to config.toml.
        .with_device_persistence(published_ids_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    // Conditions for the plugin page, not only the log.
    let notices = client.notices();
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // Rescan trigger — shared between the management command handler and the
    // bridge.  Notified when the user clicks "Rescan devices" in the admin UI
    // (via the `rescan_devices` management command).
    let rescan = Arc::new(Notify::new());

    // Enable management protocol (heartbeat + remote config/log commands,
    // plus yolink-specific `rescan_devices` action).
    let rescan_for_mgmt = Arc::clone(&rescan);
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_custom_handler(move |cmd| match cmd["action"].as_str()? {
            "rescan_devices" => {
                rescan_for_mgmt.notify_one();
                Some(serde_json::json!({ "status": "ok" }))
            }
            _ => None,
        })
        .with_capabilities(plugin_sdk_rs::types::Capabilities {
            spec: "1".into(),
            plugin_id: String::new(),
            actions: vec![plugin_sdk_rs::types::Action {
                id: "rescan_devices".into(),
                label: "Rescan devices".into(),
                description: Some(
                    "Refresh the YoLink device inventory from the cloud and republish \
                     registration for each."
                        .into(),
                ),
                params: None,
                result: None,
                stream: false,
                cancelable: false,
                concurrency: plugin_sdk_rs::types::Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: plugin_sdk_rs::types::RequiresRole::User,
                timeout_ms: None,
            }],
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

    // Start the SDK event loop so management (heartbeat, config, schema) is
    // served immediately.  Without this, queued publishes block forever once
    // the rumqttc internal buffer (64) fills up.  Keep the handle so we can
    // block on it in the not-yet-configured case below.
    let cmd_tx_clone = cmd_tx.clone();
    let sdk_task = tokio::spawn(async move {
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

    // --- Not configured yet?  Stay up serving management only ------------------
    // With neither a cloud nor a local credential section the operator hasn't
    // entered credentials.  Skip endpoint resolution + the YoLink connection
    // entirely (no unwrap, no exit) and keep the plugin running with management
    // enabled, so the Studio can edit its config.  It connects normally once
    // creds are set and the plugin is restarted.
    if cfg.yolink.cloud.is_none() && cfg.yolink.local.is_none() {
        warn!(
            "YoLink not configured yet — set cloud or local credentials in the \
             UI, then restart. Management is enabled; keeping the plugin running \
             so its config can be edited."
        );
        // Block on the SDK task so the process stays alive serving management.
        let _ = sdk_task.await;
        return Ok(());
    }

    // --- YoLink endpoints + auth (configured path) -----------------------------
    // Resolve mode-specific endpoints from config
    let ep = Endpoints::from_config(&cfg.yolink)?;

    if ep.client_id.trim().is_empty() || ep.client_secret.trim().is_empty() {
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "No YoLink Client ID and Secret are set, so this plugin cannot \
                 authenticate and will publish no devices.",
            )
            .with_remedy(
                "Get a UAC Client ID and Secret from the YoLink app under Settings → \
                 Account → Advanced Settings → User Access Credentials, and enter them \
                 under Configuration.",
            ),
        );
    }

    // --- Auth -----------------------------------------------------------------
    let tokens = auth::TokenManager::new(
        ep.token_url.clone(),
        ep.client_id.clone(),
        ep.client_secret.clone(),
    );
    tokens.init().await?;
    // No background refresh task — get_token() does lazy refresh on demand.
    // Proactive refresh was causing the YoLink hub to invalidate the active
    // MQTT session when a new token was issued, dropping the connection.
    info!("YoLink authentication successful");

    // --- YoLink API client ----------------------------------------------------
    let yolink_api = Arc::new(YolinkApi::new(ep.api_base_url.clone(), tokens.clone()));

    // --- Build MQTT topic prefix (mode-specific) ------------------------------
    // Cloud: yl-home/{home_id}   — home_id fetched from the API
    // Local: ylsubnet/{net_id}   — net_id comes directly from config credentials
    let topic_prefix = match cfg.yolink.mode {
        config::Mode::Cloud => {
            let home_id = yolink_api.get_home_id().await?;
            info!(home_id = %home_id, "YoLink home ID obtained");
            format!("yl-home/{home_id}")
        }
        config::Mode::Local => {
            let net_id = &ep.net_id;
            info!(net_id = %net_id, "Using local hub Net ID for MQTT topics");
            format!("ylsubnet/{net_id}")
        }
    };

    // --- YoLink MQTT event stream ---------------------------------------------
    let (yolink_tx, yolink_rx) = mpsc::channel::<YolinkReport>(256);
    let yl_mqtt = YolinkMqtt::new(
        ep.mqtt_host.clone(),
        ep.mqtt_port,
        ep.client_id.clone(),
        tokens.clone(),
        notices.clone(),
    );
    tokio::spawn(yl_mqtt.run(topic_prefix, yolink_tx));

    // --- Device discovery -----------------------------------------------------
    let raw_devices = yolink_api.get_device_list().await?;
    info!(count = raw_devices.len(), "Discovered YoLink devices");

    let mut bridged_devices = Vec::new();

    for info in raw_devices {
        let kind = DeviceKind::from_yolink_type(&info.device_type);

        if !kind.is_supported() {
            info!(
                device_id = %info.device_id,
                device_type = %info.device_type,
                "Skipping unsupported device type"
            );
            continue;
        }

        let hc_id = format!("yolink_{}", info.device_id);
        let hc_type = kind.homecore_device_type();

        // Register with HomeCore via DevicePublisher (PluginClient is consumed)
        if let Err(e) = publisher
            .register_device_full(&hc_id, &info.name, Some(hc_type), None, None)
            .await
        {
            warn!(hc_id, error = %e, "Failed to register device");
        }
        // The schema is retained, so it survives this plugin being down and a
        // client that connects later still knows what the device means.
        if let Err(e) = schema::publish(&publisher, &hc_id, &kind).await {
            warn!(hc_id, error = %e, "Failed to publish device schema");
        }
        if let Err(e) = publisher.subscribe_commands(&hc_id).await {
            warn!(hc_id, error = %e, "Failed to subscribe commands");
        }

        publisher.publish_availability(&hc_id, true).await?;

        bridged_devices.push((info, kind));
    }

    info!(
        registered = bridged_devices.len(),
        "All devices registered with HomeCore"
    );

    // --- Bridge event loop (runs until error / shutdown) ----------------------
    let inventory_interval_secs = cfg
        .yolink
        .inventory_interval_secs
        .unwrap_or(cfg.yolink.poll_interval_secs);

    let bridge = bridge::Bridge::new(
        bridged_devices,
        yolink_api,
        publisher,
        rescan,
        bridge::BridgeOptions {
            temp_unit: cfg.yolink.temperature_unit.clone(),
            poll_interval_secs: cfg.yolink.poll_interval_secs,
            inventory_interval_secs,
            poll_device_delay_ms: cfg.yolink.poll_device_delay_ms,
            initial_fetch_delay_secs: cfg.yolink.initial_fetch_delay_secs,
        },
    );

    bridge.run(yolink_rx, cmd_rx).await
}

/// Path of the cross-restart device-id snapshot, sibling to
/// config.toml. Owned by the SDK device tracker via
/// `PluginClient::with_device_persistence`.
fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".published-device-ids.json")
}
