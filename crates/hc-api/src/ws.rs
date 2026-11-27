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
//! ## What a caller is sent
//! Only the events they may read through each event's own resource, as
//! [`crate::event_access`] decides from the claims the stream was opened with:
//! device events with `devices:read`, rule and webhook events with
//! `automations:read`, and so on. The snapshot is cut the same way.
//!
//! The credential is checked again on every ping: a stream whose token has
//! expired, whose session has ended or whose key has been revoked is closed
//! (code 1008, reason `token_expired` / `token_revoked`), and the client
//! reconnects with a current one.
//!
//! ## Filtering (optional)
//! - `type`      — comma-separated event type names, e.g. `device_state_changed,rule_fired`.
//!   All events on the public bus are forwarded by default; `MqttMessage` events
//!   never reach the public bus and are not available here.
//! - `device_id` — only forward events for this device
//!
//! ## Snapshot (optional)
//! - `snapshot=true` — the first frame is the house as it stands:
//!   `{"type":"snapshot","timestamp":…,"devices":[…],"rooms":[…],"scenes":[…],"alerts":[…]}`,
//!   each device with its `schema` inline as `GET /devices?include_schema=true`
//!   gives it. Each section is present only if the caller may read it.
//!
//!   The stream is subscribed *before* the snapshot is read, so nothing that
//!   happens in between is lost: it arrives as an event after the snapshot,
//!   possibly repeating what the snapshot already shows. Applying events in
//!   order over the snapshot ends at the current state either way. With the
//!   device, room and scene events, a client that holds the snapshot never
//!   needs to reload to stay right.
//!
//!   Opt-in, because a client written before it would not know the frame.
//!
//! ## Falling behind
//! A client too slow to keep up (more than the bus holds) misses events. It
//! is then sent a fresh snapshot, with `"missed": n`, if it asked for
//! snapshots — the house as it is now — or `{"type":"lagged","missed":n}`
//! otherwise, so it knows to read again.

use crate::auth_middleware::whitelist_claims;
use crate::event_access::{self, View};
use crate::event_log::{event_device_id, event_type_name};
use crate::AppState;
use axum::{
    extract::{
        ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hc_auth::{Actor, Claims};
use hc_types::event::Event;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

// ── Active WS connection registry (OPS-1 piece 3) ───────────────────────────
//
// Tracks every live WebSocket connection (events_stream + logs_stream) so the
// `GET /api/v1/ws/connections` admin endpoint can answer "is this reconnect
// storm one looping client or many churning clients?" — the question that
// took ~30 min to answer manually during the 0.1.2 deploy debugging.
//
// Std-Mutex (not tokio::sync::Mutex) so the Drop-guard cleanup can run from
// sync context. The map is locked exactly once on connect (insert), once on
// disconnect (remove), and once per `/ws/connections` request — write
// contention is minimal.

/// One entry in the live-connection registry. JSON-serialisable for the
/// admin endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct WsConnection {
    /// Server-generated UUID; stable for the lifetime of this connection.
    pub connection_id: String,
    /// `events_stream` or `logs_stream` — distinguishes the two endpoints.
    pub endpoint: &'static str,
    /// Per-tab fingerprint from `?client_id=` (Leptos sets this; CLI
    /// callers that don't pass one show as `None`).
    pub client_id: Option<String>,
    /// Source IP (canonicalised, IPv4-mapped IPv6 stripped).
    pub ip: String,
    /// Authenticated user (claims.sub, or `whitelist` for IP-whitelist
    /// callers).
    pub user: String,
    pub user_agent: Option<String>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
}

/// Shared map of `connection_id` → `WsConnection`. Held in `AppState`.
pub type WsConnections = Arc<Mutex<HashMap<String, WsConnection>>>;

/// Build the empty registry used by `AppState::new_with_plugins_and_raw_bus`.
pub fn new_ws_connections() -> WsConnections {
    Arc::new(Mutex::new(HashMap::new()))
}

/// RAII guard that deregisters a connection from the registry when dropped.
/// Holding this guard across the WS event-loop means the entry stays live
/// until the loop returns, regardless of which break path took us out
/// (clean Close, ping timeout, send failure, etc.). See OPS-1 piece 1 for
/// the seven categorised reasons.
pub struct WsConnectionGuard {
    connections: WsConnections,
    id: String,
}

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.connections.lock() {
            map.remove(&self.id);
        }
    }
}

