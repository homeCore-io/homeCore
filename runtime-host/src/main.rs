//! hc-runtime-host — hosts homeCore plugins written in languages other than Rust.
//!
//! The operator runs this in a container; homeCore never manages it. The host
//! finds homeCore, asks to join, and once an administrator approves it, receives
//! the credentials its plugins need.
//!
//! One binary serves every language. What differs per language — how to create an
//! environment, install an artifact and launch it — is described in
//! `adapter.toml`, baked into the image, so a Node or .NET runtime is a new base
//! image rather than a new program. See `docs/pluginRuntimesPlan.md`, piece 4.
//!
//! Phase A covers enrollment. Placement and supervision follow.

mod enroll;
mod identity;

use anyhow::{Context, Result};
use hc_api_types::plugin_runtimes::RuntimeCapabilities;
use std::path::PathBuf;

/// Read an environment variable, or fall back.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Best-effort hostname, for the operator's benefit at approval time.
///
/// Never trusted by core, so a failure here is cosmetic rather than fatal.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".into())
}

/// What this image can host.
///
/// Baked in at build time rather than discovered, because an artifact is matched
/// against it: a runtime that guessed its own ABI wrong would be handed wheels
/// that cannot import, and the failure would surface as a plugin that will not
/// start rather than as a mismatch.
fn capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        kind: env_or("HC_RUNTIME_KIND", "python"),
        abi: env_or("HC_RUNTIME_ABI", "cp312-manylinux_2_28"),
        arch: env_or("HC_RUNTIME_ARCH", std::env::consts::ARCH),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hc_runtime_host=info".into()),
        )
        .init();

    let base_url = std::env::var("HOMECORE_URL").context(
        "HOMECORE_URL is required — the address of homeCore, e.g. http://10.0.10.150:8080.\n\
         Discovery is deliberately not automatic: a container on a bridge network \
         cannot receive multicast, so mDNS would fail in exactly the default setup.",
    )?;
    let data_dir = PathBuf::from(env_or("HC_RUNTIME_DATA", "/var/lib/hc-runtime"));

    let id = identity::Identity::load_or_create(&data_dir)
        .with_context(|| format!("loading identity from {}", data_dir.display()))?;
    tracing::info!(runtime_id = %id.runtime_id, "runtime host starting");

    let cfg = enroll::EnrollConfig {
        base_url,
        capabilities: capabilities(),
        host_version: env!("CARGO_PKG_VERSION").to_string(),
        sdk_version: env_or("HC_RUNTIME_SDK_VERSION", "unknown"),
        hostname: hostname(),
        network_mode: env_or("HC_RUNTIME_NETWORK_MODE", "unknown"),
        token: std::env::var("HOMECORE_ENROLL_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty()),
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let creds = enroll::enroll_and_wait(&http, &id, &cfg).await?;

    tracing::info!(
        plugin_id = %creds.plugin_id,
        broker = %format!("{}:{}", creds.broker_host, creds.broker_port),
        "credentials received; ready to host plugins"
    );

    // Phase A ends here. Next: connect to the broker as `creds.plugin_id`,
    // register through the SDK, and supervise placements.
    Ok(())
}
