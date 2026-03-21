//! `hc-api` — axum HTTP + WebSocket API server.

use anyhow::Result;
use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use hc_auth::JwtService;
use hc_core::EventBus;
use hc_mqtt_client::PublishHandle;
use hc_state::StateStore;
use hc_types::rule::Rule;
use ipnet::IpNet;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

pub mod auth_handlers;
pub mod auth_middleware;
pub mod event_log;
pub mod handlers;
pub mod rule_file_store;
pub mod ws;

use auth_middleware::require_auth;
use event_log::EventLog;
use rule_file_store::RuleFileStore;

/// Registered plugin record stored in-memory.
#[derive(Clone, serde::Serialize)]
pub struct PluginRecord {
    pub plugin_id: String,
    pub registered_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
}

/// Shared state injected into every handler via axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub store: StateStore,
    pub event_bus: EventBus,
    pub publish: Option<PublishHandle>,
    /// Live rule set — source of truth for the rule engine and all API reads.
    /// Updated atomically by the API (write-through to files) and by the
    /// hot-reload watcher when files change on disk.
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
}

impl AppState {
    pub fn new(
        store: StateStore,
        event_bus: EventBus,
        publish: Option<PublishHandle>,
        rules_handle: Option<Arc<RwLock<Vec<Rule>>>>,
        rule_file_store: Option<RuleFileStore>,
        jwt: JwtService,
        whitelist: Vec<IpNet>,
    ) -> Self {
        let plugins = Arc::new(RwLock::new(HashMap::new()));

        // Spawn background task to keep plugin registry in sync with bus events.
        {
            let mut rx = event_bus.subscribe();
            let plugins_clone = Arc::clone(&plugins);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(hc_types::event::Event::PluginRegistered { plugin_id, timestamp }) => {
                            let mut map = plugins_clone.write().await;
                            map.insert(plugin_id.clone(), PluginRecord {
                                plugin_id,
                                registered_at: timestamp,
                                status: "active".into(),
                            });
                        }
                        Ok(hc_types::event::Event::PluginOffline { plugin_id, .. }) => {
                            let mut map = plugins_clone.write().await;
                            if let Some(rec) = map.get_mut(&plugin_id) {
                                rec.status = "offline".into();
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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

        Self {
            store,
            event_bus,
            publish,
            rules_handle,
            rule_file_store,
            plugins,
            jwt: Arc::new(jwt),
            event_log,
            whitelist: Arc::new(whitelist),
        }
    }
}

/// Build the top-level axum `Router`.
pub fn router(state: AppState) -> Router {
    // Public routes — no auth required (auth is handled inside the handler).
    let public = Router::new()
        .route("/health", get(handlers::health))
        .route("/auth/login", post(auth_handlers::login))
        // WebSocket stream authenticates via ?token= query param (browsers can't
        // set Authorization headers during WS upgrade).
        .route("/events/stream", get(ws::ws_events_handler))
        // Webhooks are public — the path segment acts as the shared secret.
        // External services (cloud, IFTTT, etc.) POST here to fire rules.
        .route("/webhooks/:path", post(handlers::receive_webhook));

    // Protected routes — require a valid Bearer JWT *or* a whitelisted source IP.
    let protected = Router::new()
        // Auth / user management
        .route("/auth/me", get(auth_handlers::me))
        .route("/auth/change-password", post(auth_handlers::change_password))
        .route("/auth/users", get(auth_handlers::list_users).post(auth_handlers::create_user))
        .route("/auth/users/:id", delete(auth_handlers::delete_user))
        .route("/auth/users/:id/role", patch(auth_handlers::set_user_role))
        // Devices
        .route("/devices", get(handlers::list_devices))
        .route("/devices/:id", get(handlers::get_device).patch(handlers::update_device))
        .route("/devices/:id/state", patch(handlers::command_device))
        .route("/devices/:id/history", get(handlers::device_history))
        // Areas
        .route("/areas", get(handlers::list_areas).post(handlers::create_area))
        .route("/areas/:id/devices", put(handlers::set_area_devices))
        // Automations
        .route("/automations", get(handlers::list_automations).post(handlers::create_automation))
        .route(
            "/automations/:id",
            get(handlers::get_automation)
                .put(handlers::update_automation)
                .patch(handlers::patch_automation)
                .delete(handlers::delete_automation),
        )
        .route("/automations/:id/test", post(handlers::test_automation))
        .route("/automations/import", post(handlers::import_automations))
        .route("/automations/export", get(handlers::export_automations))
        // Scenes
        .route("/scenes", get(handlers::list_scenes).post(handlers::create_scene))
        .route("/scenes/:id/activate", post(handlers::activate_scene))
        // Plugins
        .route("/plugins", get(handlers::list_plugins))
        .route("/plugins/:id", delete(handlers::deregister_plugin))
        // Events
        .route("/events", get(handlers::list_events))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let api = Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state);

    Router::new().nest("/api/v1", api)
}

/// Bind and serve the API on the given address.
///
/// Uses `into_make_service_with_connect_info` so that the remote socket address
/// is available to middleware (required for IP whitelist checking).
pub async fn serve(host: &str, port: u16, state: AppState) -> Result<()> {
    let addr = format!("{host}:{port}");
    info!(%addr, "HomeCore API server starting");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