/// Register a fresh connection in the registry and return the guard. The
/// caller holds the guard for the lifetime of the connection; dropping it
/// cleans up.
#[allow(clippy::too_many_arguments)] // builder-style alternative would obscure the call site
pub fn register_connection(
    connections: &WsConnections,
    endpoint: &'static str,
    client_id: Option<String>,
    ip: IpAddr,
    user: String,
    user_agent: Option<String>,
) -> WsConnectionGuard {
    let id = uuid::Uuid::new_v4().to_string();
    let entry = WsConnection {
        connection_id: id.clone(),
        endpoint,
        client_id,
        ip: ip.to_string(),
        user,
        user_agent,
        connected_at: chrono::Utc::now(),
    };
    if let Ok(mut map) = connections.lock() {
        map.insert(id.clone(), entry);
    }
    WsConnectionGuard {
        connections: Arc::clone(connections),
        id,
    }
}

/// Snapshot of every live connection, sorted newest-first by
/// `connected_at`. Used by the `/api/v1/ws/connections` handler.
pub fn snapshot_connections(connections: &WsConnections) -> Vec<WsConnection> {
    let Ok(map) = connections.lock() else {
        return Vec::new();
    };
    let mut out: Vec<WsConnection> = map.values().cloned().collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.connected_at));
    out
}

/// Server-initiated ping cadence. Closed clients are detected within ~one
/// interval (next tick after a missed pong) instead of waiting for the next
/// event broadcast. NAT/proxy idle timeouts are kept warm by the same path.
const PING_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Send the house as it stands as the first frame. See the module docs.
    #[serde(default)]
    pub snapshot: bool,
}

pub async fn ws_events_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<EventStreamQuery>,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // Validate before accepting the upgrade.
    let claims = match authenticate_ws(&query, &state, addr.ip()).await {
        Ok(c) => c,
        Err(resp) => return *resp,
    };

    // This handler validates the JWT itself rather than going through
    // `require_auth`, so the token-version check has to be repeated here or a
    // session invalidated by a password change could still open a stream.
    if let Err(resp) = crate::auth_middleware::token_version_current(&state, &claims).await {
        return *resp;
    }

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    ws.on_upgrade(move |socket| handle_socket(socket, state, query, claims, addr.ip(), user_agent))
}

/// Authenticate a WebSocket upgrade request.
///
/// A `?token=` is validated on the same terms as a header credential — an
/// API key against the api_keys store, anything else as a JWT. Only a request
/// with no token at all falls back to the IP whitelist.
///
/// **A token wins over the whitelist, as it does for REST.** This used to
/// check the whitelist first, so anybody connecting through a whitelisted
/// address — a UI server passing the stream through for its signed-in people
/// is exactly that — was streamed as admin whoever they were, while the same
/// person's REST calls were judged as themselves.
pub(crate) async fn authenticate_ws(
    query: &EventStreamQuery,
    state: &AppState,
    remote_ip: IpAddr,
) -> Result<Claims, Box<Response>> {
    // Canonicalize IPv4-mapped IPv6 to match whitelist entries.
    let ip = match remote_ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };

    let token = query.token.as_deref().unwrap_or("");
    if token.is_empty()
        && !state.whitelist.is_empty()
        && state.whitelist.iter().any(|net| net.contains(&ip))
    {
        tracing::debug!(%ip, "IP whitelist bypass — granting Admin access (WS)");
        return Ok(whitelist_claims());
    }

    crate::auth_middleware::validate_query_token(state, token, ip).await
}

