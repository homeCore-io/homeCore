mod bridge;
mod commands;
mod config;
mod hue;
mod logging;
mod pairing;
mod sync;
mod translator;

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use bridge::{Bridge, RefreshEvent, RefreshRequest, UnpairRequest};
use config::HuePluginConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match HuePluginConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    if !cfg.hue.compact_motion_facets {
        warn!(
            "compact_motion_facets = false: each Hue motion/temperature/light_level \
             service will register as a separate HomeCore device. Set this to true \
             (the default) to merge all sensors on one physical device into one \
             HomeCore device with multi-facet attributes."
        );
    }

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-hue plugin");
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
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(config_path, "hc-hue", "hc_hue=info", &bootstrap.logging)
}

async fn try_start(
    cfg: &HuePluginConfig,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let discovered = hue::discovery::discover_bridges(&cfg.hue).await?;
    let mut bridges = cfg.effective_bridges(&discovered);

    if bridges.is_empty() {
        error!("No Hue bridges configured or discovered; set [[bridges]] in config/config.toml");
        anyhow::bail!("no hue bridges available");
    }

    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config)
        .await?
        // Cross-restart device tracking — the SDK mirrors every
        // register/unregister to disk, and `publisher.reconcile_devices`
        // can later prune anything that vanished from upstream while
        // the plugin was offline. File lives next to config.toml.
        .with_device_persistence(published_ids_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // Manual-refresh channel — `refresh_devices` and
    // `cleanup_stale_devices` manifest actions push a RefreshRequest
    // through here. The bridge runtime re-walks every configured API
    // endpoint and forwards per-bridge progress events back to the
    // streaming action's sink.
    let (refresh_tx, refresh_rx) = mpsc::channel::<RefreshRequest>(8);
    // Streaming `pair_bridge` action publishes newly-paired bridges back
    // to the runtime over this channel; the bridge runtime adds them to
    // its apis vec and refreshes without restart.
    let (new_bridge_tx, new_bridge_rx) = mpsc::channel::<hue::models::BridgeTarget>(8);
    // `unpair_bridge` action → runtime: drop a bridge and delete its devices.
    let (unpair_tx, unpair_rx) = mpsc::channel::<UnpairRequest>(4);

    // Shared view of core's durable learned-state doc (bridge app_keys). Filled
    // by the SDK state handler below; read at startup to reconnect bridges with
    // their persisted keys, and updated by pairing when a new key is learned.
    let learned_state: pairing::LearnedState = std::sync::Arc::new(std::sync::Mutex::new(None));
    let state_writer = client.state_writer();
    let pairing_handle = pairing::PairingHandle::new(
        cfg.clone(),
        new_bridge_tx,
        learned_state.clone(),
        state_writer.clone(),
    );

    // Enable management protocol (heartbeat + remote config/log commands +
    // capability manifest).
    let mut mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest());
    // Receive core's durable learned-state (bridge app_keys) — retained on
    // connect, then on change. Just stash it; startup + pairing read the cell.
    {
        let ls = learned_state.clone();
        mgmt = mgmt.with_state_handler(move |doc| {
            *ls.lock().unwrap() = Some(doc);
        });
    }
    // Publish the operator-config JSON Schema so the config editor can render a
    // typed form (rides on the capability manifest).
    if let Some(schema) = config::config_schema() {
        mgmt = mgmt.with_config_schema(schema);
    }
    // …and the plugin-authored config descriptor, which the editor renders
    // instead of guessing a form from the schema. Rides the same manifest.
    mgmt = mgmt.with_config_descriptor(config::config_descriptor());
    // Layer the streaming pair_bridge action on top of the management +
    // capabilities handle.
    let mgmt = pairing::register_actions(mgmt, pairing_handle);
    // …and its inverse, unpair_bridge: forget a bridge, delete its devices, and
    // clear its stored app_key from learned state.
    let unpair_handle =
        pairing::UnpairHandle::new(unpair_tx, learned_state.clone(), state_writer.clone());
    let mgmt = pairing::register_unpair_action(mgmt, unpair_handle);

    // Streaming refresh_devices / cleanup_stale_devices: both feed the
    // same refresh-then-reconcile path in Bridge::run, but stream
    // per-bridge progress so the operator sees each bridge starting +
    // finishing instead of staring at a 5–30 s silent spinner.
    let refresh_tx_refresh = refresh_tx.clone();
    let mgmt = mgmt.with_streaming_action(plugin_sdk_rs::StreamingAction::new(
        "refresh_devices",
        move |ctx, _params| {
            let tx = refresh_tx_refresh.clone();
            async move { refresh_devices_streaming(ctx, tx).await }
        },
    ));
    let refresh_tx_cleanup = refresh_tx.clone();
    let mgmt = mgmt.with_streaming_action(plugin_sdk_rs::StreamingAction::new(
        "cleanup_stale_devices",
        move |ctx, _params| {
            let tx = refresh_tx_cleanup.clone();
            async move { refresh_devices_streaming(ctx, tx).await }
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

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Wait briefly for core's retained learned-state to arrive (delivered on
    // connect by the state handler), then merge persisted app_keys/inventory into
    // the bridge targets. Config app_keys remain the fallback, so existing
    // installs are unaffected. Best-effort — times out to config-only.
    for _ in 0..20 {
        if learned_state.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if let Some(learned) = learned_state.lock().unwrap().clone() {
        config::apply_learned_bridges(&mut bridges, &learned);
        info!("Applied core learned-state to bridge targets");
    }

    // Register bridge devices via DevicePublisher (PluginClient is consumed).
    for bridge in &bridges {
        let bridge_device_id = bridge.device_id();
        if let Err(e) = publisher
            .register_device_full(
                &bridge_device_id,
                &format!("Hue Bridge {}", bridge.name),
                Some("bridge"),
                None,
                None,
            )
            .await
        {
            error!(device_id = %bridge_device_id, error = %e, "Failed to register bridge device");
        }
        if let Err(e) = publisher.subscribe_commands(&bridge_device_id).await {
            error!(device_id = %bridge_device_id, error = %e, "Failed to subscribe bridge commands");
        }
        if let Err(e) = publisher
            .publish_availability(&bridge_device_id, true)
            .await
        {
            error!(device_id = %bridge_device_id, error = %e, "Failed to publish bridge availability");
        }
    }

    if let Err(e) = publisher.publish_plugin_status("active").await {
        error!(error = %e, "Failed to publish plugin status");
    }

    info!(
        count = bridges.len(),
        "Hue bridges registered with HomeCore"
    );

    let bridge_runtime = Bridge::new(
        cfg.clone(),
        bridges,
        publisher,
        learned_state.clone(),
        state_writer,
    );
    bridge_runtime
        .run(cmd_rx, refresh_rx, new_bridge_rx, unpair_rx)
        .await
}

/// Capability manifest for hc-hue. Plugin actions exposed to the admin
/// UI (Plugins → Hue) and to hc-mcp via `list_plugin_actions`.
fn capabilities_manifest() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(), // SDK fills from configured plugin_id
        actions: vec![
            Action {
                id: "refresh_devices".into(),
                label: "Refresh devices".into(),
                description: Some(
                    "Re-walk every configured Hue bridge and republish all \
                     lights, groups, scenes, and sensors to homeCore. \
                     Also reconciles the published-device snapshot — \
                     anything that was registered last session but no \
                     longer exists on its bridge gets unregistered. \
                     Streams per-bridge progress so you see each bridge \
                     starting and finishing as the walk runs. Use this \
                     after renaming devices or moving them between rooms \
                     in the Hue app, or when something looks stale."
                        .into(),
                ),
                params: None,
                result: Some(serde_json::json!({
                    "bridges": { "type": "integer" },
                    "ok": { "type": "integer" },
                    "failed": { "type": "integer" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: Some("bridge_id".into()),
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(120_000),
            },
            Action {
                id: "cleanup_stale_devices".into(),
                label: "Clean up stale devices".into(),
                description: Some(
                    "Force a fresh sync from every configured bridge and \
                     unregister any homeCore devices that no longer exist \
                     on the bridge. Same effect as `refresh_devices` but \
                     framed for cleanup instead of routine refresh — use \
                     this to clear zombies left over from config edits, \
                     bridge replacements, or development churn. For a \
                     wipe-and-rebuild (re-register everything from \
                     scratch), use the core API: \
                     `DELETE /api/v1/plugins/plugin.hue/devices`."
                        .into(),
                ),
                params: None,
                result: Some(serde_json::json!({
                    "bridges": { "type": "integer" },
                    "ok": { "type": "integer" },
                    "failed": { "type": "integer" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: Some("bridge_id".into()),
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(120_000),
            },
            Action {
                id: "pair_bridge".into(),
                label: "Pair Hue bridge".into(),
                description: Some(
                    "Pair a new Hue bridge by polling its link-button. \
                     Pass `host` to target a specific IP, or omit it to \
                     discover bridges on the network. If exactly one \
                     unpaired bridge is found it's auto-selected; if \
                     multiple are found you'll be prompted to pick one. \
                     The action then waits for you to press the physical \
                     link button — once pressed, the new app key is \
                     saved securely in homeCore and the bridge starts \
                     publishing immediately."
                        .into(),
                ),
                params: Some(serde_json::json!({
                    "host": {
                        "type": "string",
                        "description": "Optional bridge IP/hostname; auto-discovers if omitted",
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional friendly name for the new bridge entry",
                    },
                })),
                result: Some(serde_json::json!({
                    "bridge_id": { "type": "string" },
                    "host": { "type": "string" },
                    "name": { "type": "string" },
                })),
                stream: true,
                cancelable: true,
                concurrency: Concurrency::Single,
                item_key: Some("bridge_id".into()),
                item_operations: Some(vec![plugin_sdk_rs::types::ItemOp::Add]),
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(90_000),
            },
            Action {
                id: "unpair_bridge".into(),
                label: "Remove bridge".into(),
                description: Some(
                    "Forget a paired Hue bridge: unregister all of its devices \
                     from homeCore, clear its stored app key, and drop its \
                     runtime connection. Works even if the bridge is offline. \
                     The bridge can be re-paired later by pressing its link \
                     button. Pass the `bridge_id` of the bridge to remove — the \
                     config editor's per-bridge Remove action drives this."
                        .into(),
                ),
                params: Some(serde_json::json!({
                    "bridge_id": {
                        "type": "string",
                        "description": "bridge_id of the bridge to remove",
                    },
                })),
                result: Some(serde_json::json!({
                    "bridge_id": { "type": "string" },
                    "devices_removed": { "type": "integer" },
                    "was_running": { "type": "boolean" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(60_000),
            },
        ],
    }
}

/// Streaming `refresh_devices` (and `cleanup_stale_devices`). Sends a
/// `RefreshRequest` carrying a progress sink to the bridge runtime,
/// then forwards each bridge-start / bridge-done event to the
/// operator as a stream item so the action visibly does something
/// instead of going silent for the duration of the walk.
async fn refresh_devices_streaming(
    ctx: plugin_sdk_rs::StreamContext,
    refresh_tx: mpsc::Sender<RefreshRequest>,
) -> anyhow::Result<()> {
    use serde_json::json;

    let (progress_tx, mut progress_rx) = mpsc::channel::<RefreshEvent>(64);
    if refresh_tx
        .send(RefreshRequest {
            progress: Some(progress_tx),
        })
        .await
        .is_err()
    {
        return ctx
            .error("bridge runtime unavailable — plugin may be shutting down")
            .await;
    }

    ctx.progress(
        Some(5),
        Some("starting"),
        Some("Walking configured Hue bridges"),
    )
    .await?;

    let mut total_bridges: Option<usize> = None;
    let mut completed = 0usize;

    while let Some(ev) = progress_rx.recv().await {
        match ev {
            RefreshEvent::AllStart {
                total_bridges: total,
            } => {
                total_bridges = Some(total);
                let _ = ctx
                    .progress(
                        Some(10),
                        Some("walking"),
                        Some(&format!("Refreshing {total} bridge(s)")),
                    )
                    .await;
            }
            RefreshEvent::BridgeStart { bridge_id, name } => {
                let _ = ctx
                    .item_add(json!({
                        "bridge_id": bridge_id,
                        "name": name,
                        "status": "refreshing",
                    }))
                    .await;
            }
            RefreshEvent::BridgeDone {
                bridge_id,
                name,
                elapsed_ms,
                ok,
                error,
            } => {
                completed += 1;
                let percent = total_bridges
                    .filter(|t| *t > 0)
                    .map(|t| (completed * 100 / t).min(95) as u8);
                let _ = ctx
                    .item_update(json!({
                        "bridge_id": bridge_id,
                        "name": name,
                        "status": if ok { "ok" } else { "error" },
                        "elapsed_ms": elapsed_ms,
                        "error": error,
                    }))
                    .await;
                if let Some(p) = percent {
                    let _ = ctx
                        .progress(
                            Some(p),
                            Some(if ok { "bridge_ok" } else { "bridge_error" }),
                            Some(&format!(
                                "{} / {} bridges done",
                                completed,
                                total_bridges.unwrap_or(0)
                            )),
                        )
                        .await;
                }
            }
            RefreshEvent::AllDone {
                bridges,
                ok,
                failed,
            } => {
                ctx.progress(Some(100), Some("done"), Some("Refresh complete"))
                    .await?;
                return ctx
                    .complete(json!({
                        "bridges": bridges,
                        "ok":      ok,
                        "failed":  failed,
                    }))
                    .await;
            }
        }
    }

    // Channel closed without an AllDone — bridge runtime exited mid-flight.
    ctx.error("bridge runtime closed before refresh completed")
        .await
}

/// Path for the cross-restart device-id snapshot, sibling to the
/// plugin's config.toml. The file is owned by the SDK's device
/// tracker — see `PluginClient::with_device_persistence`. Falls
/// back to the current directory if the config path has no parent.
fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".published-device-ids.json")
}
