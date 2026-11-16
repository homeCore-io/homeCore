//! hc-runtime-host — hosts homeCore plugins written in languages other than Rust.
//!
//! The operator runs this in a container; homeCore never manages it. The admin
//! adds the runtime in homeCore, which shows a connection key once; the
//! operator pastes it into this runtime's page (port 8790) or sets
//! `HC_RUNTIME_KEY`. With a key, the host checks in with homeCore, logs in to
//! the broker as an ordinary plugin, and converges on what homeCore says should
//! run here.
//!
//! One binary serves every language. What differs per language (how to create
//! an environment, install an artifact and launch it) is described in
//! `adapter.toml`, baked into the image, so a Node or .NET runtime is a new base
//! image rather than a new program. See `docs/pluginRuntimesPlan.md`.

mod adapter;
mod broker;
mod check;
mod core_client;
mod install;
mod key_store;
mod logs;
mod page;
mod plugin;
mod reconcile;
mod supervisor;

use anyhow::{Context, Result};
use hc_api_types::plugin_runtimes::{ConnectionKey, HelloRequest, RuntimeCapabilities};
use page::{Phase, PluginRow, Shared};
use std::path::PathBuf;
use std::sync::Arc;

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
            PluginNotice::error(code, crash_message(plugin_id)).with_remedy(
                "Check its settings. Its full output is on the runtime's page, under Logs.",
            ),
        );
    } else {
        notices.clear(&code);
    }
}

/// "keeps exiting", with what it last printed: usually the reason.
fn crash_message(plugin_id: &str) -> String {
    let last: Vec<String> = logs::book()
        .tail(plugin_id, 3)
        .into_iter()
        .map(|l| l.text)
        .collect();
    if last.is_empty() {
        return format!(
            "{plugin_id} keeps exiting and is being restarted. It printed nothing first."
        );
    }
    let mut said = last.join(" / ");
    if said.chars().count() > 300 {
        said = said.chars().take(300).collect::<String>() + "…";
    }
    format!("{plugin_id} keeps exiting and is being restarted. Last output: {said}")
}

