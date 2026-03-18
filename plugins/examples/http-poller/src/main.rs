//! `http-poller` — generic HTTP polling adapter for HomeCore.
//!
//! Periodically fetches a JSON endpoint and publishes the response as a
//! device state update.  Useful for cloud APIs that don't support push
//! notifications (e.g. weather APIs, simple REST-based smart home devices).
//!
//! Configuration is provided via environment variables:
//! - `HC_POLL_URL`       — URL to fetch (required)
//! - `HC_DEVICE_ID`      — target device ID (required)
//! - `HC_POLL_INTERVAL`  — seconds between polls (default: 30)
//! - `HC_BROKER_HOST`    — broker host (default: 127.0.0.1)
//! - `HC_BROKER_PORT`    — broker port (default: 1883)

use anyhow::{Context, Result};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let poll_url = std::env::var("HC_POLL_URL")
        .context("HC_POLL_URL environment variable is required")?;
    let device_id = std::env::var("HC_DEVICE_ID")
        .context("HC_DEVICE_ID environment variable is required")?;
    let interval_secs: u64 = std::env::var("HC_POLL_INTERVAL")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .unwrap_or(30);

    let config = PluginConfig {
        plugin_id: "plugin.http-poller".into(),
        broker_host: std::env::var("HC_BROKER_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        broker_port: std::env::var("HC_BROKER_PORT")
            .unwrap_or_else(|_| "1883".into())
            .parse()
            .unwrap_or(1883),
        ..Default::default()
    };

    let client = PluginClient::connect(config).await?;

    client
        .register_device(&device_id, &device_id, serde_json::json!({}))
        .await?;

    info!(%poll_url, %device_id, interval_secs, "HTTP poller started");

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;
        // TODO: replace with reqwest::get once reqwest is added as a dependency
        info!(%poll_url, "Polling URL (stub — no HTTP client yet)");
        let stub_state = serde_json::json!({ "polled": true });
        client.publish_state(&device_id, &stub_state).await?;
    }
}
