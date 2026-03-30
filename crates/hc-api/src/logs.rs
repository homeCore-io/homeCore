//! GET /api/v1/logs/stream — live log streaming over WebSocket.
//!
//! Query parameters:
//!   token=<JWT>         Authentication (same as /events/stream)
//!   level=info          Minimum log level (error|warn|info|debug|trace)
//!   target=hc_core      Optional target prefix filter
//!   history=50          Lines of ring-buffer history to send first (max 500)

use crate::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use hc_types::LogLine;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::net::SocketAddr;
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

    if !is_whitelisted {
        let token = params.token.as_deref().unwrap_or("");
        if token.is_empty() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing token query parameter" })),
            )
                .into_response();
        }
        if state.jwt.validate(token).is_err() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid or expired token" })),
            )
                .into_response();
        }
    }

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

    ws.on_upgrade(move |socket| {
        handle_socket(socket, log_stream, min_level, target_filter, history_count)
    })
}

async fn handle_socket(
    mut socket: WebSocket,
    state: LogStreamState,
    min_level: u8,
    target_filter: Option<String>,
    history_count: usize,
) {
    // Send ring-buffer history first.
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
            if socket.send(Message::Text(json.into())).await.is_err() {
                return;
            }
        }
    }

    // Stream live events.
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
                                if socket.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => debug!(error = %e, "log_stream: serialise error"),
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("log_stream: lagged by {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = socket.recv() => {
                if result.is_none() { break; }  // client disconnected
            }
        }
    }
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
