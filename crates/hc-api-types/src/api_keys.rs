//! API-key request/response types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request body for `POST /api/v1/auth/api-keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyRequest {
    /// Human-readable label used for audit logs + key listings.
    pub label: String,
    /// Scope subset granted to the key. Must be a subset of the owner's scopes.
    pub scopes: Vec<String>,
    /// Optional lifetime in days. `None` = no expiry.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
    /// Optional list of CIDR ranges the key can be used from.
    /// Empty means "no IP restriction".
    #[serde(default)]
    pub allowed_cidrs: Vec<String>,
    /// Target owner UID. Defaults to the calling user; requires
    /// `api_keys:admin` scope if different from self.
    #[serde(default)]
    pub owner_uid: Option<Uuid>,
}

/// Response from `POST /api/v1/auth/api-keys`. The `token` field is the
/// only time the plaintext secret is returned — callers must persist it
/// immediately or discard it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    pub id: Uuid,
    pub label: String,
    pub token: String,
    pub owner_uid: Uuid,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Listing / metadata view of an API key (no secret).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeySummary {
    pub id: Uuid,
    pub label: String,
    /// First 12 characters of the token body (after `hc_sk_`), suitable for
    /// display: `hc_sk_<prefix>...`.
    pub prefix: String,
    pub owner_uid: Uuid,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub allowed_cidrs: Vec<String>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Request body for `PATCH /api/v1/auth/api-keys/{id}`. Any field may be
/// present; omitted fields are left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateApiKeyRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub expires_in_days: Option<u32>,
    #[serde(default)]
    pub allowed_cidrs: Option<Vec<String>>,
}
