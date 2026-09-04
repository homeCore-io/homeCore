//! hc-nuheat — NuHeat Signature floor-heating thermostats, over NuHeat's cloud
//! OpenAPI.
//!
//! One homeCore `thermostat` device per thermostat on the account, polled.
//! There is no local protocol to speak: a NuHeat Signature talks to NuHeat and
//! nothing else, so the cloud is the device as far as this plugin is concerned.
//!
//! The order below is the one the SDK requires, not a stylistic one — in
//! particular `run_managed` is spawned *before* anything is registered. See the
//! comment at that call.
//!
//! ```text
//! src/
//! ├── main.rs      connect → logs → manage → describe → run → poll → consume
//! ├── config.rs    the config core hands you as argv[1], and how it renders
//! ├── auth.rs      OAuth2 against identity.mynuheat.com, and where tokens live
//! ├── api.rs       the NuHeat OpenAPI, from its live swagger documents
//! ├── device.rs    thermostat ⇄ homeCore device: state, schema, commands
//! ├── link.rs      the streaming "Link NuHeat account" action
//! ├── runtime.rs   the poll loop, the command path, and the notices
//! └── units.rs     NuHeat's integer temperatures ⇄ °C
//! ```

mod api;
mod auth;
mod config;
mod device;
mod link;
mod runtime;
mod units;

use anyhow::Result;
use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::Config;
use runtime::Runtime;

/// Bounded startup retries, then exit and let core's supervisor apply its own
/// exponential backoff. The convention every shipped plugin follows.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

#[tokio::main]
async fn main() {
    // Core passes the config path as the first argument; the fallback is only
    // for running the binary by hand.
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
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-nuheat plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) if attempt < MAX_ATTEMPTS => {
                error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
            Err(e) => {
                error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                std::process::exit(1);
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
    // `plugin_sdk_rs` is in the filter on purpose: reconnects and subscription
    // restores are logged by the SDK, and filtering to this crate alone hides
    // exactly the lines that explain a misbehaving plugin.
    plugin_sdk_rs::logging::init_logging(
        config_path,
        "hc-nuheat",
        "hc_nuheat=info,plugin_sdk_rs=info",
        &bootstrap.logging,
    )
}

/// Where the device snapshot lives: beside the config file core handed us.
fn snapshot_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join(".published-device-ids.json")
}

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    // Remember what was registered, across restarts. Without it, reconcile can
    // only see devices registered in *this* process, so a thermostat removed
    // from the account while the plugin was down would linger in homeCore
    // forever — visible, and accepting commands nothing executes.
    .with_device_persistence(snapshot_path(config_path));

    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );

    let publisher = client.device_publisher();
    let notices = client.notices();
    let state_writer = client.state_writer();

    let (api, http) = api::NuHeatApi::new()?;
    let auth = Arc::new(auth::Auth::new(
        cfg.nuheat.auth.mode,
        cfg.nuheat.auth.client_id.clone(),
        cfg.nuheat.auth.client_secret.clone(),
        cfg.nuheat.auth.redirect_uri.clone(),
        http,
        state_writer,
    ));

    let rt = Arc::new(Runtime::new(
        api.clone(),
        Arc::clone(&auth),
        publisher,
        notices.clone(),
        &cfg.nuheat,
    ));

    // Lets the link action and the `refresh` button cut a poll interval short.
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(4);

    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities());

    // Tokens live in core's durable learned state, so a restart resumes without
    // asking the operator to sign in again. This handler fires on connect with
    // the retained document and on every later change.
    let mgmt = {
        let auth = Arc::clone(&auth);
        mgmt.with_state_handler(move |doc| auth.adopt_persisted_state(&doc))
    };

    let mgmt = {
        let rt = Arc::clone(&rt);
        let wake = wake_tx.clone();
        mgmt.with_custom_handler(move |cmd| match cmd["action"].as_str()? {
            "refresh" => {
                let _ = wake.try_send(());
                Some(json!({ "status": "ok", "message": "refreshing" }))
            }
            "status" => Some(json!({ "status": "ok", "result": rt.status() })),
            _ => None,
        })
    };

    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());

    let mgmt = link::register_actions(
        mgmt,
        link::LinkHandle {
            auth: Arc::clone(&auth),
            api,
            notices,
            wake: wake_tx.clone(),
        },
    );

    // Commands arrive on a synchronous SDK callback; hand them to this channel
    // so cloud I/O never stalls the MQTT event loop.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(64);

    // ── Start the event loop BEFORE registering anything ──────────────────
    //
    // `run_managed` is what drives the MQTT connection: until it is polling,
    // nothing published leaves the process, it only queues, and the queue holds
    // 64 messages. Registering one device costs four of them, so registering
    // first works with three thermostats and hangs at startup with seventeen —
    // no error, no log line, just a stop.
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    // try_send, not send: dropping a command under load beats
                    // blocking the event loop behind it.
                    if cmd_tx.try_send((device_id.clone(), payload)).is_err() {
                        warn!(device_id = %device_id, "Command queue full, dropped");
                    }
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited");
        }
        // Dropping `cmd_tx` closes the command loop below, so the process exits
        // and core restarts it — rather than sitting there looking healthy with
        // no connection behind it.
    });

    // Let the connection establish, and give the retained learned-state
    // document a moment to arrive, so the first poll has the token a previous
    // run persisted instead of raising a spurious "not signed in".
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── The poll loop ─────────────────────────────────────────────────────
    let poll_interval = Duration::from_secs(cfg.nuheat.poll_interval_secs.max(30));
    {
        let rt = Arc::clone(&rt);
        tokio::spawn(async move {
            loop {
                rt.poll().await;
                // Wake early when someone links an account or presses Refresh,
                // rather than making them wait out the interval.
                tokio::select! {
                    _ = tokio::time::sleep(poll_interval) => {}
                    _ = wake_rx.recv() => {}
                }
            }
        });
    }

    // ── Commands ──────────────────────────────────────────────────────────
    //
    // Sequential on purpose. Two holds racing on the same thermostat would
    // produce a read-back that reflects whichever landed second, and NuHeat's
    // rate limits are generous enough that queueing costs nothing.
    while let Some((device_id, payload)) = cmd_rx.recv().await {
        rt.apply(&device_id, &payload).await;
    }
    Ok(())
}