/// The house as it stands, for a client that asked for it on connect.
///
/// Built from the same reads the REST endpoints make, so a snapshot and a
/// `GET` never disagree about what a device or a room looks like. A section
/// the caller may not read is left out rather than sent empty, so an empty
/// list always means "there are none".
pub(crate) async fn snapshot(
    state: &AppState,
    claims: &Claims,
) -> anyhow::Result<serde_json::Value> {
    let mut out = serde_json::Map::new();
    out.insert("type".into(), json!("snapshot"));
    out.insert("timestamp".into(), json!(chrono::Utc::now()));

    if claims.has_scope("devices:read") {
        let schemas: HashMap<String, hc_types::DeviceSchema> = state
            .store
            .list_device_schemas()
            .await?
            .into_iter()
            .collect();
        let devices: Vec<serde_json::Value> = state
            .store
            .list_devices()
            .await?
            .into_iter()
            .map(crate::handlers::normalize_native_device_type)
            .map(|device| {
                let schema = schemas.get(&device.device_id).cloned().map(|mut schema| {
                    schema.fill_primary(device.device_type.as_deref());
                    schema
                });
                let mut entry = serde_json::to_value(&device).unwrap_or(json!({}));
                entry["schema"] = serde_json::to_value(&schema).unwrap_or(serde_json::Value::Null);
                entry
            })
            .collect();
        out.insert("devices".into(), json!(devices));
    }
    if claims.has_scope("areas:read") {
        out.insert(
            "rooms".into(),
            json!(hc_core::rooms::list(&state.store).await?),
        );
    }
    if claims.has_scope("scenes:read") {
        out.insert("scenes".into(), json!(state.store.list_scenes().await?));
    }
    if claims.has_scope("devices:read") {
        if let Some(alerts) = &state.alerts {
            out.insert("alerts".into(), json!(alerts.list()));
        }
    }
    Ok(serde_json::Value::Object(out))
}

/// A device inside an event, shaped as `GET /devices` shapes it.
fn normalize_event(event: Event) -> Event {
    match event {
        Event::DeviceAdded { timestamp, device } => Event::DeviceAdded {
            timestamp,
            device: Box::new(crate::handlers::normalize_native_device_type(*device)),
        },
        Event::DeviceMetadataChanged { timestamp, device } => Event::DeviceMetadataChanged {
            timestamp,
            device: Box::new(crate::handlers::normalize_native_device_type(*device)),
        },
        other => other,
    }
}

/// The text frame `event` becomes for this caller, or `None` when it is not
/// sent to them: because they may not see it (see [`crate::event_access`]),
/// or because their filters leave it out.
///
/// Who may see an event is decided before the filters, so naming a type in
/// `?type=` never gets round it.
pub(crate) fn event_frame(
    claims: &Claims,
    event: Event,
    type_filter: Option<&[String]>,
    device_filter: Option<&str>,
) -> Option<String> {
    let view = event_access::view(claims, event_access::audience(&event));
    if view == View::Nothing {
        return None;
    }

    // Skip noisy heartbeats unless the client explicitly asked for them —
    // and what plugins report as alerts, which a client sees as
    // `alert_raised` / `alert_cleared` once the house has decided.
    if matches!(
        event,
        Event::PluginHeartbeat { .. } | Event::PluginAlerts { .. }
    ) && type_filter.is_none()
    {
        return None;
    }

    if let Some(wanted_device) = device_filter {
        if event_device_id(&event) != Some(wanted_device) {
            return None;
        }
    }

    if let Some(types) = type_filter {
        if !types.iter().any(|t| t == event_type_name(&event)) {
            return None;
        }
    }

    let json = match view {
        View::PluginUi => {
            serde_json::to_value(&event).map(|v| event_access::plugin_ui_only(&v).to_string())
        }
        View::CommandMasked => serde_json::to_value(normalize_event(event))
            .map(|v| event_access::command_masked(&v).to_string()),
        _ => serde_json::to_string(&normalize_event(event)),
    };
    match json {
        Ok(j) => Some(j),
        Err(e) => {
            warn!(error = %e, "Failed to serialize event");
            None
        }
    }
}

