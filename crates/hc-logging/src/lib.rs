//! `hc-logging` — config-driven multi-output logging for HomeCore.
//!
//! Initializes the global `tracing` subscriber from the `[logging]` section of
//! `homecore.toml`.  Three simultaneous outputs are supported, each with its
//! own format and level filter:
//!
//! - **stderr** — human-readable (pretty/compact/json) with optional ANSI color
//! - **file** — rolling log file (daily/hourly) in JSON or text format
//! - **syslog** — UDP or TCP to a remote syslog server (RFC 3164 or RFC 5424)
//!
//! # Usage (in `main.rs`)
//!
//! ```rust,ignore
//! // Load config first (no logging yet — use eprintln! for parse errors)
//! let config: AppConfig = /* toml parse */;
//!
//! // Initialize — keep the handle alive for the lifetime of the process.
//! let _logging = hc_logging::init(&config.logging)?;
//!
//! tracing::info!("HomeCore starting");
//! ```

pub mod broadcast_layer;
pub mod config;
pub mod filter;
mod rotating_writer;
pub mod syslog_layer;

pub use broadcast_layer::{BroadcastLayer, LogRing, LogSender};
pub use config::LoggingConfig;
pub use filter::{
    noise_suppression_directives, with_noise_suppression, NOISE_SUPPRESSION_DEFAULTS,
};

impl LoggingConfig {
    /// Resolve any empty or relative path fields against `base_dir`.
    ///
    /// Call this after deserialising from TOML and before calling [`init`].
    /// Absolute paths in the config are left unchanged.
    pub fn resolve_paths(&mut self, base_dir: &std::path::Path) {
        // Main file log dir.
        let dir = &mut self.file.dir;
        if dir.is_empty() {
            *dir = base_dir.join("logs").to_string_lossy().into_owned();
        } else if !std::path::Path::new(dir.as_str()).is_absolute() {
            *dir = base_dir.join(dir.as_str()).to_string_lossy().into_owned();
        }
        // Rules file dir — defaults to the same dir as the main file log.
        let rules_dir = &mut self.rules_file.dir;
        if rules_dir.is_empty() {
            *rules_dir = self.file.dir.clone();
        } else if !std::path::Path::new(rules_dir.as_str()).is_absolute() {
            *rules_dir = base_dir
                .join(rules_dir.as_str())
                .to_string_lossy()
                .into_owned();
        }
    }
}

use anyhow::Result;
use config::{FileConfig, OutputFormat, StderrConfig, TimeDisplay};
use filter::{build_filter, build_filter_string};
use syslog_layer::SyslogLayer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{prelude::*, Registry};

// ── timestamp formatter ────────────────────────────────────────────────────

/// Timestamp formatter for tracing layers.
///
/// `Local` → `2026-03-25T09:32:00.123-05:00`
/// `Utc`   → `2026-03-25T14:32:00.123Z`
#[derive(Clone, Debug)]
struct HcTimer {
    utc: bool,
}

impl HcTimer {
    fn from_display(display: &TimeDisplay) -> Self {
        Self {
            utc: matches!(display, TimeDisplay::Utc),
        }
    }
}

impl FormatTime for HcTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        if self.utc {
            write!(w, "{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"))
        } else {
            write!(
                w,
                "{}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            )
        }
    }
}

/// Returned by [`init`].  Must be kept alive for the entire process lifetime.
/// Dropping it flushes and closes the background file-writer threads.
pub struct LoggingHandle {
    _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    _rules_file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

type DynLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync>;

/// Type-erased trait for reloading an EnvFilter at runtime.
trait FilterReloader: Send + Sync {
    fn reload(&self, filter: tracing_subscriber::EnvFilter) -> Result<(), String>;
}

impl<S> FilterReloader for tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, S>
where
    S: 'static,
{
    fn reload(&self, filter: tracing_subscriber::EnvFilter) -> Result<(), String> {
        tracing_subscriber::reload::Handle::reload(self, filter)
            .map_err(|e| format!("failed to reload log filter: {e}"))
    }
}

/// Handle for changing the global log level at runtime.
///
/// Internally wraps a type-erased `tracing_subscriber::reload::Handle` that
/// controls a top-level `EnvFilter` on the subscriber registry.  All per-layer
/// filters still apply — this acts as a global minimum-level gate.
///
/// Clone-safe (all state is `Arc`-wrapped).
#[derive(Clone)]
pub struct LogLevelHandle {
    inner: std::sync::Arc<dyn FilterReloader>,
    current: std::sync::Arc<std::sync::Mutex<String>>,
}

impl LogLevelHandle {
    /// Create a `LogLevelHandle` from a `tracing_subscriber::reload::Handle`.
    ///
    /// This is the public constructor for plugins that manage their own
    /// `tracing` subscriber setup but want to integrate with the SDK's
    /// `set_log_level` management command.
    ///
    /// `initial_directives` should be the filter string used when the
    /// subscriber was created (e.g. `"info"` or `"hc_wled=debug,warn"`).
    pub fn from_reload_handle<S: 'static>(
        handle: tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, S>,
        initial_directives: impl Into<String>,
    ) -> Self {
        Self {
            inner: std::sync::Arc::new(handle),
            current: std::sync::Arc::new(std::sync::Mutex::new(initial_directives.into())),
        }
    }

