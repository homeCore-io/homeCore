//! hc-thermostat — software thermostats for homeCore: a temperature sensor
//! switches a plug or switch to hold a room at a temperature.
//!
//! # Startup, in order
//!
//! 1. Read the config — every thermostat that can run; the rest are said.
//! 2. Connect; start the SDK event loop.
//! 3. Subscribe to every sensor's and switch's state **and availability**, and
//!    to our own retained state from the last run.
//! 4. Wait (briefly) for learned state, then for every subscribed device to
//!    speak. Until then the controller only listens.
//! 5. Start: register every thermostat, then one settling pass.
//!
//! On SIGTERM or Ctrl-C every heater this plugin may have left running is
//! turned off before it exits.

use anyhow::Result;
use plugin_sdk_rs::{AsyncActionFuture, PluginClient, PluginConfig};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

mod bridge;
mod config;
mod control;
mod controller;
mod schema;

use bridge::{Bridge, Event, Mgmt};
use controller::Controller;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;
/// How long to wait for the broker to say anything at all (a slow connect —
/// homeCore itself just started), then for learned state, then for every
/// subscribed device to speak.
const CONNECT_WAIT: Duration = Duration::from_secs(30);
const LEARNED_WAIT: Duration = Duration::from_secs(2);
const SYNC_WAIT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    // `hc-thermostat --config PATH` or `hc-thermostat PATH`.
    let config_path = parse_flag("--config")
        .or_else(|| std::env::args().nth(1).filter(|a| !a.starts_with("--")))
        .unwrap_or_else(|| "config/config.toml".to_string());

    // `[logging]` first, so a config problem is logged to the file too.
    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging_bootstrap(&config_path);

    let loaded = match bridge::load(&config_path) {
        Ok(l) => l,
        Err(e) => {
            // Only a file that isn't TOML, or a `[homecore]` that can't be read:
            // without them it can't reach homeCore to say anything.
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    info!(
        version = VERSION,
        plugin_id = %loaded.config.homecore.plugin_id,
        thermostats = loaded.config.thermostats.len(),
        skipped = loaded.skipped.len(),
        "Starting hc-thermostat"
    );

    for attempt in 1..=MAX_ATTEMPTS {
        match run(
            loaded.clone(),
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

fn init_logging_bootstrap(
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
        "hc-thermostat",
        "hc_thermostat=info",
        &bootstrap.logging,
    )
}

fn parse_flag(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

async fn run(
    loaded: config::Loaded,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let cfg = loaded.config.clone();
    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    // Cross-restart device tracking, so a thermostat removed while the plugin
    // was stopped is unregistered at start.
    .with_device_persistence(sibling(config_path, ".published-device-ids.json"));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );

    let (tx, rx) = mpsc::unbounded_channel::<Event>();
    let learned_seen = Arc::new(AtomicBool::new(false));
    // Anything at all from the broker: the waits below count from here, so a
    // slow connect can't make the plugin start before it has heard anything.
    let heard_any = Arc::new(AtomicBool::new(false));

    let mgmt = client
        .enable_management(
            cfg.homecore.heartbeat_secs,
            Some(VERSION.to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities())
        .with_alert_kinds(alert_kinds())
        .with_async_handler({
            let tx = tx.clone();
            move |cmd| {
                let m = Mgmt::of(cmd)?;
                let tx = tx.clone();
                Some(Box::pin(async move {
                    let m = match m {
                        Ok(m) => m,
                        Err(e) => return json!({ "status": "error", "error": e }),
                    };
                    let (rtx, rrx) = oneshot::channel();
                    if tx.send(Event::Mgmt(m, rtx)).is_err() {
                        return json!({ "status": "error", "error": "the plugin is stopping" });
                    }
                    rrx.await.unwrap_or_else(
                        |_| json!({ "status": "error", "error": "the plugin is stopping" }),
                    )
                }) as AsyncActionFuture)
            }
        })
        .with_state_handler({
            let tx = tx.clone();
            let seen = Arc::clone(&learned_seen);
            let any = Arc::clone(&heard_any);
            move |doc| {
                seen.store(true, Ordering::SeqCst);
                any.store(true, Ordering::SeqCst);
                let _ = tx.send(Event::Learned(doc));
            }
        })
        .with_availability_handler({
            let tx = tx.clone();
            let any = Arc::clone(&heard_any);
            move |id, online| {
                any.store(true, Ordering::SeqCst);
                let _ = tx.send(Event::Availability(id, online));
            }
        })
        .with_config_descriptor(config::config_descriptor());
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    // Its own screens: the built `ui/` it carries.
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, VERSION),
        None => mgmt,
    };

    let publisher = client.device_publisher();
    let mut bridge = Bridge::new(
        Controller::new(loaded),
        publisher,
        client.mqtt_client(),
        client.state_writer(),
        client.notices(),
        config_path,
    );

    // The event loop first: until it polls, publishes only queue.
    let cmd_tx = tx.clone();
    let state_tx = tx.clone();
    let any = Arc::clone(&heard_any);
    let run_handle = tokio::spawn(async move {
        if let Err(e) = client
            .run_managed_with_state(
                move |id, payload| {
                    let _ = cmd_tx.send(Event::Command(id, payload));
                },
                move |id, payload| {
                    any.store(true, Ordering::SeqCst);
                    let _ = state_tx.send(Event::State(id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "PluginClient run loop exited");
        }
    });

    bridge.subscribe_initial().await;
    let actor = tokio::spawn(bridge.run(rx));

    let t0 = tokio::time::Instant::now();
    while !heard_any.load(Ordering::SeqCst) && t0.elapsed() < CONNECT_WAIT {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Learned state: retained, so it arrives shortly after connect — or not
    // at all for a plugin with nothing kept.
    let tl = tokio::time::Instant::now();
    while !learned_seen.load(Ordering::SeqCst) && tl.elapsed() < LEARNED_WAIT {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Every subscribed device's retained state, or the timeout.
    let t1 = tokio::time::Instant::now();
    let missed = loop {
        let (rtx, rrx) = oneshot::channel();
        let _ = tx.send(Event::SyncRemaining(rtx));
        let left = rrx.await.unwrap_or_default();
        if left.is_empty() || t1.elapsed() >= SYNC_WAIT {
            break left;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if !missed.is_empty() {
        warn!(missing = ?missed, "Not every device said anything before start");
    }
    info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "Initial state heard; starting"
    );
    let _ = tx.send(Event::Start);

    stopping().await;
    info!("Stopping");
    let (rtx, rrx) = oneshot::channel();
    let _ = tx.send(Event::Shutdown(rtx));
    // Within homeCore's grace (2 s) before it kills a plugin that was asked.
    let _ = tokio::time::timeout(Duration::from_secs(1), rrx).await;
    // Give the event loop a moment to put the last commands on the wire.
    tokio::time::sleep(Duration::from_millis(500)).await;
    actor.abort();
    run_handle.abort();
    Ok(())
}

/// SIGTERM (homeCore stopping or restarting it) or Ctrl-C.
async fn stopping() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "No SIGTERM handler; Ctrl-C only");
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A file beside the config.
fn sibling(config_path: &str, name: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(name)
}

/// The alerts this plugin raises. "Do something…" on any of them makes an
/// automation that starts on it.
fn alert_kinds() -> Vec<plugin_sdk_rs::types::PluginAlertKind> {
    use plugin_sdk_rs::types::{AlertSeverity, PluginAlertKind};
    let kind = |topic: &str, label: &str, desc: &str, severity, default_on| PluginAlertKind {
        topic: topic.into(),
        label: label.into(),
        description: Some(desc.into()),
        severity,
        default_on,
    };
    vec![
        kind(
            "sensors_stale",
            "Thermostat has no current reading",
            "None of its sensors has reported within its quiet time (2 h unless changed), or \
             they are offline. Its switch is turned off (unless it is set to leave it) until \
             a reading comes back.",
            AlertSeverity::Warning,
            true,
        ),
        kind(
            "actuator_not_responding",
            "Thermostat's switch isn't answering",
            "The plug or switch didn't confirm an on or off within 30 s, or is offline. It is \
             asked again, less often each time; the alert clears when it confirms.",
            AlertSeverity::Warning,
            true,
        ),
        kind(
            "max_run_exceeded",
            "Thermostat ran too long",
            "It ran its longest run without a break (4 h for a heater unless changed) and was \
             turned off. It starts again once the room reaches its target, or when someone \
             starts it again.",
            AlertSeverity::Warning,
            true,
        ),
        kind(
            "manual_override",
            "Thermostat's switch was used by hand",
            "Someone switched it by hand, so the thermostat leaves it alone for a while (1 h \
             unless changed) or until the target or mode changes. News, not a problem: off \
             unless you turn it on.",
            AlertSeverity::Info,
            false,
        ),
        kind(
            "config_entry_skipped",
            "A thermostat in Settings isn't running",
            "One [[thermostat]] can't be used as written — a missing field, a target that looks \
             like °F, a heater set to cool. The others run; the plugin page says what to fix.",
            AlertSeverity::Warning,
            true,
        ),
    ]
}

/// The plugin's own actions. Per-thermostat control is on the device
/// (`set_setpoint`, `hold`, `resume`, … — see `schema.rs`).
fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    let action = |id: &str,
                  label: &str,
                  desc: &str,
                  params: Option<serde_json::Value>,
                  role: RequiresRole,
                  danger: bool| Action {
        id: id.into(),
        label: label.into(),
        description: Some(desc.into()),
        params,
        result: None,
        stream: false,
        cancelable: false,
        concurrency: Concurrency::default(),
        item_key: None,
        item_operations: None,
        requires_role: role,
        timeout_ms: Some(10_000),
        danger,
    };
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            action(
                "status",
                "Status",
                "The thermostats that run and the ones in Settings that can't, with why and \
                 the one-press fix where there is one.",
                None,
                RequiresRole::ReadOnly,
                false,
            ),
            action(
                "get_thermostats",
                "List thermostats",
                "Every running thermostat's settings, as the file has them plus changes made \
                 since.",
                None,
                RequiresRole::ReadOnly,
                false,
            ),
            action(
                "recalculate_all",
                "Recalculate all",
                "Decide every thermostat again now. With `force`, every switch is sent what \
                 it should be even if it seems so already — for a switch that drifted.",
                Some(json!({
                    "type": "object",
                    "properties": { "force": { "type": "boolean", "default": false } }
                })),
                RequiresRole::User,
                false,
            ),
            action(
                "add_thermostat",
                "Add a thermostat",
                "Adds one to Settings and starts it. `config`: name, kind (ac | heater | \
                 fan), sensor_device_ids, setpoint (°C), actuator_device_id; the short-cycle \
                 guards and the rest default for its kind.",
                Some(json!({
                    "type": "object",
                    "properties": { "config": { "type": "object" } },
                    "required": ["config"]
                })),
                RequiresRole::Admin,
                false,
            ),
            action(
                "remove_thermostat",
                "Remove a thermostat",
                "Turns its switch off if it was running it, then removes it from Settings \
                 and its device from homeCore (with its place in automations).",
                Some(json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                })),
                RequiresRole::Admin,
                true,
            ),
            action(
                "fix_entry",
                "Fix a thermostat's target",
                "Sets the target of a thermostat in Settings that isn't running — `index` \
                 (its place in the list, from 0), `id` when it has one, `setpoint` in °C. The \
                 plugin restarts with it.",
                Some(json!({
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer" },
                        "id": { "type": "string" },
                        "setpoint": { "type": "number" }
                    },
                    "required": ["index", "setpoint"]
                })),
                RequiresRole::Admin,
                false,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_conformant_and_routed() {
        let caps = capabilities();
        plugin_sdk_rs::conformance::check_manifest(&caps).assert_ok();
        plugin_sdk_rs::conformance::check_actions_routed(&caps, bridge::MGMT_ACTIONS).assert_ok();
        for a in bridge::MGMT_ACTIONS {
            assert!(
                caps.actions.iter().any(|x| x.id == *a),
                "{a} not advertised"
            );
        }
    }

    #[test]
    fn every_alert_kind_is_one_the_controller_raises() {
        let src = include_str!("controller.rs");
        for k in alert_kinds() {
            assert!(src.contains(&format!("\"{}\"", k.topic)), "{}", k.topic);
        }
    }

    #[cfg(feature = "schema")]
    #[test]
    fn capability_manifest_fits_in_one_mqtt_packet() {
        let mut manifest = serde_json::to_value(capabilities()).unwrap();
        manifest["config_schema"] = config::config_schema().unwrap();
        manifest["config_descriptor"] = config::config_descriptor();
        assert!(serde_json::to_vec(&manifest).unwrap().len() < 1024 * 1024);
    }
}
