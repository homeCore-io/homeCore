//! hc-captest binary — connects to the broker and sits waiting for
//! streaming commands. Exists so the plugin can be driven manually for
//! exploration, separately from the integration test harness that calls
//! `hc_captest::register_actions` in-process.

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = PluginConfig {
        broker_host: std::env::var("HC_BROKER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        broker_port: std::env::var("HC_BROKER_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1883),
        plugin_id: hc_captest::PLUGIN_ID.to_string(),
        password: std::env::var("HC_BROKER_PASSWORD").unwrap_or_default(),
    };

    let client = PluginClient::connect(config).await?;
    let mgmt = client
        .enable_management(60, Some(env!("CARGO_PKG_VERSION").to_string()), None, None)
        .await?;
    let mgmt = hc_captest::register_actions(mgmt);

    info!(plugin_id = hc_captest::PLUGIN_ID, "hc-captest ready");
    client
        .run_managed(|_device_id, _payload| { /* no device commands */ }, mgmt)
        .await
}