    /// Change the global log level.
    ///
    /// Accepts any valid `EnvFilter` directive string:
    /// - Simple: `"debug"`, `"info"`, `"warn"`
    /// - Targeted: `"hc_core=debug,info"`, `"hc_api=trace,warn"`
    /// - Same syntax as `RUST_LOG`
    pub fn set_level(&self, directives: &str) -> Result<(), String> {
        let new_filter: tracing_subscriber::EnvFilter = directives
            .parse()
            .map_err(|e| format!("invalid log filter directive: {e}"))?;
        self.inner.reload(new_filter)?;
        *self.current.lock().unwrap() = directives.to_string();
        Ok(())
    }

    /// Return the current directive string.
    pub fn current_level(&self) -> String {
        self.current.lock().unwrap().clone()
    }
}

/// Initialize the global `tracing` subscriber from a [`LoggingConfig`],
/// also wiring in a [`BroadcastLayer`] for the log-streaming WebSocket.
///
/// Returns the logging handle (keep alive for process lifetime), the
/// broadcast sender and ring buffer for the API layer, and a
/// [`LogLevelHandle`] for runtime log level changes.
pub fn init_with_broadcast(
    config: &LoggingConfig,
    ring_capacity: usize,
) -> Result<(LoggingHandle, LogSender, LogRing, LogLevelHandle)> {
    let (broadcast_layer, tx, ring) = BroadcastLayer::new(ring_capacity);
    let (handle, level_handle) = init_inner(config, Some(broadcast_layer))?;
    Ok((handle, tx, ring, level_handle))
}

/// Initialize the global `tracing` subscriber from a [`LoggingConfig`].
///
/// Call this exactly once, early in `main()`, before spawning any tasks.
/// If the global subscriber is already set this returns an error.
pub fn init(config: &LoggingConfig) -> Result<LoggingHandle> {
    let (handle, _level_handle) = init_inner(config, None)?;
    Ok(handle)
}

