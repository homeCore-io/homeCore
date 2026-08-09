//! The shape of `homecore.toml`.
//!
//! Lifted out of `src/main.rs` unchanged. It lived in the binary crate, which
//! meant nothing else in the workspace could see it — including `hc-api`,
//! which serves `GET/PUT /system/config` and now needs to describe these
//! sections rather than hand the operator a text box. Moving it is the
//! prerequisite for that; the structs themselves are untouched apart from
//! visibility.
//!
//! See `claude-notes/plans/system_config_descriptor.md`.

/// `homecore.toml` described in the config-descriptor vocabulary, so a client
/// renders it with the renderer it already has for plugins.
#[cfg(feature = "descriptor")]
pub mod descriptor;

use hc_influx::InfluxConfig;
use hc_logging::LoggingConfig;
use hc_notify::ChannelConfig;
use serde::Deserialize;
use std::path::Path;

/// Resolve a path string field:
///   - empty string  → `{base}/{relative_default}`
///   - relative path → `{base}/{path}`
///   - absolute path → unchanged
pub fn resolve_path(field: &mut String, base: &Path, relative_default: &str) {
    if field.is_empty() {
        *field = base.join(relative_default).to_string_lossy().into_owned();
    } else if !Path::new(field.as_str()).is_absolute() {
        *field = base.join(field.as_str()).to_string_lossy().into_owned();
    }
}

/// Resolve an optional path string: only touches it when Some and relative.
pub fn resolve_opt_path(field: &mut Option<String>, base: &Path) {
    if let Some(p) = field {
        if !Path::new(p.as_str()).is_absolute() {
            *field = Some(base.join(p.as_str()).to_string_lossy().into_owned());
        }
    }
}

/// Top-level config shape (subset — just what main.rs needs to parse).
#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub broker: BrokerSection,
    #[serde(default)]
    pub location: LocationSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default)]
    pub profiles: ProfilesSection,
    #[serde(default)]
    pub rules: RulesSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub notify: NotifySection,
    #[serde(default)]
    pub startup: StartupSection,
    #[serde(default)]
    pub shutdown: ShutdownConfig,
    #[serde(default)]
    pub scheduler: SchedulerSection,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub web_admin: WebAdminSection,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default)]
    pub calendars: CalendarsSection,
    #[serde(default)]
    pub battery: BatterySection,
    #[serde(default)]
    pub influx: InfluxConfig,
    #[serde(default)]
    pub metrics: MetricsSection,
    #[serde(default)]
    pub registry: RegistrySection,
    #[serde(default)]
    pub plugin_runtimes: PluginRuntimesSection,
}

/// `[registry]` — the remote signed plugin registry. Both fields must be set to
/// enable browse + registry-install; otherwise those endpoints return 503.
#[derive(Deserialize, Default, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RegistrySection {
    /// URL (or local path / `file://`) of the signed `index.json`.
    #[serde(default)]
    pub url: Option<String>,
    /// Base64-encoded ed25519 public key that signs the index.
    #[serde(default)]
    pub public_key: Option<String>,
}

/// `[plugin_runtimes]` — container-hosted plugins written in other languages.
///
/// A runtime is a container the operator runs; homeCore never manages it. These
/// settings govern only how one is allowed to *join*. See
/// `docs/pluginRuntimesPlan.md`.
#[derive(Deserialize, Clone, Debug)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginRuntimesSection {
    /// Master switch. Off means the enrollment endpoints 404 — a deployment
    /// that will never host a runtime should not carry the surface.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `open` — anyone reachable may ask, and an admin approves a matching code.
    /// `token` — only an admin-issued token enrolls, and nothing is ever pending.
    #[serde(default = "default_enroll_mode")]
    pub mode: String,
    /// Restrict enrollment to `[auth].whitelist` sources. On by default: a
    /// runtime is a machine on your network, and an enrollment request from
    /// outside it is not a thing that should reach an approval screen.
    #[serde(default = "default_true")]
    pub whitelist_only: bool,
    /// How long a pending record stays answerable.
    #[serde(default = "default_pending_ttl_mins")]
    pub pending_ttl_mins: u32,
    /// Cap on simultaneous pending records. Bounds the "fill the screen with
    /// plausible requests until one is approved by fatigue" attack.
    #[serde(default = "default_max_pending")]
    pub max_pending: u32,
    /// Denials an identity may accumulate before it has to wait.
    #[serde(default = "default_max_denials")]
    pub max_denials: u32,
    /// How long that wait is.
    #[serde(default = "default_denial_cooldown_mins")]
    pub denial_cooldown_mins: u32,
}

