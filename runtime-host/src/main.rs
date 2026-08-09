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
//! Once approved it converges: core holds the list of what belongs here, the
//! host pulls it, provisions what is missing and supervises the result.

mod adapter;
mod core_client;
mod enroll;
mod identity;
mod install;
mod plugin;
mod reconcile;
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

    // Everything language-specific, and the only reason this binary can host
    // Python without knowing anything about it. Loaded before the first pass
    // rather than on demand: an image shipped without an adapter cannot host
    // anything, and finding that out at startup beats finding out the first
    // time something is placed here.
    let adapter_path = PathBuf::from(env_or("HC_RUNTIME_ADAPTER", "/etc/hc-runtime/adapter.toml"));
    let adapter = adapter::Adapter::load(&adapter_path)?;
    if adapter.kind != caps.kind || adapter.abi != caps.abi {
        anyhow::bail!(
            "this host enrolled as {} {} but its adapter is {} {} — artifacts are matched \
             against what was advertised at enrollment, so they would arrive unusable",
            caps.kind,
            caps.abi,
            adapter.kind,
            adapter.abi
        );
    }

    let client =
        core_client::CoreClient::new(http.clone(), &cfg.base_url, &id.runtime_id, &creds.api_key);
    let crash_notices = notices.clone();
    let mut reconciler = reconcile::Reconciler::new(
        data_dir.clone(),
        adapter,
        client,
        move |plugin_id, looping| report_crash_loop(&crash_notices, plugin_id, looping),
    );

    let interval = std::time::Duration::from_secs(
        env_or("HC_RUNTIME_POLL_SECS", "30")
            .parse()
            .unwrap_or(30)
            .max(5),
    );

    // Converge, forever. Core holds the desired state and this is the loop that
    // catches up with it — including everything that happened while the
    // container was down, which is the same case as everything else.
    let mut announced_empty = false;
    loop {
        match reconciler.pass().await {
            Ok(pass) => {
                if pass.changed_anything() {
                    tracing::info!(
                        started = ?pass.started, restarted = ?pass.restarted,
                        stopped = ?pass.stopped, "reconciled"
                    );
                }
                for (plugin_id, error) in &pass.failed {
                    tracing::warn!(%plugin_id, %error, "still not provisioned");
                }

                // An empty runtime and a broken one look identical in the plugin
                // list unless one of them says which it is.
                let empty = reconciler.hosted() == 0;
                if empty && !announced_empty {
                    plugin::announce_empty(&notices, &caps.kind);
                } else if !empty && announced_empty {
                    plugin::clear_empty(&notices);
                }
                announced_empty = empty;
            }
            // Everything keeps running. Stopping plugins because the list could
            // not be fetched would turn a core restart into an outage in every
            // runtime at once.
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "could not reach homeCore for the desired state — keeping what is running"
            ),
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            // A container stop is a SIGTERM, and the hosted plugins are this
            // process's children. Left to the container runtime they are killed
            // a moment later, mid-publish, with core still holding whatever
            // they last said.
            _ = terminate() => {
                tracing::info!("shutting down — stopping hosted plugins");
                reconciler.stop_all().await;
                return Ok(());
            }
        }
    }
}

/// Resolve when the operator, or the container runtime, asks this to stop.
async fn terminate() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            // Nothing to do about it, and it must not take the host down.
            Err(e) => {
                tracing::warn!(error = %e, "could not listen for SIGTERM");
                return std::future::pending().await;
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
