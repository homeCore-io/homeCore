//! Axum middleware that validates Bearer JWT tokens and injects `Claims` into
//! request extensions.  Routes that don't need auth are in the public router
//! and bypass this middleware entirely.

use async_trait::async_trait;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use hc_auth::Claims;
use serde_json::json;

use crate::AppState;

/// Extract and validate a Bearer token from the `Authorization` header.
/// Uses `State<AppState>` to get the `JwtService`.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match auth_header.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t.to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing or malformed Authorization header" })),
            )
                .into_response();
        }
    };

    match state.jwt.validate(&token) {
        Ok(claims) => {
            request.extensions_mut().insert(claims);
            next.run(request).await
        }
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or expired token" })),
        )
            .into_response(),
    }
}

/// Extractor that pulls `Claims` from request extensions (inserted by `require_auth`).
/// Handlers add `AuthUser(claims): AuthUser` to receive the authenticated user.
pub struct AuthUser(pub Claims);

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.0.is_admin()
    }
}

#[async_trait]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AuthUser {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Claims>()
            .cloned()
            .map(AuthUser)
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "authentication required" })),
                )
            })
    }
}
