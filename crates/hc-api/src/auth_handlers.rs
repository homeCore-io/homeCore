//! Handlers for authentication and user management endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_api_types::auth::{
    ChangePasswordRequest, CreateUserRequest, LoginRequest, LoginResponse, RefreshRequest,
    RefreshResponse, SetRoleRequest,
};
use hc_auth::refresh;
use hc_auth::{hash_password, verify_password, User, UserInfo};
use hc_state::RefreshTokenRecord;
use serde_json::json;
use uuid::Uuid;

const REFRESH_MAX_RETRIES: u32 = 3;

use crate::{auth_middleware::AuthUser, AppState};

// ---------- Login ----------

/// `POST /api/v1/auth/login`
/// Returns a signed JWT on success.
pub async fn login(
    State(s): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // Fetch user record.
    let user = match s.store.get_user_by_username(&body.username).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            // Constant-time-ish: still run verify against a dummy hash so timing
            // doesn't leak whether the username exists.
            let _ = tokio::task::spawn_blocking(|| {
                verify_password(
                    "dummy",
                    "$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHRzb21lc2FsdA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                )
            })
            .await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid credentials" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    // Verify password on a blocking thread (Argon2id is CPU-intensive).
    let hash = user.password_hash.clone();
    let password = body.password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false);

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid credentials" })),
        )
            .into_response();
    }

    match s.jwt.issue(&user.id.to_string(), &user.username, user.role) {
        Ok(token) => {
            let expires_in = s.jwt.expiry_hours() * 3600;
            // Mint a refresh token alongside. If minting fails, still return
            // the access token — login shouldn't break just because the
            // refresh store had a transient issue.
            let (refresh_token, refresh_expires_in) = match issue_refresh(&s, user.id, None).await {
                Ok(r) => (Some(r.0), Some(r.1)),
                Err(e) => {
                    tracing::warn!(error = %e, "refresh token issue failed on login");
                    (None, None)
                }
            };
            let body = LoginResponse {
                token,
                token_type: "Bearer".into(),
                expires_in,
                refresh_token,
                refresh_expires_in,
                user: UserInfo::from(&user),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Mint a refresh token and persist it. Returns `(full_token, ttl_seconds)`.
async fn issue_refresh(
    s: &AppState,
    user_id: Uuid,
    parent_id: Option<Uuid>,
) -> anyhow::Result<(String, u64)> {
    // Collision-safe generate.
    let new_tok = {
        let mut attempt = 0u32;
        loop {
            let candidate = refresh::generate()?;
            if !s
                .store
                .refresh_prefix_exists(&candidate.lookup_prefix)
                .await?
            {
                break candidate;
            }
            attempt += 1;
            if attempt >= REFRESH_MAX_RETRIES {
                anyhow::bail!("refresh token prefix collided {attempt} times");
            }
        }
    };

    let ttl_days = s.refresh_token_expiry_days;
    let expires_at = chrono::Utc::now() + chrono::Duration::days(ttl_days as i64);
    let rec = RefreshTokenRecord {
        id: Uuid::new_v4(),
        user_id,
        prefix: new_tok.lookup_prefix,
        hash: new_tok.hash,
        parent_id,
        created_at: chrono::Utc::now(),
        used_at: None,
        expires_at,
        revoked_at: None,
        user_agent: String::new(),
    };
    s.store.create_refresh_token(&rec).await?;
    Ok((new_tok.full_token, ttl_days * 24 * 3600))
}

/// `POST /api/v1/auth/refresh`
///
/// Trades a refresh token for a new access + refresh pair. The presented
/// refresh token is marked used; a new one is issued with `parent_id`
/// pointing at it. Presenting an already-used token triggers a full
/// chain revocation — likely indicates token theft.
pub async fn refresh(
    State(s): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> impl IntoResponse {
    let body_tok = match body.refresh_token.strip_prefix(refresh::REFRESH_TOKEN_PREFIX) {
        Some(b) => b,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "malformed refresh token" })),
            )
                .into_response();
        }
    };
    let prefix = match refresh::lookup_prefix_from_body(body_tok) {
        Some(p) => p,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "malformed refresh token" })),
            )
                .into_response();
        }
    };

    let rec = match s.store.get_refresh_by_prefix(prefix).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid refresh token" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(error = %e, "refresh_by_prefix lookup failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "store unavailable" })),
            )
                .into_response();
        }
    };

    // Reuse detection: token previously used → chain-revoke + reject.
    if rec.used_at.is_some() {
        tracing::warn!(
            user_id = %rec.user_id,
            chain_root = %rec.id,
            "Refresh token reuse detected — revoking entire chain"
        );
        let _ = s.store.revoke_refresh_chain(rec.id).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "refresh token reuse detected — chain revoked" })),
        )
            .into_response();
    }
    if rec.revoked_at.is_some() || rec.expires_at <= chrono::Utc::now() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "refresh token revoked or expired" })),
        )
            .into_response();
    }

    // Verify hash. Blocking-pool to avoid stalling the reactor.
    let stored_hash = rec.hash.clone();
    let full_tok = body.refresh_token.clone();
    let ok = tokio::task::spawn_blocking(move || refresh::verify_token(&full_tok, &stored_hash))
        .await
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid refresh token" })),
        )
            .into_response();
    }

    // Valid. Mark the presented token used, mint a new access + refresh pair.
    if let Err(e) = s.store.mark_refresh_used(rec.id).await {
        tracing::warn!(error = %e, "mark_refresh_used failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "store unavailable" })),
        )
            .into_response();
    }

    let user = match s.store.get_user_by_id(rec.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "user no longer exists" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(error = %e, "get_user_by_id failed during refresh");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "store unavailable" })),
            )
                .into_response();
        }
    };

    let access = match s.jwt.issue(&user.id.to_string(), &user.username, user.role) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let (new_refresh, refresh_ttl) = match issue_refresh(&s, rec.user_id, Some(rec.id)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to issue rotated refresh");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not rotate refresh token" })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(RefreshResponse {
            token: access,
            token_type: "Bearer".into(),
            expires_in: s.jwt.expiry_hours() * 3600,
            refresh_token: new_refresh,
            refresh_expires_in: refresh_ttl,
            user: UserInfo::from(&user),
        }),
    )
        .into_response()
}

