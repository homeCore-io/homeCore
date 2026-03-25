use serde::Deserialize;
use std::collections::HashMap;

fn default_level() -> String { "info".into() }
fn default_true() -> bool { true }
fn default_ring_size() -> usize { 500 }
fn default_log_dir() -> String { String::new() }
fn default_prefix() -> String { "homecore".into() }
fn default_rules_prefix() -> String { "rules".into() }
fn default_max_size_mb() -> u64 { 100 }
fn default_syslog_host() -> String { "127.0.0.1".into() }
fn default_syslog_port() -> u16 { 514 }
fn default_facility() -> String { "user".into() }
fn default_app_name() -> String { "homecore".into() }

/// Whether timestamps in log output use the local system timezone or UTC.
///
/// ```toml
/// [logging]
/// time_display = "local"   # default — shows local time with offset
/// time_display = "utc"     # ISO 8601 Z suffix
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum TimeDisplay {
    /// Local system timezone (e.g. `2026-03-25T09:32:00.123-05:00`).
    #[default]
    Local,
    /// UTC (e.g. `2026-03-25T14:32:00.123Z`).
    Utc,
}

/// Top-level `[logging]` config section.
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Global default log level: error | warn | info | debug | trace
    #[serde(default = "default_level")]
    pub level: String,

    /// Per-crate / per-module overrides.  Keys use underscores (Rust target
    /// names), e.g. `hc_core = "debug"`.  Equivalent to RUST_LOG directives.
    #[serde(default)]
    pub targets: HashMap<String, String>,

    /// Timestamp display mode for all log outputs: "local" (default) | "utc".
    /// Applies to stderr, rolling files, and the rules file.
    /// Syslog timestamps follow their respective RFC format using this setting.
    #[serde(default)]
    pub time_display: TimeDisplay,

    #[serde(default)]
    pub stderr: StderrConfig,

    #[serde(default)]
    pub file: FileConfig,

    #[serde(default)]
    pub syslog: SyslogConfig,

    /// Dedicated rules/automation log file — captures only `hc_core` at debug
    /// regardless of the global log level.  Disabled by default.
    #[serde(default)]
    pub rules_file: RulesFileConfig,

    /// Live log streaming over WebSocket (GET /api/v1/logs/stream).
    #[serde(default)]
    pub stream: LoggingStreamConfig,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_level(),
            targets: HashMap::new(),
            time_display: TimeDisplay::Local,
            stderr: StderrConfig::default(),
            file: FileConfig::default(),
            syslog: SyslogConfig::default(),
            rules_file: RulesFileConfig::default(),
            stream: LoggingStreamConfig::default(),
        }
    }
}

// ── stream ──────────────────────────────────────────────────────────────────

/// Configuration for the live log streaming WebSocket endpoint.
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingStreamConfig {
    /// Enable the log streaming endpoint (GET /api/v1/logs/stream).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of recent log lines to buffer in memory for new subscribers.
    #[serde(default = "default_ring_size")]
    pub ring_buffer_size: usize,
}

impl Default for LoggingStreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ring_buffer_size: default_ring_size(),
        }
    }
}

// ── stderr ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct StderrConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Output format: "pretty" | "compact" | "json"
    #[serde(default)]
    pub format: OutputFormat,
    /// Emit ANSI color codes.  Set false when piping to systemd journal.
    #[serde(default = "default_true")]
    pub ansi: bool,
}

impl Default for StderrConfig {
    fn default() -> Self { Self { enabled: true, format: OutputFormat::Pretty, ansi: true } }
}

// ── file ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct FileConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Directory for log files.  Created at startup if absent.
    #[serde(default = "default_log_dir")]
    pub dir: String,
    /// Log file name prefix.  Files are named `<prefix>.YYYY-MM-DD`.
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Rotation strategy: "daily" | "hourly" | "never"
    #[serde(default)]
    pub rotation: RotationStrategy,
    /// Used only when rotation = "never" to document expected max size; not
    /// enforced programmatically (use logrotate for size-based rotation).
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,
    /// Output format: "json" (recommended for files) | "compact" | "pretty"
    #[serde(default)]
    pub format: OutputFormat,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: default_log_dir(),
            prefix: default_prefix(),
            rotation: RotationStrategy::Daily,
            max_size_mb: default_max_size_mb(),
            format: OutputFormat::Json,
        }
    }
}

