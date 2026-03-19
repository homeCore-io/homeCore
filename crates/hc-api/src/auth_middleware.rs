//! Axum middleware that validates Bearer JWT tokens and injects `Claims` into
//! request extensions.  Routes that don't need auth are in the public router
//! and bypass this middleware entirely.
//!
//! ## Scope guards
//!
//! Individual handlers declare their required scope by adding a typed extractor
//! parameter.  Axum calls `from_request_parts` on every extractor before the
//! handler body runs, so a missing or insufficient scope returns HTTP 403
//! without any boilerplate inside the handler.
//!
//! ```rust,ignore
//! pub async fn list_devices(
//!     State(s): State<AppState>,
//!     _: DevicesRead,          // 403 if token lacks "devices:read"
//! ) -> impl IntoResponse { ... }
//! ```
//!
//! Available guards: [`DevicesRead`], [`DevicesWrite`], [`AutomationsRead`],
//! [`AutomationsWrite`], [`ScenesRead`], [`ScenesWrite`], [`AreasRead`],
//! [`AreasWrite`], [`PluginsRead`], [`PluginsWrite`].

use async_trait::async_trait;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use hc_auth::{Claims, Role};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};

use crate::AppState;

/// Middleware that enforces authentication on protected routes.
///
/// **Whitelist bypass:** if the request's source IP matches any entry in
/// `AppState::whitelist`, synthetic Admin `Claims` are injected and the JWT
/// check is skipped entirely.  Whitelisted requests are logged at debug level.
///
/// **JWT path:** for non-whitelisted sources a valid `Bearer` token must be
/// present in the `Authorization` header.
pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    // ── 1. IP whitelist check ─────────────────────────────────────────────
    if !state.whitelist.is_empty() {
        // Canonicalize IPv4-mapped IPv6 (::ffff:x.x.x.x → x.x.x.x) so that
        // clients connecting to a dual-stack 0.0.0.0 listener match IPv4 entries.
        let remote_ip = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| match ci.0.ip() {
                IpAddr::V6(v6) => v6
                    .to_ipv4_mapped()
                    .map(IpAddr::V4)
                    .unwrap_or(IpAddr::V6(v6)),
                v4 => v4,
            });

        if let Some(ip) = remote_ip {
            if state.whitelist.iter().any(|net| net.contains(&ip)) {
                tracing::debug!(%ip, "IP whitelist bypass — granting Admin access");
                let claims = whitelist_claims();
                request.extensions_mut().insert(claims);
                return next.run(request).await;
            }
        }
    }

    // ── 2. JWT validation ─────────────────────────────────────────────────
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

/// Synthetic Admin claims injected for whitelisted source IPs.
/// The `uid` and `sub` identify the request as a whitelist bypass in logs.
fn whitelist_claims() -> Claims {
    Claims {
        sub: "whitelist".into(),
        uid: "whitelist".into(),
        exp: u64::MAX,
        role: Role::Admin,
        scopes: Role::Admin.scopes(),
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

// ---------------------------------------------------------------------------
// Scope guard extractors
// ---------------------------------------------------------------------------

/// Generate a zero-cost scope guard extractor.
///
/// The generated type implements `FromRequestParts`.  When used as a handler
/// parameter it causes Axum to reject the request with HTTP 403 before the
/// handler body runs if the authenticated user's JWT lacks the required scope.
/// The inner `Claims` are exposed as `.0` if the handler needs them.
macro_rules! scope_extractor {
    ($name:ident, $scope:literal) => {
        #[doc = concat!("Scope guard: requires `", $scope, "` in the JWT claims.")]
        pub struct $name(pub Claims);

        #[async_trait]
        impl<S: Send + Sync> axum::extract::FromRequestParts<S> for $name {
            type Rejection = (StatusCode, Json<serde_json::Value>);

            async fn from_request_parts(
                parts: &mut axum::http::request::Parts,
                _state: &S,
            ) -> Result<Self, Self::Rejection> {
                let claims = parts
                    .extensions
                    .get::<Claims>()
                    .cloned()
                    .ok_or_else(|| {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "error": "authentication required" })),
                        )
                    })?;
                if !claims.has_scope($scope) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": concat!("scope '", $scope, "' required") })),
                    ));
                }
                Ok(Self(claims))
            }
        }
    };
}

