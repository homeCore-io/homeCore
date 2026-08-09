//! HTTP for plugin-runtime enrollment.
//!
//! Five routes: a runtime asks to join, polls for an answer, and an admin lists,
//! resolves and issues tokens. The decision logic lives in
//! [`crate::plugin_runtimes`]; this module is transport, storage and policy
//! gating only.
//!
//! `POST /enroll` is the one unauthenticated endpoint in the plugin story, and
//! it is gated three ways before it reaches any logic: the feature switch, the
//! whitelist, and a rate budget of its own. See `docs/pluginRuntimesPlan.md`.

use axum::{
    extract::{ConnectInfo, Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use hc_api_types::plugin_runtimes::{
    EnrollRequest, EnrollResponse, RuntimeCapabilities, RuntimeCredentials, RuntimeStatus,
    RuntimeSummary,
};
use hc_state::plugin_runtime_store::{
    EnrollTokenRecord, RuntimeRecord, RuntimeStatus as StoreStatus,
};
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::auth_middleware::PluginsWrite;
use crate::plugin_runtimes as svc;
use crate::AppState;

/// How long an issued enrollment token stays usable.
const TOKEN_TTL_HOURS: i64 = 24;

fn err(code: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

/// Runtimes are opt-out per deployment, and "off" means the surface is absent
/// rather than forbidden — there is nothing here to probe for.
fn feature_enabled(state: &AppState) -> bool {
    state.plugin_runtimes_config.enabled
}

fn store(state: &AppState) -> std::sync::Arc<hc_state::PluginRuntimeStore> {
    state.store.plugin_runtimes()
}

fn to_status(s: StoreStatus) -> RuntimeStatus {
    match s {
        StoreStatus::Pending => RuntimeStatus::Pending,
        StoreStatus::Approved => RuntimeStatus::Approved,
        StoreStatus::Denied => RuntimeStatus::Denied,
    }
}

fn summary(rec: &RuntimeRecord) -> RuntimeSummary {
    RuntimeSummary {
        runtime_id: rec.runtime_id.clone(),
        status: to_status(rec.status),
        capabilities: RuntimeCapabilities {
            kind: rec.kind.clone(),
            abi: rec.abi.clone(),
            arch: rec.arch.clone(),
        },
        host_version: rec.host_version.clone(),
        sdk_version: rec.sdk_version.clone(),
        hostname: rec.hostname.clone(),
        network_mode: rec.network_mode.clone(),
        // Only meaningful while a decision is outstanding.
        code: match rec.status {
            StoreStatus::Pending => rec.code.clone(),
            _ => None,
        },
        source_ip: rec.source_ip.clone(),
        created_at: rec.created_at,
        last_seen_at: rec.last_seen_at,
        plugin_id: rec.plugin_id.clone(),
    }
}

/// Everything an approved runtime needs, plus the hash the caller must persist.
///
/// Both secrets are minted fresh every time credentials are handed out rather
/// than stored: the broker holds the MQTT credential, this store holds only the
/// fact of approval and a hash, and a value nobody can read back cannot leak
/// from here. Handing them out again is what makes a restarted container work
/// without re-approval, and rotating on each hand-out means a leaked key stops
/// working the next time the container comes up.
fn credentials_for(
    state: &AppState,
    rec: &RuntimeRecord,
) -> anyhow::Result<(RuntimeCredentials, String)> {
    let plugin_id = match &rec.plugin_id {
        Some(id) => id.clone(),
        None => svc::plugin_id_for(
            &RuntimeCapabilities {
                kind: rec.kind.clone(),
                abi: rec.abi.clone(),
                arch: rec.arch.clone(),
            },
            &rec.runtime_id,
        )?,
    };
    // Broker coordinates come from the same place a binary plugin's seeded
    // config gets them, so a runtime and a plugin can never be told to connect
    // to different brokers.
    let install = state
        .plugin_install
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plugin install is not configured on this server"))?;
    let key = hc_auth::api_key::generate()?;
    Ok((
        RuntimeCredentials {
            broker_host: install.broker_host.clone(),
            broker_port: install.broker_port,
            plugin_id,
            mqtt_password: svc::mint_mqtt_password(),
            api_key: key.full_token,
        },
        key.hash,
    ))
}

/// Authenticate a runtime by its own API key.
///
/// Returns the record only when the key belongs to *that* runtime, so an
/// endpoint written with this cannot serve one runtime's placements to another
/// however carelessly it is written afterwards.
pub fn authenticate_runtime(
    state: &AppState,
    runtime_id: &str,
    headers: &axum::http::HeaderMap,
) -> Option<RuntimeRecord> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?
        .trim();

    let rec = store(state).get(runtime_id).ok().flatten()?;
    if rec.status != StoreStatus::Approved {
        return None;
    }
    let hash = rec.api_key_hash.as_deref()?;
    hc_auth::api_key::verify_token(presented, hash).then_some(rec)
}

// ── POST /plugin-runtimes/enroll ─────────────────────────────────────────────

pub async fn enroll(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<EnrollRequest>,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let store = store(&state);

    let cfg = &state.plugin_runtimes_config;
    let ip = addr.ip();

    // Whitelist gate. A runtime is a machine on your network; a request from
    // outside it should not reach an approval screen at all.
    if cfg.whitelist_only && !state.whitelist.iter().any(|net| net.contains(&ip)) {
        tracing::warn!(%ip, "plugin-runtime enrollment refused: source not whitelisted");
        return err(
            StatusCode::FORBIDDEN,
            "enrollment is restricted to the local network",
        );
    }

    // Authenticity before welcome. A request that is not what it claims to be
    // never reaches the policy decision.
    if let Err(e) = svc::verify_enrollment_signature(&req) {
        tracing::warn!(%ip, runtime_id = %req.runtime_id, error = %e, "enrollment signature rejected");
        return err(StatusCode::UNAUTHORIZED, format!("invalid signature: {e}"));
    }

    let now = Utc::now();
    let existing = match store.get(&req.runtime_id) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };

    // A known id presenting a different key is a different runtime wearing a
    // familiar name. It does not inherit the record, and it does not get to
    // overwrite it either — that would let anyone who read an id in a log
    // dislodge a working runtime.
    if let Some(prev) = &existing {
        if !svc::is_same_identity(prev, &req) {
            tracing::warn!(
                %ip, runtime_id = %req.runtime_id,
                "enrollment refused: runtime_id already registered to a different key"
            );
            return err(
                StatusCode::CONFLICT,
                "this runtime_id is already registered to a different key",
            );
        }
    }

    // Token, if one was presented. An invalid token is not fatal in open mode —
    // it simply fails to short-circuit the wait.
    let token_valid = match &req.token {
        Some(tok) if !tok.trim().is_empty() => {
            redeem(&store, tok, &req.runtime_id, now).unwrap_or(false)
        }
        _ => false,
    };

    let pending_count = store.pending_count(now).unwrap_or(0) as u32;
    let decision = svc::decide(&svc::EnrollContext {
        now,
        token_only: cfg.is_token_only(),
        token_valid,
        existing: existing.as_ref(),
        pending_count,
        max_pending: cfg.max_pending,
        max_denials: cfg.max_denials,
    });

    match decision {
        svc::EnrollDecision::Reject(reason) => {
            tracing::info!(%ip, runtime_id = %req.runtime_id, %reason, "enrollment refused");
            err(StatusCode::FORBIDDEN, reason)
        }
        svc::EnrollDecision::Approve => {
            let mut rec = existing.unwrap_or_else(|| new_record(&req, ip.to_string(), now));
            rec.status = StoreStatus::Approved;
            rec.code = None;
            rec.expires_at = None;
            rec.last_seen_at = Some(now);
            let (creds, api_key_hash) = match credentials_for(&state, &rec) {
                Ok(c) => c,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            rec.plugin_id = Some(creds.plugin_id.clone());
            rec.api_key_hash = Some(api_key_hash);
            if let Err(e) = store.upsert(&rec) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
            }
            tracing::info!(runtime_id = %rec.runtime_id, plugin_id = %creds.plugin_id, "plugin runtime approved");
            (
                StatusCode::OK,
                Json(EnrollResponse {
                    runtime_id: rec.runtime_id,
                    status: RuntimeStatus::Approved,
                    enrollment_secret: None,
                    code: None,
                    credentials: Some(creds),
                    reason: None,
                    expires_at: None,
                }),
            )
                .into_response()
        }
        svc::EnrollDecision::Pending => {
            let secret = match hc_auth::api_key::generate() {
                Ok(k) => k,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            let code = match svc::generate_code() {
                Ok(c) => c,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            let expires = svc::pending_expiry(now, cfg.pending_ttl_mins);

            let mut rec = existing.unwrap_or_else(|| new_record(&req, ip.to_string(), now));
            rec.status = StoreStatus::Pending;
            rec.code = Some(code.clone());
            rec.secret_hash = Some(secret.hash.clone());
            rec.expires_at = Some(expires);
            rec.last_seen_at = Some(now);
            if let Err(e) = store.upsert(&rec) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
            }
            tracing::info!(
                runtime_id = %rec.runtime_id, %code,
                "plugin runtime is waiting for approval"
            );
            (
                StatusCode::ACCEPTED,
                Json(EnrollResponse {
                    runtime_id: rec.runtime_id,
                    status: RuntimeStatus::Pending,
                    enrollment_secret: Some(secret.full_token),
                    code: Some(code),
                    credentials: None,
                    reason: None,
                    expires_at: Some(expires),
                }),
            )
                .into_response()
        }
    }
}

fn new_record(req: &EnrollRequest, source_ip: String, now: chrono::DateTime<Utc>) -> RuntimeRecord {
    RuntimeRecord {
        runtime_id: req.runtime_id.clone(),
        public_key: req.public_key.clone(),
        kind: req.capabilities.kind.clone(),
        abi: req.capabilities.abi.clone(),
        arch: req.capabilities.arch.clone(),
        host_version: req.host_version.clone(),
        sdk_version: req.sdk_version.clone(),
        hostname: req.hostname.clone(),
        network_mode: req.network_mode.clone(),
        status: StoreStatus::Pending,
        code: None,
        secret_hash: None,
        api_key_hash: None,
        source_ip: Some(source_ip),
        plugin_id: None,
        denial_count: 0,
        cooldown_until: None,
        created_at: now,
        expires_at: None,
        last_seen_at: Some(now),
    }
}

fn redeem(
    store: &hc_state::PluginRuntimeStore,
    token: &str,
    runtime_id: &str,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<bool> {
    let body = token
        .strip_prefix(hc_auth::api_key::API_KEY_PREFIX)
        .unwrap_or(token);
    let Some(prefix) = hc_auth::api_key::lookup_prefix_from_body(body) else {
        return Ok(false);
    };
    let Some(rec) = store.get_token(prefix)? else {
        return Ok(false);
    };
    if !hc_auth::api_key::verify_token(token, &rec.hash) {
        return Ok(false);
    }
    store.redeem_token(prefix, runtime_id, now)
}

// ── GET /plugin-runtimes/{id} ────────────────────────────────────────────────

/// Status poll, authenticated by the enrollment secret.
///
/// Deliberately not rate-limited: a runtime polls this every couple of seconds
/// for up to the pending TTL, and the secret is full-strength and single-purpose.
pub async fn get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let store = store(&state);

    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim()
        .to_string();

    let Ok(Some(rec)) = store.get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };

    // Same 404 for "no such runtime" and "wrong secret": the poll endpoint must
    // not become a way to discover which runtime ids exist.
    let ok = rec
        .secret_hash
        .as_deref()
        .is_some_and(|h| hc_auth::api_key::verify_token(&presented, h));
    if !ok {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    }

    let now = Utc::now();
    match rec.status {
        StoreStatus::Approved => {
            let (creds, api_key_hash) = match credentials_for(&state, &rec) {
                Ok(c) => c,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            // The poll is where an approved-by-admin runtime first receives its
            // key, so the hash has to be recorded here too, not only on the
            // enroll path.
            let mut updated = rec.clone();
            updated.api_key_hash = Some(api_key_hash);
            updated.last_seen_at = Some(now);
            if let Err(e) = store.upsert(&updated) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
            }
            let rec = updated;
            (
                StatusCode::OK,
                Json(EnrollResponse {
                    runtime_id: rec.runtime_id,
                    status: RuntimeStatus::Approved,
                    enrollment_secret: None,
                    code: None,
                    credentials: Some(creds),
                    reason: None,
                    expires_at: None,
                }),
            )
                .into_response()
        }
        StoreStatus::Denied => (
            StatusCode::OK,
            Json(EnrollResponse {
                runtime_id: rec.runtime_id,
                status: RuntimeStatus::Denied,
                enrollment_secret: None,
                code: None,
                credentials: None,
                reason: Some("an administrator denied this runtime".into()),
                expires_at: None,
            }),
        )
            .into_response(),
        StoreStatus::Pending => {
            // Tell an expired poller to stop rather than letting it spin until
            // its container is restarted.
            if !rec.is_pending_open(now) {
                return err(StatusCode::GONE, "this enrollment expired; enroll again");
            }
            (
                StatusCode::OK,
                Json(EnrollResponse {
                    runtime_id: rec.runtime_id,
                    status: RuntimeStatus::Pending,
                    enrollment_secret: None,
                    code: rec.code,
                    credentials: None,
                    reason: None,
                    expires_at: rec.expires_at,
                }),
            )
                .into_response()
        }
    }
}

// ── Admin ────────────────────────────────────────────────────────────────────

pub async fn list_runtimes(State(state): State<AppState>, _: PluginsWrite) -> impl IntoResponse {
    let store = store(&state);
    match store.list() {
        Ok(list) => {
            let out: Vec<RuntimeSummary> = list.iter().map(summary).collect();
            (StatusCode::OK, Json(json!({ "runtimes": out }))).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    }
}

pub async fn approve_runtime(
    State(state): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = store(&state);
    let now = Utc::now();
    let Ok(Some(mut rec)) = store.get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    // Approving something that already timed out would resurrect a request the
    // operator never actually saw in time.
    if rec.status == StoreStatus::Pending && !rec.is_pending_open(now) {
        return err(StatusCode::GONE, "this enrollment expired; ask it to retry");
    }

    let plugin_id = match svc::plugin_id_for(
        &RuntimeCapabilities {
            kind: rec.kind.clone(),
            abi: rec.abi.clone(),
            arch: rec.arch.clone(),
        },
        &rec.runtime_id,
    ) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    };

    rec.status = StoreStatus::Approved;
    rec.code = None;
    rec.expires_at = None;
    rec.plugin_id = Some(plugin_id.clone());
    if let Err(e) = store.upsert(&rec) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    tracing::info!(runtime_id = %id, %plugin_id, "plugin runtime approved by admin");
    (StatusCode::OK, Json(summary(&rec))).into_response()
}

pub async fn deny_runtime(
    State(state): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = store(&state);
    let cfg = &state.plugin_runtimes_config;
    let now = Utc::now();
    let Ok(Some(mut rec)) = store.get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };

    rec.status = StoreStatus::Denied;
    rec.code = None;
    rec.secret_hash = None;
    rec.expires_at = None;
    rec.denial_count = rec.denial_count.saturating_add(1);
    // The cooldown starts once the limit is reached, so an accidental deny costs
    // nothing and a pattern of them costs an hour.
    if rec.denial_count >= cfg.max_denials {
        rec.cooldown_until = Some(svc::cooldown_until(now, cfg.denial_cooldown_mins));
    }
    if let Err(e) = store.upsert(&rec) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    tracing::info!(runtime_id = %id, denials = rec.denial_count, "plugin runtime denied");
    (StatusCode::OK, Json(summary(&rec))).into_response()
}

