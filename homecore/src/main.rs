mod plugin_launcher;

use anyhow::Result;
use hc_api::{rule_file_store::RuleFileStore, AppState};
use hc_auth::{hash_password, JwtService, Role, User};
use hc_broker::{Broker, BrokerConfig, ClientAcl};
use hc_core::{rule_loader, Core, EventBus};
use hc_logging::LoggingConfig;
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_notify::{ChannelConfig, NotificationService};
use hc_state::StateStore;
use hc_topic_map::{loader::load_profiles_from_dir, EcosystemRouter};
use ipnet::IpNet;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tracing::info;
use uuid::Uuid;

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
                return if path.is_relative() { base.join(path) } else { path };
            }
        }
    }
    if let Ok(p) = std::env::var("HOMECORE_CONFIG") {
        if !p.is_empty() {
            let path = PathBuf::from(p);
            return if path.is_relative() { base.join(path) } else { path };
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
    logging: LoggingConfig,
    #[serde(default)]
    plugins: Vec<PluginEntry>,
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

fn default_true() -> bool { true }

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
    fn default() -> Self { Self { dir: String::new() } }
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
        Self { host: default_server_host(), port: default_server_port() }
    }
}

fn default_server_host() -> String { "0.0.0.0".into() }
fn default_server_port() -> u16 { 8080 }

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
        resolve_path(&mut self.state_db_path,   base, "data/state.redb");
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
    fn default() -> Self { Self { dir: String::new() } }
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
        resolve_opt_path(&mut self.key_path,  base);
    }
}

fn default_broker_host() -> String { "0.0.0.0".into() }
fn default_broker_v5_port() -> Option<u16> { Some(1884) }
fn default_broker_port() -> u16 { 1883 }

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

#[derive(Deserialize)]
struct LocationSection {
    latitude: f64,
    longitude: f64,
}