fn init_inner(
    config: &LoggingConfig,
    broadcast: Option<BroadcastLayer>,
) -> Result<(LoggingHandle, LogLevelHandle)> {
    let mut layers: Vec<DynLayer> = Vec::new();
    let mut file_guard: Option<tracing_appender::non_blocking::WorkerGuard> = None;
    let mut rules_file_guard: Option<tracing_appender::non_blocking::WorkerGuard> = None;

    let timer = HcTimer::from_display(&config.time_display);
    let use_local_time = matches!(config.time_display, TimeDisplay::Local);

    // ── Global reload filter ──────────────────────────────────────────────
    // A top-level EnvFilter wrapped in a reload::Layer so the log level can
    // be changed at runtime via the returned LogLevelHandle.  Per-layer
    // filters still apply on top — this acts as a global minimum-level gate.
    let initial_directives = build_filter_string(config, None);
    let global_filter: tracing_subscriber::EnvFilter = initial_directives
        .parse()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let (reload_layer, reload_handle) = tracing_subscriber::reload::Layer::new(global_filter);

    // ── stderr ────────────────────────────────────────────────────────────
    if config.stderr.enabled {
        let filter = build_filter(config, None);
        layers.push(build_stderr_layer(&config.stderr, filter, timer.clone()));
    }

    // ── rolling file ──────────────────────────────────────────────────────
    if config.file.enabled {
        std::fs::create_dir_all(&config.file.dir).unwrap_or_else(|e| {
            eprintln!("hc-logging: cannot create log dir {}: {e}", config.file.dir);
        });
        let filter = build_filter(config, None);
        match build_file_layer(&config.file, filter, timer.clone()) {
            Ok((layer, guard)) => {
                file_guard = Some(guard);
                layers.push(layer);
            }
            Err(e) => eprintln!("hc-logging: cannot open log file: {e}"),
        }
    }

    // ── rules file ────────────────────────────────────────────────────────
    // Dedicated file capturing only hc_core (engine, executor, scheduler)
    // at debug level — provides a clean, noise-free rule audit trail.
    if config.rules_file.enabled {
        std::fs::create_dir_all(&config.rules_file.dir).unwrap_or_else(|e| {
            eprintln!(
                "hc-logging: cannot create rules log dir {}: {e}",
                config.rules_file.dir
            );
        });
        // Hard-wired filter: only hc_core at debug, everything else OFF.
        let filter = tracing_subscriber::EnvFilter::new("hc_core=debug");
        let cfg = config::FileConfig {
            enabled: true,
            dir: config.rules_file.dir.clone(),
            prefix: config.rules_file.prefix.clone(),
            rotation: config.rules_file.rotation.clone(),
            max_size_mb: config.rules_file.max_size_mb,
            compress: config.rules_file.compress,
            prune_after_days: config.rules_file.prune_after_days,
            format: config.rules_file.format.clone(),
        };
        match build_file_layer(&cfg, filter, timer.clone()) {
            Ok((layer, guard)) => {
                rules_file_guard = Some(guard);
                layers.push(layer);
            }
            Err(e) => eprintln!("hc-logging: cannot open rules log file: {e}"),
        }
    }

    // ── syslog ────────────────────────────────────────────────────────────
    if config.syslog.enabled {
        let filter = build_filter(config, config.syslog.level.as_deref());
        match SyslogLayer::new(&config.syslog, use_local_time) {
            Ok(layer) => layers.push(layer.with_filter(filter).boxed()),
            Err(e) => eprintln!("hc-logging: syslog init failed ({e}); skipping syslog output"),
        }
    }

    // If everything is disabled, fall back to a basic stderr subscriber so
    // there is always at least one output.
    if layers.is_empty() {
        let filter = build_filter(config, None);
        layers.push(build_stderr_layer(
            &StderrConfig::default(),
            filter,
            timer.clone(),
        ));
    }

    // ── broadcast layer (optional, for WS log streaming) ──────────────────
    if let Some(bl) = broadcast {
        layers.push(bl.boxed());
    }

    // Compose: Registry → reload filter (global gate) → per-layer outputs.
    // The reload layer wraps all output layers so that changing the global
    // filter affects everything (except broadcast, which has no filter).
    use tracing_subscriber::util::SubscriberInitExt;
    let subscriber = Registry::default().with(layers).with(reload_layer);
    subscriber
        .try_init()
        .map_err(|e| anyhow::anyhow!("Failed to install global tracing subscriber: {e}"))?;

    let level_handle = LogLevelHandle {
        inner: std::sync::Arc::new(reload_handle),
        current: std::sync::Arc::new(std::sync::Mutex::new(initial_directives)),
    };

    Ok((
        LoggingHandle {
            _file_guard: file_guard,
            _rules_file_guard: rules_file_guard,
        },
        level_handle,
    ))
}

// ── layer builders ─────────────────────────────────────────────────────────

fn build_stderr_layer(
    cfg: &StderrConfig,
    filter: tracing_subscriber::EnvFilter,
    timer: HcTimer,
) -> DynLayer {
    let ansi = cfg.ansi;
    match cfg.format {
        OutputFormat::Json => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .json()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .with_filter(filter)
            .boxed(),
        OutputFormat::Compact => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .compact()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .with_filter(filter)
            .boxed(),
        OutputFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .pretty()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .with_filter(filter)
            .boxed(),
    }
}

fn build_file_layer(
    cfg: &FileConfig,
    filter: tracing_subscriber::EnvFilter,
    timer: HcTimer,
) -> anyhow::Result<(DynLayer, tracing_appender::non_blocking::WorkerGuard)> {
    use rotating_writer::RotatingWriter;

    let writer = RotatingWriter::new(
        std::path::PathBuf::from(&cfg.dir),
        cfg.prefix.clone(),
        cfg.rotation.clone(),
        cfg.max_size_mb.saturating_mul(1024 * 1024),
        cfg.compress,
        cfg.prune_after_days,
    )?;
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);

    let layer: DynLayer = match cfg.format {
        OutputFormat::Json => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .json()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(filter)
            .boxed(),
        OutputFormat::Compact => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .compact()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(filter)
            .boxed(),
        OutputFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .pretty()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(filter)
            .boxed(),
    };

    Ok((layer, guard))
}
