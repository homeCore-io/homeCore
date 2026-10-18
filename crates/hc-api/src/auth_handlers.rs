//! Handlers for authentication and user management endpoints.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hc_api_types::auth::{
    ChangePasswordRequest, CreateUserRequest, LoginRequest, LoginResponse, RefreshRequest,
    RefreshResponse, SetPasswordRequest, SetRoleRequest,
};
use hc_auth::refresh;
use hc_auth::{hash_password, verify_password, User, UserInfo};
use hc_state::RefreshTokenRecord;
use serde_json::json;
use uuid::Uuid;

const REFRESH_MAX_RETRIES: u32 = 3;

use crate::{audit, auth_middleware::AuthUser, AppState};

/// Revoke every refresh token belonging to `user_id`.
///
/// Bumping `token_version` only kills *access* tokens. A surviving refresh
/// token would mint a fresh one on the next `/auth/refresh` and hand the
/// session straight back, so a password change has to sweep both. Best-effort
/// per token: one failure should not abort the sweep, since a partial
/// revocation still beats none.
///
/// Returns the number revoked, for the audit record.
async fn revoke_all_sessions(s: &AppState, user_id: Uuid) -> usize {
    let records = match s.store.list_refresh_by_user(user_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%user_id, error = %e, "could not list refresh tokens to revoke");
            return 0;
        }
    };
    let now = chrono::Utc::now();
    let mut revoked = 0usize;
    for rec in records {
        // Already dead — don't churn the store or inflate the count.
        if rec.revoked_at.is_some() || rec.expires_at <= now {
            continue;
        }
        match s.store.revoke_refresh(rec.id).await {
            Ok(true) => revoked += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::error!(%user_id, token_id = %rec.id, error = %e, "refresh revoke failed");
            }
        }
    }
    revoked
}

/// Whether the house would still have an admin if `id` became `role` (or, with
/// `None`, were removed). Someone has to be able to manage people, plugins and
/// keys; an install with no admin can only be recovered from the command line.
async fn keeps_an_admin(
    s: &AppState,
    id: Uuid,
    role: Option<hc_auth::Role>,
) -> Result<bool, String> {
    let users = s.store.list_users().await.map_err(|e| e.to_string())?;
    Ok(users.iter().any(|u| {
        let r = if u.id == id { role } else { Some(u.role) };
        r == Some(hc_auth::Role::Admin)
    }))
}

/// A person's own API keys, brought within what they may now do. A key takes a
/// subset of its owner's scopes when it is made; without this a demoted
/// person's keys — or a removed person's — would go on doing what they no
/// longer may. With `None` (the person is gone) every key of theirs is revoked.
/// Display keys are the house's, not their maker's (H3), and are left alone.
///
/// Returns how many keys were narrowed or revoked, for the audit record.
async fn bring_keys_within(s: &AppState, owner: Uuid, role: Option<hc_auth::Role>) -> usize {
    let keys = match s.store.list_api_keys_by_owner(owner).await {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(%owner, error = %e, "could not list keys to bring within the owner's role");
            return 0;
        }
    };
    let mut changed = 0usize;
    for mut key in keys.into_iter().filter(|k| !k.display && !k.is_revoked()) {
        let result = match role {
            None => s.store.revoke_api_key(key.id).await.map(|_| true),
            Some(role) => {
                let allowed = role.scopes();
                let before = key.scopes.len();
                key.scopes.retain(|x| allowed.contains(x));
                if key.scopes.len() == before {
                    continue;
                }
                s.store.update_api_key(&key).await.map(|_| true)
            }
        };
        match result {
            Ok(_) => changed += 1,
            Err(e) => tracing::error!(%owner, key = %key.id, error = %e, "could not narrow key"),
        }
    }
    changed
}

// ---------- Login ----------

