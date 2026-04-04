//! `hc-api` — axum HTTP + WebSocket API server.

use anyhow::Result;
use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
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

pub mod auth_handlers;
pub mod auth_middleware;
pub mod backup;
pub mod dashboard_store;
pub mod event_log;
pub mod group_store;
pub mod handlers;
pub mod logs;
pub mod managed_modes;
pub mod management_rpc;
pub mod metrics;
pub mod mode_definition_store;
pub mod rule_file_store;
pub mod ws;

use auth_middleware::require_auth;
use backup::BackupPaths;
use dashboard_store::{DashboardStore, DashboardStoreData};
use event_log::EventLog;
use group_store::{GroupStore, RuleGroup};
use logs::LogStreamState;
use metrics::MetricsCollector;
use rule_file_store::RuleFileStore;

/// Runtime command sent to a plugin supervisor task.
#[derive(Debug)]
pub enum PluginCommand {
    Start,
    Stop,
    Restart,
}

/// Per-plugin command sender, indexed by plugin_id.
pub type PluginCommandChannels = Arc<RwLock<HashMap<String, tokio::sync::mpsc::Sender<PluginCommand>>>>;

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
}

/// Shared state injected into every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub store: StateStore,
    pub event_bus: EventBus,
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
    /// MQTT management RPC for remote plugin config/commands.
    pub management_rpc: Option<management_rpc::ManagementRpc>,
    /// Handle for runtime log level changes.
    pub log_level_handle: Option<hc_logging::LogLevelHandle>,
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
        Self::new_with_plugins(
            store,
            event_bus,
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

    /// Create with a pre-populated plugin registry (shared with PluginManager).
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
        // Spawn background task to keep plugin registry in sync with bus events.
        {
            let mut rx = event_bus.subscribe();
            let plugins_clone = Arc::clone(&plugins);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(hc_types::event::Event::PluginRegistered {
                            plugin_id,
                            timestamp,
                        }) => {
                            let mut map = plugins_clone.write().await;
                            let rec = map.entry(plugin_id.clone()).or_insert_with(|| PluginRecord {
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
                            });
                            rec.status = "active".into();
                            rec.registered_at = timestamp;
                        }
                        Ok(hc_types::event::Event::PluginOffline { plugin_id, .. }) => {
                            let mut map = plugins_clone.write().await;
                            if let Some(rec) = map.get_mut(&plugin_id) {
                                rec.status = "offline".into();
                            }
                        }
                        Ok(hc_types::event::Event::PluginHeartbeat {
                            plugin_id,
                            timestamp,
                            version,
                            uptime_secs,
                            device_count,
                        }) => {
                            let mut map = plugins_clone.write().await;
                            let rec = map.entry(plugin_id.clone()).or_insert_with(|| PluginRecord {
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
                            });
                            rec.last_heartbeat = Some(timestamp);
                            rec.supports_management = true;
                            if let Some(v) = version { rec.version = Some(v); }
                            if let Some(u) = uptime_secs {
                                rec.uptime_started = Some(timestamp - chrono::Duration::seconds(u as i64));
                            }
                            if let Some(d) = device_count { rec.device_count = d; }
                            // If plugin was offline, mark it active again.
                            if rec.status == "offline" {
                                rec.status = "active".into();
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

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
                        if !rec.supports_management { continue; }
                        if rec.status == "stopped" { continue; }
                        if let Some(hb) = rec.last_heartbeat {
                            if now - hb > timeout && rec.status != "offline" {
                                let prev = rec.status.clone();
                                rec.status = "offline".into();
                                let _ = bus_sweep.publish(hc_types::event::Event::PluginStatusChanged {
                                    timestamp: now,
                                    plugin_id: rec.plugin_id.clone(),
                                    status: "offline".into(),
                                    previous_status: prev,
                                });
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
            event_bus,
            publish,
            source_rules_handle,
            rules_handle,
            rule_file_store,
            plugins,
            jwt: Arc::new(jwt),
            event_log,
            whitelist: Arc::new(whitelist),
            modes_path: modes_path.map(|p| Arc::new(p)),
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
            management_rpc: None,
            log_level_handle: None,
        };

        // Spawn background task to increment metrics counters from bus events.
        metrics::spawn_metrics_listener(&state, Arc::clone(&state.metrics));

        state
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

    pub fn with_management_rpc(mut self, rpc: management_rpc::ManagementRpc) -> Self {
        self.management_rpc = Some(rpc);
        self
    }

    pub fn with_log_level_handle(mut self, handle: hc_logging::LogLevelHandle) -> Self {
        self.log_level_handle = Some(handle);
        self
    }
}

/// Build the top-level axum `Router`.
pub fn router(state: AppState, web_admin_enabled: bool) -> Router {
    // Public routes — no auth required (auth is handled inside the handler).
    let public = Router::new()
        .route("/health", get(handlers::health))
        .route("/metrics", get(metrics::metrics_handler))
        .route("/auth/login", post(auth_handlers::login))
        // WebSocket stream authenticates via ?token= query param (browsers can't
        // set Authorization headers during WS upgrade).
        .route("/events/stream", get(ws::ws_events_handler))
        // Log streaming WebSocket — same auth pattern as /events/stream.
        .route("/logs/stream", get(logs::log_stream_handler))
        // Webhooks are public — the path segment acts as the shared secret.
        // External services (cloud, IFTTT, etc.) POST here to fire rules.
        .route("/webhooks/:path", post(handlers::receive_webhook));

    // Protected routes — require a valid Bearer JWT *or* a whitelisted source IP.
    let protected = Router::new()
        // Auth / user management
        .route("/auth/me", get(auth_handlers::me))
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
        // Devices
        .route(
            "/devices",
            get(handlers::list_devices)
                .patch(handlers::bulk_patch_devices)
                .delete(handlers::bulk_delete_devices),
        )
        .route(
            "/devices/:id",
            get(handlers::get_device)
                .patch(handlers::update_device)
                .delete(handlers::delete_device),
        )
        .route("/devices/:id/state", patch(handlers::command_device))
        .route("/devices/:id/history", get(handlers::device_history))
        .route("/devices/:id/schema", get(handlers::get_device_schema))
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
        .route(
            "/plugins/:id",
            get(handlers::get_plugin).delete(handlers::deregister_plugin).patch(handlers::patch_plugin),
        )
        .route("/plugins/:id/start", post(handlers::start_plugin))
        .route("/plugins/:id/stop", post(handlers::stop_plugin))
        .route("/plugins/:id/restart", post(handlers::restart_plugin))
        .route(
            "/plugins/:id/config",
            get(handlers::get_plugin_config).put(handlers::put_plugin_config),
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
        .route("/calendars/:id", delete(handlers::delete_calendar))
        .route("/calendars/:id/events", get(handlers::list_calendar_events))
        // System
        .route("/system/status", get(handlers::system_status))
        .route("/system/backup", post(backup::backup_handler))
        .route("/system/log-level", get(handlers::get_log_level).put(handlers::set_log_level))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let api = Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state);

    let app = Router::new().nest("/api/v1", api);

    if web_admin_enabled {
        app.nest("/admin", hc_web_admin::router())
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
pub async fn serve(
    host: &str,
    port: u16,
    state: AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    drain_timeout_secs: u64,
    web_admin_enabled: bool,
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
    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state, web_admin_enabled).into_make_service_with_connect_info::<SocketAddr>(),
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
