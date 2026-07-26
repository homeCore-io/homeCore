//! `hc-api` — axum HTTP + WebSocket API server.

use anyhow::{Context, Result};
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};

/// Maximum size of an uploaded restore archive. Axum's per-route default
/// is 2 MiB, which rejects any realistic backup — state.redb plus
/// history.db plus plugin configs together easily clear that. 500 MiB is
/// roomy enough for years of history on a home installation; operators
/// with larger archives can edit this constant.
const RESTORE_BODY_LIMIT_BYTES: usize = 500 * 1024 * 1024;
use hc_auth::JwtService;
use hc_core::{CalendarHandle, EventBus};
use hc_mqtt_client::PublishHandle;
use hc_state::StateStore;
use hc_types::rule::Rule;
use ipnet::IpNet;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub mod admin_uds;
pub mod api_key_handlers;
pub mod audit;
pub mod audit_handlers;
pub mod auth_handlers;
pub mod auth_middleware;
pub mod backup;
pub mod config_writer;
pub mod dashboard_store;
pub mod event_log;
pub mod group_store;
pub mod handlers;
pub mod logs;
pub mod managed_modes;
pub mod managed_plugins;
pub mod management_rpc;
pub mod metrics;
pub mod mode_definition_store;
pub mod plugin_config_store;
pub mod plugin_config_watcher;
pub mod plugin_install;
pub mod rate_limit;
pub mod registry;
pub mod rule_file_store;
pub mod streaming;
pub mod ws;

use auth_middleware::require_auth;
use backup::BackupPaths;
use dashboard_store::{DashboardStore, DashboardStoreData};
use event_log::EventLog;
use group_store::{GroupStore, RuleGroup};
use logs::LogStreamState;
pub use managed_plugins::{ManagedPluginStore, ManagedRecord};
use metrics::MetricsCollector;
pub use plugin_config_store::PluginConfigStore;
pub use plugin_config_watcher::PluginConfigWatcher;
pub use plugin_install::{InstallContext, InstalledPlugin};
use rule_file_store::RuleFileStore;

/// Runtime command sent to a plugin supervisor task.
#[derive(Debug)]
pub enum PluginCommand {
    Start,
    Stop,
    Restart,
}

/// Per-plugin command sender, indexed by plugin_id.
pub type PluginCommandChannels =
    Arc<RwLock<HashMap<String, tokio::sync::mpsc::Sender<PluginCommand>>>>;

/// Registered plugin record stored in-memory.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginRecord {
    pub plugin_id: String,
    pub registered_at: chrono::DateTime<chrono::Utc>,
    /// "active" | "offline" | "stopped" | "starting"
    pub status: String,
    /// Whether this plugin is enabled in homecore.toml.
    #[serde(default)]
    pub enabled: bool,
    /// true = locally-launched child process; false = remote (MQTT only).
    #[serde(default)]
    pub managed: bool,
    /// Filesystem path to the plugin's config file (local plugins only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Filesystem path to the plugin binary (local plugins only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Last heartbeat received from plugin (via MQTT management channel).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
    /// Timestamp of the most recent (re)start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_restart: Option<chrono::DateTime<chrono::Utc>>,
    /// Number of restarts since homecore started.
    #[serde(default)]
    pub restart_count: u32,
    /// When the current run started (for uptime calculation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_started: Option<chrono::DateTime<chrono::Utc>>,
    /// Number of devices registered by this plugin.
    #[serde(default)]
    pub device_count: u32,
    /// Current log level if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    /// Self-reported plugin version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Plugin has responded to the management protocol (heartbeat, etc.).
    #[serde(default)]
    pub supports_management: bool,
    /// Capability manifest last published on
    /// `homecore/plugins/{id}/capabilities`. `None` until the plugin
    /// publishes, or if the published manifest failed to decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<hc_types::Capabilities>,
    /// JSON Schema for the plugin's operator config, carried on the capability
    /// manifest. Drives the config editor's typed form; `None` → raw-TOML
    /// fallback. Not shown in the plugin list — served at
    /// `GET /plugins/:id/config/schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// The plugin's own config *descriptor* — an expressive description of its
    /// configuration (sections, field kinds, conditionals, data sources) that
    /// the editor renders directly. Also carried on the capability manifest.
    /// `None` → the client auto-derives a baseline descriptor from
    /// `config_schema`. Served at `GET /plugins/:id/config/descriptor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_descriptor: Option<serde_json::Value>,
    /// Installed artifact version, for plugins added from the registry/managed
    /// store (distinct from the SDK-reported `version`). Drives "update available".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
}

impl PluginRecord {
    /// A seed record for a locally-managed plugin, before it registers/starts —
    /// used when a plugin is installed at runtime so it shows in the list
    /// immediately (supervisor status updates are update-only).
    pub fn managed_seed(
        plugin_id: String,
        config_path: Option<String>,
        binary_path: Option<String>,
        enabled: bool,
        installed_version: Option<String>,
    ) -> Self {
        Self {
            plugin_id,
            registered_at: chrono::Utc::now(),
            status: if enabled { "starting" } else { "stopped" }.into(),
            enabled,
            managed: true,
            config_path,
            binary_path,
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
            installed_version,
        }
    }
}

