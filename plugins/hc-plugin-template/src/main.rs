//! hc-plugin-template — a homeCore plugin, reduced to the parts every plugin has.
//!
//! It publishes one virtual light per `[[template.devices]]` entry, accepts
//! `on` and `brightness` commands, and echoes the result back as state. Replace
//! the device talking with your own protocol and the shape stays the same.
//!
//! What is here because every plugin needs it:
//!
//!   1. Read the config path from `argv[1]` — core hands it to you.
//!   2. Connect to the broker with the credentials in `[homecore]`.
//!   3. Forward logs, so they land in homeCore's live log stream and not only
//!      in this process's stderr.
//!   4. `enable_management`, so core can heartbeat, restart, and reconfigure
//!      you, and so the UI can render your actions as buttons.
//!   5. Remember which devices you registered, so a device that disappears
//!      while the plugin is down can still be retired.
//!   6. Register devices, then publish their state retained.
//!   7. Raise notices for conditions an operator can act on, and CLEAR them
//!      when they stop being true.
//!   8. `run_managed`, which owns the process from then on.
//!
//! Read <https://github.com/homeCore-io/hc-wled> next — it is the smallest
//! complete plugin — then <https://github.com/homeCore-io/hc-roku> for
//! discovery and streaming actions.

mod config;

use anyhow::Result;
use plugin_sdk_rs::mqtt_log_layer::{MqttLogHandle, MqttLogLayer};
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::Config;

#[tokio::main]
async fn main() {
    // Two layers: stderr for whoever is watching the process, and the SDK's
    // MQTT layer so the same lines reach homeCore's live log stream. The MQTT
    // layer starts inactive and is switched on once we have a connection.
    let (mqtt_layer, mqtt_logs) = MqttLogLayer::new();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // The SDK's own target is included on purpose: reconnects,
                // subscription restores and event-loop trouble are logged by
                // `plugin_sdk_rs`, and filtering to your own crate hides
                // exactly the lines you need when a plugin misbehaves.
                "hc_plugin_template=info,plugin_sdk_rs=info".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(mqtt_layer)
        .init();

    // Core passes the config path as the first argument. The fallback is only
    // for running the binary by hand.
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    if let Err(e) = run(&config_path, mqtt_logs).await {
        error!(error = %e, "plugin exited with an error");
        std::process::exit(1);
    }
}

/// Where the device snapshot lives: beside the config file core handed us.
///
/// The SDK inserts this plugin's id into the filename, so plugins sharing a
/// config directory cannot share a snapshot and retire each other's devices.
fn snapshot_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join(".published-device-ids.json")
}

