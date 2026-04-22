//! Handlers for API key management endpoints.
//!
//! Two-tier permission model:
//! - **Self-scoped** (any authenticated user): create/list/revoke/rotate
//!   keys whose `owner_uid` equals the caller's uid. Requested scopes must
//!   be a subset of the caller's own scopes.
//! - **Admin-scoped** (`api_keys:admin` scope, Admin role): manage any key
//!   system-wide, set `owner_uid` to any user.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{Duration, Utc};
use hc_api_types::api_keys::{
    ApiKeySummary, CreateApiKeyRequest, CreateApiKeyResponse, UpdateApiKeyRequest,
};
use hc_auth::api_key;
use hc_state::ApiKeyRecord;
use serde_json::json;
use std::collections::HashSet;
use uuid::Uuid;

use crate::{audit, auth_middleware::AuthUser, AppState};

const MAX_PREFIX_COLLISION_RETRIES: u32 = 3;

fn record_to_summary(r: &ApiKeyRecord) -> ApiKeySummary {
    ApiKeySummary {
        id: r.id,
        label: r.label.clone(),
        prefix: r.prefix.clone(),
        owner_uid: r.owner_uid,
        scopes: r.scopes.clone(),
        created_at: r.created_at,
        last_used_at: r.last_used_at,
        expires_at: r.expires_at,
        allowed_cidrs: r.allowed_cidrs.clone(),
        revoked_at: r.revoked_at,
    }
}

fn err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// `POST /api/v1/auth/api-keys`
pub async fn create_api_key(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    if body.label.trim().is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "label is required");
    }

    // The caller's identity comes from the Actor enum — not every authenticated
    // principal has a parseable uid (LocalAdmin on the UDS doesn't).
    let actor = claims.actor();
    let caller_uid = actor.effective_uid();

    // Resolve owner:
    // - User / ApiKey: default to self, require api_keys:admin if different.
    // - LocalAdmin: owner_uid MUST be supplied explicitly; admin bypass
    //   implies api_keys:admin scope.
    let owner_uid = match (body.owner_uid, caller_uid, actor.is_local_admin()) {
        (Some(u), _, _) => u,
        (None, Some(u), _) => u,
        (None, None, true) => {
            return err(
                StatusCode::BAD_REQUEST,
                "owner_uid is required when issuing via the admin UDS",
            );
        }
        (None, None, false) => {
            return err(StatusCode::BAD_REQUEST, "invalid caller identity");
        }
    };
    if !actor.is_local_admin()
        && Some(owner_uid) != caller_uid
        && !claims.has_scope("api_keys:admin")
    {
        return err(
            StatusCode::FORBIDDEN,
            "api_keys:admin scope required to issue keys for other users",
        );
    }

    // Scope subset check.
    // - User caller (self-scoped): requested ⊆ caller's own scopes.
    // - ApiKey or LocalAdmin or cross-owner: requested ⊆ owner's role scopes.
    let allowed_scopes: HashSet<String> = if actor.is_user() && Some(owner_uid) == caller_uid {
        claims.scopes.iter().cloned().collect()
    } else {
        match s.store.get_user_by_id(owner_uid).await {
            Ok(Some(u)) => u.role.scopes().into_iter().collect(),
            Ok(None) => return err(StatusCode::NOT_FOUND, "owner user not found"),
            Err(e) => {
                tracing::warn!(error = %e, "get_user_by_id failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
            }
        }
    };
    for requested in &body.scopes {
        if !allowed_scopes.contains(requested) {
            return err(
                StatusCode::FORBIDDEN,
                &format!("scope `{requested}` is not held by the key owner"),
            );
        }
    }

    // Generate token with prefix-collision retry.
    let mut attempt = 0u32;
    let (new_key, record) = loop {
        let candidate = match api_key::generate() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "api_key::generate failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed");
            }
        };
        let exists = match s.store.api_key_prefix_exists(&candidate.lookup_prefix).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "api_key_prefix_exists failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
            }
        };
        if !exists {
            let rec = ApiKeyRecord {
                id: Uuid::new_v4(),
                prefix: candidate.lookup_prefix.clone(),
                hash: candidate.hash.clone(),
                owner_uid,
                scopes: body.scopes.clone(),
                label: body.label.trim().to_string(),
                created_at: Utc::now(),
                last_used_at: None,
                expires_at: body
                    .expires_in_days
                    .map(|d| Utc::now() + Duration::days(d as i64)),
                allowed_cidrs: body.allowed_cidrs.clone(),
                revoked_at: None,
            };
            break (candidate, rec);
        }
        attempt += 1;
        if attempt >= MAX_PREFIX_COLLISION_RETRIES {
            tracing::error!(
                attempts = attempt,
                "API key prefix collided repeatedly — possible RNG problem"
            );
            return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed");
        }
        tracing::warn!(attempt, "API key prefix collision — regenerating");
    };

    if let Err(e) = s.store.create_api_key(&record).await {
        tracing::warn!(error = %e, "create_api_key failed");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
    }

    let mut audit_e = audit::entry_from_claims(&claims, "api_key.created")
        .with_target("api_key", record.id.to_string());
    audit_e.detail = serde_json::json!({
        "label": record.label,
        "owner_uid": record.owner_uid,
        "scopes": record.scopes,
    });
    audit::emit(&s, audit_e).await;

    let resp = CreateApiKeyResponse {
        id: record.id,
        label: record.label,
        token: new_key.full_token,
        owner_uid: record.owner_uid,
        scopes: record.scopes,
        created_at: record.created_at,
        expires_at: record.expires_at,
    };
    (StatusCode::CREATED, Json(resp)).into_response()
}

