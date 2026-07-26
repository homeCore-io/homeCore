//! GET /api/v1/logs/stream — live log streaming over WebSocket.
//!
//! Query parameters:
//!   token=<JWT>         Authentication (same as /events/stream)
//!   level=info          Minimum log level (error|warn|info|debug|trace)
//!   target=hc_core      Optional target prefix filter
//!   history=50          Lines of ring-buffer history to send first (max 500)

use crate::metrics::MetricsCollector;
use crate::ws::{register_connection, WsConnections};
use crate::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hc_types::LogLine;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tracing::debug;

/// Shared state for the log streaming endpoint, held in `AppState`.
#[derive(Clone)]
pub struct LogStreamState {
    pub tx: broadcast::Sender<LogLine>,
    pub ring: Arc<Mutex<VecDeque<LogLine>>>,
}

#[derive(Deserialize)]
pub struct LogStreamQuery {
    pub token: Option<String>,
    #[serde(default = "default_level")]
    pub level: String,
    pub target: Option<String>,
    #[serde(default = "default_history")]
    pub history: usize,
    /// Optional per-tab fingerprint, mirrors `/events/stream`. Surfaces
    /// in the active-connection registry (OPS-1 piece 3) so an operator
    /// can tell apart simultaneous logs/stream subscribers.
    pub client_id: Option<String>,
}

fn default_level() -> String {
    "info".to_string()
}
fn default_history() -> usize {
    50
}

pub async fn log_stream_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<LogStreamQuery>,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // Authenticate — same pattern as /events/stream.
    let ip = match addr.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(std::net::IpAddr::V4)
            .unwrap_or(std::net::IpAddr::V6(v6)),
        v4 => v4,
    };

    let is_whitelisted =
        !state.whitelist.is_empty() && state.whitelist.iter().any(|net| net.contains(&ip));

    // `user` for the active-connection registry. Whitelisted callers
    // are recorded as "whitelist"; JWT callers as the validated subject.
    let user = if is_whitelisted {
        "whitelist".to_string()
    } else {
        let token = params.token.as_deref().unwrap_or("");
        if token.is_empty() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing token query parameter" })),
            )
                .into_response();
        }
        match state.jwt.validate(token) {
            Ok(claims) => claims.sub,
            Err(_) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "invalid or expired token" })),
                )
                    .into_response();
            }
        }
    };

    // Check that the log stream feature is enabled.
    let log_stream = match &state.log_stream {
        Some(ls) => ls.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "log streaming not enabled" })),
            )
                .into_response();
        }
    };

    let min_level = parse_level(&params.level);
    let target_filter = params.target.clone();
    let history_count = params.history.min(500);
    let client_id = params.client_id.clone();
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let connections = state.ws_connections.clone();
    let metrics = state.metrics.clone();

    ws.on_upgrade(move |socket| {
        handle_socket(
            socket,
            log_stream,
            min_level,
            target_filter,
            history_count,
            connections,
            client_id,
            ip,
            user,
            user_agent,
            metrics,
        )
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_socket(
    mut socket: WebSocket,
    state: LogStreamState,
    min_level: u8,
    target_filter: Option<String>,
    history_count: usize,
    connections: WsConnections,
    client_id: Option<String>,
    ip: IpAddr,
    user: String,
    user_agent: Option<String>,
    metrics: Arc<MetricsCollector>,
) {
    // Register in the active-connection map; guard auto-removes on
    // every return path. OPS-1 piece 3.
    let _registry_guard =
        register_connection(&connections, "logs_stream", client_id, ip, user, user_agent);
    // Bump connect counter (OPS-1 piece 4); disconnect counter is
    // incremented after the labelled block below with the categorised
    // reason — same scheme as `/events/stream`.
    metrics
        .ws_connects_total
        .with_label_values(&["logs_stream"])
        .inc();

    // Send ring-buffer history first, then the live tail. Each break
    // path sets a `reason` label that flows through to the disconnect
    // counter and any future log-line we add.
    //
    //   history_send_failed — couldn't write a queued line to the socket
    //   event_send_failed   — couldn't write a live broadcast line
    //   bus_closed          — log broadcast channel closed (server shutdown)
    //   socket_closed       — recv returned None (client went away)
    let reason: &'static str = 'sock: {
        // History flush.
        let history: Vec<LogLine> = {
            let ring = state.ring.lock().unwrap();
            let start = ring.len().saturating_sub(history_count);
            ring.iter().skip(start).cloned().collect()
        };
        for line in history {
            if !passes_filter(&line, min_level, target_filter.as_deref()) {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&line) {
                if socket.send(Message::Text(json)).await.is_err() {
                    break 'sock "history_send_failed";
                }
            }
        }

        // Live tail.
        let mut rx = state.tx.subscribe();
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(line) => {
                            if !passes_filter(&line, min_level, target_filter.as_deref()) {
                                continue;
                            }
                            match serde_json::to_string(&line) {
                                Ok(json) => {
                                    if socket.send(Message::Text(json)).await.is_err() {
                                        break 'sock "event_send_failed";
                                    }
                                }
                                Err(e) => debug!(error = %e, "log_stream: serialise error"),
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug!("log_stream: lagged by {n} events");
                        }
                        Err(broadcast::error::RecvError::Closed) => break 'sock "bus_closed",
                    }
                }
                result = socket.recv() => {
                    if result.is_none() { break 'sock "socket_closed"; }
                }
            }
        }
    };

    metrics
        .ws_disconnects_total
        .with_label_values(&["logs_stream", reason])
        .inc();
}