impl Default for LocationSection {
    fn default() -> Self {
        Self { latitude: 38.9072, longitude: -77.0369 }
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

fn default_expiry() -> u64 { 24 }

impl Default for AuthSection {
    fn default() -> Self {
        Self { jwt_secret: None, token_expiry_hours: 24, whitelist: vec![] }
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
            Ok(c)  => c,
            Err(e) => {
                eprintln!("Warning: config parse error in {}: {e}; using defaults",
                    config_path.display());
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
    for subdir in &["config/profiles", "data", "logs", "rules"] {
        if let Err(e) = std::fs::create_dir_all(base_dir.join(subdir)) {
            eprintln!("Warning: could not create {}/{subdir}: {e}", base_dir.display());
        }
    }

    // ── 5. Initialise logging from config ──────────────────────────────────
    //
    // _logging_handle must remain in scope until the end of main() so the
    // background file-writer thread stays alive.
    let _logging_handle = hc_logging::init(&config.logging)?;

    info!(base = %base_dir.display(), config = %config_path.display(), "HomeCore starting");

    // ── 6. Embedded MQTT broker ────────────────────────────────────────────
    let broker_cfg = BrokerConfig {
        host:      config.broker.host.clone(),
        port:      config.broker.port,
        v5_port:   config.broker.v5_port,
        tls_port:  config.broker.tls_port,
        cert_path: config.broker.cert_path.clone(),
        key_path:  config.broker.key_path.clone(),
        clients:   config.broker.clients.iter().map(|c| ClientAcl {
            client_id:  c.id.clone(),
            password:   c.password.clone(),
            allow_pub:  c.allow_pub.clone(),
            allow_sub:  c.allow_sub.clone(),
        }).collect(),
    };
    Broker::new(broker_cfg).spawn()?;

    // ── 9. Internal MQTT client ────────────────────────────────────────────
    let internal_cred = config.broker.clients.iter()
        .find(|c| c.id == "internal.core")
        .cloned();
    let mqtt_cfg = MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: config.broker.port,
        client_id:   "internal.core".into(),
        username:    internal_cred.as_ref().map(|c| c.id.clone()),
        password:    internal_cred.as_ref().map(|c| c.password.clone()),
    };
    let (mut mqtt_client, mut mqtt_rx) = MqttClient::new(mqtt_cfg);
    let publish_handle = mqtt_client.publish_handle();

    // Set up a ready signal so plugins are launched only after the internal
    // MQTT client has subscribed to homecore/#.  This prevents registration
    // messages from being published before anyone is listening.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    mqtt_client.set_ready_notify(ready_tx);

    // ── 10. State store ─────────────────────────────────────────────────────
    let store = StateStore::open(&config.storage.state_db_path, &config.storage.history_db_path).await?;

    // ── 11. Event bus ───────────────────────────────────────────────────────
    let bus = EventBus::new(1024);

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
                    info!(count = migrated.len(), "Rules migrated and loaded from files");
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

    let modes_path = base_dir.join("config").join("modes.toml");

    let mut core = Core::new(bus.clone(), store.clone(), Some(publish_handle.clone()))
        .with_location(config.location.latitude, config.location.longitude)
        .with_modes(modes_path.clone());

    // Load ecosystem profiles and build the router.  Done before spawning the
    // MQTT client so add_subscription("#") runs first.
    match load_profiles_from_dir(&config.profiles.dir) {
        Ok(profiles) if !profiles.is_empty() => {
            match EcosystemRouter::new(profiles, None) {
                Ok(router) => {
                    mqtt_client.add_subscription("#");
                    info!("Ecosystem router ready; subscribed to all topics (#)");
                    core = core.with_router(router);
                }
                Err(e) => tracing::warn!(error = %e, "Ecosystem router init failed; running without it"),
            }
        }
        Ok(_) => info!("No ecosystem profiles found in {}; running without router", config.profiles.dir),
        Err(e) => tracing::warn!(error = %e, "Could not load profiles directory; running without router"),
    }

    // ── 13. MQTT forwarder → EventBus ──────────────────────────────────────
    {
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            loop {
                match mqtt_rx.recv().await {
                    Ok(event) => { let _ = bus_clone.publish(event); }
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

        let enabled: Vec<_> = config.plugins.iter()
            .filter(|p| p.enabled)
            .collect();

        if enabled.is_empty() {
            info!("No plugins configured");
        } else {
            info!(count = enabled.len(), "Launching plugins");
            let processes = enabled.into_iter().map(|p| {
                plugin_launcher::PluginProcess {
                    id:     p.id.clone(),
                    binary: PathBuf::from(&p.binary),
                    config: PathBuf::from(&p.config),
                }
            }).collect();
            plugin_launcher::spawn_all(processes);
        }
    }

    // ── 16. Notification service ───────────────────────────────────────────
    if !config.notify.channels.is_empty() {
        let count = config.notify.channels.len();
        let svc = NotificationService::from_configs(config.notify.channels);
        info!(channels = count, registered = svc.channel_names().len(), "Notification service ready");
        core = core.with_notify(svc);
    }

    let rules_handle = core.start(rules).await?;

    // ── Hot-reload watcher for rule TOML files ─────────────────────────────
    // Must be kept alive for the duration of the process.
    let _rule_watcher = hc_core::rule_loader::RuleWatcher::start(
        rules_dir.clone(),
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
        tracing::warn!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        );
        tracing::warn!("  Default admin account created.");
        tracing::warn!("  Username : admin");
        tracing::warn!("  Password : {password}");
        tracing::warn!("  Change this password immediately after first login!");
        tracing::warn!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        );
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
    let app_state = AppState::new(
        store,
        bus,
        Some(publish_handle),
        Some(rules_handle),
        Some(rule_file_store),
        jwt,
        whitelist,
        Some(modes_path),
    );
    hc_api::serve(&config.server.host, config.server.port, app_state).await?;

    Ok(())
}
