mod plugin_launcher;

use anyhow::Result;
use hc_api::{
    dashboard_store::DashboardStore,
    group_store::{groups_path, GroupStore},
    logs::LogStreamState,
    rule_file_store::RuleFileStore,
    AppState,
};
use hc_auth::{hash_password, JwtService, Role, User};
use hc_broker::{Broker, BrokerConfig, ClientAcl};
use hc_core::{device_naming, rule_loader, rule_resolver, Core, EventBus};
use hc_logging::LoggingConfig;
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_notify::{ChannelConfig, NotificationService};
use hc_state::StateStore;
use hc_topic_map::{loader::load_profiles_from_dir, DeviceTypeRegistry, EcosystemRouter};
use ipnet::IpNet;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
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

/// Resolve a path string field:
///   - empty string  → `{base}/{relative_default}`
///   - relative path → `{base}/{path}`
///   - absolute path → unchanged
fn resolve_path(field: &mut String, base: &Path, relative_default: &str) {
    if field.is_empty() {
        *field = base.join(relative_default).to_string_lossy().into_owned();
    } else if !Path::new(field.as_str()).is_absolute() {
        *field = base.join(field.as_str()).to_string_lossy().into_owned();
    }
}

/// Resolve an optional path string: only touches it when Some and relative.
fn resolve_opt_path(field: &mut Option<String>, base: &Path) {
    if let Some(p) = field {
        if !Path::new(p.as_str()).is_absolute() {
            *field = Some(base.join(p.as_str()).to_string_lossy().into_owned());
        }
    }
}

// ── config structs ──────────────────────────────────────────────────────────

/// Top-level config shape (subset — just what main.rs needs to parse).
#[derive(Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    broker: BrokerSection,
    #[serde(default)]
    location: LocationSection,
    #[serde(default)]
    storage: StorageSection,
    #[serde(default)]
    profiles: ProfilesSection,
    #[serde(default)]
    rules: RulesSection,
    #[serde(default)]
    auth: AuthSection,
    #[serde(default)]
    notify: NotifySection,
    #[serde(default)]
    startup: StartupSection,
    #[serde(default)]
    shutdown: ShutdownConfig,
    #[serde(default)]
    scheduler: SchedulerSection,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    plugins: Vec<PluginEntry>,
    #[serde(default)]
    calendars: CalendarsSection,
}

impl AppConfig {
    /// Fill in any empty/relative path fields using `base_dir` as the root.
    /// Called after loading the TOML file so explicit absolute paths in config
    /// are always honoured; only unset (empty) or relative paths are resolved.
    fn resolve_paths(&mut self, base: &Path) {
        self.storage.resolve(base);
        self.profiles.resolve(base);
        self.rules.resolve(base);
        self.broker.resolve(base);
        self.logging.resolve_paths(base);
        self.calendars.resolve(base);
        for plugin in &mut self.plugins {
            plugin.resolve(base);
        }
    }
}

/// A single `[[plugins]]` entry — a plugin binary HomeCore will spawn and
/// supervise.
#[derive(Deserialize, Clone)]
struct PluginEntry {
    /// Identifier used in log messages (e.g. "plugin.yolink").
    id: String,
    /// Path to the compiled plugin binary.
    /// Relative paths are resolved against base_dir.
    binary: String,
    /// Path to the plugin's config file, passed as its first argument.
    /// Relative paths are resolved against base_dir.
    config: String,
    /// Set to false to disable this plugin without removing the entry.
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

impl PluginEntry {
    fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.binary, base, "");
        resolve_path(&mut self.config, base, "");
    }
}

/// `[rules]` section of homecore.toml.
#[derive(Deserialize)]
struct RulesSection {
    /// Directory containing per-rule TOML files.
    /// Default: `{base_dir}/rules`
    #[serde(default)]
    dir: String,
}

impl Default for RulesSection {
    fn default() -> Self {
        Self { dir: String::new() }
    }
}

impl RulesSection {
    fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "rules");
    }
}

#[derive(Deserialize)]
struct ServerSection {
    #[serde(default = "default_server_host")]
    host: String,
    #[serde(default = "default_server_port")]
    port: u16,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: default_server_host(),
            port: default_server_port(),
        }
    }
}

fn default_server_host() -> String {
    "0.0.0.0".into()
}
fn default_server_port() -> u16 {
    8080
}

