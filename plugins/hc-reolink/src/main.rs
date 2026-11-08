//! hc-reolink — Reolink cameras, NVRs and Home Hubs in homeCore.
//!
//! Each configured address is signed in to once and watched (`host`): its
//! cameras become `camera` devices reporting what they see in homeCore's
//! shared names, with a spotlight (`light`) and siren (`siren`) where they
//! have them; a detection that starts takes a snapshot, kept in the plugin's
//! files folder for its screens; PTZ, presets and infrared are actions and
//! writes. Live video is go2rtc's: the plugin adds each camera's streams to
//! it (`go2rtc`), and the app plays them through its own server.
//!
//! Detections are asked for every few seconds (polling). Reolink's push on
//! port 9000 comes next, with polling kept behind it.

mod api;
mod camera;
mod config;
mod go2rtc;
mod host;

use anyhow::Result;
use plugin_sdk_rs::types::{AlertSeverity, PluginAlertKind, PluginNotice};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::Config;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 30;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());
    let (_log_guard, log_level, mqtt_logs) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        match run(&cfg, &config_path, log_level.clone(), mqtt_logs.clone()).await {
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
    plugin_sdk_rs::logging::init_logging(
        config_path,
        "hc-reolink",
        "hc_reolink=info,plugin_sdk_rs=info",
        &bootstrap.logging,
    )
}

fn beside_config(config_path: &str, name: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join(name)
}

async fn run(
    cfg: &Config,
    config_path: &str,
    log_level: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_logs: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    .with_device_persistence(beside_config(config_path, ".published-device-ids.json"));
    mqtt_logs.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let notices = client.notices();

    // Snapshots live here, served to the plugin's screens by name.
    let files_dir = beside_config(config_path, "reolink-files");

    let go2rtc = if cfg.reolink.go2rtc.url.trim().is_empty() {
        None
    } else {
        Some(go2rtc::Go2rtc::new(cfg.reolink.go2rtc.url.trim())?)
    };
    let ctx = Arc::new(host::Ctx {
        publisher: publisher.clone(),
        cfg: cfg.reolink.clone(),
        files_dir: files_dir.clone(),
        go2rtc,
        targets: Mutex::new(HashMap::new()),
        alerts: Mutex::new(HashMap::new()),
        status: Mutex::new(HashMap::new()),
    });

    let ctx_for_status = ctx.clone();
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level),
        )
        .await?
        .with_capabilities(capabilities())
        .with_alert_kinds(alert_kinds())
        .with_files_dir(files_dir)
        .with_custom_handler(move |cmd| match cmd["action"].as_str()? {
            "status" => Some(json!({
                "status": "ok",
                "hosts": ctx_for_status.status.lock().ok()?.clone(),
            })),
            _ => None,
        });
    let mgmt = match config::config_schema() {
        Some(schema) => mgmt.with_config_schema(schema),
        None => mgmt,
    };
    let mgmt = mgmt.with_config_descriptor(config::config_descriptor());
    let mgmt = match plugin_sdk_rs::ui::find_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(dir) => mgmt.with_ui_dir(dir, env!("CARGO_PKG_VERSION")),
        None => mgmt,
    };

    let (tx, mut rx) = mpsc::channel::<(String, Value)>(64);
    // The event loop first: see the template for why the order matters.
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    if tx.try_send((device_id.clone(), payload)).is_err() {
                        warn!(device_id = %device_id, "command queue full, dropped");
                    }
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited");
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    if cfg.reolink.hosts.is_empty() {
        notices.raise(
            PluginNotice::warning(
                "no_cameras_configured",
                "No cameras or NVRs are configured, so this plugin watches nothing.",
            )
            .with_remedy(
                "Add a camera or NVR under Cameras and NVRs: its address, user name and password.",
            ),
        );
    } else {
        notices.clear("no_cameras_configured");
    }
    if ctx.go2rtc.is_none() {
        notices.raise(
            PluginNotice::info(
                "no_live_video",
                "No go2rtc address is set, so the cameras have snapshots but no live video.",
            )
            .with_remedy("Set go2rtc's address under Live video."),
        );
    } else {
        notices.clear("no_live_video");
        if let Some(g) = &ctx.go2rtc {
            let up = g.alive().await;
            ctx.alert(
                "go2rtc_unreachable",
                "",
                (!up).then(|| "go2rtc isn't answering, so there's no live video.".to_string()),
            )
            .await;
        }
    }

    let mut hosts = Vec::new();
    for entry in &cfg.reolink.hosts {
        match host::Host::new(entry.clone()) {
            Ok(h) => {
                hosts.push(h.clone());
                tokio::spawn(host::run(ctx.clone(), h));
            }
            Err(e) => warn!(host = %entry.id, error = %e, "can't set up"),
        }
    }
    info!(hosts = cfg.reolink.hosts.len(), "watching");

    // Stopped or restarted by homeCore: give each session back. A device
    // allows only a few, and one left behind holds its place for an hour.
    tokio::spawn(async move {
        stopping().await;
        for h in &hosts {
            h.api.logout().await;
        }
        info!("signed out; stopping");
        std::process::exit(0);
    });

    // Commands, each on its own task: a siren's few seconds don't hold the rest.
    while let Some((device_id, payload)) = rx.recv().await {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = host::command(&ctx, &device_id, &payload).await {
                warn!(device_id = %device_id, error = %e, "command failed");
            }
        });
    }
    Ok(())
}

/// homeCore stopping the plugin (SIGTERM), or Ctrl-C by hand.
async fn stopping() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// The alerts this plugin can raise, about its addresses and go2rtc.
fn alert_kinds() -> Vec<PluginAlertKind> {
    vec![
        PluginAlertKind {
            topic: "host_unreachable".into(),
            label: "A camera or NVR isn't answering".into(),
            description: Some("Its cameras are offline until it answers again.".into()),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: "login_refused".into(),
            label: "A camera or NVR refused the sign-in".into(),
            description: Some(
                "The user name or password is wrong, or it's locked for a few minutes after too many tries.".into(),
            ),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
        PluginAlertKind {
            topic: "go2rtc_unreachable".into(),
            label: "go2rtc isn't answering".into(),
            description: Some("Cameras still report what they see; there's no live video.".into()),
            severity: AlertSeverity::Warning,
            default_on: true,
        },
    ]
}

/// The plugin's own actions: what each address says now, for its screens.
fn capabilities() -> plugin_sdk_rs::types::Capabilities {
    use plugin_sdk_rs::types::{Action, Capabilities, Concurrency, RequiresRole};
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![Action {
            id: "status".into(),
            label: "Cameras and NVRs".into(),
            description: Some("Each address: signed in or not, its model, its cameras.".into()),
            params: None,
            result: Some(json!({ "hosts": { "type": "object" } })),
            stream: false,
            cancelable: false,
            concurrency: Concurrency::default(),
            item_key: None,
            item_operations: None,
            requires_role: RequiresRole::ReadOnly,
            timeout_ms: Some(5_000),
            danger: false,
        }],
    }
}

#[cfg(test)]
mod conformance_tests {
    #[test]
    fn the_declaration_follows_the_rules() {
        plugin_sdk_rs::conformance::check_all(
            &super::camera::spotlight_schema(),
            &super::capabilities(),
        )
        .assert_ok();
    }

    #[test]
    fn every_action_is_both_advertised_and_routed() {
        plugin_sdk_rs::conformance::check_actions_routed(&super::capabilities(), &["status"])
            .assert_ok();
    }
}
