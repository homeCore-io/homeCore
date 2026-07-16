//! Configurable log rotation and compression for hc-hue.
//!
//! Drop-in replacement for the hard-coded `tracing_appender::rolling::daily()`.
//! Supports time-based rotation (hourly/daily/weekly/never), optional size-based
//! rotation, and gzip compression of rotated files.
//!
//! **Config:**
//! ```toml
//! [logging]
//! level       = "info"    # stderr filter; RUST_LOG overrides this
//! rotation    = "daily"   # daily | hourly | weekly | never
//! max_size_mb = 100       # 0 = time-only rotation (no size limit)
//! compress    = true      # gzip rotated files in a background thread
//! ```

use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

// ── Config ───────────────────────────────────────────────────────────────────

fn default_level() -> String {
    "info".into()
}
fn default_max_size_mb() -> u64 {
    100
}
fn default_compress() -> bool {
    true
}

/// Rotation strategy for the log file.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum RotationStrategy {
    #[default]
    Daily,
    Hourly,
    Weekly,
    Never,
}

/// `[logging]` section in `config/config.toml`.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoggingConfig {
    /// Stderr log level when `RUST_LOG` is not set.
    /// Accepts any `tracing` filter directive, e.g. `"debug"` or `"hc_hue=debug,rumqttc=warn"`.
    #[serde(default = "default_level")]
    pub level: String,
    /// Time-based rotation strategy: `daily` (default), `hourly`, `weekly`, or `never`.
    #[serde(default)]
    pub rotation: RotationStrategy,
    /// Rotate the active log file when it exceeds this many megabytes.
    /// Combined with `rotation` as "whichever comes first".
    /// Set to `0` to disable size-based rotation.
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u64,
    /// Gzip-compress rotated files in a background thread.
    #[serde(default = "default_compress")]
    pub compress: bool,
    /// Delete rotated log files older than this many days.  0 = never prune.
    #[serde(default)]
    pub prune_after_days: u32,
    /// Minimum log level forwarded to the HomeCore broker over MQTT.
    /// Logs below this level are only written to the local file / stderr.
    /// "info" (default) | "warn" | "debug" | "error" | "off"
    #[serde(default = "default_level")]
    pub log_forward_level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_level(),
            rotation: RotationStrategy::Daily,
            max_size_mb: default_max_size_mb(),
            compress: default_compress(),
            prune_after_days: 0,
            log_forward_level: default_level(),
        }
    }
}

// ── RotatingWriter ────────────────────────────────────────────────────────────

/// File writer that rotates on a time schedule and/or when a size limit is hit.
/// Implements `std::io::Write`; pass to `tracing_appender::non_blocking`.
pub struct RotatingWriter {
    file: File,
    bytes_written: u64,
    max_bytes: u64,
    rotation: RotationStrategy,
    current_period: String,
    dir: PathBuf,
    prefix: String,
    compress: bool,
    period_counter: u32,
    prune_after_days: u32,
}

impl RotatingWriter {
    pub fn new(
        dir: PathBuf,
        prefix: String,
        rotation: RotationStrategy,
        max_bytes: u64,
        compress: bool,
        prune_after_days: u32,
    ) -> io::Result<Self> {
        let current_period = period_str(&rotation);
        let active = active_path(&dir, &prefix);
        let file = open_append(&active)?;
        let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
        let writer = Self {
            file,
            bytes_written,
            max_bytes,
            rotation,
            current_period,
            dir,
            prefix,
            compress,
            period_counter: 0,
            prune_after_days,
        };
        if prune_after_days > 0 {
            prune_old_logs(&writer.dir, &writer.prefix, prune_after_days);
        }
        Ok(writer)
    }

    fn maybe_rotate(&mut self) -> io::Result<()> {
        let new_period = period_str(&self.rotation);
        let period_changed = !new_period.is_empty() && new_period != self.current_period;
        let size_exceeded = self.max_bytes > 0 && self.bytes_written >= self.max_bytes;

        if !period_changed && !size_exceeded {
            return Ok(());
        }

        self.file.flush()?;

        if period_changed {
            self.period_counter = 0;
            self.current_period = new_period;
        }

        let rotated = self.next_rotated_path();
        let active = active_path(&self.dir, &self.prefix);

        std::fs::rename(&active, &rotated)?;

        if self.compress {
            compress_in_background(rotated);
        }

        self.file = open_append(&active)?;
        self.bytes_written = 0;
        self.period_counter += 1;
        if self.prune_after_days > 0 {
            prune_old_logs(&self.dir, &self.prefix, self.prune_after_days);
        }

        Ok(())
    }

