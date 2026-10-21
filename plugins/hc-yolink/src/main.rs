mod actions;
mod auth;
mod bridge;
mod config;
mod devices;
mod schema;
mod state;
mod yolink;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::{Config, Endpoints, Mode};
use yolink::{api::YolinkApi, mqtt::YolinkMqtt};

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

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

    if let Err(e) = run(&cfg, &config_path, log_level_handle, mqtt_log_handle).await {
        error!(error = %e, "hc-yolink stopped");
        std::process::exit(1);
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
        "hc-yolink",
        "hc_yolink=info",
        &bootstrap.logging,
    )
}

/// Everything, in order. **The SDK connection and the event feed are made
/// once**; only talking to YoLink is retried. A failed start used to leave
/// the SDK task and the feed task running while the next attempt made new
/// ones — two MQTT clients under one id, knocking each other off the broker.
async fn run(
    cfg: &Config,
    config_path: &str,
    log_level_handle: plugin_sdk_rs::logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- homeCore (via SDK) — first, so its settings form is served even
    // before YoLink credentials exist.
    let client = PluginClient::connect(PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    })
    .await?
    .with_device_persistence(sibling(config_path, ".published-device-ids.json"));
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let notices = client.notices();

    let configured = cfg.yolink.cloud.is_some() || cfg.yolink.local.is_some();
    let ep = if configured {
        Some(Endpoints::from_config(&cfg.yolink)?)
    } else {
        None
    };
    let tokens = ep.as_ref().map(|ep| {
        auth::TokenManager::new(
            ep.token_url.clone(),
            ep.client_id.clone(),
            ep.client_secret.clone(),
        )
    });
    let shared = match (&ep, &tokens) {
        (Some(ep), Some(tokens)) => Some(bridge::Shared::new(
            publisher.clone(),
            Arc::new(YolinkApi::new(ep.api_base_url.clone(), tokens.clone())),
            notices.clone(),
            bridge::Options {
                poll_interval: Duration::from_secs(cfg.yolink.poll_interval_secs.max(60)),
                inventory_interval: Duration::from_secs(
                    cfg.yolink
                        .inventory_interval_secs
                        .unwrap_or(cfg.yolink.poll_interval_secs)
                        .max(60),
                ),
                poll_device_delay: Duration::from_millis(cfg.yolink.poll_device_delay_ms),
                initial_fetch_delay: Duration::from_secs(cfg.yolink.initial_fetch_delay_secs),
                silent_after: chrono::Duration::minutes(cfg.yolink.silent_after_mins.max(5) as i64),
            },
            ep.mode.clone(),
            ep.mqtt_host.clone(),
            sibling(config_path, ".yolink-state.json"),
        )),
        _ => None,
    };

    let for_handler = shared.clone();
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(actions::manifest())
        .with_alert_kinds(actions::alert_kinds())
        .with_custom_handler(move |cmd| actions::handle(&for_handler, cmd));
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

    // The event loop first: until it runs, publishes only queue.
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(64);
    let for_commands = shared.clone();
    let sdk_task = tokio::spawn(async move {
        let result = client
            .run_managed(
                move |device_id, payload| match &for_commands {
                    Some(s) => bridge::enqueue(s, &cmd_tx, device_id, payload),
                    None => warn!(device_id, "A command, but YoLink is not configured"),
                },
                mgmt,
            )
            .await;
        if let Err(e) = result {
            error!(error = %e, "SDK event loop exited with error");
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (Some(s), Some(ep), Some(tokens)) = (shared, ep, tokens) else {
        warn!(
            "YoLink not configured yet — set cloud or local credentials, then restart. \
             Serving the settings form meanwhile."
        );
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "No YoLink hub or account is set, so this plugin has nothing to talk to.",
            )
            .with_remedy(
                "Under Configuration, choose Local hub (a YS1606: its address and the Local \
                 API Client ID, Secret and Network ID from the YoLink app) or YoLink cloud \
                 (a UAID and Secret Key).",
            ),
        );
        let _ = sdk_task.await;
        return Ok(());
    };

    if ep.client_id.trim().is_empty() || ep.client_secret.trim().is_empty() {
        notices.raise(
            PluginNotice::error(
                "not_configured",
                "The YoLink Client ID or Secret is empty, so this plugin cannot sign in.",
            )
            .with_remedy("Enter both under Configuration."),
        );
    }

    // --- YoLink: sign in, find the feed, read the device list. Retried.
    let mut prefix = None;
    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Connecting to YoLink");
        match start_yolink(&s, &tokens, &ep).await {
            Ok(p) => {
                s.notices.clear("hub_unreachable");
                prefix = Some(p);
                break;
            }
            Err(e) => {
                bridge::startup_failed(&s.notices, &e, attempt, MAX_ATTEMPTS);
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "YoLink startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    return Err(e.context(format!("YoLink startup failed {MAX_ATTEMPTS} times")));
                }
            }
        }
    }
    let prefix = prefix.expect("set on success");

    // --- The tasks, each made once.
    let (feed_tx, feed_rx) = mpsc::channel(256);
    let feed = YolinkMqtt::new(
        ep.mqtt_host.clone(),
        ep.mqtt_port,
        ep.mode.clone(),
        ep.client_id.clone(),
        ep.client_secret.clone(),
        tokens.clone(),
        s.notices.clone(),
    );
    tokio::spawn(feed.run(prefix, feed_tx));
    tokio::spawn(bridge::command_worker(s.clone(), cmd_rx));
    tokio::spawn(bridge::poll_loop(s.clone()));
    tokio::spawn(bridge::watch_loop(s.clone()));
    info!("hc-yolink running");
    bridge::feed_loop(s, feed_rx).await;
    Ok(())
}

/// Sign in, work out the feed's topic prefix, and read the device list.
async fn start_yolink(
    s: &Arc<bridge::Shared>,
    tokens: &Arc<auth::TokenManager>,
    ep: &Endpoints,
) -> Result<String> {
    tokens.init().await?;
    info!("YoLink authentication successful");
    let prefix = match ep.mode {
        Mode::Cloud => format!("yl-home/{}", s.api.get_home_id().await?),
        // The Net ID is a credential; it stays out of the info log.
        Mode::Local => format!("ylsubnet/{}", ep.net_id),
    };
    s.sync_inventory(false).await?;
    Ok(prefix)
}

/// A file beside the config.
fn sibling(config_path: &str, name: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}