#[derive(Deserialize, Default)]
struct StorageSection {
    /// Path to the redb state database.
    /// Default: `{base_dir}/data/state.redb`
    #[serde(default)]
    state_db_path: String,
    /// Path to the SQLite history database.
    /// Default: `{base_dir}/data/history.db`
    #[serde(default)]
    history_db_path: String,
}

impl StorageSection {
    fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.state_db_path, base, "data/state.redb");
        resolve_path(&mut self.history_db_path, base, "data/history.db");
    }
}

#[derive(Deserialize)]
struct ProfilesSection {
    /// Directory containing ecosystem profile TOML files (Shelly, Tasmota, etc.).
    /// Default: `{base_dir}/config/profiles`
    #[serde(default)]
    dir: String,
}

impl Default for ProfilesSection {
    fn default() -> Self {
        Self { dir: String::new() }
    }
}

impl ProfilesSection {
    fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "config/profiles");
    }
}

/// `[broker]` section of homecore.toml.
#[derive(Deserialize)]
struct BrokerSection {
    #[serde(default = "default_broker_host")]
    host: String,
    #[serde(default = "default_broker_port")]
    port: u16,
    /// MQTT v5 listener port. Defaults to port+1 (1884 when port is 1883).
    /// Set to null to disable.
    #[serde(default = "default_broker_v5_port")]
    v5_port: Option<u16>,
    tls_port: Option<u16>,
    /// Path to TLS certificate file.  Relative paths are resolved against
    /// base_dir; absolute paths are used as-is.
    cert_path: Option<String>,
    /// Path to TLS private key file.  Same resolution rules as cert_path.
    key_path: Option<String>,
    /// Per-client credentials.  When any entries are present the broker
    /// requires authentication on all connections.
    #[serde(default)]
    clients: Vec<ClientAclConfig>,
}

impl Default for BrokerSection {
    fn default() -> Self {
        Self {
            host: default_broker_host(),
            port: default_broker_port(),
            v5_port: default_broker_v5_port(),
            tls_port: None,
            cert_path: None,
            key_path: None,
            clients: vec![],
        }
    }
}

impl BrokerSection {
    fn resolve(&mut self, base: &Path) {
        resolve_opt_path(&mut self.cert_path, base);
        resolve_opt_path(&mut self.key_path, base);
    }
}

fn default_broker_host() -> String {
    "0.0.0.0".into()
}
fn default_broker_v5_port() -> Option<u16> {
    Some(1884)
}
fn default_broker_port() -> u16 {
    1883
}

/// A single `[[broker.clients]]` entry.
#[derive(Deserialize, Clone)]
struct ClientAclConfig {
    id: String,
    password: String,
    #[serde(default)]
    allow_pub: Vec<String>,
    #[serde(default)]
    allow_sub: Vec<String>,
}

#[derive(Deserialize, Default)]
struct NotifySection {
    #[serde(default)]
    channels: Vec<ChannelConfig>,
}

/// `[startup]` section of homecore.toml.
#[derive(Deserialize)]
struct StartupSection {
    /// Seconds to wait after launch before publishing initial mode states.
    ///
    /// Plugins need time to connect and subscribe to their cmd topics.
    /// If a rule fires during this window (e.g. mode_night already on at
    /// restart) and the target plugin hasn't subscribed yet, the command is
    /// silently dropped.  Increase this value if you have plugins with long
    /// startup times.  Default: 10 s.
    #[serde(default = "default_startup_delay")]
    plugin_ready_delay_secs: u64,
}

fn default_startup_delay() -> u64 {
    10
}

impl Default for StartupSection {
    fn default() -> Self {
        Self {
            plugin_ready_delay_secs: default_startup_delay(),
        }
    }
}

/// `[shutdown]` section of homecore.toml.
#[derive(Deserialize)]
struct ShutdownConfig {
    /// Seconds to wait for in-flight rule action tasks to finish during graceful
    /// shutdown before forcing a stop.  Default: 10 s.
    #[serde(default = "default_drain_timeout")]
    drain_timeout_secs: u64,
}

fn default_drain_timeout() -> u64 {
    10
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: default_drain_timeout(),
        }
    }
}

/// `[calendars]` section of homecore.toml.
#[derive(Deserialize)]
struct CalendarsSection {
    /// Directory containing `.ics` calendar files.
    /// Default: `{base_dir}/config/calendars`
    #[serde(default)]
    dir: String,
    /// How many days forward to expand recurring events.  Default: 400.
    #[serde(default = "default_expansion_days")]
    expansion_days: u32,
}

fn default_expansion_days() -> u32 {
    400
}