    fn next_rotated_path(&self) -> PathBuf {
        let period = if self.current_period.is_empty() {
            chrono::Local::now().format("%Y-%m-%dT%H%M%S").to_string()
        } else {
            self.current_period.clone()
        };

        if self.period_counter == 0 {
            let candidate = self.dir.join(format!("{}.{}.log", self.prefix, period));
            if !candidate.exists() {
                return candidate;
            }
        }

        let start = if self.period_counter == 0 {
            1
        } else {
            self.period_counter
        };
        let mut n = start;
        loop {
            let candidate = self
                .dir
                .join(format!("{}.{}.{}.log", self.prefix, period, n));
            if !candidate.exists() {
                return candidate;
            }
            n += 1;
        }
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.maybe_rotate()?;
        let n = self.file.write(buf)?;
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn active_path(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!("{}.log", prefix))
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn period_str(rotation: &RotationStrategy) -> String {
    let now = chrono::Local::now();
    match rotation {
        RotationStrategy::Hourly => now.format("%Y-%m-%d_%H").to_string(),
        RotationStrategy::Daily => now.format("%Y-%m-%d").to_string(),
        RotationStrategy::Weekly => now.format("%Y-W%V").to_string(),
        RotationStrategy::Never => String::new(),
    }
}

fn prune_old_logs(dir: &Path, prefix: &str, max_age_days: u32) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(u64::from(max_age_days) * 86_400);

    let rotated_prefix = format!("{}.", prefix);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !name.starts_with(&rotated_prefix) {
            continue;
        }
        if !name.ends_with(".log") && !name.ends_with(".log.gz") {
            continue;
        }

        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn compress_in_background(src: PathBuf) {
    std::thread::spawn(move || {
        let mut gz_os = src.as_os_str().to_owned();
        gz_os.push(".gz");
        let gz_path = PathBuf::from(gz_os);

        let result: io::Result<()> = (|| {
            let mut input = File::open(&src)?;
            let output = File::create(&gz_path)?;
            let mut encoder = GzEncoder::new(output, Compression::default());
            io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
            drop(input);
            std::fs::remove_file(&src)?;
            Ok(())
        })();

        if let Err(e) = result {
            eprintln!("log compression failed for {:?}: {e}", src);
        }
    });
}

// ── Public init ───────────────────────────────────────────────────────────────

/// Initialise tracing: stderr layer + rotating compressed file layer.
///
/// - `config_path`: path to `config/config.toml`; derives the `logs/` dir.
/// - `prefix`: log file name prefix, e.g. `"hc-hue"` → `logs/hc-hue.log`.
/// - `stderr_default`: filter used for stderr when `RUST_LOG` is unset and
///   `cfg.level` is the default `"info"`, e.g. `"hc_hue=info"`.
/// - `cfg`: `[logging]` config from the plugin config file.
///
/// **The returned `WorkerGuard` must be kept alive for the process lifetime.**
///
/// Also returns a [`plugin_sdk_rs::logging::LogLevelHandle`] for dynamic log level changes
/// via the plugin management protocol.
pub fn init_logging(
    config_path: &str,
    prefix: &str,
    stderr_default: &str,
    cfg: &LoggingConfig,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    plugin_sdk_rs::logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    let log_dir = Path::new(config_path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    std::fs::create_dir_all(&log_dir).ok();

    let max_bytes = cfg.max_size_mb.saturating_mul(1024 * 1024);
    let writer = RotatingWriter::new(
        log_dir,
        prefix.to_string(),
        cfg.rotation.clone(),
        max_bytes,
        cfg.compress,
        cfg.prune_after_days,
    )
    .expect("failed to open log file");

    let (non_blocking, guard) = tracing_appender::non_blocking(writer);

    // When RUST_LOG is not set: use cfg.level if the user changed it from the
    // default, otherwise fall back to the plugin-specific default filter string.
    let initial_directives = if std::env::var("RUST_LOG").is_ok() {
        std::env::var("RUST_LOG").unwrap_or_default()
    } else if cfg.level == "info" {
        stderr_default.to_string()
    } else {
        cfg.level.clone()
    };
    // Prepend noise-suppression defaults (rumqttc Pingreq spam etc.); user
    // directives layered after so they win on conflict.
    let initial_directives = plugin_sdk_rs::logging::with_noise_suppression(&initial_directives);

    let global_filter: EnvFilter = initial_directives
        .parse()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let (reload_layer, reload_handle) = tracing_subscriber::reload::Layer::new(global_filter);

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    // MQTT log forwarding layer — starts inactive, activated after MQTT connects.
    let (mqtt_layer, mqtt_handle) = plugin_sdk_rs::mqtt_log_layer::MqttLogLayer::new();

    tracing_subscriber::registry()
        .with(reload_layer)
        .with(stderr_layer)
        .with(file_layer)
        .with(mqtt_layer)
        .init();

    let level_handle = plugin_sdk_rs::logging::LogLevelHandle::from_reload_handle(
        reload_handle,
        initial_directives,
    );

    (guard, level_handle, mqtt_handle)
}