fn default_enroll_mode() -> String {
    "open".into()
}
fn default_pending_ttl_mins() -> u32 {
    15
}
fn default_max_pending() -> u32 {
    5
}
fn default_max_denials() -> u32 {
    3
}
fn default_denial_cooldown_mins() -> u32 {
    60
}

impl Default for PluginRuntimesSection {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: default_enroll_mode(),
            whitelist_only: true,
            pending_ttl_mins: default_pending_ttl_mins(),
            max_pending: default_max_pending(),
            max_denials: default_max_denials(),
            denial_cooldown_mins: default_denial_cooldown_mins(),
        }
    }
}

impl PluginRuntimesSection {
    /// True when only an admin-issued token may enroll.
    pub fn is_token_only(&self) -> bool {
        self.mode.eq_ignore_ascii_case("token")
    }
}

impl AppConfig {
    /// Fill in any empty/relative path fields using `base_dir` as the root.
    /// Called after loading the TOML file so explicit absolute paths in config
    /// are always honoured; only unset (empty) or relative paths are resolved.
    pub fn resolve_paths(&mut self, base: &Path) {
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginEntry {
    /// Identifier used in log messages (e.g. "plugin.yolink").
    pub id: String,
    /// Path to the compiled plugin binary.
    /// Relative paths are resolved against base_dir.
    pub binary: String,
    /// Path to the plugin's config file, passed as its first argument.
    /// Relative paths are resolved against base_dir.
    pub config: String,
    /// Set to false to disable this plugin without removing the entry.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

pub fn default_true() -> bool {
    true
}

impl PluginEntry {
    pub fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.binary, base, "");
        resolve_path(&mut self.config, base, "");
    }
}

/// `[rules]` section of homecore.toml.
#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RulesSection {
    /// Directory containing per-rule TOML files.
    /// Default: `{base_dir}/rules`
    #[serde(default)]
    pub dir: String,
}

impl RulesSection {
    pub fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "rules");
    }
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServerSection {
    #[serde(default = "default_server_host")]
    pub host: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: default_server_host(),
            port: default_server_port(),
        }
    }
}

pub fn default_server_host() -> String {
    "0.0.0.0".into()
}
pub fn default_server_port() -> u16 {
    8080
}

/// `[battery]` section of homecore.toml — drives the battery watcher.
#[derive(Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BatterySection {
    /// Battery percentage at or below which the latch engages.
    #[serde(default = "default_battery_threshold")]
    pub threshold_pct: f64,
    /// Recovery band added to threshold to clear the latch.
    #[serde(default = "default_battery_recover")]
    pub recover_band_pct: f64,
    /// Optional hc-notify channel for the built-in notification shortcut.
    /// Leave unset to disable the shortcut (rules-engine still receives the
    /// `device.battery_low` events either way).
    #[serde(default)]
    pub notify_channel: Option<String>,
    /// When true and `notify_channel` is set, recovery edges also notify.
    #[serde(default)]
    pub notify_on_recovered: bool,
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

pub fn default_battery_threshold() -> f64 {
    20.0
}
pub fn default_battery_recover() -> f64 {
    5.0
}

#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StorageSection {
    /// Path to the redb state database.
    /// Default: `{base_dir}/data/state.redb`
    #[serde(default)]
    pub state_db_path: String,
    /// Path to the SQLite history database.
    /// Default: `{base_dir}/data/history.db`
    #[serde(default)]
    pub history_db_path: String,
}

impl StorageSection {
    pub fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.state_db_path, base, "data/state.redb");
        resolve_path(&mut self.history_db_path, base, "data/history.db");
    }
}

#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfilesSection {
    /// Directory containing ecosystem profile TOML files (Shelly, Tasmota, etc.).
    /// Default: `{base_dir}/config/profiles`
    #[serde(default)]
    pub dir: String,
}

impl ProfilesSection {
    pub fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "config/profiles");
    }
}

