//! hc-roku — Roku streaming players and Roku TVs in homeCore, over the
//! External Control Protocol.

mod actions;
mod bridge;
mod commands;
mod config;
mod discovery;
mod discovery_action;
mod ecp;
mod keys;
mod logging;
mod schema;
mod state;
#[cfg(test)]
mod testutil;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

use bridge::{Bridge, MgmtRequest};
use config::RokuConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

/// How long the SDK's synchronous management handler waits for the
/// bridge to answer before giving up.
///
/// This handler runs on the MQTT event loop, so the wait blocks it — the
/// bound is what stops one unreachable Roku from stalling heartbeats and
/// every other management command. Core's own action window is longer
/// than this, so a timeout here still produces a real error response
/// rather than a silent 504.
const MGMT_TIMEOUT: Duration = Duration::from_secs(25);

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match RokuConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-roku plugin");
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
    logging::init_logging(config_path, "hc-roku", "hc_roku=info", &bootstrap.logging)
}

async fn try_start(
    cfg: &RokuConfig,
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
        .with_device_persistence(published_ids_path(config_path));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );

    let publisher = client.device_publisher();
    // Conditions for the plugin page, not only the log.
    let notices = client.notices();
    let state_writer = client.state_writer();
    let devices: bridge::Devices = Arc::new(RwLock::new(HashMap::new()));

    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);
    let (mgmt_tx, mgmt_rx) = mpsc::channel::<MgmtRequest>(32);

    let bridge = Arc::new(Bridge::new(
        cfg.clone(),
        publisher.clone(),
        state_writer,
        Arc::clone(&devices),
    ));

    // ── Management protocol ──────────────────────────────────────────
    let mgmt_tx_for_handler = mgmt_tx.clone();
    let bridge_for_action = Arc::clone(&bridge);
    let bridge_for_state = Arc::clone(&bridge);

    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest())
        .with_custom_handler(move |cmd| {
            // The SDK's custom handler is synchronous, but every action
            // here needs the bridge's device map and an HTTP round-trip.
            // Rather than spin a nested runtime (and a second reqwest
            // connection pool), hand the request to the bridge — which is
            // already running on this process's runtime — and wait for
            // its answer on a plain channel.
            let action = cmd["action"].as_str()?;
            if !HANDLED_ACTIONS.contains(&action) {
                return None; // fall through to the SDK's unknown-action error
            }
            let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
            if mgmt_tx_for_handler
                .try_send(MgmtRequest {
                    cmd: cmd.clone(),
                    reply: reply_tx,
                })
                .is_err()
            {
                return Some(json!({
                    "status": "error",
                    "error": "plugin is busy; management queue is full",
                }));
            }
            match reply_rx.recv_timeout(MGMT_TIMEOUT) {
                Ok(v) => Some(v),
                Err(_) => Some(json!({
                    "status": "error",
                    "error": format!(
                        "timed out after {}s waiting for the device to respond",
                        MGMT_TIMEOUT.as_secs()
                    ),
                })),
            }
        })
        .with_streaming_action(plugin_sdk_rs::StreamingAction::new(
            "discover_devices",
            move |ctx, _params| {
                let b = Arc::clone(&bridge_for_action);
                async move { discovery_action::discover_devices_streaming(ctx, b).await }
            },
        ))
        // Learned state: the serial → device-id map, so a discovered Roku
        // keeps its identity across restarts and DHCP moves.
        .with_state_handler(move |doc| {
            let b = Arc::clone(&bridge_for_state);
            tokio::spawn(async move { b.apply_learned_state(&doc).await });
        });

    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    // Start the SDK event loop BEFORE registering any device. The
    // rumqttc send channel holds 64 messages and each device costs four
    // (register, schema, subscribe, availability); with the loop not yet
    // draining, the 16th device would block `publish().await` forever.
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

    // Let the event loop reach CONNACK before the first publish.
    tokio::time::sleep(Duration::from_millis(150)).await;

    if let Err(e) = publisher.publish_plugin_status("active").await {
        warn!(error = %e, "Failed to publish plugin status");
    }

    // Zero devices looks exactly like a healthy plugin from the outside, so
    // say why. SSDP is multicast, which a container bridge network drops.
    if cfg.devices.is_empty() {
        notices.raise(
            PluginNotice::warning(
                "no_devices_configured",
                "No Roku devices are configured, so this plugin publishes nothing.",
            )
            .with_remedy(
                "Run the discovery action, which sweeps for Rokus over SSDP. If it \
                 finds nothing and homeCore runs in a container on a bridge network, \
                 that is expected — SSDP is multicast and does not cross the bridge. \
                 Add each Roku by IP under Configuration instead.",
            ),
        );
    } else {
        notices.clear("no_devices_configured");
    }

    info!(
        devices = cfg.devices.len(),
        discovery = cfg.roku.discovery_enabled,
        poll_secs = cfg.roku.poll_interval_secs,
        "hc-roku started"
    );

    // Blocks until every channel closes.
    bridge.run(cmd_rx, mgmt_rx).await;
    Ok(())
}