/// The action manifest: each entry becomes a button on the plugin's page and a
/// call hc-mcp can make, with no UI code at either end.
fn capabilities() -> Capabilities {
    Capabilities {
        spec: "1".into(),
        // Left empty: the SDK fills in the id it connected with.
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "link_account".into(),
                label: "Link NuHeat account".into(),
                description: Some(
                    "Sign in to NuHeat and let this plugin read and control your thermostats."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "linked": { "type": "boolean" },
                    "account": { "type": "string" },
                    "expires_in_secs": { "type": "integer" },
                })),
                stream: true,
                cancelable: true,
                // One sign-in at a time: two concurrent flows would race to
                // persist different tokens.
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                // Generous, because the whole middle of this action is a person
                // signing in to a website in another tab.
                timeout_ms: Some(300_000),
            },
            Action {
                id: "sign_out".into(),
                label: "Sign out".into(),
                description: Some(
                    "Forget the stored NuHeat credentials. Devices stay registered.".into(),
                ),
                params: None,
                result: Some(json!({ "signed_out": { "type": "boolean" } })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(30_000),
            },
            Action {
                id: "refresh".into(),
                label: "Refresh now".into(),
                description: Some("Poll NuHeat immediately instead of waiting.".into()),
                params: None,
                result: Some(json!({ "message": { "type": "string" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "status".into(),
                label: "Show status".into(),
                description: Some(
                    "Whether the plugin is signed in, how long the token has left, and how \
                     many thermostats it has found."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "linked": { "type": "boolean" },
                    "auth_mode": { "type": "string" },
                    "thermostats": { "type": "integer" },
                })),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A declared action nothing handles is a button that does nothing. The
    /// streaming pair are registered in `link.rs`; the two synchronous ones are
    /// handled by the custom handler in `try_start`, and this pins the list the
    /// two halves have to agree on.
    #[test]
    fn every_declared_action_is_handled_somewhere() {
        let declared: Vec<String> = capabilities()
            .actions
            .iter()
            .map(|a| a.id.clone())
            .collect();
        assert_eq!(
            declared,
            vec!["link_account", "sign_out", "refresh", "status"]
        );
        for action in capabilities().actions {
            let handled_by_stream = matches!(action.id.as_str(), "link_account" | "sign_out");
            assert_eq!(
                action.stream, handled_by_stream,
                "{} is declared stream={} but handled the other way",
                action.id, action.stream
            );
        }
    }

    /// Signing in and out changes stored credentials, so neither belongs to an
    /// ordinary user; polling sooner is harmless.
    #[test]
    fn credential_actions_require_an_admin() {
        for action in capabilities().actions {
            let expected = match action.id.as_str() {
                "link_account" | "sign_out" => RequiresRole::Admin,
                _ => RequiresRole::User,
            };
            assert_eq!(action.requires_role, expected, "{}", action.id);
        }
    }

    #[test]
    fn the_snapshot_sits_beside_the_config_core_handed_us() {
        assert_eq!(
            snapshot_path("/var/lib/homecore/config/plugins/plugin.nuheat.toml"),
            PathBuf::from("/var/lib/homecore/config/plugins/.published-device-ids.json")
        );
    }
}