/// `[broker]` section of homecore.toml.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BrokerSection {
    #[serde(default = "default_broker_host")]
    pub host: String,
    #[serde(default = "default_broker_port")]
    pub port: u16,
    /// MQTT v5 listener port. Defaults to port+1 (1884 when port is 1883).
    /// Set to null to disable.
    #[serde(default = "default_broker_v5_port")]
    pub v5_port: Option<u16>,
    pub tls_port: Option<u16>,
    /// Path to TLS certificate file.  Relative paths are resolved against
    /// base_dir; absolute paths are used as-is.
    pub cert_path: Option<String>,
    /// Path to TLS private key file.  Same resolution rules as cert_path.
    pub key_path: Option<String>,
    /// Per-client credentials.  When any entries are present the broker
    /// requires authentication on all connections.
    #[serde(default)]
    pub clients: Vec<ClientAclConfig>,
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
    pub fn resolve(&mut self, base: &Path) {
        resolve_opt_path(&mut self.cert_path, base);
        resolve_opt_path(&mut self.key_path, base);
    }
}

pub fn default_broker_host() -> String {
    "0.0.0.0".into()
}
pub fn default_broker_v5_port() -> Option<u16> {
    Some(1884)
}
pub fn default_broker_port() -> u16 {
    1883
}

/// A single `[[broker.clients]]` entry.
#[derive(Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClientAclConfig {
    pub id: String,
    pub password: String,
    #[serde(default)]
    pub allow_pub: Vec<String>,
    #[serde(default)]
    pub allow_sub: Vec<String>,
}

#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NotifySection {
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

/// `[startup]` section of homecore.toml.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StartupSection {
    /// Seconds to wait after launch before publishing initial mode states.
    ///
    /// Plugins need time to connect and subscribe to their cmd topics.
    /// If a rule fires during this window (e.g. mode_night already on at
    /// restart) and the target plugin hasn't subscribed yet, the command is
    /// silently dropped.  Increase this value if you have plugins with long
    /// startup times.  Default: 10 s.
    #[serde(default = "default_startup_delay")]
    pub plugin_ready_delay_secs: u64,
}

pub fn default_startup_delay() -> u64 {
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ShutdownConfig {
    /// Seconds to wait for in-flight rule action tasks to finish during graceful
    /// shutdown before forcing a stop.  Default: 10 s.
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_secs: u64,
}

pub fn default_drain_timeout() -> u64 {
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebAdminSection {
    /// Enable the built-in admin UI served by HomeCore.
    ///
    /// When enabled, HomeCore serves the pre-built Leptos/WASM admin UI
    /// as static files and preserves the API under `/api/v1`.
    /// Requires `dist_path` to point to a valid `trunk build` output directory.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the Leptos UI build output directory (trunk build --release).
    /// Relative paths are resolved against base_dir.
    /// Required when enabled = true.
    #[serde(default)]
    pub dist_path: Option<String>,
}

/// `[calendars]` section of homecore.toml.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalendarsSection {
    /// Directory containing `.ics` calendar files.
    /// Default: `{base_dir}/config/calendars`
    #[serde(default)]
    pub dir: String,
    /// How many days forward to expand recurring events.  Default: 400.
    #[serde(default = "default_expansion_days")]
    pub expansion_days: u32,
}

pub fn default_expansion_days() -> u32 {
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
    pub fn resolve(&mut self, base: &Path) {
        resolve_path(&mut self.dir, base, "config/calendars");
    }
}

/// `[scheduler]` section of homecore.toml.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SchedulerSection {
    /// How many minutes back from startup to search for missed time-based
    /// triggers (SunEvent and TimeOfDay).  Any rule whose scheduled time falls
    /// within `(now - window, now]` is fired immediately on startup so that a
    /// brief process restart does not silently skip an automation.
    ///
    /// Set to 0 to disable catch-up entirely.  Default: 15.
    #[serde(default = "default_catchup_window")]
    pub catchup_window_minutes: u32,
}

