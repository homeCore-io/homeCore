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

mod adapter;
mod enroll;
mod identity;
mod placement;
mod plugin;
mod supervisor;

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

/// A plugin that keeps dying reaches the operator's screen, not just the
/// container logs.
///
/// Keyed per plugin so two flapping plugins do not overwrite each other's
/// notice, and cleared on recovery because a notice is state.
fn report_crash_loop(notices: &plugin_sdk_rs::PluginNotices, plugin_id: &str, looping: bool) {
    use plugin_sdk_rs::types::PluginNotice;
    let code = format!("crash_loop.{plugin_id}");
    if looping {
        notices.raise(
            PluginNotice::error(
                code,
                format!("{plugin_id} keeps exiting and is being restarted."),
            )
            .with_remedy("Check this plugin's logs and its configuration."),
        );
    } else {
        notices.clear(&code);
    }
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
        "credentials received"
    );

    let caps = capabilities();
    let notices = plugin::connect_and_register(&creds, &caps, env!("CARGO_PKG_VERSION")).await?;
    tracing::info!(plugin_id = %creds.plugin_id, "registered with homeCore");

    // What this runtime should be running. Core will hold this list and the
    // runtime will reconcile against it; for now it is local.
    let placements = placement::load(&data_dir)?;

    if placements.is_empty() {
        // An empty runtime and a broken one are indistinguishable in the plugin
        // list unless one of them says so.
        plugin::announce_empty(&notices, &caps.kind);
        tracing::info!("no plugins placed on this runtime yet");
    } else {
        plugin::clear_empty(&notices);
        let adapter_path =
            PathBuf::from(env_or("HC_RUNTIME_ADAPTER", "/etc/hc-runtime/adapter.toml"));
        let adapter = adapter::Adapter::load(&adapter_path)?;

        // Held for the lifetime of the process: dropping the sender would tell
        // every supervisor to shut down immediately.
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        for p in placements {
            let argv = adapter::render(&adapter.launch, &p.vars())
                .with_context(|| format!("rendering the launch command for {}", p.id))?;
            tracing::info!(plugin_id = %p.id, "hosting");

            let notices = notices.clone();
            let rx = shutdown_rx.clone();
            tokio::spawn(async move {
                supervisor::supervise(
                    supervisor::Supervised {
                        plugin_id: p.id.clone(),
                        argv,
                    },
                    rx,
                    move |id, looping| report_crash_loop(&notices, id, looping),
                )
                .await;
            });
        }
    }

    std::future::pending::<()>().await;
    Ok(())
}
