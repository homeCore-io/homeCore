mod jwt_secret;
mod plugin_manager;

use anyhow::Result;
use hc_api::{
    dashboard_store::DashboardStore,
    group_store::{groups_path, GroupStore},
    logs::LogStreamState,
    rule_file_store::RuleFileStore,
    skin_store::SkinStore,
    AppState,
};
use hc_auth::{hash_password, JwtService, Role, User};
use hc_broker::{Broker, BrokerConfig, ClientAcl};
use hc_config::{AppConfig, PluginEntry};
use hc_core::{device_naming, rule_loader, rule_resolver, Core, EventBus};
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_notify::NotificationService;
use hc_state::StateStore;
use hc_topic_map::{loader::load_profiles_from_dir, DeviceTypeRegistry, EcosystemRouter};
use ipnet::IpNet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

/// Wait for SIGTERM or SIGINT and return.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");
        tokio::select! {
            _ = term.recv() => { info!("Received SIGTERM — initiating graceful shutdown"); }
            _ = int.recv()  => { info!("Received SIGINT — initiating graceful shutdown"); }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for Ctrl-C");
        info!("Received Ctrl-C — initiating graceful shutdown");
    }
}

async fn wait_for_shutdown_watch(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if shutdown.changed().await.is_err() {
            break;
        }
        if *shutdown.borrow() {
            break;
        }
    }
}

// ── base directory resolution ───────────────────────────────────────────────

/// Determine the HomeCore installation directory.
///
/// Priority order (first match wins):
///   1. `--home <path>` command-line argument
///   2. `HOMECORE_HOME` environment variable
///   3. Current working directory of the process (default)
///
/// The intended deployment model is: install the package into a directory,
/// `cd` into it, and run the binary.  All data, config, and logs are then
/// visible siblings of the binary — no hidden directories, no user-home
/// scattered files.
fn resolve_base_dir() -> PathBuf {
    // 1. --home CLI arg
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len() {
        if args[i] == "--home" {
            if let Some(p) = args.get(i + 1) {
                return PathBuf::from(p);
            }
        }
    }

    // 2. HOMECORE_HOME env var
    if let Ok(p) = std::env::var("HOMECORE_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }

    // 3. Current working directory
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Determine the config file path.
///
/// Priority order:
///   1. `--config <path>` command-line argument
///   2. `HOMECORE_CONFIG` environment variable
///   3. `{base_dir}/config/homecore.toml`
fn resolve_config_path(base: &Path) -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    for i in 1..args.len() {
        if args[i] == "--config" {
            if let Some(p) = args.get(i + 1) {
                let path = PathBuf::from(p);
                return if path.is_relative() {
                    base.join(path)
                } else {
                    path
                };
            }
        }
    }
    if let Ok(p) = std::env::var("HOMECORE_CONFIG") {
        if !p.is_empty() {
            let path = PathBuf::from(p);
            return if path.is_relative() {
                base.join(path)
            } else {
                path
            };
        }
    }
    base.join("config").join("homecore.toml")
}

/// Phase 0 of plugin-config centralization: for each `(plugin_id, legacy_path)`,
/// import the plugin's existing config into the core-owned [`PluginConfigStore`]
/// (one-time byte-copy, idempotent — skipped if a central file already exists),
/// and return the **effective** config path to hand the plugin as `argv[1]`.
///
/// The effective path is the central file once it exists, otherwise the legacy
/// path verbatim — so a plugin with nothing to import behaves exactly as before.
/// `homecore.toml` is never rewritten; its `config` field stays the import source.
fn centralize_plugin_configs<'a>(
    store: &hc_api::PluginConfigStore,
    plugins: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> std::collections::HashMap<String, String> {
    plugins
        .into_iter()
        .map(|(id, legacy_path)| {
            if !store.exists(id) {
                let legacy = Path::new(legacy_path);
                if legacy.exists() {
                    match store.import_legacy(id, legacy) {
                        Ok(true) => {
                            info!(id, "Imported plugin config → central store (one-time)")
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            id, error = %e,
                            "Config import failed; using legacy path"
                        ),
                    }
                }
            }
            let effective = if store.exists(id) {
                store.path_for(id).to_string_lossy().into_owned()
            } else {
                legacy_path.to_string()
            };
            (id.to_string(), effective)
        })
        .collect()
}

// Stays here rather than moving to hc-config with the structs it operates
// on: it reads the managed-plugin store from hc-api, and hc-api is about to
// depend on hc-config to describe these same sections.
/// Merge the static `[[plugins]]` set with the managed-plugin store: drop
/// tombstoned (uninstalled) ids, then layer runtime-installed records on top
/// (records win on id collision). This is the effective set core spawns,
/// supervises, seeds registry records for, and hot-reload-watches.
fn build_effective_plugins(
    static_plugins: &[PluginEntry],
    managed: &hc_api::ManagedPluginStore,
    base: &Path,
) -> Vec<PluginEntry> {
    let removed = managed.removed_ids();
    let mut out: Vec<PluginEntry> = static_plugins
        .iter()
        .filter(|p| !removed.contains(&p.id))
        .cloned()
        .collect();
    for rec in managed.records() {
        if removed.contains(&rec.id) {
            continue;
        }
        out.retain(|p| p.id != rec.id);
        let mut entry = PluginEntry {
            id: rec.id,
            binary: rec.binary,
            config: rec.config,
            enabled: rec.enabled,
        };
        // Resolve any relative record paths against base_dir exactly as static
        // plugins are (absolute paths pass through), so activation doesn't depend
        // on the process's CWD.
        entry.resolve(base);
        out.push(entry);
    }
    out
}

// ── config structs ──────────────────────────────────────────────────────────
//
// Moved to the `hc-config` crate so `hc-api` can describe the same sections
// it already serves through GET/PUT /system/config. `main.rs` keeps parsing
// exactly the same shape.

// ── helpers ─────────────────────────────────────────────────────────────────

/// Default destination for the first-boot admin password file:
/// `<base_dir>/INITIAL_ADMIN_PASSWORD` — at the root of homeCore's home,
/// where it's the most discoverable. Operators bind-mounting the home
/// dir (appliance setup) see the file at the top of their host volume
/// rather than tucked inside `data/`.
fn default_admin_password_path(base_dir: &std::path::Path) -> std::path::PathBuf {
    base_dir.join("INITIAL_ADMIN_PASSWORD")
}

/// Parse `[auth] whitelist` into single-address entries.
///
/// Accepts a bare IP (`10.0.10.200`) or an explicit single-host prefix
/// (`10.0.10.200/32`, `::1/128`). **Ranges are refused.**
///
/// A whitelisted source gets synthetic Admin claims with no token at all, so a
/// range hands unauthenticated admin to every host inside it. `10.0.10.0/24`
/// reads like "my LAN" and means "254 devices may administer this house" — on a
/// home-automation VLAN that population is mostly IoT gear, the least
/// trustworthy set of hosts on the network.
///
/// Bad entries are skipped with a warning rather than failing startup, matching
/// how a malformed entry has always been treated: a typo should not take the
/// server down. Skipping is also the safe direction to fail here, since it
/// grants *less* access than intended rather than more.
///
/// Deliberately not applied to `[metrics] whitelist`, which gates a read-only
/// Prometheus endpoint and grants no admin — a range there is reasonable.
fn parse_auth_whitelist(entries: &[String]) -> Vec<IpNet> {
    entries
        .iter()
        .filter_map(|s| {
            let net = s
                .parse::<IpNet>()
                .or_else(|_| s.parse::<std::net::IpAddr>().map(IpNet::from))
                .map_err(
                    |e| tracing::warn!(entry = %s, error = %e, "Invalid whitelist entry — skipping"),
                )
                .ok()?;

            if net.prefix_len() != net.max_prefix_len() {
                // Computed, not counted: `net.hosts()` is an iterator, so
                // counting it walks every address — 16M for a /8, worse for v6.
                let width = 1u128
                    .checked_shl((net.max_prefix_len() - net.prefix_len()) as u32)
                    .unwrap_or(u128::MAX);
                tracing::warn!(
                    entry = %s,
                    addresses = width,
                    "[auth] whitelist entry covers a range — skipping. A whitelisted \
                     source gets Admin with no token, so this would grant \
                     unauthenticated admin to every host in it. List the addresses \
                     explicitly instead (e.g. \"10.0.10.200\")."
                );
                return None;
            }
            Some(net)
        })
        .collect()
}

/// Write the auto-generated admin password to `path` with 0600 perms,
/// creating the parent directory if needed. Body is a small banner so
/// the file is self-explanatory if anyone opens it months later.
fn write_initial_admin_password(path: &std::path::Path, password: &str) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let body = format!(
        "homeCore initial admin credentials\n\
         ---------------------------------\n\
         Username: admin\n\
         Password: {password}\n\
         \n\
         Generated automatically on first boot. Change the password\n\
         after your first login and DELETE THIS FILE.\n"
    );

    // Write with 0600 directly (open-with-mode) rather than write+chmod,
    // so the password is never on-disk world-readable for any window.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(path)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    Ok(())
}