/// Shared state injected into every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub store: StateStore,
    pub event_bus: EventBus,
    /// Raw `Event::MqttMessage` bus. The SSE bridge for plugin streaming
    /// actions needs this because stream events are plain MQTT publishes
    /// on `homecore/plugins/{id}/commands/{rid}/events` — they never get
    /// converted into typed events on `event_bus`. In production this is
    /// `internal_bus`; in tests with a single merged bus it's a clone of
    /// `event_bus`.
    pub raw_bus: EventBus,
    pub publish: Option<PublishHandle>,
    /// Live source rule set exactly as authored on disk/API input.
    pub source_rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
    /// Live rule set — source of truth for the rule engine and all API reads.
    /// Updated atomically by the API and by the hot-reload watcher.
    /// This handle contains compiled rules with device references resolved to
    /// concrete device IDs for execution.
    pub rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
    /// Write-through file store for automation rules.
    pub rule_file_store: Option<RuleFileStore>,
    /// In-memory plugin registry (plugin_id → record).
    pub plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    /// JWT service for issuing and validating tokens.
    pub jwt: Arc<JwtService>,
    /// Bounded ring buffer of recent events for GET /events.
    pub event_log: EventLog,
    /// IP/CIDR ranges that bypass JWT authentication and receive Admin access.
    pub whitelist: Arc<Vec<IpNet>>,
    /// IP/CIDR ranges allowed to scrape `GET /api/v1/metrics`. Separate from
    /// the auth whitelist because Prometheus scrapers can't set Authorization
    /// headers, and the policy "who may bypass auth" is not the same as
    /// "who may scrape metrics". Empty (default) means the endpoint is locked
    /// down — every caller gets 403.
    pub metrics_whitelist: Arc<Vec<IpNet>>,
    /// UIDs allowed to connect to the admin UDS listener. Empty = "only the
    /// homecore service UID", which is resolved and added by main.rs at
    /// startup. Checked defensively after filesystem perms so a misconfigured
    /// socket mode can't silently grant admin access to every local user.
    pub uds_allowed_uids: Arc<std::collections::HashSet<u32>>,
    /// Refresh-token lifetime in days. Set at startup from
    /// `[auth].refresh_token_expiry_days`. Default 30.
    pub refresh_token_expiry_days: u64,
    /// Path to `config/modes.toml` — used by mode API handlers.
    pub modes_path: Option<Arc<std::path::PathBuf>>,
    /// Log streaming state (broadcast channel + ring buffer).
    /// None when the log streaming feature is not configured.
    pub log_stream: Option<LogStreamState>,
    /// Prometheus metrics collector — counters updated by background task,
    /// gauges refreshed on every `/metrics` scrape.
    pub metrics: std::sync::Arc<MetricsCollector>,
    /// File paths required to produce a backup archive.
    pub backup_paths: Option<BackupPaths>,
    /// Wall-clock time the server started, for uptime calculation.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Per-rule ring buffer of recent evaluation results; provided by the rule engine.
    pub fire_history: Option<hc_core::FireHistoryHandle>,
    /// Named rule groups (id, name, description, rule_ids).
    pub rule_groups: Option<Arc<RwLock<Vec<RuleGroup>>>>,
    /// Persistent store for rule groups (groups.json in rules dir).
    pub group_store: Option<Arc<GroupStore>>,
    /// Dashboard definitions and per-user defaults.
    pub dashboards: Option<Arc<RwLock<DashboardStoreData>>>,
    /// Persistent store for dashboards (`data/dashboards.json`).
    pub dashboard_store: Option<Arc<DashboardStore>>,
    /// Live calendar store — loaded `.ics` files with expanded events.
    /// `None` when no calendar directory is configured.
    pub calendar: Option<CalendarHandle>,
    /// Absolute path to the calendar directory (for fetch/delete operations).
    pub calendar_dir: Option<Arc<std::path::PathBuf>>,
    /// RRULE expansion window in days (for re-parse after fetch).
    pub calendar_expansion_days: u32,
    /// Per-plugin command channels for start/stop/restart (local plugins only).
    pub plugin_commands: PluginCommandChannels,
    /// Runtime-mutable managed-plugin store (`config/plugins/managed.toml`).
    /// Uninstall tombstones ids here so they stay removed across restarts.
    /// `None` in tests / when unconfigured.
    pub managed_plugins: Option<Arc<ManagedPluginStore>>,
    /// Where + how to install plugins (paths + broker coords). `None` in tests.
    pub plugin_install: Option<Arc<InstallContext>>,
    /// Sends a freshly-installed plugin to the supervisor for dynamic activation
    /// (no restart). Wired to a listener in `main.rs`. `None` in tests.
    pub plugin_spawn: Option<tokio::sync::mpsc::Sender<InstalledPlugin>>,
    /// Client for the remote signed plugin registry. `None` when `[registry]`
    /// isn't configured (browse/registry-install then return 503).
    pub registry: Option<Arc<registry::RegistryClient>>,
    /// MQTT management RPC for remote plugin config/commands.
    pub management_rpc: Option<management_rpc::ManagementRpc>,
    /// Handle for runtime log level changes.
    pub log_level_handle: Option<hc_logging::LogLevelHandle>,
    /// Version reported by `/health`, `/system/status`, and the
    /// `/system/versions` non-appliance fallback. Defaults to this
    /// crate's `CARGO_PKG_VERSION`, which is fine for tests but is
    /// hc-api's version, not the binary's. Production `main.rs` MUST
    /// override via `with_homecore_version(env!("CARGO_PKG_VERSION"))`
    /// — that's homecore's binary version. See HEALTH-VERSION-SOURCE-1
    /// in `release_0_1_4.md` for the fragility this works around.
    pub homecore_version: &'static str,
    /// Live registry of WebSocket connections (events_stream +
    /// logs_stream). Backs `GET /api/v1/ws/connections` so an operator
    /// can distinguish "one looping client" from "many churning
    /// clients" during a reconnect storm. OPS-1 piece 3.
    pub ws_connections: ws::WsConnections,
    /// Active streaming requests. Concurrency enforcement +
    /// plugin-offline / timeout injection hang off this.
    pub streaming_registry: streaming::StreamingRegistry,
    /// In-process event cache per streaming request_id. The SSE handler
    /// replays cached events to subscribers that connect after emission
    /// began (common for fast actions — the HTTP accept→open round-trip
    /// is longer than the whole action).
    pub stream_cache: streaming::StreamCache,
    /// Live battery watcher config — `Some` when the watcher is enabled.
    /// Read by `GET /system/battery_settings`. Holding the sender here keeps
    /// open the option of a future `PATCH` updating thresholds at runtime.
    pub battery_config:
        Option<Arc<tokio::sync::watch::Sender<hc_core::battery_watcher::BatteryConfig>>>,
    /// Path to homecore.toml — used by handlers that need to write
    /// runtime changes back to disk (currently the plugin
    /// enable/disable toggle persists via this). `None` if hc-core
    /// was started in a way that didn't surface a config path.
    pub homecore_config_path: Option<Arc<std::path::PathBuf>>,
    /// Sender on the graceful-shutdown channel. Held by AppState so
    /// the `POST /system/restart` handler can request a clean exit;
    /// the runtime supervisor (systemd / docker / hand-rolled) is
    /// expected to spawn the process again.
    pub shutdown_tx: Option<Arc<tokio::sync::watch::Sender<bool>>>,
}