impl Default for CalendarsSection {
    fn default() -> Self {
        Self {
            dir: String::new(),
            expansion_days: default_expansion_days(),
        }
    }
}

impl CalendarsSection {
    fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "config/calendars");
    }
}

/// `[scheduler]` section of homecore.toml.
#[derive(Deserialize)]
struct SchedulerSection {
    /// How many minutes back from startup to search for missed time-based
    /// triggers (SunEvent and TimeOfDay).  Any rule whose scheduled time falls
    /// within `(now - window, now]` is fired immediately on startup so that a
    /// brief process restart does not silently skip an automation.
    ///
    /// Set to 0 to disable catch-up entirely.  Default: 15.
    #[serde(default = "default_catchup_window")]
    catchup_window_minutes: u32,
}

fn default_catchup_window() -> u32 {
    15
}

impl Default for SchedulerSection {
    fn default() -> Self {
        Self {
            catchup_window_minutes: default_catchup_window(),
        }
    }
}

#[derive(Deserialize)]
struct LocationSection {
    latitude: f64,
    longitude: f64,
}

impl Default for LocationSection {
    fn default() -> Self {
        Self {
            latitude: 38.9072,
            longitude: -77.0369,
        }
    }
}

#[derive(Deserialize)]
struct AuthSection {
    /// HMAC-SHA256 secret for signing JWTs.  If not set, a random secret is
    /// generated at startup (tokens will be invalidated on restart).
    jwt_secret: Option<String>,
    #[serde(default = "default_expiry")]
    token_expiry_hours: u64,
    /// IP addresses or CIDR ranges that may access all API endpoints without
    /// a JWT.  Requests from these addresses receive full Admin access.
    /// Parsed as standard CIDR notation.  Both IPv4 and IPv6 are supported.
    /// Example: ["127.0.0.1/32", "::1/128", "192.168.1.0/24"]
    #[serde(default)]
    whitelist: Vec<String>,
}

