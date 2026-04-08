//! WebSocket handler for `GET /api/v1/events/stream`.
//!
//! Upgrades the connection and forwards events from the `EventBus` broadcast
//! channel as JSON-encoded WebSocket text frames.
//!
//! ## Authentication
//!
//! Browsers cannot set custom headers during a WebSocket upgrade, so the
//! standard `Authorization: Bearer` approach used by REST endpoints does not
//! work here.  Instead, clients pass the JWT as a query parameter:
//!
//!   `GET /api/v1/events/stream?token=<jwt>`
//!
//! The token is validated **before** the upgrade handshake is accepted.  If
//! it is missing or invalid the server returns HTTP 401 and the connection is
//! never upgraded.
//!
//! ## Filtering (optional)
//! - `type`      — comma-separated event type names, e.g. `device_state_changed,rule_fired`.
//!                 All events on the public bus are forwarded by default; `MqttMessage` events
//!                 never reach the public bus and are not available here.
//! - `device_id` — only forward events for this device

use crate::auth_middleware::whitelist_claims;
use crate::event_log::{event_device_id, event_type_name};
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
use hc_auth::Claims;
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use tracing::{debug, info, warn};

#[derive(Deserialize, Default)]
pub struct EventStreamQuery {
    /// JWT token (required — browsers cannot send Authorization headers for WS).
    pub token: Option<String>,
    /// Comma-separated event type names (snake_case). Empty = all events.
    #[serde(rename = "type", default)]
    pub event_types: Option<String>,
    /// If set, only events with a matching device_id are forwarded.
    pub device_id: Option<String>,
    /// Optional client fingerprint to correlate reconnect storms.
    pub client_id: Option<String>,
}

pub async fn ws_events_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<EventStreamQuery>,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // Validate before accepting the upgrade.
    let claims = match authenticate_ws(&query, &state, addr.ip()) {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    ws.on_upgrade(move |socket| handle_socket(socket, state, query, claims, addr.ip(), user_agent))
}

/// Authenticate a WebSocket upgrade request.
///
/// Checks the IP whitelist first (same logic as `require_auth` middleware).
/// If the source IP is whitelisted the `?token=` parameter is not required.
/// Otherwise falls back to JWT validation via `?token=`.
fn authenticate_ws(
    query: &EventStreamQuery,
    state: &AppState,
    remote_ip: IpAddr,
) -> Result<Claims, Response> {
    // Canonicalize IPv4-mapped IPv6 to match whitelist entries.
    let ip = match remote_ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };

    if !state.whitelist.is_empty() && state.whitelist.iter().any(|net| net.contains(&ip)) {
        tracing::debug!(%ip, "IP whitelist bypass — granting Admin access (WS)");
        return Ok(whitelist_claims());
    }

    validate_ws_token(query.token.as_deref(), &state.jwt)
}

/// Inner validation logic, separated so it can be unit-tested without a full `AppState`.
fn validate_ws_token(token: Option<&str>, jwt: &hc_auth::JwtService) -> Result<Claims, Response> {
    let token = token.unwrap_or("");
    if token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing token query parameter" })),
        )
            .into_response());
    }
    jwt.validate(token).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or expired token" })),
        )
            .into_response()
    })
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    query: EventStreamQuery,
    claims: Claims,
    remote_ip: IpAddr,
    user_agent: Option<String>,
) {
    let mut rx = state.event_bus.subscribe();

    // Pre-parse type filter once.
    let type_filter: Option<Vec<String>> = query.event_types.as_deref().map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    });
    let device_filter = query.device_id.clone();
    let client_id = query.client_id.clone();

    debug!(
        ip = %remote_ip,
        user = %claims.sub,
        client_id = client_id.as_deref().unwrap_or("-"),
        user_agent = user_agent.as_deref().unwrap_or("-"),
        types = ?type_filter,
        device = ?device_filter,
        "WebSocket client connected to event stream"
    );

    loop {
        match rx.recv().await {
            Ok(event) => {
                // Apply device_id filter.
                if let Some(ref wanted_device) = device_filter {
                    if !event_device_id(&event)
                        .map(|d| d == wanted_device)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }

                // Apply event type filter.
                if let Some(ref types) = type_filter {
                    if !types.iter().any(|t| t == event_type_name(&event)) {
                        continue;
                    }
                }

                let json = match serde_json::to_string(&event) {
                    Ok(j) => j,
                    Err(e) => {
                        warn!(error = %e, "Failed to serialize event");
                        continue;
                    }
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    debug!(
                        ip = %remote_ip,
                        user = %claims.sub,
                        client_id = client_id.as_deref().unwrap_or("-"),
                        user_agent = user_agent.as_deref().unwrap_or("-"),
                        "WebSocket client disconnected"
                    );
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("WS event stream lagged by {n} events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    info!(
        ip = %remote_ip,
        user = %claims.sub,
        client_id = client_id.as_deref().unwrap_or("-"),
        user_agent = user_agent.as_deref().unwrap_or("-"),
        "WebSocket client disconnected"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_auth::{user::Role, Claims, JwtService};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

    fn svc() -> JwtService {
        JwtService::new_hs256(b"test-secret-key-32-bytes-minimum!", 24)
    }

    fn valid_token(svc: &JwtService) -> String {
        svc.issue("uid-1", "alice", Role::User).unwrap()
    }

    #[test]
    fn missing_token_returns_err() {
        let svc = svc();
        let result = validate_ws_token(None, &svc);
        assert!(result.is_err());
    }

    #[test]
    fn empty_token_returns_err() {
        let svc = svc();
        let result = validate_ws_token(Some(""), &svc);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_token_returns_err() {
        let svc = svc();
        let result = validate_ws_token(Some("not.a.jwt"), &svc);
        assert!(result.is_err());
    }

    #[test]
    fn tampered_token_returns_err() {
        let svc = svc();
        let token = valid_token(&svc);
        let tampered = format!("{token}x");
        let result = validate_ws_token(Some(&tampered), &svc);
        assert!(result.is_err());
    }

    #[test]
    fn expired_token_returns_err() {
        let svc = svc();
        let claims = Claims {
            sub: "alice".into(),
            uid: "uid-1".into(),
            exp: 1, // way in the past
            role: Role::User,
            scopes: Role::User.scopes(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-key-32-bytes-minimum!"),
        )
        .unwrap();
        let result = validate_ws_token(Some(&token), &svc);
        assert!(result.is_err());
    }

    #[test]
    fn valid_token_returns_claims() {
        let svc = svc();
        let token = valid_token(&svc);
        let claims = validate_ws_token(Some(&token), &svc).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.uid, "uid-1");
        assert_eq!(claims.role, Role::User);
    }

    #[test]
    fn wrong_secret_returns_err() {
        let issuer = svc();
        let token = valid_token(&issuer);
        let validator = JwtService::new_hs256(b"completely-different-secret-here!", 24);
        let result = validate_ws_token(Some(&token), &validator);
        assert!(result.is_err());
    }
}
