//! `http-poller` — generic HTTP polling adapter for HomeCore.
//!
//! Periodically fetches HTTP endpoints and publishes parsed responses as
//! HomeCore device states.  Each polled endpoint becomes a separate device.
//! All configuration lives in a TOML file — see `http-poller.example.toml`.
//!
//! ## Usage
//!
//! ```sh
//! cargo run -p http-poller -- --config http-poller.toml
//! ```
//!
//! Or via environment variable:
//! ```sh
//! HC_CONFIG=/etc/homecore/poller.toml http-poller
//! ```
//!
//! ## Response mapping
//!
//! Three modes, evaluated in this order:
//!
//! 1. **`transform`** — a Rhai script with `response` in scope; must evaluate
//!    to a map (`#{ ... }`).  Most expressive; supports arithmetic and conditionals.
//! 2. **`field_map`** — dot-notation paths into the JSON response
//!    (`temperature = "main.temp"`).  Simple and readable.
//! 3. **Raw passthrough** — no mapping configured; the full JSON response body
//!    becomes the device state.

mod config;
mod poller;

use anyhow::{Context, Result};
use config::AppConfig;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config_path = config_path_from_args();
    let cfg = AppConfig::load(&config_path)
        .with_context(|| format!("Failed to load config from '{config_path}'"))?;

    if cfg.pollers.is_empty() {
        anyhow::bail!("No [[poller]] sections in '{config_path}' — nothing to do");
    }

    // Validate all Rhai transform scripts at startup so syntax errors surface
    // immediately rather than on the first poll tick.
    validate_transforms(&cfg)?;

    info!(
        config     = %config_path,
        pollers    = cfg.pollers.len(),
        broker     = %cfg.plugin.broker_host,
        plugin_id  = %cfg.plugin.id,
        "http-poller starting",
    );

    let plugin_config = PluginConfig {
        plugin_id: cfg.plugin.id.clone(),
        broker_host: cfg.plugin.broker_host.clone(),
        broker_port: cfg.plugin.broker_port,
        password: cfg.plugin.password.clone(),
    };

    let client = PluginClient::connect(plugin_config).await?;

    // Register every device and start them offline.  The first successful poll
    // transitions each one to online.
    for p in &cfg.pollers {
        let name = &p.name;
        client
            .register_device(&p.device_id, name, p.capabilities.clone())
            .await?;
        client.set_available(&p.device_id, false).await?;
        info!(device_id = %p.device_id, url = %p.url, "Device registered");
    }

    // Grab a clonable publish handle before run() consumes the client.
    let publisher = client.device_publisher();

    // Spawn one independent polling task per device.
    for poller_cfg in cfg.pollers {
        let pub_clone = publisher.clone();
        tokio::spawn(poller::run_poller(poller_cfg, pub_clone));
    }

    info!("All pollers started — driving MQTT event loop (Ctrl-C to stop)");

    // Drive the MQTT event loop.  http-poller is write-only — it never receives
    // commands — so the handler is a no-op.
    client.run(|_, _| {}).await?;

    Ok(())
}

/// Determine config file path from CLI args or environment.
fn config_path_from_args() -> String {
    let args: Vec<String> = std::env::args().collect();
    // --config <path>
    if let Some(path) = arg_value(&args, "--config") {
        return path;
    }
    // HC_CONFIG env var
    if let Ok(path) = std::env::var("HC_CONFIG") {
        return path;
    }
    // Default
    "http-poller.toml".into()
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

/// Pre-compile every Rhai transform script.  Returns an error on the first
/// script that fails to parse so the operator sees all problems at startup.
fn validate_transforms(cfg: &AppConfig) -> Result<()> {
    let engine = rhai::Engine::new();
    for p in &cfg.pollers {
        if let Some(script) = &p.transform {
            engine.compile(script).map_err(|e| {
                anyhow::anyhow!(
                    "device '{}': transform script compile error — {e}",
                    p.device_id
                )
            })?;
        }
    }
    Ok(())
}