/// Subscribe to `event_bus` and mirror `PluginRegistered`,
/// `PluginHeartbeat`, `PluginOffline`, and `PluginCapabilities` events
/// into the shared `plugins` map.
///
/// **Call this BEFORE plugins are spawned.** Plugins publish their retained
/// capability manifest on CONNACK; the MQTT retained delivery → state_bridge
/// → pub_bus chain happens during plugin startup, well before
/// `AppState::new_with_plugins` is built. If this listener is spawned only
/// inside the AppState constructor, the initial manifest events fire while
/// it has no subscriber and are lost forever (tokio broadcast does not
/// replay history on late subscribe).
pub fn spawn_plugin_registry_listener(
    event_bus: EventBus,
    plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
) {
    let mut rx = event_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(hc_types::event::Event::PluginRegistered {
                    plugin_id,
                    timestamp,
                }) => {
                    let mut map = plugins.write().await;
                    let rec = map
                        .entry(plugin_id.clone())
                        .or_insert_with(|| PluginRecord {
                            plugin_id: plugin_id.clone(),
                            registered_at: timestamp,
                            status: "active".into(),
                            enabled: false,
                            managed: false,
                            config_path: None,
                            binary_path: None,
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
                            installed_version: None,
                        });
                    rec.status = "active".into();
                    rec.registered_at = timestamp;
                }
                Ok(hc_types::event::Event::PluginOffline { plugin_id, .. }) => {
                    let mut map = plugins.write().await;
                    if let Some(rec) = map.get_mut(&plugin_id) {
                        rec.status = "offline".into();
                    }
                }
                Ok(hc_types::event::Event::PluginHeartbeat {
                    plugin_id,
                    timestamp,
                    version,
                    sdk_version: _,
                    uptime_secs,
                    device_count,
                }) => {
                    let mut map = plugins.write().await;
                    let rec = map
                        .entry(plugin_id.clone())
                        .or_insert_with(|| PluginRecord {
                            plugin_id: plugin_id.clone(),
                            registered_at: timestamp,
                            status: "active".into(),
                            enabled: false,
                            managed: false,
                            config_path: None,
                            binary_path: None,
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
                            installed_version: None,
                        });
                    rec.last_heartbeat = Some(timestamp);
                    rec.supports_management = true;
                    if let Some(v) = version {
                        rec.version = Some(v);
                    }
                    if let Some(u) = uptime_secs {
                        rec.uptime_started = Some(timestamp - chrono::Duration::seconds(u as i64));
                    }
                    if let Some(d) = device_count {
                        rec.device_count = d;
                    }
                    if rec.status == "offline" || rec.status == "starting" {
                        rec.status = "active".into();
                    }
                }
                Ok(hc_types::event::Event::PluginCapabilities {
                    plugin_id,
                    timestamp,
                    capabilities,
                    config_schema,
                    config_descriptor,
                }) => {
                    let mut map = plugins.write().await;
                    let rec = map
                        .entry(plugin_id.clone())
                        .or_insert_with(|| PluginRecord {
                            plugin_id: plugin_id.clone(),
                            registered_at: timestamp,
                            status: "active".into(),
                            enabled: false,
                            managed: false,
                            config_path: None,
                            binary_path: None,
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
                            installed_version: None,
                        });
                    rec.capabilities = Some(capabilities);
                    if config_schema.is_some() {
                        rec.config_schema = config_schema;
                    }
                    if config_descriptor.is_some() {
                        rec.config_descriptor = config_descriptor;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Parse `homecore/plugins/<id>/state/set` → `<id>`, or `None` if the topic is
/// not a plugin state-write.
fn parse_plugin_state_set_topic(topic: &str) -> Option<&str> {
    match topic.split('/').collect::<Vec<_>>().as_slice() {
        ["homecore", "plugins", id, "state", "set"] => Some(*id),
        _ => None,
    }
}

/// Core of the write-back path: a plugin published a learned-state delta to
/// `homecore/plugins/<id>/state/set`. Merge it into durable storage and return
/// the `(retained topic, serialized doc)` to re-publish as the authoritative
/// `homecore/plugins/<id>/state`. `Ok(None)` when the topic isn't a state-write.
pub async fn ingest_plugin_state_set(
    store: &StateStore,
    topic: &str,
    payload: &[u8],
) -> Result<Option<(String, Vec<u8>)>> {
    let Some(id) = parse_plugin_state_set_topic(topic) else {
        return Ok(None);
    };
    let delta: serde_json::Value = serde_json::from_slice(payload)
        .with_context(|| format!("parse state/set payload for {id}"))?;
    let merged = store.plugin_state_merge(id, &delta).await?;
    let bytes = serde_json::to_vec(&merged)?;
    Ok(Some((format!("homecore/plugins/{id}/state"), bytes)))
}

/// Subscribe to the raw MQTT bus and persist plugin learned-state writes
/// (`homecore/plugins/<id>/state/set`) to redb, re-publishing the merged result
/// as the retained `homecore/plugins/<id>/state`. This is the plugin→core half
/// of the D8 learned-state channel (the core→plugin half is the retained topic,
/// restored at boot in `main.rs`).
///
/// Spawn on the **raw / internal** bus (the one carrying `Event::MqttMessage`).
pub fn spawn_plugin_state_listener(raw_bus: EventBus, store: StateStore, publish: PublishHandle) {
    let mut rx = raw_bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(hc_types::event::Event::MqttMessage { topic, payload, .. }) => {
                    match ingest_plugin_state_set(&store, &topic, &payload).await {
                        Ok(Some((out_topic, bytes))) => {
                            if let Err(e) = publish.publish_retained(&out_topic, bytes).await {
                                warn!(topic = %out_topic, error = %e, "Failed to re-publish plugin state");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(topic = %topic, error = %e, "Failed to ingest plugin state/set")
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

impl AppState {
    pub fn new(
        store: StateStore,
        event_bus: EventBus,
        publish: Option<PublishHandle>,
        source_rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rule_file_store: Option<RuleFileStore>,
        jwt: JwtService,
        whitelist: Vec<IpNet>,
        modes_path: Option<std::path::PathBuf>,
    ) -> Self {
        // Default single-bus construction — suitable for test harnesses
        // that merge raw MqttMessage and typed events onto one channel.
        let raw_bus = event_bus.clone();
        Self::new_with_plugins_and_raw_bus(
            store,
            event_bus,
            raw_bus,
            publish,
            source_rules_handle,
            rules_handle,
            rule_file_store,
            jwt,
            whitelist,
            modes_path,
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    /// Back-compat: creates with a pre-populated plugin registry, using
    /// `event_bus` for the raw bus as well. Prefer
    /// `new_with_plugins_and_raw_bus` in production where internal_bus
    /// (MqttMessage only) is distinct from pub_bus (typed events only).
    pub fn new_with_plugins(
        store: StateStore,
        event_bus: EventBus,
        publish: Option<PublishHandle>,
        source_rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rule_file_store: Option<RuleFileStore>,
        jwt: JwtService,
        whitelist: Vec<IpNet>,
        modes_path: Option<std::path::PathBuf>,
        plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    ) -> Self {
        let raw_bus = event_bus.clone();
        Self::new_with_plugins_and_raw_bus(
            store,
            event_bus,
            raw_bus,
            publish,
            source_rules_handle,
            rules_handle,
            rule_file_store,
            jwt,
            whitelist,
            modes_path,
            plugins,
        )
    }

    /// Primary constructor used by production. `raw_bus` carries
    /// `Event::MqttMessage` (in production this is `internal_bus`);
    /// `event_bus` carries typed events (production = `pub_bus`). The
    /// plugin-stream SSE handler and terminal observer subscribe to
    /// `raw_bus`.
    pub fn new_with_plugins_and_raw_bus(
        store: StateStore,
        event_bus: EventBus,
        raw_bus: EventBus,
        publish: Option<PublishHandle>,
        source_rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rule_file_store: Option<RuleFileStore>,
        jwt: JwtService,
        whitelist: Vec<IpNet>,
        modes_path: Option<std::path::PathBuf>,
        plugins: Arc<RwLock<HashMap<String, PluginRecord>>>,
    ) -> Self {
        // Plugin-registry sync listener is spawned by the caller BEFORE
        // plugins spawn (see `spawn_plugin_registry_listener`), so the
        // retained manifest events aren't missed. Spawning it here is too
        // late: plugins publish on CONNACK which happens minutes before
        // AppState is built.

        // Spawn heartbeat timeout sweep — marks plugins offline if no heartbeat
        // received within 90 seconds (for plugins that support management).
        {
            let plugins_sweep = Arc::clone(&plugins);
            let bus_sweep = event_bus.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let now = chrono::Utc::now();
                    let timeout = chrono::Duration::seconds(90);
                    let mut map = plugins_sweep.write().await;
                    for rec in map.values_mut() {
                        if !rec.supports_management {
                            continue;
                        }
                        if rec.status == "stopped" {
                            continue;
                        }
                        if let Some(hb) = rec.last_heartbeat {
                            if now - hb > timeout && rec.status != "offline" {
                                let prev = rec.status.clone();
                                rec.status = "offline".into();
                                let _ = bus_sweep.publish(
                                    hc_types::event::Event::PluginStatusChanged {
                                        timestamp: now,
                                        plugin_id: rec.plugin_id.clone(),
                                        status: "offline".into(),
                                        previous_status: prev,
                                    },
                                );
                            }
                        }
                    }
                }
            });
        }

        // Spawn background task: feed events into the ring buffer.
        let event_log = EventLog::new(event_log::DEFAULT_CAPACITY);
        {
            let mut rx = event_bus.subscribe();
            let log = event_log.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => log.push(&event),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Event log subscriber lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let metrics = std::sync::Arc::new(
            MetricsCollector::new().expect("failed to register Prometheus metrics"),
        );

        let state = Self {
            store,
            raw_bus,
            event_bus,
            publish,
            source_rules_handle,
            rules_handle,
            rule_file_store,
            plugins,
            jwt: Arc::new(jwt),
            event_log,
            whitelist: Arc::new(whitelist),
            metrics_whitelist: Arc::new(Vec::new()),
            uds_allowed_uids: Arc::new(std::collections::HashSet::new()),
            refresh_token_expiry_days: 30,
            modes_path: modes_path.map(Arc::new),
            log_stream: None,
            metrics,
            backup_paths: None,
            started_at: chrono::Utc::now(),
            fire_history: None,
            rule_groups: None,
            group_store: None,
            dashboards: None,
            dashboard_store: None,
            calendar: None,
            calendar_dir: None,
            calendar_expansion_days: 400,
            plugin_commands: Arc::new(RwLock::new(HashMap::new())),
            managed_plugins: None,
            plugin_install: None,
            plugin_spawn: None,
            registry: None,
            management_rpc: None,
            log_level_handle: None,
            homecore_version: env!("CARGO_PKG_VERSION"),
            ws_connections: ws::new_ws_connections(),
            streaming_registry: streaming::StreamingRegistry::new(),
            stream_cache: streaming::StreamCache::new(),
            battery_config: None,
            homecore_config_path: None,
            shutdown_tx: None,
        };

        // Spawn background task to increment metrics counters from bus events.
        metrics::spawn_metrics_listener(&state, Arc::clone(&state.metrics));
        // Watch the bus for terminal stream events → release concurrency
        // slots. Stream events are raw MQTT publishes, not typed events,
        // so we need the raw bus (=internal_bus in production).
        streaming::spawn_terminal_observer(state.streaming_registry.clone(), &state.raw_bus);
        // Populate the stream-event cache from the same raw bus so late
        // SSE subscribers can see events that landed before they connected.
        streaming::spawn_stream_cache_populator(&state.raw_bus, state.stream_cache.clone());
        // Watch for PluginStatusChanged → offline, inject synthetic error
        // on every open stream belonging to the plugin.
        if let Some(pub_handle) = state.publish.clone() {
            let registry = state.streaming_registry.clone();
            let mut rx = state.event_bus.subscribe();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(hc_types::event::Event::PluginStatusChanged {
                            plugin_id,
                            status,
                            ..
                        }) if status == "offline" => {
                            streaming::inject_plugin_offline(&registry, &pub_handle, &plugin_id)
                                .await;
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        state
    }

    /// Populate the UDS-peer UID allow-list. Empty means no UDS connections
    /// are accepted; this must be called with at least the service UID
    /// before the UDS admin listener starts.
    pub fn with_uds_allowed_uids(mut self, uids: std::collections::HashSet<u32>) -> Self {
        self.uds_allowed_uids = Arc::new(uids);
        self
    }

    /// Attach the live battery watcher config sender. Holding the sender
    /// allows future PATCH endpoints to update thresholds at runtime.
    pub fn with_battery_config(
        mut self,
        sender: Arc<tokio::sync::watch::Sender<hc_core::battery_watcher::BatteryConfig>>,
    ) -> Self {
        self.battery_config = Some(sender);
        self
    }

    /// Override the refresh-token lifetime (default 30 days).
    pub fn with_refresh_token_expiry_days(mut self, days: u64) -> Self {
        self.refresh_token_expiry_days = days;
        self
    }

    /// Check whether a peer UID from SO_PEERCRED is allowed on the admin UDS.
    /// `None` peer UID means the platform could not determine it — rejected.
    pub fn uds_uid_allowed(&self, peer_uid: Option<u32>) -> bool {
        let Some(uid) = peer_uid else {
            return false;
        };
        self.uds_allowed_uids.contains(&uid)
    }

    /// Attach log streaming state (broadcast channel + ring buffer) obtained
    /// from the `BroadcastLayer` created during logging initialisation.
    pub fn with_log_stream(mut self, state: LogStreamState) -> Self {
        self.log_stream = Some(state);
        self
    }

    /// Attach file paths used by `POST /system/backup`.
    pub fn with_backup_paths(mut self, paths: BackupPaths) -> Self {
        self.backup_paths = Some(paths);
        self
    }

    /// Attach the homecore.toml path so handlers can write runtime
    /// changes back to disk (currently used by the plugin
    /// enable/disable toggle in PATCH /plugins/{id}).
    pub fn with_homecore_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.homecore_config_path = Some(Arc::new(path));
        self
    }

    /// Attach the graceful-shutdown sender so POST /system/restart
    /// can trigger a clean exit. The runtime supervisor is expected
    /// to spawn the process again after exit.
    pub fn with_shutdown_tx(mut self, tx: tokio::sync::watch::Sender<bool>) -> Self {
        self.shutdown_tx = Some(Arc::new(tx));
        self
    }

    /// Attach the rule fire history handle from the rule engine.
    pub fn with_fire_history(mut self, handle: hc_core::FireHistoryHandle) -> Self {
        self.fire_history = Some(handle);
        self
    }

    /// Attach the rule group store and pre-loaded groups.
    pub fn with_group_store(mut self, gs: GroupStore, groups: Vec<RuleGroup>) -> Self {
        self.group_store = Some(Arc::new(gs));
        self.rule_groups = Some(Arc::new(RwLock::new(groups)));
        self
    }

    /// Attach the dashboard store and pre-loaded dashboards.
    pub fn with_dashboard_store(mut self, store: DashboardStore, data: DashboardStoreData) -> Self {
        self.dashboard_store = Some(Arc::new(store));
        self.dashboards = Some(Arc::new(RwLock::new(data)));
        self
    }

    /// Attach the calendar store handle and directory path.
    pub fn with_calendar(
        mut self,
        handle: CalendarHandle,
        dir: std::path::PathBuf,
        expansion_days: u32,
    ) -> Self {
        self.calendar = Some(handle);
        self.calendar_dir = Some(Arc::new(dir));
        self.calendar_expansion_days = expansion_days;
        self
    }

    pub fn with_plugin_commands(mut self, channels: PluginCommandChannels) -> Self {
        self.plugin_commands = channels;
        self
    }

    pub fn with_managed_plugins(mut self, store: Arc<ManagedPluginStore>) -> Self {
        self.managed_plugins = Some(store);
        self
    }

    pub fn with_plugin_install(mut self, ctx: Arc<InstallContext>) -> Self {
        self.plugin_install = Some(ctx);
        self
    }

    pub fn with_plugin_spawn(mut self, tx: tokio::sync::mpsc::Sender<InstalledPlugin>) -> Self {
        self.plugin_spawn = Some(tx);
        self
    }

    pub fn with_registry(mut self, client: Arc<registry::RegistryClient>) -> Self {
        self.registry = Some(client);
        self
    }

    pub fn with_management_rpc(mut self, rpc: management_rpc::ManagementRpc) -> Self {
        self.management_rpc = Some(rpc);
        self
    }

    /// Override the version reported by `/health`, `/system/status`, and
    /// the `/system/versions` fallback with the binary crate's
    /// `CARGO_PKG_VERSION`. Production `main.rs` calls this; tests don't
    /// need to.
    pub fn with_homecore_version(mut self, version: &'static str) -> Self {
        self.homecore_version = version;
        self
    }

    pub fn with_log_level_handle(mut self, handle: hc_logging::LogLevelHandle) -> Self {
        self.log_level_handle = Some(handle);
        self
    }

    /// Configure the IP whitelist for `GET /api/v1/metrics`. An empty list
    /// (the default) leaves the endpoint locked down — every caller gets 403.
    pub fn with_metrics_whitelist(mut self, allow: Vec<IpNet>) -> Self {
        self.metrics_whitelist = Arc::new(allow);
        self
    }
}

/// Build the top-level axum `Router`.
pub fn router(state: AppState, web_admin_dist: Option<std::path::PathBuf>) -> Router {
    // Public routes — no auth required (auth is handled inside the handler).
    let public = Router::new()
        .route("/health", get(handlers::health))
        // Bill-of-materials for the install — public for symmetry with
        // /health and to support pre-auth client-version comparison
        // (CLIENT-VER-1 in 0.1.3 plan). Reads /etc/homecore/versions.json
        // when present, falls back to {"core": <CARGO_PKG_VERSION>}.
        .route("/system/versions", get(handlers::system_versions))
        .route("/metrics", get(metrics::metrics_handler))
        .route(
            "/auth/login",
            post(auth_handlers::login).layer(middleware::from_fn(rate_limit::login_rate_limit)),
        )
        .route("/auth/refresh", post(auth_handlers::refresh))
        // WebSocket stream authenticates via ?token= query param (browsers can't
        // set Authorization headers during WS upgrade).
        .route("/events/stream", get(ws::ws_events_handler))
        // Log streaming WebSocket — same auth pattern as /events/stream.
        .route("/logs/stream", get(logs::log_stream_handler))
        // Plugin command SSE — same ?token= pattern. Can't live behind the
        // Bearer middleware because EventSource can't send headers.
        .route(
            "/plugins/:id/command/:request_id/stream",
            get(handlers::get_plugin_stream_sse),
        )
        // Webhooks are public — the path segment acts as the shared secret.
        // External services (cloud, IFTTT, etc.) POST here to fire rules.
        .route("/webhooks/:path", post(handlers::receive_webhook));

    // Protected routes — require a valid Bearer JWT *or* a whitelisted source IP.
    let protected = Router::new()
        // Auth / user management
        .route("/auth/me", get(auth_handlers::me))
        .route("/auth/roles", get(auth_handlers::list_roles))
        .route(
            "/auth/change-password",
            post(auth_handlers::change_password),
        )
        .route(
            "/auth/users",
            get(auth_handlers::list_users).post(auth_handlers::create_user),
        )
        .route("/auth/users/:id", delete(auth_handlers::delete_user))
        .route("/auth/users/:id/role", patch(auth_handlers::set_user_role))
        // API keys
        .route(
            "/auth/api-keys",
            get(api_key_handlers::list_api_keys).post(api_key_handlers::create_api_key),
        )
        .route(
            "/auth/api-keys/:id",
            delete(api_key_handlers::revoke_api_key).patch(api_key_handlers::update_api_key),
        )
        .route(
            "/auth/api-keys/:id/rotate",
            post(api_key_handlers::rotate_api_key),
        )
        // Audit log — Admin only (handler enforces).
        .route("/audit", get(audit_handlers::list_audit))
        // Devices
        .route(
            "/devices",
            get(handlers::list_devices)
                .patch(handlers::bulk_patch_devices)
                .delete(handlers::bulk_delete_devices),
        )
        // Must stay ahead of `/devices/:id` so "orphaned" is not read as an id.
        .route("/devices/orphaned", get(handlers::orphaned_devices))
        .route(
            "/devices/:id",
            get(handlers::get_device)
                .patch(handlers::update_device)
                .delete(handlers::delete_device),
        )
        .route("/devices/:id/state", patch(handlers::command_device))
        .route("/devices/:id/history", get(handlers::device_history))
        .route("/devices/:id/schema", get(handlers::get_device_schema))
        // Artwork lives on the device (a Sonos speaker serves it from its own
        // LAN address), which a browser generally cannot reach. Core fetches it
        // and hands it back same-origin.
        .route("/devices/:id/media/art", get(handlers::device_media_art))
        // Timers (timer devices are also visible via /devices)
        .route(
            "/timers",
            get(handlers::list_timers).post(handlers::create_timer),
        )
        .route("/timers/:id", get(handlers::get_timer))
        // Switches (switch devices are also visible via /devices)
        .route(
            "/switches",
            get(handlers::list_switches).post(handlers::create_switch),
        )
        // Glue devices (unified CRUD for all glue device types)
        .route(
            "/glue",
            get(handlers::list_glue).post(handlers::create_glue),
        )
        .route("/glue/:id", delete(handlers::delete_glue))
        // Modes (mode devices are also visible via /devices)
        .route(
            "/modes",
            get(handlers::list_modes).post(handlers::create_mode),
        )
        .route(
            "/modes/:id",
            get(handlers::get_mode).delete(handlers::delete_mode),
        )
        .route(
            "/modes/:id/definition",
            get(handlers::get_mode_definition)
                .put(handlers::put_mode_definition)
                .delete(handlers::delete_mode_definition),
        )
        // Areas
        .route(
            "/areas",
            get(handlers::list_areas).post(handlers::create_area),
        )
        .route(
            "/areas/:id",
            patch(handlers::patch_area).delete(handlers::delete_area),
        )
        .route("/areas/:id/devices", put(handlers::set_area_devices))
        // Automations
        .route(
            "/automations",
            get(handlers::list_automations)
                .post(handlers::create_automation)
                .patch(handlers::bulk_patch_automations),
        )
        .route(
            "/automations/:id",
            get(handlers::get_automation)
                .put(handlers::update_automation)
                .patch(handlers::patch_automation)
                .delete(handlers::delete_automation),
        )
        .route("/automations/:id/test", post(handlers::test_automation))
        .route(
            "/automations/vocabulary",
            get(handlers::get_rule_vocabulary),
        )
        .route("/automations/:id/ron", get(handlers::get_automation_ron))
        .route(
            "/automations/:id/history",
            get(handlers::automation_history),
        )
        .route("/automations/:id/clone", post(handlers::clone_automation))
        .route("/automations/stale-refs", get(handlers::stale_refs))
        .route("/automations/import", post(handlers::import_automations))
        .route("/automations/export", get(handlers::export_automations))
        // Rule groups
        .route(
            "/automations/groups",
            get(handlers::list_groups).post(handlers::create_group),
        )
        .route(
            "/automations/groups/:id",
            get(handlers::get_group)
                .patch(handlers::patch_group)
                .delete(handlers::delete_group),
        )
        .route(
            "/automations/groups/:id/:action",
            post(handlers::set_group_enabled),
        )
        // Scenes
        .route(
            "/dashboards",
            get(handlers::list_dashboards).post(handlers::create_dashboard),
        )
        .route(
            "/dashboards/templates",
            get(handlers::list_dashboard_templates),
        )
        .route(
            "/dashboards/templates/:id",
            post(handlers::create_dashboard_from_template),
        )
        .route("/dashboards/import", post(handlers::import_dashboard))
        .route("/dashboards/reload", post(handlers::reload_dashboards))
        .route(
            "/dashboards/:id",
            get(handlers::get_dashboard)
                .put(handlers::update_dashboard)
                .delete(handlers::delete_dashboard),
        )
        .route(
            "/dashboards/:id/access",
            put(handlers::set_dashboard_access),
        )
        .route("/dashboards/:id/export", get(handlers::export_dashboard))
        .route(
            "/dashboards/:id/duplicate",
            post(handlers::duplicate_dashboard),
        )
        .route(
            "/dashboards/:id/default",
            post(handlers::set_default_dashboard),
        )
        .route(
            "/scenes",
            get(handlers::list_scenes).post(handlers::create_scene),
        )
        .route(
            "/scenes/:id",
            get(handlers::get_scene)
                .put(handlers::update_scene)
                .delete(handlers::delete_scene),
        )
        .route("/scenes/export", get(handlers::export_scenes))
        .route("/scenes/import", post(handlers::import_scenes))
        .route("/scenes/:id/activate", post(handlers::activate_scene))
        // Plugins
        .route("/plugins", get(handlers::list_plugins))
        .route("/plugins/install", post(handlers::install_plugin))
        .route("/registry/plugins", get(handlers::browse_registry))
        .route("/registry/plugins/:id", get(handlers::get_registry_plugin))
        .route(
            "/plugins/:id",
            get(handlers::get_plugin)
                .delete(handlers::deregister_plugin)
                .patch(handlers::patch_plugin),
        )
        .route("/plugins/:id/start", post(handlers::start_plugin))
        .route("/plugins/:id/stop", post(handlers::stop_plugin))
        .route("/plugins/:id/restart", post(handlers::restart_plugin))
        .route(
            "/plugins/:id/config",
            get(handlers::get_plugin_config).put(handlers::put_plugin_config),
        )
        .route(
            "/plugins/:id/config/schema",
            get(handlers::get_plugin_config_schema),
        )
        .route(
            "/plugins/:id/config/descriptor",
            get(handlers::get_plugin_config_descriptor),
        )
        .route("/plugins/:id/command", post(handlers::post_plugin_command))
        .route(
            "/plugins/:id/capabilities",
            get(handlers::get_plugin_capabilities),
        )
        .route(
            "/plugins/:id/devices",
            delete(handlers::delete_plugin_devices),
        )
        .route(
            "/plugins/matter/commission",
            post(handlers::matter_commission),
        )
        .route("/plugins/matter/nodes", get(handlers::list_matter_nodes))
        .route(
            "/plugins/matter/reinterview",
            post(handlers::matter_reinterview),
        )
        .route(
            "/plugins/matter/nodes/:id",
            delete(handlers::remove_matter_node),
        )
        // Events
        .route("/events", get(handlers::list_events))
        // Calendars
        .route("/calendars", get(handlers::list_calendars))
        .route("/calendars/fetch", post(handlers::fetch_calendar))
        .route("/calendars/upload", post(handlers::upload_calendar))
        .route("/calendars/:id", delete(handlers::delete_calendar))
        .route("/calendars/:id/events", get(handlers::list_calendar_events))
        // System
        .route("/system/status", get(handlers::system_status))
        .route("/system/battery_settings", get(handlers::battery_settings))
        .route("/system/backup", post(backup::backup_handler))
        .route(
            "/system/restore",
            post(backup::restore_handler).layer(DefaultBodyLimit::max(RESTORE_BODY_LIMIT_BYTES)),
        )
        .route(
            "/system/log-level",
            get(handlers::get_log_level).put(handlers::set_log_level),
        )
        .route(
            "/system/config",
            get(handlers::get_system_config).put(handlers::put_system_config),
        )
        .route("/system/restart", post(handlers::system_restart))
        // WebSocket connection registry (OPS-1 piece 3). Admin-only;
        // role check is in the handler itself, the route_layer below
        // already enforces authentication.
        .route("/ws/connections", get(handlers::list_ws_connections))
        // REST log-tail (OPS-1 piece 2). Same auth as /logs/stream;
        // CLI-friendly companion for `curl | jq` workflows.
        .route("/logs", get(logs::list_logs))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let api = Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state);

    let app = Router::new().nest("/api/v1", api);

    if let Some(dist_path) = web_admin_dist {
        app.merge(hc_web_admin::router(dist_path))
    } else {
        app
    }
}

/// Bind and serve the API on the given address.
///
/// Uses `into_make_service_with_connect_info` so that the remote socket address
/// is available to middleware (required for IP whitelist checking).
///
/// When `shutdown` receives `true` the server stops accepting new connections,
/// drains in-flight requests, and returns.
/// Admin UDS listener configuration.
#[derive(Clone, Debug)]
pub struct AdminUdsConfig {
    pub path: std::path::PathBuf,
    pub group: String,
    /// Octal file mode (e.g. 0o660).
    pub mode: u32,
}

pub async fn serve(
    host: &str,
    port: u16,
    state: AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    drain_timeout_secs: u64,
    web_admin_dist: Option<std::path::PathBuf>,
    admin_uds: Option<AdminUdsConfig>,
) -> Result<()> {
    let addr = format!("{host}:{port}");
    info!(%addr, "HomeCore API server starting");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let mut shutdown_timeout = shutdown.clone();
    let signal = async move {
        loop {
            if shutdown.changed().await.is_err() {
                break;
            }
            if *shutdown.borrow() {
                break;
            }
        }
        info!("API server: shutdown signal received — draining connections");
    };

    // Build the router once, then clone it for each listener. The Router
    // uses Arc internally so clones are cheap.
    let app = router(state.clone(), web_admin_dist);

    // Admin UDS listener — if configured, share the same router but with
    // the admin-bypass middleware layer applied at the transport boundary.
    if let Some(uds_cfg) = admin_uds {
        match prepare_uds(&uds_cfg).await {
            Ok(uds_listener) => {
                let app_for_uds = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = admin_uds::serve(uds_listener, app_for_uds).await {
                        warn!(error = %e, "Admin UDS listener exited with error");
                    }
                });
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Failed to bind admin UDS — continuing without it"
                );
            }
        }
    }

    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(signal)
        .await
    });

    loop {
        tokio::select! {
            joined = &mut server => {
                joined??;
                return Ok(());
            }
            changed = shutdown_timeout.changed() => {
                match changed {
                    Ok(()) if *shutdown_timeout.borrow() => {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(drain_timeout_secs),
                            &mut server,
                        )
                        .await
                        {
                            Ok(joined) => {
                                joined??;
                                return Ok(());
                            }
                            Err(_) => {
                                warn!(
                                    drain_timeout_secs,
                                    "API server graceful shutdown timed out — aborting remaining connections"
                                );
                                server.abort();
                                let _ = server.await;
                                return Ok(());
                            }
                        }
                    }
                    Ok(()) => {}
                    Err(_) => {
                        let joined = server.await;
                        joined??;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Bind the admin UDS, set its group + mode, and return the listener.
async fn prepare_uds(cfg: &AdminUdsConfig) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    // Clean any stale socket from a previous run.
    let _ = std::fs::remove_file(&cfg.path);

    // Ensure parent directory exists.
    if let Some(parent) = cfg.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating UDS parent dir {}", parent.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(&cfg.path)
        .with_context(|| format!("binding UDS at {}", cfg.path.display()))?;

    // chown to the admin group — members of this group can connect.
    let gid = admin_uds::resolve_group_gid(&cfg.group)
        .with_context(|| format!("resolving admin UDS group `{}`", cfg.group))?;
    nix::unistd::chown(&cfg.path, None, Some(nix::unistd::Gid::from_raw(gid)))
        .with_context(|| format!("chown {} to gid {gid}", cfg.path.display()))?;

    std::fs::set_permissions(&cfg.path, std::fs::Permissions::from_mode(cfg.mode))
        .with_context(|| format!("chmod {} to {:o}", cfg.path.display(), cfg.mode))?;

    admin_uds::warn_if_mode_too_loose(&cfg.path);

    info!(
        path = %cfg.path.display(),
        group = %cfg.group,
        mode = format!("{:o}", cfg.mode),
        "Admin UDS listener bound"
    );
    Ok(listener)
}

#[cfg(test)]
mod plugin_state_tests {
    use super::*;

    #[test]
    fn parse_state_set_topic_matches_only_state_set() {
        assert_eq!(
            parse_plugin_state_set_topic("homecore/plugins/plugin.hue/state/set"),
            Some("plugin.hue")
        );
        assert_eq!(
            parse_plugin_state_set_topic("homecore/plugins/plugin.hue/state"),
            None
        );
        assert_eq!(
            parse_plugin_state_set_topic("homecore/plugins/plugin.hue/cmd"),
            None
        );
        assert_eq!(
            parse_plugin_state_set_topic("homecore/devices/x/state/set"),
            None
        );
    }

    #[tokio::test]
    async fn ingest_merges_and_returns_retained_republish() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            dir.path().join("state.redb").to_str().unwrap(),
            dir.path().join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();

        // Non-matching topic → Ok(None).
        assert!(
            ingest_plugin_state_set(&store, "homecore/plugins/p/state", b"{}")
                .await
                .unwrap()
                .is_none()
        );

        // First write establishes the doc; returns the retained republish target.
        let (topic, bytes) = ingest_plugin_state_set(
            &store,
            "homecore/plugins/plugin.hue/state/set",
            br#"{"app_key":"k1"}"#,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(topic, "homecore/plugins/plugin.hue/state");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc, serde_json::json!({ "app_key": "k1" }));

        // Second write shallow-merges (adds a key, keeps the first).
        let (_t, bytes2) = ingest_plugin_state_set(
            &store,
            "homecore/plugins/plugin.hue/state/set",
            br#"{"published_ids":["a","b"]}"#,
        )
        .await
        .unwrap()
        .unwrap();
        let doc2: serde_json::Value = serde_json::from_slice(&bytes2).unwrap();
        assert_eq!(
            doc2,
            serde_json::json!({ "app_key": "k1", "published_ids": ["a", "b"] })
        );

        // Persisted durably.
        assert_eq!(
            store.plugin_state_get("plugin.hue").await.unwrap().unwrap(),
            doc2
        );

        // Bad payload on a matching topic → Err (the listener logs and drops it).
        assert!(ingest_plugin_state_set(
            &store,
            "homecore/plugins/plugin.hue/state/set",
            b"not json"
        )
        .await
        .is_err());
    }
}
