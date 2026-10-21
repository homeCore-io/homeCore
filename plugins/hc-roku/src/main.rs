//! hc-roku — Roku streaming players and Roku TVs in homeCore, over the
//! External Control Protocol.

mod actions;
mod bridge;
mod commands;
mod config;
mod discovery;
mod discovery_action;
mod ecp;
mod icons;
mod keys;
mod schema;
mod state;
#[cfg(test)]
mod testutil;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use plugin_sdk_rs::{AsyncActionFuture, PluginClient, PluginConfig};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use bridge::Bridge;
use config::RokuConfig;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

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
        logging: plugin_sdk_rs::logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    plugin_sdk_rs::logging::init_logging(config_path, "hc-roku", "hc_roku=info", &bootstrap.logging)
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
    // Channel icons, kept beside the config and served to the screens.
    let files_dir = sibling(config_path, ".roku-files");

    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    let bridge = Arc::new(Bridge::new(
        cfg.clone(),
        publisher.clone(),
        state_writer,
        notices,
        &files_dir,
    ));

    // ── Management protocol ──────────────────────────────────────────
    let bridge_for_action = Arc::clone(&bridge);
    let bridge_for_handler = Arc::clone(&bridge);
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
        .with_alert_kinds(alert_kinds())
        // Answered on their own tasks: several of them ask every Roku, and
        // none may hold up the MQTT loop or a runtime thread while they wait.
        .with_async_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            if !HANDLED_ACTIONS.contains(&action.as_str()) {
                return None; // the SDK's unknown-action error
            }
            let b = Arc::clone(&bridge_for_handler);
            let cmd = cmd.clone();
            Some(Box::pin(async move {
                match action.as_str() {
                    "status" => b.status().await,
                    "list_devices" => b.list_devices().await,
                    "refresh_catalog" => b.refresh_catalog(&cmd).await,
                    "device_info" => b.device_info(&cmd).await,
                    "send_command" => b.send_command(&cmd).await,
                    "forget_device" => b.forget_device(&cmd).await,
                    "forget_stale_devices" => b.forget_stale_devices().await,
                    other => {
                        json!({ "status": "error", "error": format!("unknown action '{other}'") })
                    }
                }
            }) as AsyncActionFuture)
        })
        .with_streaming_action(plugin_sdk_rs::StreamingAction::new(
            "discover_devices",
            move |ctx, _params| {
                let b = Arc::clone(&bridge_for_action);
                async move { discovery_action::discover_devices_streaming(ctx, b).await }
            },
        ))
        // Learned state: each Roku by serial — its id, address, MACs,
        // capabilities, channels and launch counts — so it keeps its identity
        // across restarts and DHCP moves, and its pickers are never empty.
        .with_state_handler(move |doc| {
            let b = Arc::clone(&bridge_for_state);
            tokio::spawn(async move { b.apply_learned_state(&doc).await });
        })
        .with_files_dir(&files_dir);

    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());
    // Its own screens: the built `ui/` folder it carries, served from here.
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION")),
        None => mgmt,
    };

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

    info!(
        devices = cfg.devices.len(),
        discovery = cfg.roku.discovery_enabled,
        poll_secs = cfg.roku.poll_interval_secs,
        "hc-roku started"
    );

    // Blocks until the command channel closes.
    bridge.run(cmd_rx).await;
    Ok(())
}

/// Actions routed to the bridge. Anything not listed falls through to the
/// SDK so `ping` / `get_config` / `set_config` / `set_log_level` keep
/// their built-in behaviour.
const HANDLED_ACTIONS: &[&str] = &[
    "status",
    "list_devices",
    "refresh_catalog",
    "device_info",
    "send_command",
    "forget_device",
    "forget_stale_devices",
];

fn published_ids_path(config_path: &str) -> std::path::PathBuf {
    sibling(config_path, ".published-device-ids.json")
}

/// A file or folder beside the config.
fn sibling(config_path: &str, name: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(name)
}

