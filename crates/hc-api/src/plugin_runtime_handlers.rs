//! HTTP handlers for plugin runtimes.
//!
//! Admin: add a runtime (which makes its login and returns the connection key
//! once), reset its login, remove it, list them, and open the broker to the
//! network. Runtime: say hello, fetch its placements and their artifacts, each
//! authenticated by the runtime's one secret.
//!
//! See `docs/pluginRuntimesPlan.md`.

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use hc_api_types::plugin_runtimes::{
    abi_for, AddRuntimeRequest, ConnectionKey, HelloRequest, HelloResponse, IssuedKey,
    RuntimeSummary,
};
use hc_state::plugin_runtime_store::RuntimeRecord;
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::auth_middleware::PluginsWrite;
use crate::plugin_runtimes as svc;
use crate::AppState;

fn err(code: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn feature_enabled(state: &AppState) -> bool {
    state.plugin_runtimes_config.enabled
}

fn store(state: &AppState) -> std::sync::Arc<hc_state::PluginRuntimeStore> {
    state.store.plugin_runtimes()
}

fn summary(rec: &RuntimeRecord) -> RuntimeSummary {
    RuntimeSummary {
        runtime_id: rec.runtime_id.clone(),
        name: rec.name.clone(),
        kind: rec.kind.clone(),
        abi: rec.abi.clone(),
        plugin_id: rec.plugin_id.clone(),
        homecore_url: rec.homecore_url.clone(),
        created_at: rec.created_at,
        arch: rec.arch.clone(),
        host_version: rec.host_version.clone(),
        hostname: rec.hostname.clone(),
        network_mode: rec.network_mode.clone(),
        source_ip: rec.source_ip.clone(),
        last_seen_at: rec.last_seen_at,
    }
}

/// Make a new secret for `rec`, store its hash, and build the key around it.
///
/// The one place a secret is made, for adding and for Reset login alike, so the
/// two cannot drift. Storing the new hash is what cuts an old key off.
fn issue_key(state: &AppState, mut rec: RuntimeRecord) -> anyhow::Result<IssuedKey> {
    let install = state
        .plugin_install
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plugin install is not configured on this server"))?;
    let secret = hc_auth::api_key::generate()?;
    rec.key_hash = secret.hash;
    store(state).upsert(&rec)?;
    let details = ConnectionKey {
        url: rec.homecore_url.clone(),
        broker_host: svc::broker_host_for_key(&install.broker_host, &rec.homecore_url),
        broker_port: install.broker_port,
        runtime_id: rec.runtime_id.clone(),
        username: rec.plugin_id.clone(),
        secret: secret.full_token,
        kind: rec.kind.clone(),
        abi: rec.abi.clone(),
    };
    Ok(IssuedKey {
        runtime: summary(&rec),
        key: details.encode(),
        details,
    })
}

/// Authenticate a runtime by its secret.
///
/// Returns the record only when the secret belongs to *that* runtime, so an
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
    hc_auth::api_key::verify_token(presented, &rec.key_hash).then_some(rec)
}

// ── Admin ────────────────────────────────────────────────────────────────────