fn default_expiry() -> u64 {
    24
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            jwt_secret: None,
            token_expiry_hours: 24,
            whitelist: vec![],
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Generate a random alphanumeric password of the given length.
fn random_password(len: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(42);
    let pid = std::process::id() as u128;

    let charset: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut result = String::with_capacity(len);
    let mut state = seed ^ (pid << 32);
    for i in 0..len {
        let mut h = DefaultHasher::new();
        (state ^ (i as u128 * 0x9e3779b97f4a7c15)).hash(&mut h);
        state = h.finish() as u128;
        result.push(charset[state as usize % charset.len()] as char);
    }
    result
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
    let (_logging_handle, log_tx, log_ring) =
        hc_logging::init_with_broadcast(&config.logging, config.logging.stream.ring_buffer_size)?;

    info!(base = %base_dir.display(), config = %config_path.display(), "HomeCore starting");

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

    // ── Graceful shutdown channel ──────────────────────────────────────────
    //
    // `shutdown_tx` is used by the signal handler task to broadcast to the
    // rule engine, scheduler, and HTTP server.  `shutdown_rx` is cloned for
    // each subsystem.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn a task that waits for SIGTERM/SIGINT and then sends the shutdown signal.
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let calendar_dir = PathBuf::from(&config.calendars.dir);
    let calendar_expansion_days = config.calendars.expansion_days;

    let mut core = Core::new(
        internal_bus.clone(),
        pub_bus.clone(),
        store.clone(),
        Some(publish_handle.clone()),
    )
    .with_location(config.location.latitude, config.location.longitude)
    .with_modes(modes_path.clone())
    .with_startup_delay(config.startup.plugin_ready_delay_secs)
    .with_drain_timeout(config.shutdown.drain_timeout_secs)
    .with_catchup_window(config.scheduler.catchup_window_minutes)
    .with_rules_dir(rules_dir.clone())
    .with_calendar_dir(calendar_dir.clone())
    .with_calendar_expansion_days(calendar_expansion_days)
    .with_shutdown(shutdown_rx.clone());

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

    // ── 14. MQTT event loop ────────────────────────────────────────────────
    tokio::spawn(async move {
        if let Err(e) = mqtt_client.run().await {
            tracing::error!(error = %e, "MQTT client exited");
        }
    });

    // ── 15. Launch plugins (after MQTT is subscribed) ─────────────────────
    //
    // Wait for the internal MQTT client to confirm its homecore/# subscription
    // before spawning plugins.  This ensures that registration messages
    // published by plugins on startup are not missed due to a race condition.
    {
        let _ = ready_rx.await;

        let enabled: Vec<_> = config.plugins.iter().filter(|p| p.enabled).collect();

        if enabled.is_empty() {
            info!("No plugins configured");
        } else {
            info!(count = enabled.len(), "Launching plugins");
            let processes = enabled
                .into_iter()
                .map(|p| plugin_launcher::PluginProcess {
                    id: p.id.clone(),
                    binary: PathBuf::from(&p.binary),
                    config: PathBuf::from(&p.config),
                })
                .collect();
            plugin_launcher::spawn_all(processes);
        }
    }

    // ── 16. Notification service ───────────────────────────────────────────
    if !config.notify.channels.is_empty() {
        let count = config.notify.channels.len();
        let svc = NotificationService::from_configs(config.notify.channels);
        info!(
            channels = count,
            registered = svc.channel_names().len(),
            "Notification service ready"
        );
        core = core.with_notify(svc);
    }

    let (rules_handle, fire_history, calendar_handle) = core.start(rules).await?;

    // ── Hot-reload watcher for rule TOML files ─────────────────────────────
    // Must be kept alive for the duration of the process.
    let _rule_watcher = hc_core::rule_loader::RuleWatcher::start(
        rules_dir.clone(),
        store.clone(),
        std::sync::Arc::clone(&source_rules_handle),
        std::sync::Arc::clone(&rules_handle),
    )?;

    // ── 17. JWT service ────────────────────────────────────────────────────
    let jwt_secret = match &config.auth.jwt_secret {
        Some(s) => s.clone(),
        None => {
            tracing::warn!("No jwt_secret configured — generating a random secret. Tokens will not survive restarts.");
            random_password(64)
        }
    };
    let jwt = JwtService::new_hs256(jwt_secret.as_bytes(), config.auth.token_expiry_hours);

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
        };
        store.create_user(&admin).await?;
        tracing::warn!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        tracing::warn!("  Default admin account created.");
        tracing::warn!("  Username : admin");
        tracing::warn!("  Password : {password}");
        tracing::warn!("  Change this password immediately after first login!");
        tracing::warn!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    }

    // ── 19. REST + WebSocket API ───────────────────────────────────────────

    // Parse IP whitelist CIDRs.  Invalid entries are skipped with a warning
    // rather than failing startup — a typo in the whitelist shouldn't take
    // down the server.
    let whitelist: Vec<IpNet> = config.auth.whitelist.iter().filter_map(|s| {
        // Accept both CIDR notation ("10.0.0.1/32") and bare IPs ("10.0.0.1").
        s.parse::<IpNet>()
            .or_else(|_| s.parse::<std::net::IpAddr>().map(IpNet::from))
            .map_err(|e| tracing::warn!(entry = %s, error = %e, "Invalid whitelist entry — skipping"))
            .ok()
    }).collect();

    if !whitelist.is_empty() {
        info!(
            count = whitelist.len(),
            entries = %whitelist.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "),
            "IP whitelist active — these addresses bypass JWT authentication"
        );
    }

    let rule_file_store = RuleFileStore::new(&rules_dir);

    let group_store = GroupStore::new(groups_path(&rules_dir));
    let groups = group_store.load().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load rule groups — starting with empty group list");
        Vec::new()
    });
    let dashboard_store = DashboardStore::new(base_dir.join("data").join("dashboards.json"));
    let dashboard_data = dashboard_store.load().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load dashboards — starting with empty dashboard list");
        Default::default()
    });

    let backup_paths = hc_api::backup::BackupPaths {
        state_db_path: std::path::PathBuf::from(&config.storage.state_db_path),
        history_db_path: std::path::PathBuf::from(&config.storage.history_db_path),
        config_path: config_path.clone(),
        rules_dir: rules_dir.clone(),
    };
    let app_state = AppState::new(
        store,
        pub_bus,
        Some(publish_handle),
        Some(source_rules_handle),
        Some(rules_handle),
        Some(rule_file_store),
        jwt,
        whitelist,
        Some(modes_path),
    );

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
    .with_dashboard_store(dashboard_store, dashboard_data);

    let app_state = if let Some(cal) = calendar_handle {
        app_state.with_calendar(cal, calendar_dir, calendar_expansion_days)
    } else {
        app_state
    };
    hc_api::serve(
        &config.server.host,
        config.server.port,
        app_state,
        shutdown_rx,
    )
    .await?;

    Ok(())
}