/// `POST /plugin-runtimes/tokens` — issue a one-time enrollment token.
///
/// The plaintext is returned once and never again; only its hash is stored.
pub async fn issue_token(State(state): State<AppState>, _: PluginsWrite) -> impl IntoResponse {
    let store = store(&state);
    let key = match hc_auth::api_key::generate() {
        Ok(k) => k,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let now = Utc::now();
    let rec = EnrollTokenRecord {
        prefix: key.lookup_prefix.clone(),
        hash: key.hash.clone(),
        created_at: now,
        expires_at: now + chrono::Duration::hours(TOKEN_TTL_HOURS),
        used_at: None,
        used_by: None,
    };
    if let Err(e) = store.create_token(&rec) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "token": key.full_token,
            "expires_at": rec.expires_at,
            "note": "Shown once. Set it as HOMECORE_ENROLL_TOKEN on the runtime container.",
        })),
    )
        .into_response()
}

// ── GET /plugin-runtimes/{id}/placements ─────────────────────────────────────

/// What this runtime should be running.
///
/// The desired state, in core's words. The runtime diffs it against what it
/// actually has and re-provisions the difference, which is why a lost container
/// volume costs nothing but a re-enrollment.
///
/// Authenticated by the runtime's own key, which identifies exactly one runtime
/// — so this cannot serve another's placements even by mistake.
pub async fn list_placements(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let Some(rec) = authenticate_runtime(&state, &id, &headers) else {
        // Same answer for "no such runtime", "not approved" and "wrong key":
        // this endpoint must not become a way to discover which runtimes exist
        // or which of them are live.
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };

    let store = store(&state);
    let placements = match store.placements_for(&rec.runtime_id) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };

    // Seeing its own placements is the runtime saying hello, so it counts as
    // being seen — the alternative is a healthy runtime looking stale on the
    // admin screen between heartbeats.
    let mut touched = rec;
    touched.last_seen_at = Some(Utc::now());
    let _ = store.upsert(&touched);

    let body: Vec<Value> = placements
        .iter()
        .map(|p| {
            json!({
                "plugin_id": p.plugin_id,
                "version": p.version,
                "sha256": p.sha256,
                "config": p.config,
                "placed_at": p.placed_at,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "placements": body }))).into_response()
}