/// `GET /plugin-runtimes`
pub async fn list_runtimes(State(state): State<AppState>, _: PluginsWrite) -> impl IntoResponse {
    match store(&state).list() {
        Ok(list) => {
            let out: Vec<RuntimeSummary> = list.iter().map(summary).collect();
            (StatusCode::OK, Json(json!({ "runtimes": out }))).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    }
}

/// `POST /plugin-runtimes`: add a runtime and make its login.
///
/// The key in the answer is shown once. Losing it costs a Reset login, which is
/// cheaper than keeping a recoverable copy of a password anywhere.
pub async fn add_runtime(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Json(req): Json<AddRuntimeRequest>,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(
            StatusCode::NOT_FOUND,
            "plugin runtimes are turned off in homecore.toml",
        );
    }
    let name = req.name.trim();
    if name.is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "give the runtime a name");
    }
    let kind = req
        .kind
        .as_deref()
        .unwrap_or("python")
        .trim()
        .to_lowercase();
    let Some(abi) = abi_for(&kind) else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("homeCore can't host {kind} plugins yet; python is the one it can"),
        );
    };
    let homecore_url = match svc::check_homecore_url(&req.homecore_url) {
        Ok(u) => u,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")),
    };
    let runtime_id = match svc::new_runtime_id() {
        Ok(id) => id,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let plugin_id = match svc::plugin_id_for(&kind, &runtime_id) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    };
    let rec = RuntimeRecord {
        runtime_id: runtime_id.clone(),
        name: name.to_string(),
        kind,
        abi: abi.to_string(),
        plugin_id,
        key_hash: String::new(),
        homecore_url,
        created_at: Utc::now(),
        arch: None,
        host_version: None,
        hostname: None,
        network_mode: None,
        source_ip: None,
        last_seen_at: None,
    };
    match issue_key(&state, rec) {
        Ok(issued) => {
            let e = crate::audit::entry_from_claims(&claims, "plugin_runtime.added")
                .with_target("plugin_runtime", runtime_id.clone());
            crate::audit::emit(&state, e).await;
            tracing::info!(%runtime_id, "plugin runtime added");
            (StatusCode::CREATED, Json(issued)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `POST /plugin-runtimes/{id}/reset`: a new key; the old one stops working now.
///
/// The runtime is disconnected at its next broker login or API call. Its
/// plugins keep their own logins and come back when it does.
pub async fn reset_runtime(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Ok(Some(rec)) = store(&state).get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    match issue_key(&state, rec) {
        Ok(issued) => {
            let e = crate::audit::entry_from_claims(&claims, "plugin_runtime.login_reset")
                .with_target("plugin_runtime", id.clone());
            crate::audit::emit(&state, e).await;
            tracing::info!(runtime_id = %id, "plugin runtime login reset");
            (StatusCode::OK, Json(issued)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// `DELETE /plugin-runtimes/{id}`: remove a runtime and everything placed on it.
///
/// Each plugin goes the way an uninstall takes it (devices, config and its
/// login with it), then the runtime's own record, which ends its key. The
/// container is the operator's to stop.
pub async fn remove_runtime(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let store = store(&state);
    let Ok(Some(rec)) = store.get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    let placed = store.placements_for(&id).unwrap_or_default();
    let mut removed_plugins = Vec::new();
    for p in &placed {
        let _ = crate::handlers::deregister_plugin(
            State(state.clone()),
            PluginsWrite(claims.clone()),
            Path(p.plugin_id.clone()),
            Query(crate::handlers::UninstallQuery::default()),
        )
        .await;
        removed_plugins.push(p.plugin_id.clone());
    }
    // The runtime registered as a plugin too; forget it the same way, so it does
    // not linger in the plugin list as offline.
    let _ = crate::handlers::deregister_plugin(
        State(state.clone()),
        PluginsWrite(claims.clone()),
        Path(rec.plugin_id.clone()),
        Query(crate::handlers::UninstallQuery::default()),
    )
    .await;
    if let Err(e) = store.delete(&id) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    let mut e = crate::audit::entry_from_claims(&claims, "plugin_runtime.removed")
        .with_target("plugin_runtime", id.clone());
    e.detail = json!({ "plugins_removed": removed_plugins });
    crate::audit::emit(&state, e).await;
    tracing::info!(runtime_id = %id, plugins = removed_plugins.len(), "plugin runtime removed");
    (
        StatusCode::OK,
        Json(json!({ "removed": id, "plugins_removed": removed_plugins })),
    )
        .into_response()
}

/// `GET /plugin-runtimes/broker`: can a runtime reach the broker at all?
///
/// A broker that listens only on homeCore's own machine cannot be reached from
/// a container or another computer. The app says so before anyone adds a
/// runtime that would never connect.
pub async fn broker_status(State(state): State<AppState>, _: PluginsWrite) -> impl IntoResponse {
    let Some(install) = state.plugin_install.as_ref() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin install is not configured on this server",
        );
    };
    (
        StatusCode::OK,
        Json(json!({
            "host": install.broker_host,
            "port": install.broker_port,
            "reachable_from_network": !svc::is_loopback(&install.broker_host),
            "can_open": state.homecore_config_path.is_some(),
        })),
    )
        .into_response()
}

/// `POST /plugin-runtimes/broker/open`: let the broker listen on the network.
///
/// Edits `homecore.toml` (keeping its comments) and asks for a restart, which
/// the app offers next. Adds a login for core itself when the file has none,
/// because core will not start a network broker without one.
pub async fn open_broker(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
) -> impl IntoResponse {
    let Some(path) = state.homecore_config_path.as_ref() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "homeCore was started without --config, so it can't change its own settings",
        );
    };
    let text = match std::fs::read_to_string(path.as_path()) {
        Ok(t) => t,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading homecore.toml: {e}"),
            )
        }
    };
    let updated = match svc::open_broker_in_config(&text) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(path.as_path(), &updated) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("writing homecore.toml: {e}"),
        );
    }
    let e = crate::audit::entry_from_claims(&claims, "broker.opened_to_network");
    crate::audit::emit(&state, e).await;
    tracing::info!("broker set to listen on the network; takes effect at restart");
    (StatusCode::OK, Json(json!({ "restart_required": true }))).into_response()
}

