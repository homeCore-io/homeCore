mod jwt_secret;
mod plugin_manager;

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
use hc_influx::InfluxConfig;
use hc_logging::LoggingConfig;
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_notify::{ChannelConfig, NotificationService};
use hc_state::StateStore;
use hc_topic_map::{loader::load_profiles_from_dir, DeviceTypeRegistry, EcosystemRouter};
use ipnet::IpNet;
use serde::Deserialize;
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
    web_admin: WebAdminSection,
    #[serde(default)]
    plugins: Vec<PluginEntry>,
    #[serde(default)]
    calendars: CalendarsSection,
    #[serde(default)]
    battery: BatterySection,
    #[serde(default)]
    influx: InfluxConfig,
    #[serde(default)]
    metrics: MetricsSection,
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
#[derive(Deserialize, Default)]
struct RulesSection {
    /// Directory containing per-rule TOML files.
    /// Default: `{base_dir}/rules`
    #[serde(default)]
    dir: String,
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

/// `[battery]` section of homecore.toml — drives the battery watcher.
#[derive(Deserialize, Clone)]
struct BatterySection {
    /// Battery percentage at or below which the latch engages.
    #[serde(default = "default_battery_threshold")]
    threshold_pct: f64,
    /// Recovery band added to threshold to clear the latch.
    #[serde(default = "default_battery_recover")]
    recover_band_pct: f64,
    /// Optional hc-notify channel for the built-in notification shortcut.
    /// Leave unset to disable the shortcut (rules-engine still receives the
    /// `device.battery_low` events either way).
    #[serde(default)]
    notify_channel: Option<String>,
    /// When true and `notify_channel` is set, recovery edges also notify.
    #[serde(default)]
    notify_on_recovered: bool,
}

impl Default for BatterySection {
    fn default() -> Self {
        Self {
            threshold_pct: default_battery_threshold(),
            recover_band_pct: default_battery_recover(),
            notify_channel: None,
            notify_on_recovered: false,
        }
    }
}

fn default_battery_threshold() -> f64 {
    20.0
}
fn default_battery_recover() -> f64 {
    5.0
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

#[derive(Deserialize, Default)]
struct ProfilesSection {
    /// Directory containing ecosystem profile TOML files (Shelly, Tasmota, etc.).
    /// Default: `{base_dir}/config/profiles`
    #[serde(default)]
    dir: String,
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

/// `[web_admin]` section of homecore.toml.
#[derive(Deserialize, Default)]
struct WebAdminSection {
    /// Enable the built-in admin UI served by HomeCore.
    ///
    /// When enabled, HomeCore serves the pre-built Leptos/WASM admin UI
    /// as static files and preserves the API under `/api/v1`.
    /// Requires `dist_path` to point to a valid `trunk build` output directory.
    #[serde(default)]
    enabled: bool,

    /// Path to the Leptos UI build output directory (trunk build --release).
    /// Relative paths are resolved against base_dir.
    /// Required when enabled = true.
    #[serde(default)]
    dist_path: Option<String>,
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

/// `[metrics]` section — gates `GET /api/v1/metrics` by source IP.
///
/// Prometheus scrapers can't easily set Authorization headers, so the
/// metrics endpoint is gated by network identity instead. The whitelist
/// defaults to empty, which means **no IPs are allowed** — operators must
/// explicitly list the scrape source(s) before metrics become reachable.
#[derive(Deserialize, Default)]
struct MetricsSection {
    /// IP addresses or CIDR ranges allowed to scrape `/api/v1/metrics`.
    /// Both IPv4 and IPv6 are supported.
    /// Example: `whitelist = ["127.0.0.1/32", "10.0.0.0/24"]`.
    /// Empty (default) means the endpoint returns 403 to every caller.
    #[serde(default)]
    whitelist: Vec<String>,
}

#[derive(Deserialize)]
struct AuthSection {
    /// HMAC-SHA256 secret for signing JWTs. **Deprecated** — prefer leaving
    /// this unset and letting the core manage `jwt_secret_file` automatically.
    /// If set, takes precedence over `jwt_secret_file` and emits a warning.
    jwt_secret: Option<String>,
    /// Path to a file holding the persistent JWT HS256 secret. When unset,
    /// defaults to `<parent-of-state_db_path>/jwt_secret`. The file is
    /// auto-generated with 0600 perms on first startup and re-used across
    /// restarts so issued tokens survive reboots.
    #[serde(default)]
    jwt_secret_file: Option<std::path::PathBuf>,
    #[serde(default = "default_expiry")]
    token_expiry_hours: u64,
    /// Refresh-token lifetime in days. A successful login also returns a
    /// long-lived refresh token; each `/auth/refresh` call rotates it.
    /// Default: 30 days.
    #[serde(default = "default_refresh_days")]
    refresh_token_expiry_days: u64,
    /// How many days of audit-log history to keep. Entries older than this
    /// are pruned by a background task that runs every 6 hours.
    /// Default: 365 days.
    #[serde(default = "default_audit_retention_days")]
    audit_retention_days: u64,
    /// IP addresses or CIDR ranges that may access all API endpoints without
    /// a JWT.  Requests from these addresses receive full Admin access.
    /// Parsed as standard CIDR notation.  Both IPv4 and IPv6 are supported.
    /// Example: ["127.0.0.1/32", "::1/128", "192.168.1.0/24"]
    ///
    /// **Deprecated** — prefer `[auth.admin_uds]` for same-host admin
    /// tooling. This option will be removed in a future release.
    #[serde(default)]
    whitelist: Vec<String>,
    /// Admin-only Unix domain socket listener for `hc-cli` and other
    /// same-host admin tooling. Replaces the CIDR whitelist.
    #[serde(default)]
    admin_uds: AdminUdsSection,
    /// Path where the auto-generated initial admin password is written
    /// the first time homeCore boots with no users in the store. Set to
    /// the empty string to disable file output (password is still
    /// printed to logs).
    ///
    /// Defaults to `<parent-of-state_db_path>/INITIAL_ADMIN_PASSWORD`,
    /// 0600. The file should be deleted by the operator after first
    /// login; homeCore does NOT re-write it on subsequent boots.
    #[serde(default)]
    initial_admin_password_file: Option<std::path::PathBuf>,
}

#[derive(Deserialize, Clone)]
struct AdminUdsSection {
    #[serde(default)]
    enabled: bool,
    /// Default: `/run/homecore/admin.sock`.
    #[serde(default = "default_admin_uds_path")]
    path: String,
    /// POSIX group that owns the socket. Members of this group can connect.
    #[serde(default = "default_admin_uds_group")]
    group: String,
    /// Mode for the socket file, as an octal string (e.g. "0660").
    #[serde(default = "default_admin_uds_mode")]
    mode: String,
    /// Extra UIDs allowed to connect. The process UID is always allowed.
    #[serde(default)]
    allowed_uids: Vec<u32>,
}

fn default_admin_uds_path() -> String {
    "/run/homecore/admin.sock".into()
}
fn default_admin_uds_group() -> String {
    "homecore-admin".into()
}
fn default_admin_uds_mode() -> String {
    "0660".into()
}

impl Default for AdminUdsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_admin_uds_path(),
            group: default_admin_uds_group(),
            mode: default_admin_uds_mode(),
            allowed_uids: vec![],
        }
    }
}

fn default_expiry() -> u64 {
    24
}

fn default_refresh_days() -> u64 {
    30
}

fn default_audit_retention_days() -> u64 {
    365
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            jwt_secret: None,
            jwt_secret_file: None,
            token_expiry_hours: 24,
            refresh_token_expiry_days: 30,
            audit_retention_days: 365,
            whitelist: vec![],
            admin_uds: AdminUdsSection::default(),
            initial_admin_password_file: None,
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Default destination for the first-boot admin password file:
/// `<base_dir>/INITIAL_ADMIN_PASSWORD` — at the root of homeCore's home,
/// where it's the most discoverable. Operators bind-mounting the home
/// dir (appliance setup) see the file at the top of their host volume
/// rather than tucked inside `data/`.
fn default_admin_password_path(base_dir: &std::path::Path) -> std::path::PathBuf {
    base_dir.join("INITIAL_ADMIN_PASSWORD")
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
fn random_password(len: usize) -> String {
    use rand::{rngs::OsRng, Rng};

    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = OsRng;
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
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
    let (_logging_handle, log_tx, log_ring, log_level_handle) =
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
    if !config.notify.channels.is_empty() {
        let count = config.notify.channels.len();
        let svc = NotificationService::from_configs(config.notify.channels)?;
        info!(
            channels = count,
            registered = svc.channel_names().len(),
            "Notification service ready"
        );
        core = core.with_notify(svc);
    }

    let (rules_handle, fire_history, calendar_handle, purge_fn) = core.start(rules).await?;

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
                        config_path: Some(p.config.clone()),
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
                    });
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
                    config: PathBuf::from(&p.config),
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

    // Parse the separate metrics whitelist — same lenient parser as the auth
    // whitelist (CIDR or bare IP). Empty list means /metrics is unreachable.
    let metrics_whitelist: Vec<IpNet> = config.metrics.whitelist.iter().filter_map(|s| {
        s.parse::<IpNet>()
            .or_else(|_| s.parse::<std::net::IpAddr>().map(IpNet::from))
            .map_err(|e| tracing::warn!(entry = %s, error = %e, "Invalid metrics whitelist entry — skipping"))
            .ok()
    }).collect();

    if metrics_whitelist.is_empty() {
        info!("/api/v1/metrics is locked down (no metrics.whitelist configured)");
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
    let publish_handle_rpc = publish_handle.clone();
    let pub_bus_rpc = pub_bus.clone();

    // InfluxDB v2 metrics exporter (opt-in). Subscribes to pub_bus so it
    // sees the same DeviceStateChanged events the rule engine + WebSocket
    // clients see. Errors during writes are logged and dropped — never
    // block the bus.
    if config.influx.enabled {
        hc_influx::spawn(config.influx.clone(), pub_bus.subscribe());
    }

    let app_state = AppState::new_with_plugins_and_raw_bus(
        store,
        pub_bus,
        internal_bus.clone(),
        Some(publish_handle),
        Some(source_rules_handle),
        Some(rules_handle),
        Some(rule_file_store),
        jwt,
        whitelist,
        Some(modes_path),
        plugin_registry.clone(),
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
    .with_dashboard_store(dashboard_store, dashboard_data)
    .with_battery_config(battery_tx)
    .with_homecore_config_path(config_path.clone())
    .with_shutdown_tx(shutdown_tx.clone());

    let app_state = if let Some(cal) = calendar_handle {
        app_state.with_calendar(cal, calendar_dir, calendar_expansion_days)
    } else {
        app_state
    }
    .with_plugin_commands(plugin_commands)
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