/// Best-effort hostname, for the admin's list in homeCore.
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
/// Baked in at build time rather than discovered, because the key names what
/// the runtime was added for: a runtime that guessed its own ABI wrong would be
/// handed wheels that cannot import.
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
        // Also kept for the runtime's page; plain text reads there and in a
        // container log alike.
        .with_writer(logs::Tee)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hc_runtime_host=info".into()),
        )
        .init();

    let data_dir = PathBuf::from(env_or("HC_RUNTIME_DATA", "/var/lib/hc-runtime"));
    let caps = capabilities();

    // Everything language-specific, and the only reason this binary can host
    // Python without knowing anything about it. Loaded first: an image shipped
    // without an adapter cannot host anything, and finding that out at startup
    // beats finding out the first time something is placed here.
    let adapter_path = PathBuf::from(env_or("HC_RUNTIME_ADAPTER", "/etc/hc-runtime/adapter.toml"));
    let adapter = adapter::Adapter::load(&adapter_path)?;
    if adapter.kind != caps.kind || adapter.abi != caps.abi {
        anyhow::bail!(
            "this image says it hosts {} {} but its adapter is {} {}",
            caps.kind,
            caps.abi,
            adapter.kind,
            adapter.abi
        );
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let from_env = key_store::from_env();
    let key_from_env = matches!(from_env, Ok(Some(_)));
    let (problem, first_key) = match from_env {
        Ok(Some(k)) => (None, Some(k)),
        Ok(None) => match key_store::load(&data_dir) {
            Ok(k) => (None, k),
            Err(e) => (Some(format!("{e:#}")), None),
        },
        Err(e) => (Some(format!("{e:#}")), None),
    };

    let port: u16 = env_or("HC_RUNTIME_PAGE_PORT", "8790")
        .parse()
        .unwrap_or(8790);

    let (new_key_tx, mut new_key_rx) = tokio::sync::watch::channel::<Option<ConnectionKey>>(None);
    let shared = Arc::new(Shared {
        status: Default::default(),
        current: Default::default(),
        new_key: new_key_tx,
        looping: Default::default(),
        data_dir: data_dir.clone(),
        hello: HelloRequest {
            capabilities: caps.clone(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            hostname: hostname(),
            network_mode: env_or("HC_RUNTIME_NETWORK_MODE", "unknown"),
            page_port: (port != 0).then_some(port),
            page_url: std::env::var("HC_RUNTIME_PAGE_URL")
                .ok()
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
        },
        broker_pin: std::env::var("HC_RUNTIME_BROKER_HOST")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        http: http.clone(),
    });
    shared.set(|s| {
        s.kind = caps.kind.clone();
        s.abi = caps.abi.clone();
        s.arch = caps.arch.clone();
        s.key_from_env = key_from_env;
        s.message = problem;
    });

    if port != 0 {
        tokio::spawn(page::serve(shared.clone(), port));
    }

    // Wait for a key: one saved or set already, or the first pasted on the page.
    let key = match first_key {
        Some(k) => k,
        None => {
            tracing::info!(%port, "no key yet; paste one on this runtime's page");
            loop {
                tokio::select! {
                    changed = new_key_rx.changed() => {
                        if changed.is_err() { return Ok(()); }
                        if let Some(k) = new_key_rx.borrow_and_update().clone() { break k; }
                    }
                    _ = terminate() => return Ok(()),
                }
            }
        }
    };

    let outcome = run(shared.clone(), adapter, key, &mut new_key_rx).await;
    match outcome {
        Ok(Ended::Stopped) => Ok(()),
        // A new key from the page. The SDK connection and every hosted plugin
        // belong to the old one; starting this program again is the one way to
        // be sure none of it lingers. Same PID, so tini and the container never
        // notice.
        Ok(Ended::NewKey) => restart_self(),
        Err(e) => Err(e),
    }
}

enum Ended {
    Stopped,
    NewKey,
}

/// Connect with `key` and converge, until stopped or handed a new key.
///
/// A problem connecting is shown on the page and retried; it never ends the
/// container, because the fix (open the broker, reset the login, fix a
/// firewall) happens somewhere else and the runtime should notice by itself.
async fn run(
    shared: Arc<Shared>,
    adapter: adapter::Adapter,
    key: ConnectionKey,
    new_key: &mut tokio::sync::watch::Receiver<Option<ConnectionKey>>,
) -> Result<Ended> {
    if let Ok(mut c) = shared.current.lock() {
        *c = Some(key.clone());
    }
    let broker_host = page::broker_host(&key, &shared.broker_pin);
    shared.set(|s| {
        s.state = Phase::Connecting;
        s.url = Some(key.url.clone());
        s.broker = Some(format!("{broker_host} : {}", key.broker_port));
        s.username = Some(key.username.clone());
    });
    tracing::info!(url = %key.url, runtime_id = %key.runtime_id, "connecting");

    // Check in and log in to the broker, retrying with the reason on the page.
    let mut wait = std::time::Duration::from_secs(5);
    let name = loop {
        match page::test_key(&shared, &key).await {
            Ok(name) => break name,
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(error = %msg, "not connected yet; trying again");
                shared.set(|s| {
                    s.state = Phase::Problem;
                    s.message = Some(msg);
                });
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = new_key.changed() => return Ok(Ended::NewKey),
            _ = terminate() => return Ok(Ended::Stopped),
        }
        wait = (wait * 2).min(std::time::Duration::from_secs(60));
    };

    let notices = plugin::connect_and_register(
        &key,
        &broker_host,
        &shared.hello.capabilities,
        env!("CARGO_PKG_VERSION"),
    )
    .await?;
    tracing::info!(plugin_id = %key.username, %name, "connected to homeCore");
    shared.set(|s| {
        s.state = Phase::Connected;
        s.message = None;
        s.name = Some(name);
        s.last_heard = Some(chrono::Utc::now());
    });

    let client =
        core_client::CoreClient::new(shared.http.clone(), &key.url, &key.runtime_id, &key.secret);
    let crash_notices = notices.clone();
    let crash_shared = shared.clone();
    let mut reconciler = reconcile::Reconciler::new(
        shared.data_dir.clone(),
        adapter,
        client,
        move |plugin_id, looping| {
            report_crash_loop(&crash_notices, plugin_id, looping);
            if let Ok(mut set) = crash_shared.looping.lock() {
                if looping {
                    set.insert(plugin_id.to_string());
                } else {
                    set.remove(plugin_id);
                }
            }
        },
    )
    // Core writes each hosted plugin's broker address from its own point of
    // view; make it the one this container can dial.
    .with_broker(broker::BrokerHost::new(&key.url, shared.broker_pin.clone()));
    let checkin =
        core_client::CoreClient::new(shared.http.clone(), &key.url, &key.runtime_id, &key.secret);

    let interval = std::time::Duration::from_secs(
        env_or("HC_RUNTIME_POLL_SECS", "30")
            .parse()
            .unwrap_or(30)
            .max(5),
    );

    // Converge, forever. Core holds the desired state and this is the loop that
    // catches up with it, including everything that happened while the
    // container was down, which is the same case as everything else.
    let mut announced_empty = false;
    loop {
        match checkin.hello(&shared.hello).await {
            Ok(answer) => shared.set(|s| {
                s.state = Phase::Connected;
                s.message = None;
                s.last_heard = Some(chrono::Utc::now());
                s.outdated = answer
                    .min_host_version
                    .filter(|min| page::older(env!("CARGO_PKG_VERSION"), min))
                    .map(|min| {
                        format!(
                            "This runtime is version {}, and homeCore expects {min} or later. \
                             Pull the newer image and restart the container.",
                            env!("CARGO_PKG_VERSION")
                        )
                    });
            }),
            // The key was reset or the runtime removed. Its plugins' own logins
            // may be gone too, and nothing here should keep running for a
            // homeCore that has let go of it. Stop them and start again, which
            // also drops this runtime's broker connection; the fresh start
            // shows why on the page and waits for a new key.
            Err(e) if e.downcast_ref::<core_client::KeyRefused>().is_some() => {
                tracing::warn!(error = %e, "homeCore refused this runtime's key; stopping hosted plugins");
                reconciler.stop_all().await;
                return Ok(Ended::NewKey);
            }
            Err(e) => shared.set(|s| {
                s.state = Phase::Problem;
                s.message = Some(format!("{e:#}"));
            }),
        }
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
                    plugin::announce_empty(&notices, &shared.hello.capabilities.kind);
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
                "could not reach homeCore for the desired state; keeping what is running"
            ),
        }
        let looping = shared.looping.lock().map(|l| l.clone()).unwrap_or_default();
        let rows: Vec<PluginRow> = reconciler
            .hosted_list()
            .into_iter()
            .map(|h| PluginRow {
                looping: looping.contains(&h.plugin_id),
                plugin_id: h.plugin_id,
                version: h.version,
                since: h.since,
            })
            .collect();
        shared.set(|s| s.plugins = rows);

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = new_key.changed() => {
                tracing::info!("a new key was saved; stopping hosted plugins to reconnect with it");
                reconciler.stop_all().await;
                return Ok(Ended::NewKey);
            }
            // A container stop is a SIGTERM, and the hosted plugins are this
            // process's children. Left to the container runtime they are killed
            // a moment later, mid-publish, with core still holding whatever
            // they last said.
            _ = terminate() => {
                tracing::info!("shutting down; stopping hosted plugins");
                reconciler.stop_all().await;
                return Ok(Ended::Stopped);
            }
        }
    }
}

/// Start this program again in place, with the same arguments.
fn restart_self() -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().context("finding this program to restart it")?;
        let err = std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec();
        Err(err).context("restarting with the new key")
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("a new key was saved; restart the runtime to use it")
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