// ── POST /plugin-runtimes/{id}/hello ─────────────────────────────────────────

/// A runtime checking in: what it runs on, and proof its key works.
///
/// Called when it connects and on every pass after, and by its page to test a
/// key before saving it. A runtime whose image is a different language or ABI
/// from what it was added for is refused with that reason, since every
/// artifact placed on it would arrive unusable.
pub async fn hello(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<HelloRequest>,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(
            StatusCode::NOT_FOUND,
            "plugin runtimes are turned off in homecore.toml",
        );
    }
    let Some(mut rec) = authenticate_runtime(&state, &id, &headers) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "homeCore doesn't know this key: it was reset, or the runtime was removed",
        );
    };
    if req.capabilities.kind != rec.kind || req.capabilities.abi != rec.abi {
        return err(
            StatusCode::CONFLICT,
            format!(
                "this runtime was added for {} {}, and this image hosts {} {}",
                rec.kind, rec.abi, req.capabilities.kind, req.capabilities.abi
            ),
        );
    }
    rec.arch = Some(req.capabilities.arch);
    rec.host_version = Some(req.host_version);
    rec.hostname = Some(req.hostname);
    rec.network_mode = Some(req.network_mode);
    rec.source_ip = Some(addr.ip().to_string());
    rec.last_seen_at = Some(Utc::now());
    if let Err(e) = store(&state).upsert(&rec) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    (
        StatusCode::OK,
        Json(HelloResponse {
            runtime_id: rec.runtime_id,
            name: rec.name,
            plugin_id: rec.plugin_id,
        }),
    )
        .into_response()
}

// ── Placing a plugin on a runtime ────────────────────────────────────────────