// ── GET /plugin-runtimes/{id}/artifacts/{plugin_id} ──────────────────────────

/// Serve the artifact bytes for one placement.
///
/// **Core verifies; the runtime executes.** The signature over the index and the
/// sha256 of the bytes are checked here, in one implementation, rather than in
/// every runtime — Python, Node and .NET reimplementing ed25519 verification is
/// how you end up with one that skips it. The runtime re-checks only the sha256
/// core hands it, which catches transport corruption and nothing else. That is
/// the boundary: a compromised core is already past the point where a plugin's
/// credentials matter.
///
/// Fetched on demand rather than cached. A placement is installed once and the
/// artifact is tens of megabytes, so a cache would mostly be disk spent on
/// something read a single time.
pub async fn get_placement_artifact(
    State(state): State<AppState>,
    Path((id, plugin_id)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let Some(rec) = authenticate_runtime(&state, &id, &headers) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    let Some(reg) = state.registry.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no plugin registry is configured",
        );
    };

    // Only what this runtime was actually given. Asking for someone else's
    // plugin is the same 404 as asking for one that does not exist.
    let placement = match store(&state).placements_for(&rec.runtime_id) {
        Ok(list) => list.into_iter().find(|p| p.plugin_id == plugin_id),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };
    let Some(placement) = placement else {
        return err(StatusCode::NOT_FOUND, "no such placement on this runtime");
    };

    // Re-resolve rather than trusting the URL recorded at placement time: this
    // re-runs signature verification over the index, so a stored row can never
    // become a way to fetch something the registry no longer vouches for.
    let pv = match reg.version_of(&plugin_id, Some(&placement.version)).await {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("registry lookup failed: {e:#}"),
            )
        }
    };
    let Some(art) = pv.artifact_for_runtime(&rec.kind, &rec.abi, &rec.arch) else {
        return err(
            StatusCode::CONFLICT,
            format!(
                "{plugin_id} {} no longer publishes a {} {} artifact for {}",
                placement.version, rec.kind, rec.abi, rec.arch
            ),
        );
    };

    let file = match reg.download_artifact(art).await {
        Ok(f) => f,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("artifact download failed: {e:#}"),
            )
        }
    };
    let bytes = match std::fs::read(file.path()) {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading verified artifact: {e}"),
            )
        }
    };

    tracing::info!(
        runtime_id = %rec.runtime_id, %plugin_id, version = %placement.version,
        bytes = bytes.len(), "served a verified artifact to a runtime"
    );
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            // The runtime checks this against what it received. Repeated here
            // rather than only in the placement list so a client that fetches
            // directly still has it.
            (
                axum::http::HeaderName::from_static("x-hc-sha256"),
                art.sha256.clone(),
            ),
        ],
        bytes,
    )
        .into_response()
}