/// Whether the credential a stream was opened with still holds, checked on
/// every ping (so within [`PING_INTERVAL`]). A stream used to outlive its
/// token: a session signed out, a password changed, a key revoked or a token
/// expired, and the socket went on carrying the house. `Err` is the reason it
/// is closed, logged like the others.
pub(crate) async fn credential_holds(
    state: &AppState,
    claims: &Claims,
) -> Result<(), &'static str> {
    let now = chrono::Utc::now();
    // `u64::MAX` for the synthetic claims (keys, whitelist, UDS); keys carry
    // their own expiry on the record, checked below.
    if claims.exp != u64::MAX && claims.exp <= now.timestamp().max(0) as u64 {
        return Err("token_expired");
    }
    match claims.actor() {
        Actor::User { .. } => crate::auth_middleware::token_version_current(state, claims)
            .await
            .map_err(|_| "token_revoked"),
        Actor::ApiKey { id, .. } => match state.store.get_api_key_by_id(id).await {
            Ok(Some(record)) if record.is_usable(now) => Ok(()),
            Ok(_) => Err("token_revoked"),
            // Fails closed, as `token_version_current` does: the client
            // reconnects, and is let back in if the key is good.
            Err(e) => {
                warn!(error = %e, "Could not re-check an event stream's API key");
                Err("token_revoked")
            }
        },
        _ => Ok(()),
    }
}