/// Generate a random alphanumeric password of the given length.
///
/// This is the initial admin password, so it is drawn from the OS CSPRNG and
/// rejection-sampled — `% CHARSET.len()` on a 55-character set would skew the
/// first few characters, and a bias in an admin credential is not academic.
fn random_password(len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    // Largest multiple of the set size that fits in a byte; anything at or
    // above it is discarded rather than folded in.
    const LIMIT: u8 = u8::MAX - (u8::MAX % CHARSET.len() as u8);

    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).expect("OS randomness unavailable");
        for &b in &buf {
            if b < LIMIT {
                out.push(CHARSET[(b % CHARSET.len() as u8) as usize] as char);
                if out.len() == len {
                    break;
                }
            }
        }
    }
    out
}

// ── main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // ── 1. Determine base directory and config file path ──────────────────
    //
    // This happens before any logging is initialised, so errors go to stderr.
    let base_dir = resolve_base_dir();
    let config_path = resolve_config_path(&base_dir);

    eprintln!("HomeCore base directory: {}", base_dir.display());
    eprintln!("HomeCore config file:    {}", config_path.display());

    // ── 2. Load config (path defaults filled in by resolve_paths below) ───
    let mut config: AppConfig = match std::fs::read_to_string(&config_path) {
        Ok(s) => match toml::from_str::<AppConfig>(&s) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "Warning: config parse error in {}: {e}; using defaults",
                    config_path.display()
                );
                AppConfig::default()
            }
        },
        Err(_) => AppConfig::default(),
    };

    // ── 3. Resolve all path fields relative to base_dir ───────────────────
    config.resolve_paths(&base_dir);

    // ── 4. Create standard directory layout under base_dir ────────────────
    //
    // Harmless if directories already exist.  Failures are non-fatal so that
    // explicitly configured absolute paths elsewhere on the filesystem work.
    for subdir in &[
        "config/profiles",
        "config/calendars",
        "config/plugins",
        "data",
        "logs",
        "rules",
    ] {
        if let Err(e) = std::fs::create_dir_all(base_dir.join(subdir)) {
            eprintln!(
                "Warning: could not create {}/{subdir}: {e}",
                base_dir.display()
            );
        }
    }

    // ── 5. Initialise logging from config ──────────────────────────────────
    //
    // _logging_handle must remain in scope until the end of main() so the
    // background file-writer thread stays alive.
    // We also wire in a BroadcastLayer so the log-streaming WebSocket endpoint
    // can replay recent lines and subscribe to live log events.
    //
    // hc_time::init MUST run before init_with_broadcast — the very first log
    // line is formatted by the configured-tz timer, and OnceLock-set after
    // that point is silently lost. Parse failures fall through to the
    // default UTC (no panic) and emit a warning via eprintln! since logging
    // isn't up yet.
    if let Some(name) = config.location.timezone.as_deref() {
        match hc_time::parse_iana(name) {
            Ok(tz) => hc_time::init(tz),
            Err(e) => eprintln!(
                "[location].timezone unparseable ({e}); falling back to UTC for log/display formatting"
            ),
        }
    }
    let (_logging_handle, log_tx, log_ring, log_level_handle) =
        hc_logging::init_with_broadcast(&config.logging, config.logging.stream.ring_buffer_size)?;

    info!(base = %base_dir.display(), config = %config_path.display(), "HomeCore starting");

    // Log core's own SDK protocol version at startup. Plugins emit their
    // SDK version on heartbeat (component_versioning Phase B); state_bridge
    // compares incoming heartbeats against this `core_compat_version` and
    // warns on a mismatch. Single source of truth: hc-types::PROTOCOL_VERSION
    // (this crate is the wire-format authority — every event variant + topic
    // shape lives there). Don't substitute the binary's CARGO_PKG_VERSION;
    // it could drift independently from hc-types under per-component SemVer.
    info!(
        core_compat_version = hc_types::PROTOCOL_VERSION,
        "Core SDK protocol version (heartbeat compat baseline)"
    );

    // ── 6. Embedded MQTT broker ────────────────────────────────────────────
    let broker_cfg = BrokerConfig {
        host: config.broker.host.clone(),
        port: config.broker.port,
        v5_port: config.broker.v5_port,
        tls_port: config.broker.tls_port,
        cert_path: config.broker.cert_path.clone(),
        key_path: config.broker.key_path.clone(),
        clients: config
            .broker
            .clients
            .iter()
            .map(|c| ClientAcl {
                client_id: c.id.clone(),
                password: c.password.clone(),
                allow_pub: c.allow_pub.clone(),
                allow_sub: c.allow_sub.clone(),
            })
            .collect(),
    };
    Broker::new(broker_cfg).spawn()?;

    // ── 9. Internal MQTT client ────────────────────────────────────────────
    let internal_cred = config
        .broker
        .clients
        .iter()
        .find(|c| c.id == "internal.core")
        .cloned();
    let mqtt_cfg = MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: config.broker.port,
        client_id: "internal.core".into(),
        username: internal_cred.as_ref().map(|c| c.id.clone()),
        password: internal_cred.as_ref().map(|c| c.password.clone()),
    };
    let (mut mqtt_client, mut mqtt_rx) = MqttClient::new(mqtt_cfg);
    let publish_handle = mqtt_client.publish_handle();

    // Set up a ready signal so plugins are launched only after the internal
    // MQTT client has subscribed to homecore/#.  This prevents registration
    // messages from being published before anyone is listening.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    mqtt_client.set_ready_notify(ready_tx);

    // ── 10. State store ─────────────────────────────────────────────────────
    let store = StateStore::open(
        &config.storage.state_db_path,
        &config.storage.history_db_path,
    )
    .await?;

    match device_naming::backfill_missing_canonical_names(&store).await {
        Ok(0) => {}
        Ok(count) => info!(count, "Backfilled missing device canonical names"),
        Err(e) => tracing::warn!(error = %e, "Failed to backfill device canonical names"),
    }

    // ── 11. Event buses ─────────────────────────────────────────────────────
    // internal_bus: carries only Event::MqttMessage (raw MQTT traffic).
    //   Subscribers: state_bridge, timer_manager, switch_manager, mode_manager, engine.
    // pub_bus: carries all typed events (DeviceStateChanged, RuleFired, etc.).
    //   Subscribers: engine, hc-api (event log, WS stream, plugin registry).
    let internal_bus = EventBus::new(1024);
    let pub_bus = EventBus::new(1024);

    // Shared plugin registry — populated by config seeding and PluginManager,
    // consumed by AppState and API handlers.
    let plugin_registry: Arc<RwLock<HashMap<String, hc_api::PluginRecord>>> =
        Arc::new(RwLock::new(HashMap::new()));
    // Per-plugin command channels for start/stop/restart from API handlers.
    let plugin_commands: hc_api::PluginCommandChannels = Arc::new(RwLock::new(HashMap::new()));

    // Subscribe the plugin-registry listener early — BEFORE plugins spawn.
    // Plugins publish their retained capability manifest on CONNACK during
    // startup; spawning this inside AppState::new_with_plugins is too late
    // because broadcast channels don't replay history on late subscribe.
    hc_api::spawn_plugin_registry_listener(pub_bus.clone(), plugin_registry.clone());

    // Persist plugin learned-state writes (homecore/plugins/<id>/state/set) to
    // redb and re-publish the merged doc as the retained authoritative state.
    // Subscribes to the raw MQTT bus (internal_bus carries Event::MqttMessage).
    hc_api::spawn_plugin_state_listener(
        internal_bus.clone(),
        store.clone(),
        publish_handle.clone(),
    );

    // ── 12. Load rules from TOML files ────────────────────────────────────
    let rules_dir = PathBuf::from(&config.rules.dir);
    let rules = {
        let dir = rules_dir.clone();
        tokio::task::spawn_blocking(move || rule_loader::load_all(&dir)).await??
    };

    let rules = if rules.is_empty() {
        // Migration: if the rules directory is empty but redb has rules, write
        // each out to a TOML file so the new file-based system picks them up.
        let legacy = store.list_rules().await.unwrap_or_default();
        if !legacy.is_empty() {
            info!(
                count = legacy.len(),
                dir = %rules_dir.display(),
                "Migrating rules from redb → TOML files (one-time)"
            );
            let fs = RuleFileStore::new(&rules_dir);
            for rule in &legacy {
                if let Err(e) = fs.write_rule(rule) {
                    tracing::warn!(rule_id = %rule.id, error = %e, "Failed to migrate rule");
                }
            }
            // Reload from files so the migrated set is canonical.
            let dir = rules_dir.clone();
            match tokio::task::spawn_blocking(move || rule_loader::load_all(&dir)).await? {
                Ok(migrated) => {
                    info!(
                        count = migrated.len(),
                        "Rules migrated and loaded from files"
                    );
                    migrated
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Migrated rule reload failed; using redb rules");
                    legacy
                }
            }
        } else {
            info!("No rules found — starting with empty rule set");
            vec![]
        }
    } else {
        info!(count = rules.len(), dir = %rules_dir.display(), "Loaded rules from files");
        rules
    };

    let source_rules_handle = Arc::new(tokio::sync::RwLock::new(rules.clone()));
    let rules = rule_resolver::compile_rules_for_store(&store, rules).await?;

    let modes_path = base_dir.join("config").join("modes.toml");
    let glue_path = base_dir.join("config").join("glue.toml");

    // ── Graceful shutdown channel ──────────────────────────────────────────
    //
    // `shutdown_tx` is used by the signal handler task AND the API's
    // POST /system/restart handler to broadcast a shutdown to the rule
    // engine, scheduler, HTTP server, and other long-running tasks.
    // Each subsystem holds a cloned receiver.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn a task that waits for SIGTERM/SIGINT and then sends the shutdown signal.
    let shutdown_tx_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx_signal.send(true);
    });

    let calendar_dir = PathBuf::from(&config.calendars.dir);
    let calendar_expansion_days = config.calendars.expansion_days;

    // Live battery watcher config — held by AppState so REST handlers can
    // read (and one day patch) it; the receiver is read by the watcher.
    let battery_initial = hc_core::battery_watcher::BatteryConfig {
        threshold_pct: config.battery.threshold_pct,
        recover_band_pct: config.battery.recover_band_pct,
        notify_channel: config.battery.notify_channel.clone(),
        notify_on_recovered: config.battery.notify_on_recovered,
    };
    let (battery_tx, battery_rx) = tokio::sync::watch::channel(battery_initial);
    let battery_tx = Arc::new(battery_tx);

    let mut core = Core::new(
        internal_bus.clone(),
        pub_bus.clone(),
        store.clone(),
        Some(publish_handle.clone()),
    )
    .with_location(config.location.latitude, config.location.longitude)
    .with_modes(modes_path.clone())
    .with_glue(glue_path)
    .with_startup_delay(config.startup.plugin_ready_delay_secs)
    .with_drain_timeout(config.shutdown.drain_timeout_secs)
    .with_catchup_window(config.scheduler.catchup_window_minutes)
    .with_rules_dir(rules_dir.clone())
    .with_calendar_dir(calendar_dir.clone())
    .with_calendar_expansion_days(calendar_expansion_days)
    .with_shutdown(shutdown_rx.clone())
    .with_log_stream(log_tx.clone(), log_ring.clone())
    .with_battery_config(battery_rx);

    let device_types_path = Path::new(&config.profiles.dir).join("device-types.toml");
    match DeviceTypeRegistry::from_file(&device_types_path.to_string_lossy()) {
        Ok(registry) => {
            let count = registry.type_names().count();
            info!(path = %device_types_path.display(), count, "Device type registry loaded");
            core = core.with_device_types(Arc::new(registry));
        }
        Err(_e) if !device_types_path.exists() => {
            info!(path = %device_types_path.display(), "No device type registry found; typed devices will not have auto schemas");
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %device_types_path.display(), "Could not load device type registry")
        }
    }

    // Load ecosystem profiles and build the router.  Done before spawning the
    // MQTT client so add_subscription("#") runs first.
    match load_profiles_from_dir(&config.profiles.dir) {
        Ok(profiles) if !profiles.is_empty() => match EcosystemRouter::new(profiles, None) {
            Ok(router) => {
                mqtt_client.add_subscription("#");
                info!("Ecosystem router ready; subscribed to all topics (#)");
                core = core.with_router(router);
            }
            Err(e) => {
                tracing::warn!(error = %e, "Ecosystem router init failed; running without it")
            }
        },
        Ok(_) => info!(
            "No ecosystem profiles found in {}; running without router",
            config.profiles.dir
        ),
        Err(e) => {
            tracing::warn!(error = %e, "Could not load profiles directory; running without router")
        }
    }

    // ── 13. MQTT forwarder → internal bus ──────────────────────────────────
    // Only MqttMessage events flow through here; typed events go to pub_bus.
    {
        let bus_clone = internal_bus.clone();
        tokio::spawn(async move {
            loop {
                match mqtt_rx.recv().await {
                    Ok(event) => {
                        let _ = bus_clone.publish(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("MQTT→bus forwarder lagged by {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // ── 13b. Notification service + Core.start BEFORE MQTT client runs ─────
    //
    // State_bridge subscribes to internal_bus inside Core::start. It must be
    // live before the MQTT client begins receiving retained messages on
    // `homecore/#`, otherwise those early deliveries (plugin capability
    // manifests in particular) are broadcast to zero subscribers and lost.
    // tokio::broadcast does not buffer for future subscribers.
    // Held so the API layer gets the same instance the rule executor uses.
    let mut notify_service: Option<std::sync::Arc<NotificationService>> = None;
    if !config.notify.channels.is_empty() {
        let count = config.notify.channels.len();
        let svc = std::sync::Arc::new(NotificationService::from_configs(config.notify.channels)?);
        info!(
            channels = count,
            registered = svc.channel_names().len(),
            "Notification service ready"
        );
        notify_service = Some(svc.clone());
        core = core.with_notify(svc);
    }

    let (rules_handle, fire_history, calendar_handle, purge_fn) = core.start(rules).await?;

    // ── 14. MQTT event loop ────────────────────────────────────────────────
    tokio::spawn(async move {
        if let Err(e) = mqtt_client.run().await {
            tracing::error!(error = %e, "MQTT client exited");
        }
    });

    // ── Managed-plugin store (Phase A) ────────────────────────────────────
    // The effective plugin set = static [[plugins]] minus uninstalled
    // (tombstoned) ids, plus runtime-installed managed records (records win on
    // id collision). Reassigning `config.plugins` routes every downstream use —
    // config centralization, record seeding, spawn, hot-reload watcher — through
    // it with no other change. Uninstall tombstones an id in this store so it
    // stays removed across restarts even while still declared in homecore.toml.
    let managed_plugins = std::sync::Arc::new(hc_api::ManagedPluginStore::load(
        base_dir.join("config").join("plugins"),
    ));
    {
        let removed = managed_plugins.removed_ids();
        if !removed.is_empty() {
            info!(?removed, "Managed store: suppressing uninstalled plugin(s)");
        }
    }
    config.plugins = build_effective_plugins(&config.plugins, &managed_plugins, &base_dir);

    // Absolute install root, so an installed plugin's recorded binary/config
    // paths work regardless of the process CWD (dynamic spawn + next boot).
    let install_base = base_dir.canonicalize().unwrap_or_else(|_| base_dir.clone());
    // Channel: the install handler pushes a freshly-installed plugin here and a
    // listener (below) spawns it into the running supervisor — no restart.
    let (plugin_spawn_tx, mut plugin_spawn_rx) =
        tokio::sync::mpsc::channel::<hc_api::InstalledPlugin>(8);

    // ── 15. Launch plugins (after MQTT is subscribed) ─────────────────────
    //
    // Wait for the internal MQTT client to confirm its homecore/# subscription
    // before spawning plugins.  This ensures that registration messages
    // published by plugins on startup are not missed due to a race condition.
    {
        let _ = ready_rx.await;

        // Publish the configured TZ as a retained MQTT message so plugin
        // SDKs can pick it up on connect and apply it to their tracing
        // subscriber. Plugin tracing init runs before broker connect, so
        // the very first log lines render in UTC; the SDK's subscription
        // to this topic delivers the retained payload within a few ms of
        // connect and `hc_time::init` swaps the formatter zone in place.
        // No catch-up logic needed — `RwLock<Tz>` updates are seen by the
        // next log event automatically.
        let tz_name = hc_time::configured_tz().to_string();
        if let Err(e) = publish_handle
            .publish_retained("homecore/system/tz", tz_name.clone().into_bytes())
            .await
        {
            tracing::warn!(error = %e, "Failed to publish retained homecore/system/tz");
        } else {
            tracing::debug!(tz = %tz_name, "Published retained homecore/system/tz");
        }

        // Restore each plugin's durable learned-state as a retained MQTT message
        // so a (re)connecting plugin loads it on subscribe. The broker's retained
        // store is in-memory (lost on restart); redb is the durable source.
        match store.plugin_state_list_ids().await {
            Ok(ids) => {
                for id in ids {
                    if let Ok(Some(doc)) = store.plugin_state_get(&id).await {
                        let bytes = serde_json::to_vec(&doc).unwrap_or_default();
                        let topic = format!("homecore/plugins/{id}/state");
                        if let Err(e) = publish_handle.publish_retained(&topic, bytes).await {
                            tracing::warn!(plugin_id = %id, error = %e, "Failed to restore retained plugin state");
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "Could not list plugin learned-state at boot"),
        }

        // ── Plugin config centralization (Phase 0) ──────────────────────────
        // Move each plugin's config to a core-owned central location
        // (`{base}/config/plugins/<id>.toml`) so a fetch+uncompress upgrade of a
        // plugin can't clobber it, and the API/editor/file-edit all target one
        // file. The import is a one-time byte-copy (preserves comments +
        // secrets); homecore.toml is NOT rewritten — its `config` field stays as
        // the import source. A plugin with nothing to import falls back to its
        // legacy path verbatim, so this is a no-op for anything unexpected.
        let plugin_config_store =
            hc_api::PluginConfigStore::new(base_dir.join("config").join("plugins"));
        let plugin_config_paths = centralize_plugin_configs(
            &plugin_config_store,
            config
                .plugins
                .iter()
                .map(|p| (p.id.as_str(), p.config.as_str())),
        );
        let effective_config = |p: &PluginEntry| {
            plugin_config_paths
                .get(&p.id)
                .cloned()
                .unwrap_or_else(|| p.config.clone())
        };

        // Seed plugin records for ALL configured plugins (enabled and disabled)
        // so the API can list them before registration messages arrive.
        {
            let mut map = plugin_registry.write().await;
            for p in &config.plugins {
                map.entry(p.id.clone())
                    .or_insert_with(|| hc_api::PluginRecord {
                        plugin_id: p.id.clone(),
                        registered_at: chrono::Utc::now(),
                        status: if p.enabled {
                            "starting".into()
                        } else {
                            "stopped".into()
                        },
                        enabled: p.enabled,
                        managed: true,
                        config_path: Some(effective_config(p)),
                        binary_path: Some(p.binary.clone()),
                        last_heartbeat: None,
                        last_restart: None,
                        restart_count: 0,
                        uptime_started: None,
                        device_count: 0,
                        log_level: None,
                        version: None,
                        supports_management: false,
                        capabilities: None,
                        config_schema: None,
                        config_descriptor: None,
                        notices: Vec::new(),
                        installed_version: None,
                    });
            }
            // Stamp installed_version from the managed store (registry/installed
            // plugins) so the UI can compute "update available".
            for rec in managed_plugins.records() {
                if let Some(pr) = map.get_mut(&rec.id) {
                    if !rec.version.is_empty() {
                        pr.installed_version = Some(rec.version.clone());
                    }
                }
            }
        }

        if config.plugins.is_empty() {
            info!("No plugins configured");
        } else {
            let total = config.plugins.len();
            let enabled = config.plugins.iter().filter(|p| p.enabled).count();
            info!(total, enabled, "Launching plugins via PluginManager");
            let processes: Vec<_> = config
                .plugins
                .iter()
                .map(|p| plugin_manager::PluginProcess {
                    id: p.id.clone(),
                    binary: PathBuf::from(&p.binary),
                    config: PathBuf::from(effective_config(p)),
                    enabled: p.enabled,
                })
                .collect();
            plugin_manager::spawn_all(
                processes,
                plugin_registry.clone(),
                plugin_commands.clone(),
                pub_bus.clone(),
                shutdown_rx.clone(),
            )
            .await;
        }
    };

    // Activate freshly-installed plugins without a restart: convert each install
    // request to a supervised process. Lives for the whole run.
    {
        let plugins = plugin_registry.clone();
        let cmds = plugin_commands.clone();
        let bus = pub_bus.clone();
        let sd = shutdown_rx.clone();
        tokio::spawn(async move {
            while let Some(req) = plugin_spawn_rx.recv().await {
                info!(plugin_id = %req.id, "Activating installed plugin (dynamic spawn)");
                plugin_manager::spawn_one(
                    plugin_manager::PluginProcess {
                        id: req.id,
                        binary: PathBuf::from(req.binary),
                        config: PathBuf::from(req.config),
                        enabled: req.enabled,
                    },
                    plugins.clone(),
                    cmds.clone(),
                    bus.clone(),
                    sd.clone(),
                )
                .await;
            }
        });
    }

    // ── 16. Notification service + core.start moved up to 13b so the state
    //        bridge subscribes to internal_bus before the MQTT client begins
    //        delivering retained manifest messages.

    // ── Hot-reload watcher for rule TOML files ─────────────────────────────
    // Must be kept alive for the duration of the process.
    let _rule_watcher = hc_core::rule_loader::RuleWatcher::start(
        rules_dir.clone(),
        store.clone(),
        std::sync::Arc::clone(&source_rules_handle),
        std::sync::Arc::clone(&rules_handle),
        Some(purge_fn),
    )?;

    // Hot-reload plugin config: an operator editing config/plugins/<id>.toml (or
    // an API PUT) restarts that plugin so it re-reads its config. Bound at
    // function scope so the watcher lives for the whole run.
    let _plugin_config_watcher = hc_api::PluginConfigWatcher::start(
        hc_api::PluginConfigStore::new(base_dir.join("config").join("plugins")),
        plugin_commands.clone(),
        config.plugins.iter().map(|p| p.id.clone()).collect(),
    )?;

    // ── 17. JWT service ────────────────────────────────────────────────────
    let jwt_secret_path = config.auth.jwt_secret_file.clone().unwrap_or_else(|| {
        jwt_secret::default_secret_path(std::path::Path::new(&config.storage.state_db_path))
    });
    let jwt_secret_bytes =
        jwt_secret::load_or_create(config.auth.jwt_secret.as_deref(), &jwt_secret_path)?;
    let jwt = JwtService::new_hs256(&jwt_secret_bytes, config.auth.token_expiry_hours);

    // ── 18. Bootstrap default admin account ───────────────────────────────
    let count = store.user_count().await?;
    if count == 0 {
        let password = random_password(16);
        let hash = tokio::task::spawn_blocking({
            let p = password.clone();
            move || hash_password(&p)
        })
        .await??;

        let admin = User {
            id: Uuid::new_v4(),
            username: "admin".to_string(),
            password_hash: hash,
            role: Role::Admin,
            created_at: chrono::Utc::now(),
            token_version: 0,
        };
        store.create_user(&admin).await?;

        // Resolve where to drop the password file. Empty path opts out
        // entirely; default sits at base_dir/INITIAL_ADMIN_PASSWORD.
        // Relative overrides resolve against base_dir, matching the
        // pattern used by storage / rules / profiles / logging paths.
        let pw_file_path: Option<std::path::PathBuf> =
            match config.auth.initial_admin_password_file.as_ref() {
                Some(p) if p.as_os_str().is_empty() => None,
                Some(p) if p.is_absolute() => Some(p.clone()),
                Some(p) => Some(base_dir.join(p)),
                None => Some(default_admin_password_path(&base_dir)),
            };

        if let Some(ref path) = pw_file_path {
            if let Err(e) = write_initial_admin_password(path, &password) {
                tracing::warn!(path = %path.display(), error = %e,
                    "Failed to write initial admin password file — \
                     password is in the log banner below");
            } else {
                tracing::info!(path = %path.display(),
                    "Initial admin password written (delete this file after first login)");
            }
        }

        tracing::warn!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        tracing::warn!("  Default admin account created.");
        tracing::warn!("  Username : admin");
        tracing::warn!("  Password : {password}");
        if let Some(ref path) = pw_file_path {
            tracing::warn!("  Saved to : {}", path.display());
        }
        tracing::warn!("  Change this password immediately after first login!");
        tracing::warn!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    }

    // ── 19. REST + WebSocket API ───────────────────────────────────────────

    let whitelist = parse_auth_whitelist(&config.auth.whitelist);

    if !whitelist.is_empty() {
        info!(
            count = whitelist.len(),
            entries = %whitelist.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "),
            "IP whitelist active — these addresses bypass JWT authentication"
        );
    }

    // Parse the separate metrics whitelist — same lenient parser as the auth
    // whitelist (CIDR or bare IP). Empty list means /metrics is unreachable.
    let metrics_whitelist: Vec<IpNet> = config.metrics.whitelist.iter().filter_map(|s| {
        s.parse::<IpNet>()
            .or_else(|_| s.parse::<std::net::IpAddr>().map(IpNet::from))
            .map_err(|e| tracing::warn!(entry = %s, error = %e, "Invalid metrics whitelist entry — skipping"))
            .ok()
    }).collect();

    if metrics_whitelist.is_empty() {
        info!(
            "/api/v1/metrics is closed to every caller — [metrics].whitelist is not configured. \
             This is the default and is deliberate: the endpoint is opened explicitly, never \
             auto-discovered from the host's network. To allow a scraper, set e.g. \
             `[metrics] whitelist = [\"10.0.0.5/32\"]` in homecore.toml and restart."
        );
    } else {
        info!(
            count = metrics_whitelist.len(),
            entries = %metrics_whitelist.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "),
            "/api/v1/metrics whitelist active — only these source IPs may scrape"
        );
    }

    let rule_file_store = RuleFileStore::new(&rules_dir);

    let group_store = GroupStore::new(groups_path(&rules_dir));
    let groups = group_store.load().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load rule groups — starting with empty group list");
        Vec::new()
    });
    let dashboard_store = DashboardStore::new(base_dir.join("data").join("dashboards.json"));
    let skin_store = SkinStore::new(base_dir.join("data").join("skins.json"));
    // A malformed skins file must not stop the house booting: the built-in four
    // are compiled into the client, so the worst case is the appearance someone
    // had before they wrote it.
    let skin_data = skin_store.load().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not read skins; starting with none");
        Default::default()
    });
    let dashboard_data = dashboard_store.load().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load dashboards — starting with empty dashboard list");
        Default::default()
    });

    // Back up the authoritative (central) config file for each plugin, falling
    // back to the legacy path for anything not migrated. Migration ran during
    // plugin startup above, so central files already exist by now.
    let backup_config_store =
        hc_api::PluginConfigStore::new(base_dir.join("config").join("plugins"));
    let plugin_configs: Vec<hc_api::backup::PluginConfigEntry> = config
        .plugins
        .iter()
        .map(|p| hc_api::backup::PluginConfigEntry {
            id: p.id.clone(),
            path: if backup_config_store.exists(&p.id) {
                backup_config_store.path_for(&p.id)
            } else {
                std::path::PathBuf::from(&p.config)
            },
        })
        .collect();
    let backup_paths = hc_api::backup::BackupPaths {
        state_db_path: std::path::PathBuf::from(&config.storage.state_db_path),
        history_db_path: std::path::PathBuf::from(&config.storage.history_db_path),
        config_path: config_path.clone(),
        rules_dir: rules_dir.clone(),
        plugin_configs,
    };
    let publish_handle_rpc = publish_handle.clone();
    let pub_bus_rpc = pub_bus.clone();

    // InfluxDB v2 metrics exporter (opt-in). Subscribes to pub_bus so it
    // sees the same DeviceStateChanged events the rule engine + WebSocket
    // clients see. Errors during writes are logged and dropped — never
    // block the bus.
    if config.influx.enabled {
        hc_influx::spawn(config.influx.clone(), pub_bus.subscribe());
    }

    let app_state = AppState::new(hc_api::AppStateParams {
        raw_bus: Some(internal_bus.clone()),
        publish: Some(publish_handle),
        source_rules_handle: Some(source_rules_handle),
        rules_handle: Some(rules_handle),
        rule_file_store: Some(rule_file_store),
        whitelist,
        modes_path: Some(modes_path),
        plugins: plugin_registry.clone(),
        ..hc_api::AppStateParams::new(store, pub_bus, jwt)
    });

    let app_state = if config.logging.stream.enabled {
        app_state.with_log_stream(LogStreamState {
            tx: log_tx,
            ring: log_ring,
        })
    } else {
        app_state
    }
    .with_backup_paths(backup_paths)
    .with_fire_history(fire_history)
    .with_group_store(group_store, groups)
    .with_dashboard_store(dashboard_store, dashboard_data)
    .with_skin_store(skin_store, skin_data)
    .with_battery_config(battery_tx)
    .with_homecore_config_path(config_path.clone())
    // Inject the binary crate's CARGO_PKG_VERSION so /health,
    // /system/status, and /system/versions all report the homecore
    // version, not hc-api's. Without this override, AppState defaults
    // to hc-api's version — which caused the v0.1.4 hotfix
    // (operator-visible v0.1.2 inside a v0.1.3 image because hc-api
    // was at 0.1.2). See HEALTH-VERSION-SOURCE-1.
    .with_homecore_version(env!("CARGO_PKG_VERSION"))
    .with_shutdown_tx(shutdown_tx.clone());

    let app_state = if let Some(cal) = calendar_handle {
        app_state.with_calendar(cal, calendar_dir, calendar_expansion_days)
    } else {
        app_state
    }
    .with_plugin_commands(plugin_commands)
    .with_managed_plugins(managed_plugins.clone())
    .with_plugin_install(std::sync::Arc::new(hc_api::InstallContext {
        plugins_dir: install_base.join("plugins"),
        config_plugins_dir: install_base.join("config").join("plugins"),
        broker_host: config.broker.host.clone(),
        broker_port: config.broker.port,
    }))
    .with_plugin_runtimes(config.plugin_runtimes.clone())
    .with_plugin_spawn(plugin_spawn_tx)
    .with_management_rpc(hc_api::management_rpc::ManagementRpc::new(
        publish_handle_rpc,
        &pub_bus_rpc,
    ))
    .with_log_level_handle(log_level_handle)
    .with_uds_allowed_uids(hc_api::admin_uds::resolve_allowed_uids(
        &config.auth.admin_uds.allowed_uids,
    ))
    .with_refresh_token_expiry_days(config.auth.refresh_token_expiry_days)
    .with_metrics_whitelist(metrics_whitelist);

    // The same notification service the rule executor holds, so a send-test
    // exercises the channel that actually delivers.
    let app_state = match notify_service {
        Some(svc) => app_state.with_notify(svc),
        None => app_state,
    };

    // Enable the plugin registry when `[registry]` has both a url and a pubkey.
    let app_state = match (&config.registry.url, &config.registry.public_key) {
        (Some(url), Some(pk)) if !url.trim().is_empty() && !pk.trim().is_empty() => {
            info!(url = %url, "Plugin registry enabled");
            app_state.with_registry(std::sync::Arc::new(hc_api::registry::RegistryClient::new(
                url.clone(),
                pk.clone(),
            )))
        }
        _ => app_state,
    };

    // Reconcile plugin status: plugins that registered before the AppState
    // subscriber was active will still show "starting".  Check device store
    // for evidence of registration and promote to "active".
    {
        let reg = app_state.plugins.clone();
        let store = app_state.store.clone();
        tokio::spawn(async move {
            // Small delay to let any in-flight registrations settle.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(devices) = store.list_devices().await {
                let active_plugins: std::collections::HashSet<String> =
                    devices.iter().map(|d| d.plugin_id.clone()).collect();
                let mut map = reg.write().await;
                for rec in map.values_mut() {
                    if rec.status == "starting" && active_plugins.contains(&rec.plugin_id) {
                        rec.status = "active".into();
                    }
                }
            }
        });
    }

    let api_host = config.server.host.clone();
    let api_port = config.server.port;
    let drain_timeout_secs = config.shutdown.drain_timeout_secs;
    let api_shutdown_rx = shutdown_rx.clone();

    // Resolve web_admin dist_path relative to base_dir.
    let web_admin_dist_path = if config.web_admin.enabled {
        config.web_admin.dist_path.as_ref().map(|p| {
            let path = std::path::Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(p)
            }
        })
    } else {
        None
    };

    // Admin UDS listener (optional). If misconfigured at parse time (e.g.
    // unparseable mode), log and skip — don't fail startup.
    let admin_uds_cfg = if config.auth.admin_uds.enabled {
        match u32::from_str_radix(
            config
                .auth
                .admin_uds
                .mode
                .trim_start_matches("0o")
                .trim_start_matches('0'),
            8,
        ) {
            Ok(mode) => Some(hc_api::AdminUdsConfig {
                path: std::path::PathBuf::from(&config.auth.admin_uds.path),
                group: config.auth.admin_uds.group.clone(),
                mode,
            }),
            Err(e) => {
                tracing::warn!(
                    mode = %config.auth.admin_uds.mode,
                    error = %e,
                    "Invalid auth.admin_uds.mode — admin UDS disabled"
                );
                None
            }
        }
    } else {
        None
    };

    // Periodic prune of used/revoked refresh tokens. Keeps the store from
    // growing unbounded over long uptimes. Fires every hour; cheap.
    {
        let store = app_state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match store.prune_refresh_tokens().await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(pruned = n, "refresh tokens pruned"),
                    Err(e) => tracing::warn!(error = %e, "refresh token prune failed"),
                }
            }
        });
    }

    // Periodic prune of the audit log to honour the retention window.
    // Fires every 6 hours.
    {
        let store = app_state.store.clone();
        let retention_days = config.auth.audit_retention_days as i64;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days);
                match store.prune_audit_before(cutoff).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(pruned = n, "audit log pruned"),
                    Err(e) => tracing::warn!(error = %e, "audit prune failed"),
                }
            }
        });
    }

    let mut api_task = tokio::spawn(async move {
        hc_api::serve(
            &api_host,
            api_port,
            app_state,
            api_shutdown_rx,
            drain_timeout_secs,
            web_admin_dist_path,
            admin_uds_cfg,
        )
        .await
    });
    let shutdown_wait = wait_for_shutdown_watch(shutdown_rx.clone());
    tokio::pin!(shutdown_wait);

    let mut shutdown_requested = false;
    tokio::select! {
        result = &mut api_task => {
            result??;
        }
        _ = &mut shutdown_wait => {
            shutdown_requested = true;
            match tokio::time::timeout(
                Duration::from_secs(drain_timeout_secs + 1),
                &mut api_task,
            )
            .await
            {
                Ok(result) => {
                    result??;
                }
                Err(_) => {
                    warn!(
                        drain_timeout_secs,
                        "HomeCore shutdown timed out waiting for API task — aborting"
                    );
                    api_task.abort();
                    let _ = api_task.await;
                }
            }
        }
    }

    if shutdown_requested {
        info!("HomeCore shutdown sequence complete");
        std::process::exit(0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_whitelist_accepts_single_addresses_in_either_notation() {
        let got = parse_auth_whitelist(&[
            "10.0.10.200".into(),
            "10.0.10.201/32".into(),
            "::1".into(),
            "fe80::1/128".into(),
        ]);
        let rendered: Vec<String> = got.iter().map(|n| n.to_string()).collect();
        assert_eq!(
            rendered,
            vec!["10.0.10.200/32", "10.0.10.201/32", "::1/128", "fe80::1/128"]
        );
    }

    #[test]
    fn auth_whitelist_refuses_ranges() {
        // The live deployment briefly carried 10.0.10.0/24, which granted
        // tokenless admin to every host on the VLAN. That must not parse.
        let got = parse_auth_whitelist(&[
            "10.0.10.0/24".into(),
            "10.0.0.0/8".into(),
            "0.0.0.0/0".into(),
            "2001:db8::/32".into(),
        ]);
        assert!(got.is_empty(), "ranges must be refused, got: {got:?}");
    }

    #[test]
    fn auth_whitelist_skips_bad_entries_without_dropping_good_ones() {
        // One typo should not cost the operator the entries either side of it,
        // and should not take startup down.
        let got = parse_auth_whitelist(&[
            "10.0.10.200".into(),
            "not-an-ip".into(),
            "10.0.10.0/24".into(),
            "10.0.10.201".into(),
        ]);
        let rendered: Vec<String> = got.iter().map(|n| n.to_string()).collect();
        assert_eq!(rendered, vec!["10.0.10.200/32", "10.0.10.201/32"]);
    }

    #[test]
    fn auth_whitelist_empty_stays_empty() {
        // Empty means "no bypass at all" — it must never widen to a default.
        assert!(parse_auth_whitelist(&[]).is_empty());
    }

    #[test]
    fn centralize_imports_legacy_and_returns_central_path() {
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("config.toml");
        let original = "[yolink]\nmode = \"local\"\n# keep this comment\n";
        std::fs::write(&legacy, original).unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let store = hc_api::PluginConfigStore::new(store_dir.path());

        let map = centralize_plugin_configs(&store, [("plugin.yolink", legacy.to_str().unwrap())]);

        // Effective path is now the central file, and it's a byte-for-byte copy.
        let central = store.path_for("plugin.yolink");
        assert_eq!(map["plugin.yolink"], central.to_string_lossy());
        assert_eq!(std::fs::read_to_string(&central).unwrap(), original);
    }

    #[test]
    fn centralize_is_idempotent_and_authoritative_after_import() {
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("config.toml");
        std::fs::write(&legacy, "v = 1\n").unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let store = hc_api::PluginConfigStore::new(store_dir.path());

        centralize_plugin_configs(&store, [("plugin.a", legacy.to_str().unwrap())]);
        // A later legacy edit must not overwrite the now-authoritative central copy.
        std::fs::write(&legacy, "v = 2\n").unwrap();
        let map = centralize_plugin_configs(&store, [("plugin.a", legacy.to_str().unwrap())]);

        assert_eq!(
            map["plugin.a"],
            store.path_for("plugin.a").to_string_lossy()
        );
        assert_eq!(store.read("plugin.a").unwrap(), "v = 1\n");
    }

    #[test]
    fn centralize_falls_back_to_legacy_path_when_nothing_to_import() {
        // No file at the legacy path and no central file → effective path is the
        // legacy string verbatim, matching pre-centralization behavior exactly.
        let store_dir = tempfile::tempdir().unwrap();
        let store = hc_api::PluginConfigStore::new(store_dir.path());

        let map = centralize_plugin_configs(
            &store,
            [("plugin.missing", "plugins/hc-missing/config/config.toml")],
        );

        assert_eq!(
            map["plugin.missing"],
            "plugins/hc-missing/config/config.toml"
        );
        assert!(!store.exists("plugin.missing"));
    }
}