/// Actions routed to the bridge. Anything not listed falls through to the
/// SDK so `ping` / `get_config` / `set_config` / `set_log_level` keep
/// their built-in behaviour.
const HANDLED_ACTIONS: &[&str] = &[
    "list_devices",
    "refresh_catalog",
    "device_info",
    "send_command",
    "app_icon",
    "forget_stale_devices",
];

fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".published-device-ids.json")
}

/// Plugin-wide capability manifest.
///
/// Per-device control (play, launch a channel, press a key) is *not*
/// here: it flows through `PATCH /devices/{id}/state` and the device's
/// `cmd` topic, which is where homeCore expects device control to live.
/// What is here is the plugin-scoped work — discovery, catalogue
/// refresh, diagnostics — plus `send_command`, which exists only because
/// the device `cmd` topic is fire-and-forget and cannot report whether a
/// keypress actually reached the device.
fn capabilities_manifest() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "discover_devices".into(),
                label: "Discover Rokus".into(),
                description: Some(
                    "Broadcast an SSDP `roku:ecp` search and stream each device \
                     as it answers, then query it directly for its serial and \
                     name. Also probes every address under `manual_hosts`. \
                     Devices are registered as they are found when \
                     `auto_add_discovered` is on. SSDP is link-local, so this \
                     only reaches the subnet homeCore is on — use \
                     `manual_hosts` for anything across a VLAN."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                    "registered": { "type": "integer" },
                })),
                stream: true,
                cancelable: true,
                // One sweep at a time: concurrent M-SEARCHes on the same
                // socket family just duplicate replies and race each
                // other to register the same device.
                concurrency: Concurrency::Single,
                item_key: Some("host".into()),
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(60_000),
            },
            Action {
                id: "list_devices".into(),
                label: "List managed devices".into(),
                description: Some(
                    "Every Roku this plugin currently manages, with its address, \
                     serial, whether it came from config or discovery, and \
                     whether it is answering."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "devices": { "type": "array" },
                    "count": { "type": "integer" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_catalog".into(),
                label: "Refresh channels + inputs".into(),
                description: Some(
                    "Re-read the installed-channel list, TV inputs, and (on a \
                     Roku TV) the tuner's channel lineup from every managed \
                     device, and republish them on device state. These are \
                     normally read on a slow timer, so run this after \
                     installing a channel or re-scanning for TV channels \
                     instead of waiting."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(30_000),
            },
            Action {
                id: "device_info".into(),
                label: "Device info".into(),
                description: Some(
                    "Raw `query/device-info` from every managed device — model, \
                     firmware, MACs, power mode, and every capability flag the \
                     device advertises. The diagnostic to reach for first when \
                     a device behaves unexpectedly."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(20_000),
            },
            Action {
                id: "send_command".into(),
                label: "Send a command".into(),
                description: Some(
                    "Run one device command and report whether it succeeded. \
                     The same commands work through `PATCH /devices/{id}/state`, \
                     which is the normal path — but that is fire-and-forget, so \
                     this exists for scripting and for diagnosing a device that \
                     appears to ignore commands. `command` takes the usual \
                     payload, e.g. `{\"action\":\"key\",\"key\":\"Home\"}` or \
                     `{\"action\":\"launch_app\",\"app\":\"Netflix\"}`."
                        .into(),
                ),
                params: Some(json!({
                    "hc_id":   { "type": "string", "description": "Target device id" },
                    "command": { "type": "object", "description": "Command payload" },
                })),
                result: Some(json!({ "result": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(20_000),
            },
            Action {
                id: "app_icon".into(),
                label: "Get channel icon".into(),
                description: Some(
                    "Fetch a channel's icon from the device as a data URI, for \
                     rendering a source picker. Requires \"Control by mobile \
                     apps\" to be enabled on the Roku."
                        .into(),
                ),
                params: Some(json!({
                    "hc_id":  { "type": "string" },
                    "app_id": { "type": "string", "description": "Channel id, e.g. \"12\"" },
                })),
                result: Some(json!({
                    "data_uri": { "type": "string" },
                    "content_type": { "type": "string" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(15_000),
            },
            Action {
                id: "forget_stale_devices".into(),
                label: "Forget unreachable devices".into(),
                description: Some(
                    "Unregister every discovered device that is not answering \
                     right now. Deliberately manual: a Roku that is merely \
                     unplugged, or a TV that is fully powered off, looks exactly \
                     like one that has been thrown away, so the plugin will not \
                     do this on its own. Devices listed in `[[devices]]` are \
                     never removed."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "retired": { "type": "array" },
                    "unreachable": { "type": "array" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(30_000),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action the manifest advertises must actually be routed —
    /// either to the bridge or to a streaming handler. An advertised
    /// action with no route returns "unknown action" to a UI that is
    /// showing the operator a button for it.
    #[test]
    fn every_manifest_action_is_routed() {
        const STREAMING: &[&str] = &["discover_devices"];
        for action in capabilities_manifest().actions {
            assert!(
                HANDLED_ACTIONS.contains(&action.id.as_str())
                    || STREAMING.contains(&action.id.as_str()),
                "manifest advertises '{}' but nothing handles it",
                action.id,
            );
        }
    }

    /// The capability manifest carries the config schema and the editor
    /// descriptor, and the SDK caps the MQTT packet at 1 MiB. Over that,
    /// rumqttc drops the publish *at the event loop* — the plugin stays
    /// connected and heartbeating while its schema never arrives, so the
    /// config editor silently falls back to a raw TOML textarea.
    #[cfg(feature = "schema")]
    #[test]
    fn capability_manifest_fits_in_one_mqtt_packet() {
        let mut manifest = serde_json::to_value(capabilities_manifest()).unwrap();
        manifest["config_schema"] = config::config_schema().unwrap();
        manifest["config_descriptor"] = config::config_descriptor();
        let bytes = serde_json::to_vec(&manifest).unwrap().len();
        assert!(
            bytes < 1024 * 1024,
            "capability manifest is {bytes} bytes, over the SDK's 1 MiB packet limit",
        );
    }

    /// …and the reverse: a routed action nothing advertises is invisible.
    #[test]
    fn every_routed_action_is_advertised() {
        let advertised: Vec<String> = capabilities_manifest()
            .actions
            .into_iter()
            .map(|a| a.id)
            .collect();
        for handled in HANDLED_ACTIONS {
            assert!(
                advertised.iter().any(|a| a == handled),
                "'{handled}' is routed but missing from the capability manifest",
            );
        }
    }
}