/// Record that a plugin belongs on a runtime, and seed its config.
///
/// The artifact is deliberately **not** downloaded here. Placement is a
/// statement of intent that the runtime converges on; fetching tens of megabytes
/// inside the request that made it would only mean the admin waits for a
/// download that has to happen again on the runtime anyway.
///
/// Config is seeded exactly as a binary install seeds it — same store, same
/// `[homecore]` bootstrap, same minted password — so an operator editing a
/// hosted plugin's config is editing the same file in the same place, and only
/// existing configs are preserved on a re-place.
pub fn place_on_runtime(
    state: &AppState,
    runtime_id: &str,
    plugin_id: &str,
    version: &str,
    artifact_url: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    let ctx = state
        .plugin_install
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plugin install is not configured on this server"))?;

    let config_store =
        crate::plugin_config_store::PluginConfigStore::new(ctx.config_plugins_dir.clone());
    if !config_store.exists(plugin_id) {
        let bootstrap = format!(
            "[homecore]\nbroker_host = \"{}\"\nbroker_port = {}\nplugin_id = \"{}\"\npassword = \"{}\"\n\n",
            ctx.broker_host,
            ctx.broker_port,
            plugin_id,
            svc::mint_mqtt_password(),
        );
        config_store.write(plugin_id, &bootstrap)?;
    }

    store(state).place(&hc_state::plugin_runtime_store::PlacementRecord {
        runtime_id: runtime_id.to_string(),
        plugin_id: plugin_id.to_string(),
        version: version.to_string(),
        artifact_url: artifact_url.to_string(),
        sha256: sha256.to_string(),
        config: config_store.read(plugin_id)?,
        placed_at: Utc::now(),
    })?;
    Ok(())
}

// ── GET /plugin-runtimes/placements (admin) ──────────────────────────────────

/// Every placement, for the admin UI.
///
/// Distinct from the per-runtime endpoint below, which a *runtime* calls with
/// its own key to learn what it should be running. This one answers the two
/// questions an operator asks — what does this runtime host, and where does
/// this plugin run — from a single read, so a page showing both does not make
/// a request per runtime and another per plugin.
///
/// Carries no config: that is the runtime's business and it holds a minted
/// broker credential. The admin already edits the same file through the plugin
/// config surface.
pub async fn list_all_placements(
    State(state): State<AppState>,
    _: crate::auth_middleware::PluginsRead,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let placements = match store(&state).all_placements() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };
    let body: Vec<Value> = placements
        .iter()
        .map(|p| {
            json!({
                "runtime_id": p.runtime_id,
                "plugin_id": p.plugin_id,
                "version": p.version,
                "placed_at": p.placed_at,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "placements": body }))).into_response()
}

// ── GET /plugin-runtimes/{id}/placements ─────────────────────────────────────

/// What this runtime should be running.
///
/// The desired state, in core's words. The runtime diffs it against what it
/// actually has and re-provisions the difference, which is why a lost container
/// volume costs nothing but a reinstall.
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

    // The config served is the one on disk now, not the copy taken when the
    // plugin was placed. Core owns that file and the operator edits it in the
    // UI exactly as they do a binary plugin's; serving it here is what carries
    // the edit to the runtime, which restarts the plugin because the text
    // changed. No second mechanism, and nothing to keep in sync.
    let config_store = state.plugin_install.as_ref().map(|ctx| {
        crate::plugin_config_store::PluginConfigStore::new(ctx.config_plugins_dir.clone())
    });

    let body: Vec<Value> = placements
        .iter()
        .map(|p| {
            let config = config_store
                .as_ref()
                .and_then(|s| s.read(&p.plugin_id).ok())
                .unwrap_or_else(|| p.config.clone());
            json!({
                "plugin_id": p.plugin_id,
                "version": p.version,
                "sha256": p.sha256,
                "config": config,
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
    let pv = match reg
        .version_of(&plugin_id, Some(&placement.version), state.homecore_version)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("registry lookup failed: {e:#}"),
            )
        }
    };
    let Some(arch) = rec.arch.as_deref() else {
        return err(
            StatusCode::CONFLICT,
            "this runtime has not said what it runs on yet; it does when it connects",
        );
    };
    let Some(art) = pv.artifact_for_runtime(&rec.kind, &rec.abi, arch) else {
        return err(
            StatusCode::CONFLICT,
            format!(
                "{plugin_id} {} no longer publishes a {} {} artifact for {arch}",
                placement.version, rec.kind, rec.abi
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