pub fn default_catchup_window() -> u32 {
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocationSection {
    pub latitude: f64,
    pub longitude: f64,
    /// IANA zone name (e.g. `"America/New_York"`). Drives every
    /// user-facing timestamp — log file/stderr output, console-style
    /// API endpoints, mode-manager "what time is it locally" checks.
    /// Storage stays UTC. Falls back to UTC when unset or unparseable;
    /// the parse error is logged at startup so a typo is visible
    /// without reading the source.
    #[serde(default)]
    pub timezone: Option<String>,
}

impl Default for LocationSection {
    fn default() -> Self {
        Self {
            latitude: 38.9072,
            longitude: -77.0369,
            timezone: None,
        }
    }
}

/// `[metrics]` section — gates `GET /api/v1/metrics` by source IP.
///
/// Prometheus scrapers can't easily set Authorization headers, so the
/// metrics endpoint is gated by network identity instead. The whitelist
/// defaults to empty, which means **no IPs are allowed** — operators must
/// explicitly list the scrape source(s) before metrics become reachable.
///
/// This is deliberate and is not going to be auto-discovered from the host's
/// interfaces. Metrics expose device counts, rule counts, and plugin health;
/// defaulting them open to whatever subnet the machine happens to sit on would
/// widen the attack surface silently, and "it worked without me configuring it"
/// is exactly how an endpoint ends up exposed on a network nobody audited.
/// Opening it stays an explicit act.
#[derive(Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MetricsSection {
    /// IP addresses or CIDR ranges allowed to scrape `/api/v1/metrics`.
    /// Both IPv4 and IPv6 are supported.
    /// Example: `whitelist = ["127.0.0.1/32", "10.0.0.0/24"]`.
    ///
    /// Empty (the default) means the endpoint returns 403 to every caller. It
    /// says so at startup, and the 403 body names the exact CIDR line to add.
    #[serde(default)]
    pub whitelist: Vec<String>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuthSection {
    /// HMAC-SHA256 secret for signing JWTs. **Deprecated** — prefer leaving
    /// this unset and letting the core manage `jwt_secret_file` automatically.
    /// If set, takes precedence over `jwt_secret_file` and emits a warning.
    pub jwt_secret: Option<String>,
    /// Path to a file holding the persistent JWT HS256 secret. When unset,
    /// defaults to `<parent-of-state_db_path>/jwt_secret`. The file is
    /// auto-generated with 0600 perms on first startup and re-used across
    /// restarts so issued tokens survive reboots.
    #[serde(default)]
    pub jwt_secret_file: Option<std::path::PathBuf>,
    #[serde(default = "default_expiry")]
    pub token_expiry_hours: u64,
    /// Refresh-token lifetime in days. A successful login also returns a
    /// long-lived refresh token; each `/auth/refresh` call rotates it.
    /// Default: 30 days.
    #[serde(default = "default_refresh_days")]
    pub refresh_token_expiry_days: u64,
    /// How many days of audit-log history to keep. Entries older than this
    /// are pruned by a background task that runs every 6 hours.
    /// Default: 365 days.
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,
    /// IP addresses or CIDR ranges that may access all API endpoints without
    /// a JWT.  Requests from these addresses receive full Admin access.
    /// Parsed as standard CIDR notation.  Both IPv4 and IPv6 are supported.
    /// Example: ["127.0.0.1/32", "::1/128", "192.168.1.0/24"]
    ///
    /// **Deprecated** — prefer `[auth.admin_uds]` for same-host admin
    /// tooling. This option will be removed in a future release.
    #[serde(default)]
    pub whitelist: Vec<String>,
    /// Admin-only Unix domain socket listener for `hc-cli` and other
    /// same-host admin tooling. Replaces the CIDR whitelist.
    #[serde(default)]
    pub admin_uds: AdminUdsSection,
    /// Path where the auto-generated initial admin password is written
    /// the first time homeCore boots with no users in the store. Set to
    /// the empty string to disable file output (password is still
    /// printed to logs).
    ///
    /// Defaults to `<parent-of-state_db_path>/INITIAL_ADMIN_PASSWORD`,
    /// 0600. The file should be deleted by the operator after first
    /// login; homeCore does NOT re-write it on subsequent boots.
    #[serde(default)]
    pub initial_admin_password_file: Option<std::path::PathBuf>,
}

#[derive(Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AdminUdsSection {
    #[serde(default)]
    pub enabled: bool,
    /// Default: `/run/homecore/admin.sock`.
    #[serde(default = "default_admin_uds_path")]
    pub path: String,
    /// POSIX group that owns the socket. Members of this group can connect.
    #[serde(default = "default_admin_uds_group")]
    pub group: String,
    /// Mode for the socket file, as an octal string (e.g. "0660").
    #[serde(default = "default_admin_uds_mode")]
    pub mode: String,
    /// Extra UIDs allowed to connect. The process UID is always allowed.
    #[serde(default)]
    pub allowed_uids: Vec<u32>,
}

pub fn default_admin_uds_path() -> String {
    "/run/homecore/admin.sock".into()
}
pub fn default_admin_uds_group() -> String {
    "homecore-admin".into()
}
pub fn default_admin_uds_mode() -> String {
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

pub fn default_expiry() -> u64 {
    24
}

pub fn default_refresh_days() -> u64 {
    30
}

pub fn default_audit_retention_days() -> u64 {
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