/// Inner validation logic, separated so it can be unit-tested without a full `AppState`.
pub(crate) fn validate_ws_token(
    token: Option<&str>,
    jwt: &hc_auth::JwtService,
) -> Result<Claims, Box<Response>> {
    let token = token.unwrap_or("");
    if token.is_empty() {
        return Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing token query parameter" })),
            )
                .into_response(),
        ));
    }
    jwt.validate(token).map_err(|_| {
        Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid or expired token" })),
            )
                .into_response(),
        )
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

    // Register in the active-connection map for the lifetime of this
    // function. Guard auto-removes via Drop when handle_socket returns,
    // regardless of which break path we take. OPS-1 piece 3.
    let _registry_guard = register_connection(
        &state.ws_connections,
        "events_stream",
        client_id.clone(),
        remote_ip,
        claims.sub.clone(),
        user_agent.clone(),
    );
    // Bump connect counter (OPS-1 piece 4). Disconnect counter is
    // incremented after the loop with the categorised `reason`.
    state
        .metrics
        .ws_connects_total
        .with_label_values(&["events_stream"])
        .inc();

    // After subscribing, so nothing that happens while it is read is lost.
    let opening: Option<&'static str> = if query.snapshot {
        match snapshot(&state, &claims).await {
            Ok(frame) => socket
                .send(Message::Text(frame.to_string().into()))
                .await
                .err()
                .map(|_| "event_send_failed"),
            Err(e) => {
                warn!(error = %e, "Could not read the snapshot for an event stream");
                Some("snapshot_failed")
            }
        }
    } else {
        None
    };

    // Ping/pong liveness. The interval's first tick fires immediately —
    // consume it before the loop so the first ping goes out one full
    // PING_INTERVAL after connect, not at t=0.
    let mut ping_ticker = interval(PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping_ticker.tick().await;
    let mut outstanding_ping = false;

    // Disconnect-reason categories (OPS-1 piece 1). Carried out of the
    // event loop and stamped onto the disconnect log so a reconnect
    // storm is diagnosable from the log alone — distinguishing a clean
    // client-initiated close from a network drop, a stalled pong, or
    // a server-side bus closure. Stable strings; clients/dashboards
    // can match on them.
    //
    //   client_close      — Close frame received from the client
    //   socket_closed     — recv returned None (FIN without Close frame)
    //   recv_error        — recv returned Err (transport-level)
    //   pong_timeout      — server ping sent, no pong by next tick
    //   ping_send_failed  — couldn't send the periodic ping (write err)
    //   event_send_failed — couldn't push an event payload (write err)
    //   bus_closed        — broadcast channel closed (server shutdown)
    //   snapshot_failed   — the requested snapshot could not be read
    //   token_expired     — the token it was opened with has expired
    //   token_revoked     — that session ended, its password changed, its
    //                       user or key is gone or revoked
    let reason: &'static str = 'stream: {
        if let Some(reason) = opening {
            break 'stream reason;
        }
        loop {
            tokio::select! {
                // Drain client->server frames first so a busy event bus can't
                // starve a pending Close.
                biased;

                recv = socket.recv() => {
                    match recv {
                        None => break "socket_closed",
                        Some(Err(_)) => break "recv_error",
                        Some(Ok(Message::Close(_))) => break "client_close",
                        Some(Ok(Message::Pong(_))) => {
                            outstanding_ping = false;
                        }
                        // Text/Binary/Ping from the client are ignored — this
                        // endpoint is a one-way push. axum auto-pongs incoming Ping.
                        Some(Ok(_)) => {}
                    }
                }

                _ = ping_ticker.tick() => {
                    if let Err(reason) = credential_holds(&state, &claims).await {
                        let _ = socket
                            .send(Message::Close(Some(CloseFrame {
                                code: close_code::POLICY,
                                reason: reason.into(),
                            })))
                            .await;
                        break reason;
                    }
                    if outstanding_ping {
                        // Previous ping never got a pong — assume the socket is dead.
                        break "pong_timeout";
                    }
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        break "ping_send_failed";
                    }
                    outstanding_ping = true;
                }

                event = rx.recv() => {
                    match event {
                        Ok(event) => {
                            let Some(json) = event_frame(
                                &claims,
                                event,
                                type_filter.as_deref(),
                                device_filter.as_deref(),
                            ) else {
                                continue;
                            };
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break "event_send_failed";
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("WS event stream lagged by {n} events");
                            // The client missed `n` events, so what it shows is
                            // stale. One that asked for a snapshot gets a fresh
                            // one — the house as it is now, which it already
                            // knows how to take; any other is told, so it can
                            // read again.
                            let frame = if query.snapshot {
                                snapshot(&state, &claims).await.ok().map(|mut f| {
                                    f["missed"] = json!(n);
                                    f
                                })
                            } else {
                                None
                            }
                            .unwrap_or_else(|| json!({ "type": "lagged", "missed": n }));
                            if socket
                                .send(Message::Text(frame.to_string().into()))
                                .await
                                .is_err()
                            {
                                break "event_send_failed";
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break "bus_closed",
                    }
                }
            }
        }
    };

    state
        .metrics
        .ws_disconnects_total
        .with_label_values(&["events_stream", reason])
        .inc();

    info!(
        ip = %remote_ip,
        user = %claims.sub,
        client_id = client_id.as_deref().unwrap_or("-"),
        user_agent = user_agent.as_deref().unwrap_or("-"),
        reason = reason,
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
        svc.issue("uid-1", "alice", Role::User, 0).unwrap()
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
            actor: None,
            tv: 0,
            sid: None,
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

    /// What a narrow key's stream carries: device events, and none of the
    /// rule or webhook events beside them, whatever it filters for.
    #[test]
    fn a_device_reading_key_streams_device_events_only() {
        use crate::event_access::tests::{device_event, key, rule_removed, webhook_event};
        let k = key(&["devices:read"]);
        assert!(event_frame(&k, device_event(), None, None).is_some());
        assert!(event_frame(&k, webhook_event(), None, None).is_none());
        assert!(event_frame(&k, rule_removed(), None, None).is_none());

        let asked = ["custom".to_string(), "rule_removed".to_string()];
        assert!(event_frame(&k, webhook_event(), Some(&asked), None).is_none());
        assert!(event_frame(&k, rule_removed(), Some(&asked), None).is_none());
    }

    #[test]
    fn an_admin_streams_everything() {
        use crate::event_access::tests::{device_event, rule_removed, webhook_event};
        for claims in [
            crate::event_access::tests::user(Role::Admin),
            whitelist_claims(),
        ] {
            for e in [device_event(), webhook_event(), rule_removed()] {
                assert!(event_frame(&claims, e, None, None).is_some());
            }
        }
        let frame = event_frame(&whitelist_claims(), webhook_event(), None, None).unwrap();
        assert!(frame.contains("s3cret-path"));
    }

    #[test]
    fn filters_still_apply_to_what_may_be_seen() {
        use crate::event_access::tests::{device_event, key};
        let k = key(&["devices:read"]);
        assert!(event_frame(&k, device_event(), None, Some("light.kitchen")).is_some());
        assert!(event_frame(&k, device_event(), None, Some("light.hall")).is_none());
        let other = ["rule_fired".to_string()];
        assert!(event_frame(&k, device_event(), Some(&other), None).is_none());
    }

    async fn a_state() -> AppState {
        let base = std::env::temp_dir().join(format!("hc_api_ws_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&base);
        let store = hc_state::StateStore::open(
            &base.join("state.redb").to_string_lossy(),
            &base.join("history.sqlite").to_string_lossy(),
        )
        .await
        .expect("state store opens");
        AppState::new(crate::AppStateParams::new(
            store,
            hc_core::EventBus::new(16),
            svc(),
        ))
    }

    /// An open stream is closed once its credential no longer holds.
    #[tokio::test]
    async fn a_stream_ends_with_its_credential() {
        let state = a_state().await;

        // A revoked key.
        let mut k = crate::event_access::tests::key(&["devices:read"]);
        let Some(Actor::ApiKey { id, .. }) = k.actor.clone() else {
            unreachable!()
        };
        let record = hc_state::api_key_store::ApiKeyRecord {
            id,
            prefix: "abcd1234".into(),
            hash: "unused".into(),
            owner_uid: uuid::Uuid::new_v4(),
            scopes: k.scopes.clone(),
            label: "test".into(),
            created_at: chrono::Utc::now(),
            last_used_at: None,
            expires_at: None,
            allowed_cidrs: vec![],
            revoked_at: None,
            display: false,
            plugin: None,
        };
        state.store.create_api_key(&record).await.unwrap();
        assert_eq!(credential_holds(&state, &k).await, Ok(()));
        state.store.revoke_api_key(id).await.unwrap();
        assert_eq!(credential_holds(&state, &k).await, Err("token_revoked"));

        // A key that was never there (deleted).
        k.actor = Some(Actor::ApiKey {
            id: uuid::Uuid::new_v4(),
            owner_uid: uuid::Uuid::new_v4(),
            label: "gone".into(),
        });
        assert_eq!(credential_holds(&state, &k).await, Err("token_revoked"));

        // An expired token.
        let mut expired = crate::event_access::tests::user(Role::User);
        expired.exp = 1;
        assert_eq!(
            credential_holds(&state, &expired).await,
            Err("token_expired")
        );

        // A user who is not there any more.
        let gone = crate::event_access::tests::user(Role::User);
        assert_eq!(credential_holds(&state, &gone).await, Err("token_revoked"));

        // The whitelist's synthetic admin has nothing to expire.
        assert_eq!(credential_holds(&state, &whitelist_claims()).await, Ok(()));
    }
}