/// The alerts this plugin raises. A Roku that stops answering is homeCore's
/// own Offline alert, not repeated here.
fn alert_kinds() -> Vec<plugin_sdk_rs::types::PluginAlertKind> {
    use plugin_sdk_rs::types::{AlertSeverity, PluginAlertKind};
    vec![PluginAlertKind {
        topic: "ecp_control_disabled".into(),
        label: "Roku refuses control".into(),
        description: Some(
            "The Roku's \"Control by mobile apps\" is disabled: homeCore sees what it \
             plays but cannot press a button or open a channel. Fixed on the Roku: \
             Settings › System › Advanced system settings › Control by mobile apps › \
             Network access: Default (or Permissive). Clears once a command goes through."
                .into(),
        ),
        severity: AlertSeverity::Warning,
        default_on: true,
    }]
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
                id: "status".into(),
                label: "Status".into(),
                description: Some(
                    "Each Roku as the plugin knows it, from memory — its address and \
                     network, model, Roku OS, whether it allows phone control, its \
                     channel and input counts, what it shows, and the last command \
                     error — and when the plugin last looked for Rokus. Asks no Roku \
                     anything."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "rokus": { "type": "array" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::ReadOnly,
                timeout_ms: Some(5_000),
                danger: false,
            },
            Action {
                id: "discover_devices".into(),
                label: "Discover Rokus".into(),
                description: Some(
                    "Look for Rokus now: an SSDP `roku:ecp` search (with discovery on), \
                     plus every address under `manual_hosts` and every address a Roku \
                     was configured or last seen at, each asked for its serial. A Roku \
                     that moved is found again by its serial and keeps its id. New \
                     ones are registered when discovery and `auto_add_discovered` are \
                     on. SSDP is link-local — use `manual_hosts` across a VLAN."
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
                danger: false,
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
                danger: false,
            },
            Action {
                id: "refresh_catalog".into(),
                label: "Refresh channels + inputs".into(),
                description: Some(
                    "Re-read the installed-channel list, TV inputs, and (on a \
                     Roku TV) the tuner's channel lineup — from one Roku (`hc_id`) \
                     or every one — and republish them, fetching any new channel's \
                     icon. Normally read on a slow timer, standby included."
                        .into(),
                ),
                params: Some(json!({
                    "hc_id": { "type": "string", "description": "One Roku; every one when absent" },
                })),
                result: Some(json!({ "devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(30_000),
                danger: false,
            },
            Action {
                id: "device_info".into(),
                label: "Device info".into(),
                description: Some(
                    "Raw `query/device-info` from one Roku (`hc_id`) or every one — \
                     model, firmware, MACs, power mode, and every capability flag \
                     the device advertises. The diagnostic to reach for first when \
                     a device behaves unexpectedly; none of it is in device state."
                        .into(),
                ),
                params: Some(json!({
                    "hc_id": { "type": "string", "description": "One Roku; every one when absent" },
                })),
                result: Some(json!({ "devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(20_000),
                danger: false,
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
                timeout_ms: Some(45_000),
                danger: false,
            },
            Action {
                id: "forget_device".into(),
                label: "Forget a Roku".into(),
                description: Some(
                    "For a Roku that is gone for good: it stops being managed, its \
                     device is removed (with it, its place in automations), and it is \
                     forgotten. One that is only switched off or unplugged comes back by \
                     itself — leave it. A Roku listed in the settings is removed there."
                        .into(),
                ),
                params: Some(json!({
                    "hc_id": { "type": "string", "description": "The Roku's device id" },
                })),
                result: Some(json!({ "forgotten": { "type": "string" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(15_000),
                danger: true,
            },
            Action {
                id: "forget_stale_devices".into(),
                label: "Forget unreachable devices".into(),
                description: Some(
                    "Forget every discovered Roku that is not answering right now. \
                     Deliberately manual: a Roku that is merely unplugged, or a TV that \
                     is fully powered off, looks exactly like one that has been thrown \
                     away, so the plugin never does this on its own. Devices listed in \
                     `[[devices]]` are never removed."
                        .into(),
                ),
                params: None,
                result: Some(json!({ "retired": { "type": "array" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(30_000),
                danger: true,
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