// ── REST log-tail endpoint (OPS-1 piece 2) ──────────────────────────────────
//
// `/api/v1/logs/stream` is WebSocket-only — friendly to browsers, hostile to
// `curl | jq` scripting. This REST companion serves the same ring-buffer
// contents synchronously so CLI tooling (and one-off troubleshooting from
// the operator's laptop) doesn't need a WS client.

const DEFAULT_TAIL_LIMIT: usize = 100;
const MAX_TAIL_LIMIT: usize = 1000;

#[derive(Deserialize)]
pub struct LogTailQuery {
    /// Minimum level (`error|warn|info|debug|trace`). Default: `info`.
    #[serde(default = "default_level")]
    pub level: String,
    /// Optional target prefix filter (matches `starts_with`, same as
    /// `/logs/stream`). Example: `target=hc_api::ws`.
    pub target: Option<String>,
    /// Optional RFC3339 timestamp; only lines newer than this are
    /// returned. Useful for incremental polling.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Maximum number of lines (default 100, cap 1000). Returns the
    /// most recent matching entries.
    pub limit: Option<usize>,
}

/// `GET /api/v1/logs` — REST tail of the same ring buffer the
/// `/logs/stream` WebSocket reads from. Same filter semantics
/// (`level`, `target` prefix); plus `since` for incremental polling
/// and a `limit` cap.
///
/// Returns:
/// ```text
/// {
///   "count": <usize>,
///   "lines": [LogLine, ...]   // chronological, oldest first
/// }
/// ```
///
/// Same auth as `/logs/stream` (JWT or whitelist). 503 when the
/// log-stream feature isn't enabled in config.
pub async fn list_logs(
    State(state): State<AppState>,
    Query(params): Query<LogTailQuery>,
) -> Response {
    let log_stream = match &state.log_stream {
        Some(ls) => ls.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "log streaming not enabled" })),
            )
                .into_response();
        }
    };

    let min_level = parse_level(&params.level);
    let target = params.target.as_deref();
    let since = params.since;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_TAIL_LIMIT)
        .min(MAX_TAIL_LIMIT);

    // Snapshot the ring under the lock, then filter outside the lock
    // so we don't block live writers.
    let snapshot: Vec<LogLine> = {
        let ring = log_stream.ring.lock().unwrap();
        ring.iter().cloned().collect()
    };

    let filtered: Vec<LogLine> = snapshot
        .into_iter()
        .filter(|l| passes_filter(l, min_level, target))
        .filter(|l| since.map(|t| l.timestamp > t).unwrap_or(true))
        .collect();

    // "Last N matching" — take from the tail of the chronological list.
    let start = filtered.len().saturating_sub(limit);
    let lines: Vec<LogLine> = filtered.into_iter().skip(start).collect();

    Json(json!({
        "count": lines.len(),
        "lines": lines,
    }))
    .into_response()
}

fn passes_filter(line: &LogLine, min_level: u8, target: Option<&str>) -> bool {
    if level_to_u8(&line.level) < min_level {
        return false;
    }
    if let Some(t) = target {
        if !line.target.starts_with(t) {
            return false;
        }
    }
    true
}

/// Higher number = more severe (matches tracing convention).
fn level_to_u8(level: &str) -> u8 {
    match level.to_uppercase().as_str() {
        "TRACE" => 1,
        "DEBUG" => 2,
        "INFO" => 3,
        "WARN" => 4,
        "ERROR" => 5,
        _ => 3,
    }
}

fn parse_level(s: &str) -> u8 {
    level_to_u8(s)
}