// ---------- Me ----------

/// `GET /api/v1/auth/me`
/// Returns the authenticated user's profile.
pub async fn me(State(s): State<AppState>, AuthUser(claims): AuthUser) -> impl IntoResponse {
    match s
        .store
        .get_user_by_id(Uuid::parse_str(&claims.uid).unwrap_or_default())
        .await
    {
        Ok(Some(user)) => (StatusCode::OK, Json(json!(UserInfo::from(&user)))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "user not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------- Change password ----------

/// `POST /api/v1/auth/change-password`
/// Authenticated users can change their own password.
pub async fn change_password(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    if body.new_password.len() < 8 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "password must be at least 8 characters" })),
        )
            .into_response();
    }

    let uid = match Uuid::parse_str(&claims.uid) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid user id in token" })),
            )
                .into_response()
        }
    };

    let mut user = match s.store.get_user_by_id(uid).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "user not found" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    // Verify current password.
    let hash = user.password_hash.clone();
    let current = body.current_password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&current, &hash))
        .await
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "current password is incorrect" })),
        )
            .into_response();
    }

    // Hash new password.
    let new_pass = body.new_password.clone();
    let new_hash = match tokio::task::spawn_blocking(move || hash_password(&new_pass)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    user.password_hash = new_hash;
    match s.store.update_user(&user).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------- User management (admin only) ----------

/// `POST /api/v1/auth/users` — admin only
pub async fn create_user(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<CreateUserRequest>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    if body.password.len() < 8 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "password must be at least 8 characters" })),
        )
            .into_response();
    }

    // Check username uniqueness.
    match s.store.get_user_by_username(&body.username).await {
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "username already exists" })),
            )
                .into_response()
        }
        Ok(None) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }

    let password = body.password.clone();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    let user = User {
        id: Uuid::new_v4(),
        username: body.username,
        password_hash: hash,
        role: body.role,
        created_at: chrono::Utc::now(),
    };
    match s.store.create_user(&user).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(UserInfo::from(&user)))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/v1/auth/users` — admin only
pub async fn list_users(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    match s.store.list_users().await {
        Ok(users) => {
            let infos: Vec<UserInfo> = users.iter().map(UserInfo::from).collect();
            (StatusCode::OK, Json(json!(infos))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `DELETE /api/v1/auth/users/{id}` — admin only
pub async fn delete_user(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    // Prevent self-deletion.
    if claims.uid == id.to_string() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cannot delete your own account" })),
        )
            .into_response();
    }
    match s.store.delete_user(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "user not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `PATCH /api/v1/auth/users/{id}/role` — admin only
pub async fn set_user_role(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetRoleRequest>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    let mut user = match s.store.get_user_by_id(id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "user not found" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    user.role = body.role;
    match s.store.update_user(&user).await {
        Ok(_) => (StatusCode::OK, Json(json!(UserInfo::from(&user)))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