/// `POST /api/v1/auth/login`
/// Returns a signed JWT on success.
pub async fn login(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // What signed in, for the person's list of sessions.
    let agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .chars()
        .take(256)
        .collect::<String>();
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
            audit::login_failed(&s, &body.username).await;
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
        audit::login_failed(&s, &body.username).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid credentials" })),
        )
            .into_response();
    }

    audit::login_success(&s, user.id, &user.username).await;
    match s.jwt.issue(
        &user.id.to_string(),
        &user.username,
        user.role,
        user.token_version,
    ) {
        Ok(token) => {
            let expires_in = s.jwt.expiry_hours() * 3600;
            // Mint a refresh token alongside. If minting fails, still return
            // the access token — login shouldn't break just because the
            // refresh store had a transient issue.
            let (refresh_token, refresh_expires_in) =
                match issue_refresh(&s, user.id, None, &agent).await {
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

// ---- your sessions ----

/// `GET /api/v1/auth/sessions` — where you're signed in: each browser or app
/// holding a refresh token of yours, by what it said it was and when it
/// signed in. Your own only; an admin sees theirs too.
pub async fn list_sessions(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
) -> impl IntoResponse {
    let Some(uid) = claims.actor().effective_uid() else {
        return Json(json!([])).into_response();
    };
    let now = chrono::Utc::now();
    match s.store.list_refresh_by_user(uid).await {
        Ok(mut list) => {
            list.retain(|r| r.is_usable(now));
            list.sort_by_key(|r| std::cmp::Reverse(r.created_at));
            Json(
                list.iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "user_agent": r.user_agent,
                            "started_at": r.session_started,
                            "last_active": r.created_at,
                            "expires_at": r.expires_at,
                        })
                    })
                    .collect::<Vec<_>>(),
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

/// `DELETE /api/v1/auth/sessions/{id}` — end one: it can't refresh, so it
/// signs out when its current access token runs out (an hour at most).
pub async fn end_session(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let uid = claims.actor().effective_uid();
    match s.store.get_refresh_by_id(id).await {
        Ok(Some(r)) if Some(r.user_id) == uid => {}
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such session of yours" })),
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
    }
    if let Err(e) = s.store.revoke_refresh(id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    let e = audit::entry_from_claims(&claims, "auth.session_ended")
        .with_target("session", id.to_string());
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/v1/auth/sessions/end-all` — sign out everywhere, this one
/// included: every refresh token goes, and every access token issued before
/// now stops working at once.
pub async fn end_all_sessions(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
) -> impl IntoResponse {
    let Some(uid) = claims.actor().effective_uid() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "not a person's session" })),
        )
            .into_response();
    };
    if !claims.actor().is_user() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "an API key has no sessions to end" })),
        )
            .into_response();
    }
    let mut user = match s.store.get_user_by_id(uid).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such person" })),
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
    user.token_version += 1;
    if let Err(e) = s.store.update_user(&user).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    let revoked = revoke_all_sessions(&s, uid).await;
    let mut e = audit::entry_from_claims(&claims, "auth.signed_out_everywhere")
        .with_target("user", uid.to_string());
    e.detail = json!({ "refresh_tokens_revoked": revoked });
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Mint a refresh token and persist it. Returns `(full_token, ttl_seconds)`.
async fn issue_refresh(
    s: &AppState,
    user_id: Uuid,
    parent: Option<&RefreshTokenRecord>,
    user_agent: &str,
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
        parent_id: parent.map(|p| p.id),
        created_at: chrono::Utc::now(),
        used_at: None,
        expires_at,
        revoked_at: None,
        // The session goes on: what signed in, and when.
        user_agent: parent.map_or_else(|| user_agent.to_string(), |p| p.user_agent.clone()),
        session_started: Some(parent.map_or_else(chrono::Utc::now, |p| {
            p.session_started.unwrap_or(p.created_at)
        })),
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
    let body_tok = match body
        .refresh_token
        .strip_prefix(refresh::REFRESH_TOKEN_PREFIX)
    {
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
        audit::refresh_reuse_detected(&s, rec.user_id).await;
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

    let access = match s.jwt.issue(
        &user.id.to_string(),
        &user.username,
        user.role,
        user.token_version,
    ) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let (new_refresh, refresh_ttl) = match issue_refresh(&s, rec.user_id, Some(&rec), "").await {
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

    audit::refresh_success(&s, user.id, &user.username).await;
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
///
/// The authenticated user's profile, **plus the scopes this credential
/// actually carries.**
///
/// Those are not the same question when the credential is an API key. A key
/// names its owner in `uid`, so the profile is the owner's — including the
/// owner's role — while the key's authority is its own `scopes`, a subset
/// chosen when it was issued. A caller that read the role and looked up what
/// that role may do would hand a deliberately read-only key the full rights of
/// the admin who created it. hc-web-lit authorises its own content API this
/// way, which is how the gap was found.
///
/// So the effective scopes come straight off the claims, for both credential
/// kinds: a password token carries `role.scopes()` and a key carries its own.
/// A whitelisted caller has no user record and still gets 404 here, which is
/// correct — the bypass is an address being trusted, not somebody being known.
pub async fn me(State(s): State<AppState>, AuthUser(claims): AuthUser) -> impl IntoResponse {
    // A display's key answers as the display (H3), not as whoever made it.
    if let Some(display) = crate::display_handlers::me_as_display(&s, &claims).await {
        return (StatusCode::OK, Json(display)).into_response();
    }
    match s
        .store
        .get_user_by_id(Uuid::parse_str(&claims.uid).unwrap_or_default())
        .await
    {
        Ok(Some(user)) => {
            let mut body = json!(UserInfo::from(&user));
            // Added to the response rather than to `UserInfo`, which several
            // other endpoints return and where a per-credential field would be
            // meaningless.
            body["scopes"] = json!(claims.scopes);
            body["has_pin"] = json!(user.pin_hash.is_some());
            (StatusCode::OK, Json(body)).into_response()
        }
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

// ---------- Roles ----------

/// `GET /api/v1/auth/roles`
///
/// The full set of roles and the scopes each grants, straight from
/// [`Role::scopes`] so the mapping has one home. The client renders this to
/// show what a role can do and to offer an access key's scopes as a subset of
/// its owner's role. Not sensitive — any authenticated user may read it.
pub async fn list_roles(_: AuthUser) -> impl IntoResponse {
    let roles: Vec<_> = hc_auth::Role::all()
        .iter()
        .map(|r| json!({ "role": r.wire(), "scopes": r.scopes() }))
        .collect();
    (StatusCode::OK, Json(json!(roles)))
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
    // Invalidates every access token issued before this moment, including the
    // one that authenticated this very request — the caller must sign in again.
    // That is the point: "change my password" is worthless as a response to a
    // stolen session if the stolen session survives it.
    user.token_version += 1;
    match s.store.update_user(&user).await {
        Ok(_) => {
            let revoked = revoke_all_sessions(&s, uid).await;
            // user.created, user.deleted and user.role_changed were all audited;
            // a password change was the one account change that left no trace.
            let mut e = audit::entry_from_claims(&claims, "user.password_changed")
                .with_target("user", uid.to_string());
            e.detail = json!({
                "username": user.username,
                "token_version": user.token_version,
                "refresh_tokens_revoked": revoked,
            });
            audit::emit(&s, e).await;
            StatusCode::NO_CONTENT.into_response()
        }
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
        token_version: 0,
        pin_hash: None,
    };
    match s.store.create_user(&user).await {
        Ok(_) => {
            let mut e = audit::entry_from_claims(&claims, "user.created")
                .with_target("user", user.id.to_string());
            e.detail = json!({ "username": user.username, "role": user.role });
            audit::emit(&s, e).await;
            (StatusCode::CREATED, Json(json!(UserInfo::from(&user)))).into_response()
        }
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
    // The caller is usually an admin who stays one, but a whitelisted local
    // admin or an admin key is not a person, and could remove the last.
    match keeps_an_admin(&s, id, None).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "cannot remove the house's only admin" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    }
    match s.store.delete_user(id).await {
        Ok(true) => {
            let sessions = revoke_all_sessions(&s, id).await;
            let keys = bring_keys_within(&s, id, None).await;
            let mut e = audit::entry_from_claims(&claims, "user.deleted")
                .with_target("user", id.to_string());
            e.detail = json!({ "refresh_tokens_revoked": sessions, "api_keys_revoked": keys });
            audit::emit(&s, e).await;
            StatusCode::NO_CONTENT.into_response()
        }
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
    let old_role = user.role;
    if old_role == body.role {
        return (StatusCode::OK, Json(json!(UserInfo::from(&user)))).into_response();
    }
    if old_role == hc_auth::Role::Admin {
        match keeps_an_admin(&s, id, Some(body.role)).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "the house's only admin must stay an admin" })),
                )
                    .into_response()
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e })),
                )
                    .into_response()
            }
        }
    }
    user.role = body.role;
    // A token carries the scopes of the role it was minted under, so a new
    // role only applies once the old tokens stop working: end every session,
    // exactly as a password reset does. They sign in again under the new role.
    user.token_version += 1;
    match s.store.update_user(&user).await {
        Ok(_) => {
            let sessions = revoke_all_sessions(&s, id).await;
            let keys = bring_keys_within(&s, id, Some(user.role)).await;
            let mut e = audit::entry_from_claims(&claims, "user.role_changed")
                .with_target("user", id.to_string());
            e.detail = json!({
                "from": old_role,
                "to": user.role,
                "refresh_tokens_revoked": sessions,
                "api_keys_narrowed": keys,
            });
            audit::emit(&s, e).await;
            (StatusCode::OK, Json(json!(UserInfo::from(&user)))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `PATCH /api/v1/auth/users/:id/password` — admin only
///
/// An admin setting a password for an account they cannot log into. This is the
/// only way back in for a user who has forgotten theirs: there is no email on a
/// user record and so no reset-link flow, and until this existed the answer was
/// to delete the account and make a new one, which loses its id and every audit
/// entry pointing at it.
///
/// No current-password check, by definition — the admin does not know it. The
/// authority is the admin role, exactly as it is for changing someone's role or
/// deleting them outright, and it is audited for the same reason.
///
/// Existing sessions ARE invalidated: this bumps `User::token_version`, which
/// kills every access token issued before now, and revokes the target's refresh
/// tokens so none can be minted back. The target is signed out everywhere and
/// must log in with the new password — which is what makes this a usable
/// response to a compromised account, not just a forgotten one.
///
/// API keys owned by the user are deliberately left alive. They are a separate
/// credential with their own revocation path (`DELETE /auth/api-keys/:id`);
/// silently killing a service account's key because a human changed their
/// password would break integrations with no obvious cause.
pub async fn set_user_password(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetPasswordRequest>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    // Same floor as create_user and change_password. Stated here rather than
    // shared, because a validator that drifts between the three is worse than
    // three that agree.
    if body.new_password.len() < 8 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "password must be at least 8 characters" })),
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
    // An admin reset is the compromised-account path, so it must terminate the
    // target's live sessions rather than just change what they'd type next time.
    user.token_version += 1;
    match s.store.update_user(&user).await {
        Ok(_) => {
            let revoked = revoke_all_sessions(&s, id).await;
            // Records who reset whose, never the password. `self` distinguishes
            // an admin resetting their own account from an admin reaching into
            // someone else's, which is the case worth being able to find later.
            let mut e = audit::entry_from_claims(&claims, "user.password_reset")
                .with_target("user", id.to_string());
            e.detail = json!({
                "username": user.username,
                "self": claims.uid == id.to_string(),
                "token_version": user.token_version,
                "refresh_tokens_revoked": revoked,
            });
            audit::emit(&s, e).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod me_tests {
    use super::*;
    use axum::body::to_bytes;
    use hc_auth::{JwtService, Role};
    use hc_core::EventBus;

    async fn state_with(user: &User) -> AppState {
        let base = std::env::temp_dir().join(format!("hc_api_me_{}", Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&base);
        let store = hc_state::StateStore::open(
            &base.join("state.redb").to_string_lossy(),
            &base.join("history.sqlite").to_string_lossy(),
        )
        .await
        .expect("state store opens");
        store.create_user(user).await.expect("user is stored");

        AppState::new(crate::AppStateParams::new(
            store,
            EventBus::new(16),
            JwtService::new_hs256(b"test-secret-key-32-bytes-minimum!", 24),
        ))
    }

    fn an_admin() -> User {
        User {
            id: Uuid::new_v4(),
            username: "admin".into(),
            password_hash: "x".into(),
            role: Role::Admin,
            created_at: chrono::Utc::now(),
            token_version: 0,
            pin_hash: None,
        }
    }

    async fn body_of(response: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("body is json")
    }

    /// An API key names its owner, so the profile is the owner's — including
    /// the owner's role — while the key's authority is the narrower set of
    /// scopes it was issued with. A caller that reads `role` and looks up what
    /// that role may do hands a read-only key the rights of the admin who
    /// created it. That is not hypothetical: hc-web-lit authorised its own
    /// content API exactly that way, which is how this was found.
    #[tokio::test]
    async fn scopes_are_the_credentials_own_not_the_roles() {
        let user = an_admin();
        let state = state_with(&user).await;

        let claims = hc_auth::Claims {
            sub: "api_key:hallway panel".into(),
            uid: user.id.to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: vec!["dashboards:read".into(), "content:read".into()],
            actor: None,
            tv: 0,
        };

        let body = body_of(me(State(state), AuthUser(claims)).await.into_response()).await;

        // The profile is the owner's...
        assert_eq!(body["role"], "admin");
        // ...and the authority is the key's.
        assert_eq!(
            body["scopes"],
            serde_json::json!(["dashboards:read", "content:read"])
        );
        assert!(!body["scopes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("dashboards:write")));
    }

    /// A password token carries its role's scopes, so the two agree there —
    /// which is why reading the role looked correct for as long as keys went
    /// unused.
    #[tokio::test]
    async fn a_password_session_reports_its_roles_scopes() {
        let user = an_admin();
        let state = state_with(&user).await;

        let claims = hc_auth::Claims {
            sub: user.username.clone(),
            uid: user.id.to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: Role::Admin.scopes(),
            actor: None,
            tv: 0,
        };

        let body = body_of(me(State(state), AuthUser(claims)).await.into_response()).await;
        assert!(body["scopes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("content:write")));
    }
}

#[cfg(test)]
mod people_tests {
    //! People and access (A5), through the real router.
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use chrono::Utc;
    use hc_auth::Role;
    use tower::ServiceExt;

    struct House {
        app: axum::Router,
        store: hc_state::StateStore,
        jwt: hc_auth::JwtService,
        people: Vec<User>,
    }

    impl House {
        fn token(&self, u: &User) -> String {
            self.jwt
                .issue(&u.id.to_string(), &u.username, u.role, u.token_version)
                .unwrap()
        }
    }

    async fn house(roles: &[(&str, Role)]) -> House {
        let base = std::env::temp_dir().join(format!("hc_api_people_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = hc_state::StateStore::open(
            &base.join("state.redb").to_string_lossy(),
            &base.join("history.sqlite").to_string_lossy(),
        )
        .await
        .unwrap();
        let mut people = Vec::new();
        for (name, role) in roles {
            let u = User {
                id: Uuid::new_v4(),
                username: (*name).into(),
                password_hash: hc_auth::hash_pin("correct horse").unwrap(),
                role: *role,
                created_at: Utc::now(),
                token_version: 0,
                pin_hash: None,
            };
            store.create_user(&u).await.unwrap();
            people.push(u);
        }
        const SECRET: &[u8] = b"people-test-secret-32-bytes-min!!";
        let jwt = hc_auth::JwtService::new_hs256(SECRET, 24);
        let state = crate::AppState::new(crate::AppStateParams::new(
            store.clone(),
            hc_core::EventBus::new(16),
            hc_auth::JwtService::new_hs256(SECRET, 24),
        ));
        House {
            app: crate::router(state, None),
            store,
            jwt,
            people,
        }
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        token: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        // Not a whitelisted address: every request here stands on its token.
        req.extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [10, 9, 8, 7],
                1234,
            ))));
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn a_household_member_uses_devices_and_changes_none() {
        let h = house(&[("ada", Role::Admin), ("kid", Role::HouseholdMember)]).await;
        let kid = h.token(&h.people[1]);

        let (st, _) = call(
            &h.app,
            "PATCH",
            "/devices/lamp/state",
            &kid,
            json!({"on": true}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "a member commands a device");

        let (st, _) = call(&h.app, "PATCH", "/devices/lamp", &kid, json!({"name": "x"})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "but doesn't rename one");
        let (st, _) = call(&h.app, "DELETE", "/devices/lamp", &kid, json!({})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "or delete one");
        let (st, _) = call(&h.app, "GET", "/auth/users", &kid, json!({})).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_new_role_ends_the_old_sessions() {
        let h = house(&[("ada", Role::Admin), ("bo", Role::User)]).await;
        let admin = h.token(&h.people[0]);
        let bo = h.token(&h.people[1]);
        let id = h.people[1].id;

        let (st, _) = call(&h.app, "GET", "/auth/me", &bo, json!({})).await;
        assert_eq!(st, StatusCode::OK);
        let (st, body) = call(
            &h.app,
            "PATCH",
            &format!("/auth/users/{id}/role"),
            &admin,
            json!({"role": "household_member"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["role"], "household_member");

        let (st, _) = call(&h.app, "GET", "/auth/me", &bo, json!({})).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "a token minted under the old role stops working"
        );
        let bo_now = h.store.get_user_by_id(id).await.unwrap().unwrap();
        let (st, me) = call(&h.app, "GET", "/auth/me", &h.token(&bo_now), json!({})).await;
        assert_eq!(st, StatusCode::OK);
        assert!(!me["scopes"]
            .as_array()
            .unwrap()
            .contains(&json!("automations:write")));
    }

    #[tokio::test]
    async fn the_house_keeps_an_admin() {
        let h = house(&[("ada", Role::Admin), ("bo", Role::User)]).await;
        let ada = h.people[0].clone();
        let admin = h.token(&ada);

        let (st, _) = call(
            &h.app,
            "PATCH",
            &format!("/auth/users/{}/role", ada.id),
            &admin,
            json!({"role": "user"}),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "the only admin stays one");

        // With a second admin, the first may step down.
        let bo = h.people[1].id;
        let (st, _) = call(
            &h.app,
            "PATCH",
            &format!("/auth/users/{bo}/role"),
            &admin,
            json!({"role": "admin"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(
            &h.app,
            "PATCH",
            &format!("/auth/users/{}/role", ada.id),
            &admin,
            json!({"role": "user"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_demoted_persons_keys_do_no_more_than_they_may() {
        let h = house(&[("ada", Role::Admin), ("bo", Role::User)]).await;
        let admin = h.token(&h.people[0]);
        let bo = h.people[1].clone();
        let (st, made) = call(
            &h.app,
            "POST",
            "/auth/api-keys",
            &h.token(&bo),
            json!({"label": "script", "scopes": ["devices:read", "devices:command", "automations:write"]}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{made}");

        let (st, _) = call(
            &h.app,
            "PATCH",
            &format!("/auth/users/{}/role", bo.id),
            &admin,
            json!({"role": "household_member"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let keys = h.store.list_api_keys_by_owner(bo.id).await.unwrap();
        assert_eq!(keys[0].scopes, vec!["devices:read", "devices:command"]);

        // And a removed person's keys stop working.
        let (st, _) = call(
            &h.app,
            "DELETE",
            &format!("/auth/users/{}", bo.id),
            &admin,
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let keys = h.store.list_api_keys_by_owner(bo.id).await.unwrap();
        assert!(keys[0].is_revoked());
    }

    #[tokio::test]
    async fn an_admin_clears_a_forgotten_pin() {
        let h = house(&[("ada", Role::Admin), ("kid", Role::HouseholdMember)]).await;
        let admin = h.token(&h.people[0]);
        let kid = h.token(&h.people[1]);
        let id = h.people[1].id;

        let (st, _) = call(
            &h.app,
            "PUT",
            "/auth/pin",
            &kid,
            json!({"pin": "4821", "password": "correct horse"}),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, users) = call(&h.app, "GET", "/auth/users", &admin, json!({})).await;
        let row = |users: &serde_json::Value| {
            users
                .as_array()
                .unwrap()
                .iter()
                .find(|u| u["username"] == "kid")
                .unwrap()["has_pin"]
                .clone()
        };
        assert_eq!(row(&users), json!(true));

        let (st, _) = call(
            &h.app,
            "DELETE",
            &format!("/auth/users/{id}/pin"),
            &kid,
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "only an admin clears another's");
        let (st, _) = call(
            &h.app,
            "DELETE",
            &format!("/auth/users/{id}/pin"),
            &admin,
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        let (_, users) = call(&h.app, "GET", "/auth/users", &admin, json!({})).await;
        assert_eq!(row(&users), json!(false));
    }
}