scope_extractor!(DevicesRead,       "devices:read");
scope_extractor!(DevicesWrite,      "devices:write");
scope_extractor!(AutomationsRead,   "automations:read");
scope_extractor!(AutomationsWrite,  "automations:write");
scope_extractor!(ScenesRead,        "scenes:read");
scope_extractor!(ScenesWrite,       "scenes:write");
scope_extractor!(AreasRead,         "areas:read");
scope_extractor!(AreasWrite,        "areas:write");
scope_extractor!(PluginsRead,       "plugins:read");
scope_extractor!(PluginsWrite,      "plugins:write");

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use hc_auth::JwtService;
    use hc_auth::user::Role;
    use tower::ServiceExt; // for `oneshot`

    /// Minimal router that requires `DevicesRead` — used for all scope tests.
    fn make_router(jwt: JwtService) -> Router {
        let jwt = std::sync::Arc::new(jwt);
        Router::new()
            .route("/guarded", get(guarded_handler))
            .route_layer(axum::middleware::from_fn_with_state(
                jwt.clone(),
                |axum::extract::State(j): axum::extract::State<std::sync::Arc<JwtService>>,
                 mut req: Request<Body>,
                 next: axum::middleware::Next| async move {
                    let auth_header = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok());
                    let token = match auth_header.and_then(|h| h.strip_prefix("Bearer ")) {
                        Some(t) => t.to_string(),
                        None => return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(serde_json::json!({ "error": "missing auth" })),
                        ).into_response(),
                    };
                    match j.validate(&token) {
                        Ok(claims) => {
                            req.extensions_mut().insert(claims);
                            next.run(req).await
                        }
                        Err(_) => (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(serde_json::json!({ "error": "invalid token" })),
                        ).into_response(),
                    }
                },
            ))
            .with_state(jwt)
    }

    async fn guarded_handler(_: DevicesRead) -> impl IntoResponse {
        StatusCode::OK
    }

    fn jwt() -> JwtService {
        JwtService::new_hs256(b"scope-test-secret-32-bytes-minimum", 24)
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    #[tokio::test]
    async fn admin_token_passes_devices_read() {
        let svc = jwt();
        let token = svc.issue("uid", "alice", Role::Admin).unwrap();
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .header("Authorization", bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn user_token_passes_devices_read() {
        let svc = jwt();
        let token = svc.issue("uid", "bob", Role::User).unwrap();
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .header("Authorization", bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readonly_token_passes_devices_read() {
        let svc = jwt();
        let token = svc.issue("uid", "carol", Role::ReadOnly).unwrap();
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .header("Authorization", bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_token_returns_401() {
        let svc = jwt();
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_token_returns_401() {
        let svc = jwt();
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .header("Authorization", "Bearer not-a-jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Token that has no scopes at all — simulates a token with a stripped-down
    /// role that lacks `devices:read`.  We build a Claims struct manually via
    /// `jsonwebtoken` since `JwtService::issue` always uses a known Role.
    #[tokio::test]
    async fn token_missing_scope_returns_403() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let secret = b"scope-test-secret-32-bytes-minimum";
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;

        // Claims with an empty scopes vec.
        let claims = Claims {
            sub: "dave".into(),
            uid: "uid-dave".into(),
            exp,
            role: hc_auth::user::Role::ReadOnly,
            scopes: vec![], // no scopes
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap();

        let svc = JwtService::new_hs256(secret, 24);
        let app = make_router(svc);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/guarded")
                    .header("Authorization", bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn readonly_token_blocked_on_devices_write() {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let secret = b"scope-test-secret-32-bytes-minimum";
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;

        // Only devices:read — no devices:write.
        let claims = Claims {
            sub: "dave".into(),
            uid: "uid-dave".into(),
            exp,
            role: hc_auth::user::Role::ReadOnly,
            scopes: vec!["devices:read".into()],
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap();

        // Router guarded by DevicesWrite.
        let jwt = std::sync::Arc::new(JwtService::new_hs256(secret, 24));
        let app = Router::new()
            .route("/write", get(write_handler))
            .route_layer(axum::middleware::from_fn_with_state(
                jwt.clone(),
                |axum::extract::State(j): axum::extract::State<std::sync::Arc<JwtService>>,
                 mut req: Request<Body>,
                 next: axum::middleware::Next| async move {
                    let token = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|h| h.strip_prefix("Bearer "))
                        .map(|t| t.to_string());
                    match token.and_then(|t| j.validate(&t).ok()) {
                        Some(claims) => {
                            req.extensions_mut().insert(claims);
                            next.run(req).await
                        }
                        None => StatusCode::UNAUTHORIZED.into_response(),
                    }
                },
            ))
            .with_state(jwt);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/write")
                    .header("Authorization", bearer(&token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    async fn write_handler(_: DevicesWrite) -> impl IntoResponse {
        StatusCode::OK
    }
}
