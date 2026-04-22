//! Helpers for emitting audit-log events from handlers.
//!
//! Usage is deliberately fire-and-forget — recording a failure should not
//! crash the originating operation. Handlers call [`emit`] or [`emit_with`]
//! and move on.

use hc_auth::{Actor, Claims};
use hc_state::{AuditActorType, AuditEntry, AuditResult};
use uuid::Uuid;

use crate::AppState;

/// Convert an auth [`Actor`] to the audit-log's [`AuditActorType`] column.
pub fn actor_type_of(actor: &Actor) -> AuditActorType {
    match actor {
        Actor::User { .. } => AuditActorType::User,
        Actor::ApiKey { .. } => AuditActorType::ApiKey,
        Actor::LocalAdmin { .. } => AuditActorType::LocalAdmin,
    }
}

/// Build an `AuditEntry` from a handler's claims. Caller fills in the
/// event-type-specific fields afterwards.
pub fn entry_from_claims(claims: &Claims, event_type: impl Into<String>) -> AuditEntry {
    let actor = claims.actor();
    let actor_type = actor_type_of(&actor);
    let actor_id = match &actor {
        Actor::User { uid, .. } => Some(*uid),
        Actor::ApiKey { owner_uid, .. } => Some(*owner_uid),
        Actor::LocalAdmin { .. } => None,
    };
    AuditEntry::success(actor_type, actor_id, actor.audit_label(), event_type)
}

/// Fire an audit event. Errors are logged and swallowed — the originating
/// operation's correctness does not depend on the audit log succeeding.
pub async fn emit(state: &AppState, entry: AuditEntry) {
    if let Err(e) = state.store.record_audit(&entry).await {
        tracing::warn!(error = %e, "audit record failed");
    }
}

/// Anonymous audit — typically for auth failures (no claims yet).
pub fn anon_entry(
    event_type: impl Into<String>,
    result: AuditResult,
    label: impl Into<String>,
) -> AuditEntry {
    AuditEntry::success(AuditActorType::Anonymous, None, label, event_type).with_result(result)
}

/// Emit a no-claims event (e.g. login failed) without blocking the response.
pub async fn emit_anon(
    state: &AppState,
    event_type: impl Into<String>,
    result: AuditResult,
    label: impl Into<String>,
    detail: Option<serde_json::Value>,
) {
    let mut e = anon_entry(event_type, result, label);
    if let Some(d) = detail {
        e.detail = d;
    }
    emit(state, e).await;
}

/// Common audit patterns exposed as convenience fns so the call sites read
/// like documentation.
pub async fn login_success(state: &AppState, user_id: Uuid, username: &str) {
    emit(
        state,
        AuditEntry::success(
            AuditActorType::User,
            Some(user_id),
            format!("user:{username}"),
            "auth.login",
        ),
    )
    .await;
}

pub async fn login_failed(state: &AppState, attempted_username: &str) {
    emit_anon(
        state,
        "auth.login",
        AuditResult::Denied,
        format!("attempted_username:{attempted_username}"),
        None,
    )
    .await;
}

pub async fn refresh_success(state: &AppState, user_id: Uuid, username: &str) {
    emit(
        state,
        AuditEntry::success(
            AuditActorType::User,
            Some(user_id),
            format!("user:{username}"),
            "auth.refresh",
        ),
    )
    .await;
}

pub async fn refresh_reuse_detected(state: &AppState, user_id: Uuid) {
    let mut e = AuditEntry::success(
        AuditActorType::User,
        Some(user_id),
        format!("user:{user_id}"),
        "auth.refresh.reuse",
    )
    .with_result(AuditResult::Denied);
    e.detail = serde_json::json!({ "action": "chain_revoked" });
    emit(state, e).await;
}