async fn run(config_path: &str, mqtt_logs: MqttLogHandle) -> Result<()> {
    let cfg = Config::load(config_path)?;

    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    // Remember what we registered, across restarts. Without this, the
    // reconcile below can only see devices registered in *this* process, so a
    // device dropped from config while the plugin was down would linger in
    // homeCore forever — visible, and accepting commands nothing executes.
    .with_device_persistence(snapshot_path(config_path));

    // Now that there is a connection, start shipping logs down it.
    mqtt_logs.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.forward_level,
    );

    let publisher = client.device_publisher();
    let notices = client.notices();

    // Management: heartbeat, remote config, log level, and the action manifest
    // the web UI turns into buttons. Without this core cannot supervise you.
    let mgmt = client
        .enable_management(
            60, // heartbeat seconds
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            None, // a LogLevelHandle, if you wire one up
        )
        .await?
        .with_capabilities(capabilities())
        .with_custom_handler(|cmd| match cmd["action"].as_str()? {
            "say_hello" => Some(json!({ "status": "ok", "message": "hello from the template" })),
            _ => None,
        });

    // An empty device list is what a fresh install looks like, and without a
    // notice it is indistinguishable from a healthy plugin: active, zero
    // devices, no explanation.
    if cfg.template.devices.is_empty() {
        notices.raise(
            PluginNotice::warning(
                "no_devices_configured",
                "No devices are configured, so this plugin publishes nothing.",
            )
            .with_remedy("Add a [[template.devices]] entry in this plugin's configuration."),
        );
    } else {
        // Always the other half. A notice is state: raising it without ever
        // clearing it leaves the operator staring at a problem they fixed.
        notices.clear("no_devices_configured");
    }

    // State lives here so both the command handler and the publisher can see
    // it. A real plugin would read it back from the device instead.
    let state: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));

    for device in &cfg.template.devices {
        publisher
            .register_device_full(&device.id, &device.name, Some("light"), None, None)
            .await?;

        // Registration and command subscription are SEPARATE, and forgetting
        // this line is the classic first bug: the device appears in homeCore,
        // its state updates, and every command silently goes nowhere, because
        // nothing is subscribed to its cmd topic.
        publisher.subscribe_commands(&device.id).await?;

        let initial = json!({ "on": false, "brightness": 0 });
        state
            .lock()
            .expect("state mutex")
            .insert(device.id.clone(), initial.clone());
        publisher.publish_state(&device.id, &initial).await?;
        publisher.publish_availability(&device.id, true).await?;

        info!(device_id = %device.id, "registered");
    }

    // Retire anything homeCore still holds for us that is no longer live.
    //
    // For this template the live set is simply what config lists, so the
    // reconcile is safe to run unconditionally. A real plugin whose devices
    // come from a bridge or a cloud API must only do this after a sync it
    // trusts: on a partial fetch, reconcile unregisters devices that are
    // perfectly healthy behind a hub that happened to be unreachable. The
    // usual shape is an `all_sources_succeeded` flag tracked across the
    // per-source loop, with the reconcile inside the `if`.
    //
    // Plugins whose upstream reports irregularly — battery sensors that go
    // quiet for hours — should keep the persistence and skip the reconcile
    // entirely. An operator can clear zombies with
    // `DELETE /api/v1/plugins/{id}/devices`.
    let live: HashSet<String> = cfg.template.devices.iter().map(|d| d.id.clone()).collect();
    match publisher.reconcile_devices(live).await {
        Ok(report) => {
            for id in &report.stale_unregistered {
                info!(device_id = %id, "retired a device that is no longer configured");
            }
        }
        Err(e) => warn!(error = %e, "reconcile failed; stale devices may linger"),
    }

    // Commands arrive on the SDK's callback, which is synchronous. Hand them
    // to a task over a channel so slow device I/O never stalls the event loop.
    let (tx, mut rx) = mpsc::channel::<(String, Value)>(64);

    let state_for_task = Arc::clone(&state);
    let publisher_for_task = publisher.clone();
    tokio::spawn(async move {
        while let Some((device_id, payload)) = rx.recv().await {
            if let Err(e) = apply(&publisher_for_task, &state_for_task, &device_id, &payload).await
            {
                warn!(device_id = %device_id, error = %e, "command failed");
            }
        }
    });

    // Owns the process from here. Returns only if the loop fails.
    client
        .run_managed(
            move |device_id, payload| {
                // try_send, not send: dropping a command under load beats
                // blocking the MQTT event loop behind it.
                if tx.try_send((device_id.clone(), payload)).is_err() {
                    warn!(device_id = %device_id, "command queue full, dropped");
                }
            },
            mgmt,
        )
        .await
}

/// Apply a command and publish the result.
///
/// homeCore never writes device state directly — the plugin owns it. A command
/// arrives on `.../cmd`, you do whatever the device needs, and you publish what
/// actually happened on `.../state`. That is why the UI can show a light as off
/// when the bulb refused to turn on.
async fn apply(
    publisher: &plugin_sdk_rs::DevicePublisher,
    state: &Arc<Mutex<HashMap<String, Value>>>,
    device_id: &str,
    payload: &Value,
) -> Result<()> {
    let next = {
        let mut guard = state.lock().expect("state mutex");
        let Some(current) = guard.get_mut(device_id) else {
            warn!(device_id, "command for a device this plugin does not own");
            return Ok(());
        };

        // ...talk to the real device here, and only record what it confirms.
        if let Some(on) = payload.get("on").and_then(Value::as_bool) {
            current["on"] = json!(on);
        }
        if let Some(brightness) = payload.get("brightness").and_then(Value::as_u64) {
            current["brightness"] = json!(brightness.min(255));
            current["on"] = json!(brightness > 0);
        }
        current.clone()
    };

    // `_for_command` attaches provenance, so the UI and the audit log can say
    // what caused the change rather than showing an anonymous update.
    publisher
        .publish_state_for_command(device_id, &next, payload, "template")
        .await?;
    Ok(())
}

/// The action manifest. Every entry becomes a button on the plugin's page in
/// the web UI, and is callable from hc-mcp — with no UI code at either end.
fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        // Left empty: the SDK fills in the id it connected with.
        plugin_id: String::new(),
        actions: vec![Action {
            id: "say_hello".into(),
            label: "Say hello".into(),
            description: Some("Proves the manifest and the handler are wired together.".into()),
            params: None,
            result: Some(json!({ "message": { "type": "string" } })),
            stream: false,
            cancelable: false,
            concurrency: Concurrency::default(),
            item_key: None,
            item_operations: None,
            requires_role: RequiresRole::User,
            timeout_ms: None,
        }],
    }
}
