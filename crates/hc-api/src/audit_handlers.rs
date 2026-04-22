//! Handlers for the audit query endpoint.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_state::{AuditActorType, AuditEntry, AuditQuery, AuditResult};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{auth_middleware::AuthUser, AppState};

/// Query string for `GET /api/v1/audit`.
#[derive(Deserialize)]
pub struct AuditQueryParams {
    pub actor_id: Option<Uuid>,
    pub actor_type: Option<String>,
    pub event_type: Option<String>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub result: Option<String>,
    /// RFC3339 timestamp lower bound (inclusive).
    pub from: Option<String>,
    /// RFC3339 timestamp upper bound (inclusive).
    pub to: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    100
}

/// `GET /api/v1/audit` — Admin-only.
pub async fn list_audit(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Query(params): Query<AuditQueryParams>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }

    let actor_type = match params.actor_type.as_deref() {
        None => None,
        Some(s) => match parse_actor_type(s) {
            Some(a) => Some(a),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown actor_type `{s}`") })),
                )
                    .into_response();
            }
        },
    };
    let result = match params.result.as_deref() {
        None => None,
        Some(s) => match parse_result(s) {
            Some(r) => Some(r),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown result `{s}`") })),
                )
                    .into_response();
            }
        },
    };
    let from = match params.from.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(d) => Some(d.with_timezone(&chrono::Utc)),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid `from`: {e}") })),
                )
                    .into_response();
            }
        },
    };
    let to = match params.to.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(d) => Some(d.with_timezone(&chrono::Utc)),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid `to`: {e}") })),
                )
                    .into_response();
            }
        },
    };

    let q = AuditQuery {
        actor_id: params.actor_id,
        actor_type,
        event_type: params.event_type,
        target_kind: params.target_kind,
        target_id: params.target_id,
        result,
        from,
        to,
        limit: params.limit,
        offset: params.offset,
    };
    match s.store.query_audit(&q).await {
        Ok(rows) => (StatusCode::OK, Json::<Vec<AuditEntry>>(rows)).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "audit query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "audit store unavailable" })),
            )
                .into_response()
        }
    }
}

fn parse_actor_type(s: &str) -> Option<AuditActorType> {
    Some(match s {
        "user" => AuditActorType::User,
        "api_key" => AuditActorType::ApiKey,
        "local_admin" => AuditActorType::LocalAdmin,
        "ip_whitelist" => AuditActorType::IpWhitelist,
        "system" => AuditActorType::System,
        "anonymous" => AuditActorType::Anonymous,
        _ => return None,
    })
}

fn parse_result(s: &str) -> Option<AuditResult> {
    Some(match s {
        "success" => AuditResult::Success,
        "denied" => AuditResult::Denied,
        "error" => AuditResult::Error,
        _ => return None,
    })
}
