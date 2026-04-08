//! Handlers for authentication and user management endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_auth::{hash_password, verify_password, Role, User, UserInfo};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{auth_middleware::AuthUser, AppState};

// ---------- Login ----------

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

/// `POST /api/v1/auth/login`
/// Returns a signed JWT on success.
pub async fn login(State(s): State<AppState>, Json(body): Json<LoginBody>) -> impl IntoResponse {
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
            (
                StatusCode::OK,
                Json(json!({
                    "token": token,
                    "token_type": "Bearer",
                    "expires_in": expires_in,
                    "user": UserInfo::from(&user),
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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

#[derive(Deserialize)]
pub struct ChangePasswordBody {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /api/v1/auth/change-password`
/// Authenticated users can change their own password.
pub async fn change_password(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<ChangePasswordBody>,
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

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username: String,
    pub password: String,
    pub role: Role,
}

/// `POST /api/v1/auth/users` — admin only
pub async fn create_user(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<CreateUserBody>,
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
#[derive(Deserialize)]
pub struct SetRoleBody {
    pub role: Role,
}

pub async fn set_user_role(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetRoleBody>,
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