/// `GET /api/v1/auth/api-keys`
///
/// Self-scoped callers get their own keys. Callers with `api_keys:admin`
/// get every key system-wide.
pub async fn list_api_keys(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
) -> impl IntoResponse {
    let actor = claims.actor();
    // LocalAdmin and any api_keys:admin holder see everything.
    let admin = actor.is_local_admin() || claims.has_scope("api_keys:admin");
    let records = if admin {
        s.store.list_api_keys().await
    } else {
        let Some(caller_uid) = actor.effective_uid() else {
            return err(StatusCode::BAD_REQUEST, "invalid caller identity");
        };
        s.store.list_api_keys_by_owner(caller_uid).await
    };
    match records {
        Ok(recs) => {
            let out: Vec<ApiKeySummary> = recs.iter().map(record_to_summary).collect();
            (StatusCode::OK, Json(out)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "list_api_keys failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable")
        }
    }
}

/// `DELETE /api/v1/auth/api-keys/{id}` — revoke.
pub async fn revoke_api_key(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let actor = claims.actor();
    // Load to check ownership.
    let rec = match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "API key not found"),
        Err(e) => {
            tracing::warn!(error = %e, "get_api_key_by_id failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
        }
    };
    let is_admin = actor.is_local_admin() || claims.has_scope("api_keys:admin");
    let is_self = actor.effective_uid() == Some(rec.owner_uid);
    if !is_admin && !is_self {
        return err(StatusCode::FORBIDDEN, "not authorised to revoke this key");
    }

    match s.store.revoke_api_key(id).await {
        Ok(true) => {
            let audit_e = audit::entry_from_claims(&claims, "api_key.revoked")
                .with_target("api_key", id.to_string());
            audit::emit(&s, audit_e).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "API key not found"),
        Err(e) => {
            tracing::warn!(error = %e, "revoke_api_key failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable")
        }
    }
}

