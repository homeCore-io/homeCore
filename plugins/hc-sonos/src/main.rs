mod actions;
mod alarms;
mod api;
mod bridge;
mod config;
mod discovery;
mod discovery_action;
mod events;
mod grouping;
mod roster;
mod shared_state;
mod speaker;
mod subscription;
mod topology;

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

    // Speakers are never removed because they went quiet, so there is no
    // device-set reconciliation (the SDK's `with_device_persistence` +
    // `reconcile_devices`): it deleted every speaker that was asleep when the
    // plugin started. Identity lives in the plugin's own roster instead.
    let client = PluginClient::connect(sdk_config).await?;
    let roster_path = roster::path_for(config_path, &cfg.homecore.plugin_id);
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
    // A rediscovery somebody asked for declares known speakers again.
    let redeclare: bridge::Redeclare = Default::default();
    let redeclare_for_rescan = redeclare.clone();
    let redeclare_for_stream = redeclare.clone();

    // ── Discovery channel ─────────────────────────────────────────────────
    // Created before management wiring so the streaming
    // `discover_speakers` action can hold a clone of the sender and
    // forward newly-found speakers into the bridge's integration path.
    let (discovery_tx, discovery_rx) = mpsc::channel::<sonor::Speaker>(32);
    let discovery_tx_for_action = discovery_tx.clone();
    let cfg_for_action = cfg.clone();

    // ── The plugin's actions that need the bridge ─────────────────────────
    let (req_tx, req_rx) = mpsc::channel::<bridge::Request>(8);
    let status_board: bridge::StatusBoard = Default::default();
    let status_for_action = status_board.clone();
    // When discovery last swept, for the plugin page's "found … ago".
    let last_sweep = discovery::LastSweep::default();
    let status_sweep = last_sweep.clone();
    let ssdp = cfg.sonos.ssdp;
    let manual_count = cfg.sonos.manual_hosts.len();

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
                    mark(&redeclare_for_rescan);
                    let _ = rescan_tx.try_send(());
                    Some(serde_json::json!({ "status": "ok" }))
                }
                "status" => {
                    let mut v = status_for_action
                        .lock()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    if !v.is_object() {
                        v = serde_json::json!({});
                    }
                    v["status"] = "ok".into();
                    v["last_sweep"] = serde_json::json!(status_sweep.get());
                    v["ssdp"] = ssdp.into();
                    v["manual_hosts"] = manual_count.into();
                    Some(v)
                }
                "group_speakers" => Some(group_speakers(&req_tx, cmd)),
                "forget_speaker" => Some(match cmd["device_id"].as_str() {
                    Some(id) if !id.is_empty() => {
                        let device_id = id.to_string();
                        ask(&req_tx, |reply| bridge::Request::Forget { device_id, reply })
                    }
                    _ => serde_json::json!({ "status": "error", "error": "forget_speaker needs 'device_id'" }),
                }),
                "list_alarms" => Some(ask(&req_tx, |reply| bridge::Request::ListAlarms { reply })),
                "set_alarm" => Some(match (cmd["id"].as_str().map(str::to_string).or_else(|| cmd["id"].as_u64().map(|n| n.to_string())), cmd["enabled"].as_bool()) {
                    (Some(id), Some(enabled)) => ask(&req_tx, |reply| bridge::Request::SetAlarm { id, enabled, reply }),
                    _ => serde_json::json!({ "status": "error", "error": "set_alarm needs 'id' and 'enabled'" }),
                }),
                _ => None,
            })
            .with_streaming_action(plugin_sdk_rs::StreamingAction::new(
                "discover_speakers",
                move |ctx, _params| {
                    mark(&redeclare_for_stream);
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

    // Its own screens: the built `ui/` folder it carries, served from here.
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION")),
        None => mgmt,
    };

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
    // With SSDP on, the addresses speakers were last seen at are worth a look
    // too — for one SSDP misses. With it off, only the listed hosts are ever
    // contacted.
    let mut probe_hosts = cfg.sonos.manual_hosts.clone();
    if cfg.sonos.ssdp {
        for h in bridge::Bridge::last_known_hosts(&roster_path) {
            if !probe_hosts.contains(&h) {
                probe_hosts.push(h);
            }
        }
    }
    discovery::spawn(
        last_sweep,
        Duration::from_secs(cfg.sonos.discovery_interval_secs),
        Duration::from_secs(cfg.sonos.discovery_timeout_secs),
        cfg.sonos.ssdp,
        probe_hosts,
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
    let bridge = bridge::Bridge::new(
        cfg,
        publisher,
        app_state,
        notices,
        roster_path,
        redeclare,
        status_board,
    );
    bridge.run(discovery_rx, cmd_rx, event_rx, req_rx).await;

    Ok(())
}

/// `group_speakers`: hand the request to the bridge and wait for its answer.
///
/// The SDK's custom handler is synchronous and runs on a task of its own, so
/// the wait happens on a plain thread — a `blocking_recv` on a runtime thread
/// would panic. The bridge answers within a few seconds (each speaker gets
/// [`grouping::STEP_TIMEOUT`]), well inside the action's `timeout_ms`.
fn group_speakers(
    tx: &mpsc::Sender<bridge::Request>,
    cmd: &serde_json::Value,
) -> serde_json::Value {
    let Some(leader) = cmd["leader"].as_str().map(str::to_string) else {
        return serde_json::json!({ "status": "error", "error": "group_speakers needs 'leader' (a device id)" });
    };
    let members = bridge::members_of(cmd);
    ask(tx, |reply| bridge::Request::Group {
        leader,
        members,
        reply,
    })
}

/// Hand a request to the bridge and wait for its answer, on a thread of its
/// own (see [`group_speakers`]).
fn ask(
    tx: &mpsc::Sender<bridge::Request>,
    make: impl FnOnce(tokio::sync::oneshot::Sender<serde_json::Value>) -> bridge::Request,
) -> serde_json::Value {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if tx.try_send(make(reply_tx)).is_err() {
        return serde_json::json!({ "status": "error", "error": "the plugin is busy; try again" });
    }
    std::thread::spawn(move || reply_rx.blocking_recv())
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| serde_json::json!({ "status": "error", "error": "the plugin stopped before answering" }))
}

/// Note that somebody asked for a rediscovery, now.
fn mark(r: &bridge::Redeclare) {
    if let Ok(mut g) = r.lock() {
        *g = Some(std::time::Instant::now());
    }
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
                danger: false,
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
                danger: false,
            },
            Action {
                id: "group_speakers".into(),
                label: "Play together".into(),
                description: Some(
                    "Make one speaker's group exactly the speakers listed: they \
                     join it, the others in its group leave. Answers speaker by \
                     speaker — joined, left, already, or no answer. `leader` and \
                     `members` are homeCore device ids; no members plays the \
                     leader on its own. The same as the `group` device action, \
                     with the answers."
                        .into(),
                ),
                params: Some(serde_json::json!({
                    "leader": { "type": "string" },
                    "members": { "type": "array", "items": { "type": "string" } },
                })),
                result: Some(serde_json::json!({
                    "leader": { "type": "string" },
                    "results": { "type": "array" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(30_000),
                danger: false,
            },
            Action {
                id: "forget_speaker".into(),
                label: "Forget a speaker".into(),
                description: Some(
                    "Remove a speaker that is gone for good — sold, broken, bonded into \
                     another — from homeCore, with its room and its place in automations. \
                     A speaker that is only switched off or asleep stays, shown offline; \
                     this is the only way one is ever removed. Refused while the speaker \
                     answers, since it would be found and added again."
                        .into(),
                ),
                params: Some(serde_json::json!({
                    "device_id": { "type": "string", "description": "The speaker's device id" },
                })),
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(10_000),
                danger: true,
            },
            Action {
                id: "list_alarms".into(),
                label: "Alarms".into(),
                description: Some(
                    "The household's Sonos alarms: when, which days, which speaker, what it \
                     plays and whether it is on."
                        .into(),
                ),
                params: None,
                result: Some(serde_json::json!({ "alarms": { "type": "array" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::ReadOnly,
                timeout_ms: Some(10_000),
                danger: false,
            },
            Action {
                id: "set_alarm".into(),
                label: "Switch an alarm on or off".into(),
                description: Some(
                    "Turn one Sonos alarm on or off, leaving the rest of it as it is — \
                     to skip tomorrow's alarm on a holiday, say."
                        .into(),
                ),
                params: Some(serde_json::json!({
                    "id": { "type": "string", "description": "The alarm's id, from list_alarms" },
                    "enabled": { "type": "boolean" },
                })),
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: Some(10_000),
                danger: false,
            },
            Action {
                id: "status".into(),
                label: "Speakers found".into(),
                description: Some(
                    "How many speakers are known and how many answer, which are \
                     offline, when discovery last looked, and whether it uses SSDP — for \
                     the plugin's page."
                        .into(),
                ),
                params: None,
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::ReadOnly,
                timeout_ms: Some(5_000),
                danger: false,
            },
        ],
    }
}
