//! Auth + user-management request/response types.

use chrono::{DateTime, Utc};
use hc_auth::{Role, UserInfo};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Login ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Access token (short-lived JWT) — include as `Authorization: Bearer ...`
    /// on every subsequent request.
    pub token: String,
    pub token_type: String, // "Bearer"
    /// Seconds until the access token expires.
    pub expires_in: u64,
    /// Long-lived refresh token. Exchanged for a new access token via
    /// `POST /auth/refresh`. Single-use rotating — each successful
    /// refresh returns a new refresh token and invalidates the old one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Seconds until the refresh token expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_in: Option<u64>,
    pub user: UserInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// Response from `POST /auth/refresh`. Mirrors `LoginResponse` — callers
/// replace both stored tokens with these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub refresh_token: String,
    pub refresh_expires_in: u64,
    pub user: UserInfo,
}

// ── Me / change password ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

// ── User CRUD ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoleRequest {
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSummary {
    pub id: Uuid,
    pub username: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

impl From<UserInfo> for UserSummary {
    fn from(u: UserInfo) -> Self {
        Self {
            id: u.id,
            username: u.username,
            role: u.role,
            created_at: u.created_at,
        }
    }
}