/// `PATCH /api/v1/auth/api-keys/{id}` — rotate metadata (not the secret).
pub async fn update_api_key(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateApiKeyRequest>,
) -> impl IntoResponse {
    let actor = claims.actor();
    let mut rec = match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "API key not found"),
        Err(e) => {
            tracing::warn!(error = %e, "get_api_key_by_id failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
        }
    };
    let is_admin = actor.is_local_admin() || claims.has_scope("api_keys:admin");
    let is_self = actor.effective_uid() == Some(rec.owner_uid);
    if !is_admin && !is_self {
        return err(StatusCode::FORBIDDEN, "not authorised to update this key");
    }

    if let Some(label) = body.label {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "label cannot be blank");
        }
        rec.label = trimmed.to_string();
    }
    if let Some(new_scopes) = body.scopes {
        // Enforce scope subset against key owner's current role scopes.
        let owner_scopes: HashSet<String> = match s.store.get_user_by_id(rec.owner_uid).await {
            Ok(Some(u)) => u.role.scopes().into_iter().collect(),
            Ok(None) => return err(StatusCode::NOT_FOUND, "owner user not found"),
            Err(e) => {
                tracing::warn!(error = %e, "get_user_by_id failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
            }
        };
        for requested in &new_scopes {
            if !owner_scopes.contains(requested) {
                return err(
                    StatusCode::FORBIDDEN,
                    &format!("scope `{requested}` is not held by the key owner"),
                );
            }
        }
        rec.scopes = new_scopes;
    }
    if let Some(days) = body.expires_in_days {
        rec.expires_at = Some(Utc::now() + Duration::days(days as i64));
    }
    if let Some(cidrs) = body.allowed_cidrs {
        rec.allowed_cidrs = cidrs;
    }

    match s.store.update_api_key(&rec).await {
        Ok(_) => {
            let audit_e = audit::entry_from_claims(&claims, "api_key.updated")
                .with_target("api_key", rec.id.to_string());
            audit::emit(&s, audit_e).await;
            (StatusCode::OK, Json(record_to_summary(&rec))).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "update_api_key failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable")
        }
    }
}

/// `POST /api/v1/auth/api-keys/{id}/rotate`
///
/// Replaces the secret material on an existing key. Owner, scopes, label,
/// CIDRs, and expiry are preserved. Returns the new token once — same
/// "save it now" contract as create.
pub async fn rotate_api_key(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let actor = claims.actor();
    let rec = match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "API key not found"),
        Err(e) => {
            tracing::warn!(error = %e, "get_api_key_by_id failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
        }
    };
    let is_admin = actor.is_local_admin() || claims.has_scope("api_keys:admin");
    let is_self = actor.effective_uid() == Some(rec.owner_uid);
    if !is_admin && !is_self {
        return err(StatusCode::FORBIDDEN, "not authorised to rotate this key");
    }

    // Mint a fresh secret (with prefix-collision retry) and swap it in.
    let mut attempt = 0u32;
    let new_key = loop {
        let candidate = match api_key::generate() {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "api_key::generate failed during rotate");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed");
            }
        };
        let exists = match s.store.api_key_prefix_exists(&candidate.lookup_prefix).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "api_key_prefix_exists failed");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
            }
        };
        if !exists {
            break candidate;
        }
        attempt += 1;
        if attempt >= MAX_PREFIX_COLLISION_RETRIES {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed");
        }
    };

    let rotated = match s
        .store
        .replace_api_key_secret(id, new_key.lookup_prefix.clone(), new_key.hash)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, "API key vanished during rotate"),
        Err(e) => {
            tracing::warn!(error = %e, "replace_api_key_secret failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
        }
    };

    let audit_e = audit::entry_from_claims(&claims, "api_key.rotated")
        .with_target("api_key", rotated.id.to_string());
    audit::emit(&s, audit_e).await;

    let resp = CreateApiKeyResponse {
        id: rotated.id,
        label: rotated.label,
        token: new_key.full_token,
        owner_uid: rotated.owner_uid,
        scopes: rotated.scopes,
        created_at: rotated.created_at,
        expires_at: rotated.expires_at,
    };
    (StatusCode::OK, Json(resp)).into_response()
}
