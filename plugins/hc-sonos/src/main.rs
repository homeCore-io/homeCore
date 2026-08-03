mod actions;
mod api;
mod bridge;
mod config;
mod discovery;
mod discovery_action;
mod events;
mod shared_state;
mod speaker;
mod subscription;

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

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

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match SonosConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-sonos plugin");
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
// Logging
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
        "hc-sonos",
        "hc_sonos=info",
        &bootstrap.logging,
    )
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async fn try_start(
    cfg: &SonosConfig,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // ── Shared Sonos speaker state (bridge + HTTP API) ─────────────────────
    let app_state = shared_state::new_state();

    // ── HomeCore SDK ─────────────────────────────────────────────────────
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking — SDK mirrors every speaker
        // register/unregister to disk so retire_stale_speakers can
        // reconcile across restarts (catches speakers physically
        // removed while the plugin was offline).
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

    // ── Manual rediscover signal ──────────────────────────────────────────
    // `rediscover_speakers` manifest action pings this so the discovery
    // loop runs an immediate scan instead of waiting for the next
    // `discovery_interval_secs` tick.
    let (rescan_tx, rescan_rx) = mpsc::channel::<()>(8);

    // ── Discovery channel ─────────────────────────────────────────────────
    // Created before management wiring so the streaming
    // `discover_speakers` action can hold a clone of the sender and
    // forward newly-found speakers into the bridge's integration path.
    let (discovery_tx, discovery_rx) = mpsc::channel::<sonor::Speaker>(32);
    let discovery_tx_for_action = discovery_tx.clone();
    let cfg_for_action = cfg.clone();

    // Enable management protocol (heartbeat + remote config/log commands).
    let mgmt =
        client
            .enable_management(
                60,
                Some(env!("CARGO_PKG_VERSION").to_string()),
                Some(config_path.to_string()),
                Some(log_level_handle),
            )
            .await?
            .with_capabilities(capabilities_manifest())
            .with_custom_handler(move |cmd| match cmd["action"].as_str()? {
                "rediscover_speakers" => {
                    let _ = rescan_tx.try_send(());
                    Some(serde_json::json!({ "status": "ok" }))
                }
                _ => None,
            })
            .with_streaming_action(plugin_sdk_rs::StreamingAction::new(
                "discover_speakers",
                move |ctx, _params| {
                    let cfg = cfg_for_action.clone();
                    let bridge_tx = discovery_tx_for_action.clone();
                    async move {
                        discovery_action::discover_speakers_streaming(ctx, cfg, bridge_tx).await
                    }
                },
            ));

    // Publish the operator-config JSON Schema so the hc-web editor renders a
    // typed form (rides on the capability manifest).
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };

    // Publish our own config descriptor — how the config should be presented
    // (units, conditionals, live sources, prose), which the schema can't say.
    // The editor renders this directly instead of inferring a form.
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // ── Spawn SDK event loop ─────────────────────────────────────────────
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

    // ── Spawn discovery task ──────────────────────────────────────────────
    discovery::spawn(
        Duration::from_secs(cfg.sonos.discovery_interval_secs),
        Duration::from_secs(cfg.sonos.discovery_timeout_secs),
        cfg.sonos.manual_hosts.clone(),
        discovery_tx,
        rescan_rx,
    );

    // ── GENA event channel (Sonos → API handler → bridge) ────────────────
    let (event_tx, event_rx) = mpsc::channel::<(String, events::NotifyEvent)>(256);

    // ── Spawn HTTP API server ──────────────────────────────────────────────
    if cfg.api.enabled {
        let api_state = app_state.clone();
        let api_host = cfg.api.host.clone();
        let api_port = cfg.api.port;
        let api_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::serve(&api_host, api_port, api_state, api_tx).await {
                error!(error = %e, "HTTP API server failed");
            }
        });
    }

    info!(
        discovery_interval_secs = cfg.sonos.discovery_interval_secs,
        manual_hosts = cfg.sonos.manual_hosts.len(),
        api_enabled = cfg.api.enabled,
        api_port = cfg.api.port,
        callback_host = cfg.api.callback_host.as_deref().unwrap_or("127.0.0.1"),
        "hc-sonos started (GENA mode)"
    );

    // ── Run bridge (blocks until command channel closes) ─────────────────
    let bridge = bridge::Bridge::new(cfg, publisher, app_state, notices);
    bridge.run(discovery_rx, cmd_rx, event_rx).await;

    Ok(())
}

/// Capability manifest for hc-sonos. SSDP discovery is finicky on
/// dual-NIC hosts and across Wi-Fi mesh segments; the manual rediscover
/// action lets ops kick a fresh scan without restarting the plugin.
fn capabilities_manifest() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(), // SDK fills from configured plugin_id
        actions: vec![
            Action {
                id: "rediscover_speakers".into(),
                label: "Rediscover speakers".into(),
                description: Some(
                    "Run an immediate SSDP + manual-host discovery sweep \
                     instead of waiting for the next `discovery_interval_secs` \
                     tick. Useful after adding or moving a Sonos speaker on \
                     the network, or when SSDP didn't pick up a speaker on \
                     the previous cycle (multi-NIC hosts, Wi-Fi mesh \
                     segments)."
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
            Action {
                id: "discover_speakers".into(),
                label: "Discover speakers".into(),
                description: Some(
                    "Run SSDP / manual-host discovery and stream each \
                     speaker as it's found (uuid, room name, host:port). \
                     Found speakers are also forwarded into the bridge's \
                     integration path, so any new ones get registered \
                     automatically. Use this when you want to see what's \
                     on the network right now — `rediscover_speakers` is \
                     the lightweight fire-and-forget version that just \
                     kicks the periodic loop."
                        .into(),
                ),
                params: None,
                result: Some(serde_json::json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: Some("uuid".into()),
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(60_000),
            },
        ],
    }
}

/// Path of the cross-restart device-id snapshot, sibling to
/// config.toml. Owned by the SDK device tracker.
fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".published-device-ids.json")
}