// ── rules file ─────────────────────────────────────────────────────────────

/// Separate log file for the rule/automation engine.
///
/// Captures only `hc_core` (engine, executor, scheduler) at **debug** level,
/// regardless of the global `[logging].level`.  This gives a clean,
/// noise-free audit trail of every trigger evaluation, condition check,
/// and action execution without mixing broker/API/state messages.
///
/// ```toml
/// [logging.rules_file]
/// enabled  = true
/// dir      = "logs"          # defaults to same dir as [logging.file]
/// prefix   = "rules"         # files named rules.YYYY-MM-DD
/// rotation = "daily"
/// format   = "pretty"        # human-readable; use "json" for log aggregators
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct RulesFileConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Directory for rule log files. Empty string = inherit from `[logging.file].dir`.
    #[serde(default = "default_log_dir")]
    pub dir: String,
    #[serde(default = "default_rules_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub rotation: RotationStrategy,
    /// Output format: "pretty" (default, human-readable) | "compact" | "json"
    #[serde(default = "default_rules_format")]
    pub format: OutputFormat,
}

fn default_rules_format() -> OutputFormat { OutputFormat::Pretty }

impl Default for RulesFileConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_log_dir(),
            prefix: default_rules_prefix(),
            rotation: RotationStrategy::Daily,
            format: OutputFormat::Pretty,
        }
    }
}

// ── syslog ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct SyslogConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "udp" (default, recommended) | "tcp"
    #[serde(default)]
    pub transport: SyslogTransport,
    #[serde(default = "default_syslog_host")]
    pub host: String,
    #[serde(default = "default_syslog_port")]
    pub port: u16,
    /// "rfc5424" (default) | "rfc3164"
    #[serde(default)]
    pub protocol: SyslogProtocol,
    /// Syslog facility name: kern | user | mail | daemon | auth | syslog |
    /// lpr | news | uucp | cron | authpriv | ftp | local0–local7
    #[serde(default = "default_facility")]
    pub facility: String,
    #[serde(default = "default_app_name")]
    pub app_name: String,
    /// Level override for syslog only (defaults to global level if absent).
    /// Useful to send only warn+ to the remote syslog server while keeping
    /// debug locally.
    pub level: Option<String>,
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: SyslogTransport::Udp,
            host: default_syslog_host(),
            port: default_syslog_port(),
            protocol: SyslogProtocol::Rfc5424,
            facility: default_facility(),
            app_name: default_app_name(),
            level: None,
        }
    }
}

// ── shared enums ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Pretty,
    Compact,
    Json,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum RotationStrategy {
    #[default]
    Daily,
    Hourly,
    Never,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyslogTransport {
    #[default]
    Udp,
    Tcp,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub enum SyslogProtocol {
    #[serde(rename = "rfc3164")]
    Rfc3164,
    #[default]
    #[serde(rename = "rfc5424")]
    Rfc5424,
}

// ── facility helper ────────────────────────────────────────────────────────

/// Convert a facility name string to its numeric value (facility * 8 gives
/// the base PRI; individual severities are added on top).
pub fn facility_code(name: &str) -> u8 {
    match name.to_lowercase().as_str() {
        "kern"     => 0,
        "user"     => 1,
        "mail"     => 2,
        "daemon"   => 3,
        "auth"     => 4,
        "syslog"   => 5,
        "lpr"      => 6,
        "news"     => 7,
        "uucp"     => 8,
        "cron"     => 9,
        "authpriv" => 10,
        "ftp"      => 11,
        "local0"   => 16,
        "local1"   => 17,
        "local2"   => 18,
        "local3"   => 19,
        "local4"   => 20,
        "local5"   => 21,
        "local6"   => 22,
        "local7"   => 23,
        _          => 1, // default: user
    }
}
