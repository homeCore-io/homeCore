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
    platform_of, series_of, valid_kind, AddRuntimeRequest, ConnectionKey, HelloRequest,
    HelloResponse, IssuedKey, RuntimeSummary, UpdateRuntimeRequest,
};
use hc_state::plugin_runtime_store::{PlacedInterpreter, RuntimeRecord};
use serde_json::{json, Value};
use std::net::SocketAddr;

use crate::auth_middleware::PluginsWrite;
use crate::plugin_runtimes as svc;
use crate::AppState;

fn err(code: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

/// Whether runtimes may connect. Only what a runtime itself calls is refused
/// while it is off; an admin can still list, rename, reset and remove the
/// runtimes already added, to tidy up after turning the feature off.
fn feature_enabled(state: &AppState) -> bool {
    state.plugin_runtimes_config.enabled
}

fn store(state: &AppState) -> std::sync::Arc<hc_state::PluginRuntimeStore> {
    state.store.plugin_runtimes()
}

/// Plugins a runtime runs: everything placed on one, and each runtime's own
/// connection. A runtime runs what is placed on it, so these are on for as long
/// as they are placed; none of them is in homecore.toml, where `enabled` comes
/// from for everything else.
pub fn hosted_plugin_ids(state: &AppState) -> std::collections::HashSet<String> {
    let runtimes = store(state);
    let mut ids: std::collections::HashSet<String> = runtimes
        .all_placements()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.plugin_id)
        .collect();
    ids.extend(
        runtimes
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.plugin_id),
    );
    ids
}

/// `rec` as a client should see it: a hosted plugin reads as turned on.
pub fn as_seen(
    rec: &crate::PluginRecord,
    hosted: &std::collections::HashSet<String>,
) -> crate::PluginRecord {
    let mut rec = rec.clone();
    if hosted.contains(&rec.plugin_id) {
        rec.enabled = true;
    }
    rec
}

fn summary(rec: &RuntimeRecord) -> RuntimeSummary {
    RuntimeSummary {
        runtime_id: rec.runtime_id.clone(),
        name: rec.name.clone(),
        kind: rec.kind.clone(),
        abi: rec.abi.clone(),
        plugin_id: rec.plugin_id.clone(),
        homecore_url: rec.homecore_url.clone(),
        max_plugins: rec.max_plugins,
        created_at: rec.created_at,
        arch: rec.arch.clone(),
        host_version: rec.host_version.clone(),
        hostname: rec.hostname.clone(),
        network_mode: rec.network_mode.clone(),
        source_ip: rec.source_ip.clone(),
        page_port: rec.page_port,
        page_url: rec.page_url.clone(),
        last_seen_at: rec.last_seen_at,
        outdated: svc::is_outdated(rec.host_version.as_deref()),
        min_host_version: svc::MIN_HOST_VERSION.to_string(),
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

/// Push these clients off the broker now. See `hc_mqtt_client::take_over`.
pub(crate) async fn take_over(state: &AppState, client_ids: &[String]) {
    if let Some(cfg) = state.mqtt_takeover.as_ref() {
        hc_mqtt_client::take_over(cfg, client_ids).await;
    }
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
    verified(runtime_id, presented, &rec.key_hash).then_some(rec)
}

/// Whether `presented` is the runtime's key. A runtime checks in every few
/// seconds, and each check was a full Argon2 verify, open to anyone who
/// knows a runtime id (it is on the runtime's own page): a way to load the
/// server. A key verified once is known by its SHA-256 for as long as the
/// stored hash is unchanged (a Reset login changes it), and at most two
/// verifies run at once; past that a check is refused, not queued.
pub(crate) fn verified(runtime_id: &str, presented: &str, key_hash: &str) -> bool {
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    static KNOWN: Mutex<Option<HashMap<String, (String, [u8; 32])>>> = Mutex::new(None);
    static CHECKING: AtomicUsize = AtomicUsize::new(0);
    const CHECKS_AT_ONCE: usize = 2;

    let print: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let known = |m: &mut Option<HashMap<String, (String, [u8; 32])>>| {
        m.get_or_insert_with(HashMap::new)
            .get(runtime_id)
            .is_some_and(|(h, p)| h == key_hash && *p == print)
    };
    if known(&mut KNOWN.lock().unwrap_or_else(|e| e.into_inner())) {
        return true;
    }
    if CHECKING.fetch_add(1, Ordering::SeqCst) >= CHECKS_AT_ONCE {
        CHECKING.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    let ok = hc_auth::api_key::verify_token(presented, key_hash);
    CHECKING.fetch_sub(1, Ordering::SeqCst);
    if ok {
        KNOWN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(runtime_id.to_string(), (key_hash.to_string(), print));
    }
    ok
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
    let kind = req.kind.trim().to_lowercase();
    if !valid_kind(&kind) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the kind is the language the runtime hosts, a lowercase name such as python or node",
        );
    }
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
        // The image says its platform when it first connects (`hello`).
        abi: String::new(),
        plugin_id,
        key_hash: String::new(),
        homecore_url,
        max_plugins: req.max_plugins.filter(|n| *n > 0),
        created_at: Utc::now(),
        arch: None,
        host_version: None,
        hostname: None,
        network_mode: None,
        source_ip: None,
        page_port: None,
        page_url: None,
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

/// `GET /plugin-runtimes/kinds`: the languages a runtime can be added for.
///
/// homeCore keeps no list. A kind is offered when the signed index publishes
/// plugin builds or interpreters for it, or a runtime of that kind is already
/// added; so a new language needs a runtime image and registry entries, not a
/// homeCore release.
pub async fn list_kinds(State(state): State<AppState>, _: PluginsWrite) -> impl IntoResponse {
    let mut kinds: Vec<String> = store(&state)
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.kind)
        .collect();
    if let Some(reg) = state.registry.clone() {
        match reg.index().await {
            Ok(index) => kinds.extend(index_kinds(&index)),
            Err(e) => {
                tracing::warn!(error = %e, "runtime kinds: the registry index is unavailable")
            }
        }
    }
    kinds.sort_unstable();
    kinds.dedup();
    (StatusCode::OK, Json(json!({ "kinds": kinds }))).into_response()
}

/// The runtime kinds a signed index mentions.
pub fn index_kinds(index: &crate::registry::RegistryIndex) -> Vec<String> {
    let mut kinds: Vec<String> = index
        .interpreters
        .iter()
        .map(|i| i.kind.clone())
        .chain(
            index
                .plugins
                .iter()
                .flat_map(|p| &p.versions)
                .flat_map(|v| v.runtime_kinds())
                .map(str::to_string),
        )
        .filter(|k| valid_kind(k))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds
}

/// `PATCH /plugin-runtimes/{id}`: rename it, or set or clear its plugin limit.
///
/// Lowering the limit below what it already holds removes nothing; it only
/// stops new plugins from going there.
pub async fn update_runtime(
    State(state): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
    Json(req): Json<UpdateRuntimeRequest>,
) -> impl IntoResponse {
    let Ok(Some(mut rec)) = store(&state).get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    if let Some(name) = req.name {
        let name = name.trim();
        if name.is_empty() {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "give the runtime a name");
        }
        rec.name = name.to_string();
    }
    if let Some(max) = req.max_plugins {
        rec.max_plugins = max.filter(|n| *n > 0);
    }
    if let Err(e) = store(&state).upsert(&rec) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
    (StatusCode::OK, Json(summary(&rec))).into_response()
}

/// `POST /plugin-runtimes/{id}/reset`: a new key; the old one stops working now.
///
/// The runtime is pushed off the broker at once and refused until the new key
/// is pasted in. Its plugins keep their own logins and come back when it does.
pub async fn reset_runtime(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Ok(Some(rec)) = store(&state).get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    let username = rec.plugin_id.clone();
    match issue_key(&state, rec) {
        Ok(issued) => {
            // The old login is gone; its connection would last until it dropped.
            // Its plugins keep theirs and stay connected.
            take_over(&state, &[username]).await;
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
    // The runtime's login goes first, so that when it is pushed off below its
    // reconnect is refused.
    if let Err(e) = store.delete(&id) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
    }
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
    // Whatever an uninstall above failed to take off (or an install that ran
    // meanwhile put on) would stay placed on a runtime that no longer exists:
    // listed as on, with no way to turn it off.
    for p in store.placements_for(&id).unwrap_or_default() {
        if let Err(e) = store.unplace(&id, &p.plugin_id) {
            tracing::warn!(runtime_id = %id, plugin_id = %p.plugin_id, error = %e, "could not take a plugin off a removed runtime");
        }
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
    // Every login is gone now, but connections made before stay up until they
    // drop: push them off, then forget anything a last heartbeat re-registered
    // in between.
    let mut ids = removed_plugins.clone();
    ids.push(rec.plugin_id.clone());
    take_over(&state, &ids).await;
    {
        let mut map = state.plugins.write().await;
        for id in &ids {
            map.remove(id);
        }
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
    let edit = crate::config_writer::core_config_lock();
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
    // Atomic, like every other write of this file: a crash halfway left
    // homeCore with a config it couldn't load.
    if let Err(e) = crate::config_writer::write_atomic(path.as_path(), updated.as_bytes()) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("writing homecore.toml: {e}"),
        );
    }
    drop(edit);
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
    // An image that names an interpreter series brings its own interpreter,
    // which is how runtimes worked before interpreters came from the registry.
    // It would ignore the interpreter each plugin is placed with.
    if series_of(&req.capabilities.abi).is_some() {
        return err(
            StatusCode::CONFLICT,
            format!(
                "this runtime's image is older than homeCore {}: it brings its own interpreter ({}). \
                 Pull the latest hc-runtime-{} image and restart the container",
                state.homecore_version, req.capabilities.abi, req.capabilities.kind
            ),
        );
    }
    // A runtime's platform is learned from its first hello; after that, an
    // image on a different platform is a different runtime.
    if req.capabilities.kind != rec.kind
        || (!rec.abi.is_empty() && platform_of(&req.capabilities.abi) != platform_of(&rec.abi))
    {
        return err(
            StatusCode::CONFLICT,
            format!(
                "this runtime was added for {}, and this image hosts {} {}",
                if rec.abi.is_empty() {
                    rec.kind.clone()
                } else {
                    format!("{} {}", rec.kind, platform_of(&rec.abi))
                },
                req.capabilities.kind,
                req.capabilities.abi
            ),
        );
    }
    // A runtime added before interpreters came from the registry is stored
    // with the series it was pinned to (`cp312-manylinux_2_28`). Its image now
    // reports the platform alone, and so does its record from here on.
    rec.abi = req.capabilities.abi.clone();
    rec.arch = Some(req.capabilities.arch);
    rec.host_version = Some(req.host_version);
    rec.hostname = Some(req.hostname);
    rec.network_mode = Some(req.network_mode);
    rec.source_ip = Some(addr.ip().to_string());
    rec.page_port = req.page_port;
    rec.page_url = req
        .page_url
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
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
            min_host_version: Some(svc::MIN_HOST_VERSION.to_string()),
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
    interpreter: Option<PlacedInterpreter>,
) -> anyhow::Result<()> {
    let ctx = state
        .plugin_install
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plugin install is not configured on this server"))?;

    // An install racing the runtime's removal must not place onto it.
    if store(state).get(runtime_id)?.is_none() {
        anyhow::bail!("no runtime {runtime_id}");
    }

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

    // A plugin lives on one runtime. Installed onto another, it used to stay
    // on the first too: both ran it under one broker login and client id,
    // each pushing the other off the broker, and uninstalling removed one.
    for other in store(state).all_placements()? {
        if other.plugin_id == plugin_id && other.runtime_id != runtime_id {
            store(state).unplace(&other.runtime_id, plugin_id)?;
            nudge_runtime(state, &other.runtime_id);
        }
    }

    store(state).place(&hc_state::plugin_runtime_store::PlacementRecord {
        runtime_id: runtime_id.to_string(),
        plugin_id: plugin_id.to_string(),
        version: version.to_string(),
        artifact_url: artifact_url.to_string(),
        sha256: sha256.to_string(),
        config: config_store.read(plugin_id)?,
        placed_at: Utc::now(),
        interpreter,
    })?;
    nudge_runtime(state, runtime_id);
    Ok(())
}

/// A plugin placed on a runtime: core's own copy of its config
/// (`config/plugins/<id>.toml`, which is what the runtime is served and
/// restarts the plugin on when it changes) and the runtime it is on.
///
/// The plugin's own copy, inside the runtime, is not the config: the next
/// check-in replaces it with this one. So reading and writing a placed
/// plugin's config goes here, never through the plugin's set_config.
pub fn placed_config(state: &AppState, plugin_id: &str) -> Option<(String, String)> {
    let ctx = state.plugin_install.as_ref()?;
    let placement = store(state)
        .all_placements()
        .ok()?
        .into_iter()
        .find(|p| p.plugin_id == plugin_id)?;
    let path = crate::plugin_config_store::PluginConfigStore::new(ctx.config_plugins_dir.clone())
        .path_for(plugin_id);
    Some((path.display().to_string(), placement.runtime_id))
}

/// Ask a runtime to check in now, so a placement it has just been given (or
/// lost) is installed, updated or removed in seconds instead of on its next
/// poll.
///
/// Best effort, and nothing waits on it. A runtime older than host 0.2.2
/// answers "unknown action" and picks the change up on its next poll as
/// before; one that is offline picks it up when it is back.
pub fn nudge_runtime(state: &AppState, runtime_id: &str) {
    let Some(rpc) = state.management_rpc.clone() else {
        return;
    };
    let Ok(Some(runtime)) = store(state).get(runtime_id) else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        if let Err(e) = rpc.request(&runtime.plugin_id, "check_in", None).await {
            tracing::debug!(
                runtime = %runtime.runtime_id, error = %e,
                "the runtime did not take check_in; it will see the change on its next poll"
            );
        }
    });
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
                "interpreter": p.interpreter.as_ref().map(|i| format!("{} {}", i.kind, i.version)),
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
    let mut placements = match store.placements_for(&rec.runtime_id) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };
    fill_missing_interpreters(&state, &rec, &mut placements).await;

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
                // What the runtime makes the plugin's environment from. The
                // runtime fetches it from core, as it does the artifact.
                "interpreter": p.interpreter.as_ref().map(|i| json!({
                    "kind": i.kind,
                    "version": i.version,
                    "sha256": i.sha256,
                })),
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "placements": body }))).into_response()
}

/// Give placements made before interpreters came from the registry the
/// interpreter their build needs, and store it.
///
/// Those placements were made for a runtime whose image carried its own
/// Python, and record none. The same plugin version, matched to the newest
/// interpreter of a series it publishes for, is what that runtime now runs.
/// The index is read only when a placement is missing one, so a runtime's
/// regular poll costs nothing extra once this has happened. A placement that
/// can't be matched is left as it is, and the runtime reports why it can't
/// install it.
async fn fill_missing_interpreters(
    state: &AppState,
    rec: &RuntimeRecord,
    placements: &mut [hc_state::plugin_runtime_store::PlacementRecord],
) {
    if placements.iter().all(|p| p.interpreter.is_some()) {
        return;
    }
    let (Some(reg), Some(arch)) = (state.registry.clone(), rec.arch.as_deref()) else {
        return;
    };
    let index = match reg.index().await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(runtime_id = %rec.runtime_id, error = %format!("{e:#}"),
                "could not read the registry to choose interpreters for older placements");
            return;
        }
    };
    let platform = platform_of(&rec.abi);
    for p in placements.iter_mut().filter(|p| p.interpreter.is_none()) {
        let Some(pv) = index
            .plugin(&p.plugin_id)
            .and_then(|plugin| plugin.version(&p.version))
        else {
            continue;
        };
        let Some((art, interp)) = index.runtime_build(pv, &rec.kind, platform, arch) else {
            continue;
        };
        p.artifact_url = art.url.clone();
        p.sha256 = art.sha256.clone();
        p.interpreter = Some(PlacedInterpreter {
            kind: interp.kind.clone(),
            version: interp.version.clone(),
            abi: interp.abi.clone(),
            sha256: interp.sha256.clone(),
        });
        match store(state).place(p) {
            Ok(()) => tracing::info!(
                runtime_id = %rec.runtime_id, plugin_id = %p.plugin_id,
                interpreter = %interp.version, "chose an interpreter for an older placement"
            ),
            Err(e) => tracing::warn!(plugin_id = %p.plugin_id, error = %e,
                "could not store the interpreter chosen for a placement"),
        }
    }
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
    let (pv, index) = match reg
        .version_and_index(&plugin_id, Some(&placement.version), state.homecore_version)
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
    // The build for the series the placement's interpreter belongs to: the
    // runtime makes the environment from that interpreter, and wheels for
    // another series would not import in it.
    let platform = platform_of(&rec.abi);
    let art = match &placement.interpreter {
        Some(interp) => pv.artifacts.iter().find(|a| {
            a.runtime == rec.kind
                && a.arch == arch
                && platform_of(&a.abi) == platform
                && series_of(&a.abi) == Some(interp.abi.as_str())
        }),
        None => index
            .runtime_build(&pv, &rec.kind, platform, arch)
            .map(|(a, _)| a),
    };
    let Some(art) = art else {
        return err(
            StatusCode::CONFLICT,
            format!(
                "{plugin_id} {} no longer publishes a {} {platform} build for {arch}{}",
                placement.version,
                rec.kind,
                placement
                    .interpreter
                    .as_ref()
                    .map(|i| format!(" on {} {}", i.kind, i.version))
                    .unwrap_or_default()
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

// ── GET /plugin-runtimes/{id}/interpreters/{kind}/{version} ─────────────────

/// Serve an interpreter a placement on this runtime runs on.
///
/// The same boundary as artifacts: core re-reads and verifies the signed
/// index, downloads the interpreter and checks its sha256; the runtime checks
/// only the sha256 core hands it. Only an interpreter one of the runtime's own
/// placements names is served, so the endpoint is no way to fetch anything
/// else from the registry.
pub async fn get_placement_interpreter(
    State(state): State<AppState>,
    Path((id, kind, version)): Path<(String, String, String)>,
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
    let placed = match store(&state).placements_for(&rec.runtime_id) {
        Ok(list) => list.into_iter().any(|p| {
            p.interpreter
                .is_some_and(|i| i.kind == kind && i.version == version)
        }),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };
    if !placed {
        return err(
            StatusCode::NOT_FOUND,
            "no placement on this runtime runs on that interpreter",
        );
    }
    let Some(arch) = rec.arch.as_deref() else {
        return err(
            StatusCode::CONFLICT,
            "this runtime has not said what it runs on yet; it does when it connects",
        );
    };
    let index = match reg.index().await {
        Ok(i) => i,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("registry lookup failed: {e:#}"),
            )
        }
    };
    let Some(interp) = index.interpreter(&kind, &version, arch) else {
        return err(
            StatusCode::CONFLICT,
            format!("the registry no longer lists {kind} {version} for {arch}"),
        );
    };
    let file = match reg.download_interpreter(interp).await {
        Ok(f) => f,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("interpreter download failed: {e:#}"),
            )
        }
    };
    let bytes = match std::fs::read(file.path()) {
        Ok(b) => b,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading verified interpreter: {e}"),
            )
        }
    };
    tracing::info!(
        runtime_id = %rec.runtime_id, %kind, %version,
        bytes = bytes.len(), "served a verified interpreter to a runtime"
    );
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::HeaderName::from_static("x-hc-sha256"),
                interp.sha256.clone(),
            ),
        ],
        bytes,
    )
        .into_response()
}

// ── Interpreter updates (admin) ──────────────────────────────────────────────

/// One interpreter series in use on one runtime, and whether the index has a
/// newer patch of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InterpreterUse {
    pub runtime_id: String,
    pub kind: String,
    /// e.g. `cp313`.
    pub series: String,
    /// The versions its plugins run on now; normally one.
    pub versions: Vec<String>,
    /// The newest the index lists, when it is newer than one in use.
    pub update: Option<String>,
    /// True when the series gets no more fixes upstream.
    pub eol: bool,
    /// The plugins on this runtime that run on it, and so restart when it is
    /// updated.
    pub plugins: Vec<String>,
}

/// Group a runtime's placements by interpreter series, against the index.
pub fn interpreter_uses(
    runtime_id: &str,
    arch: Option<&str>,
    placements: &[hc_state::plugin_runtime_store::PlacementRecord],
    index: &crate::registry::RegistryIndex,
) -> Vec<InterpreterUse> {
    let mut out: Vec<InterpreterUse> = Vec::new();
    for p in placements.iter().filter(|p| p.runtime_id == runtime_id) {
        let Some(i) = &p.interpreter else { continue };
        let at = match out
            .iter()
            .position(|u| u.kind == i.kind && u.series == i.abi)
        {
            Some(at) => at,
            None => {
                out.push(InterpreterUse {
                    runtime_id: runtime_id.to_string(),
                    kind: i.kind.clone(),
                    series: i.abi.clone(),
                    versions: Vec::new(),
                    update: None,
                    eol: false,
                    plugins: Vec::new(),
                });
                out.len() - 1
            }
        };
        let u = &mut out[at];
        if !u.versions.contains(&i.version) {
            u.versions.push(i.version.clone());
        }
        u.plugins.push(p.plugin_id.clone());
    }
    for u in &mut out {
        u.versions
            .sort_by(|a, b| crate::registry::compare_versions(a, b));
        u.plugins.sort();
        let newest = arch.and_then(|arch| index.newest_interpreter(&u.kind, &u.series, arch));
        if let Some(newest) = newest {
            u.eol = newest.eol;
            let behind = u.versions.iter().any(|v| {
                crate::registry::compare_versions(&newest.version, v) == std::cmp::Ordering::Greater
            });
            if behind {
                u.update = Some(newest.version.clone());
            }
        }
    }
    out.sort_by(|a, b| (&a.kind, &a.series).cmp(&(&b.kind, &b.series)));
    out
}

/// `GET /plugin-runtimes/interpreters`: the interpreters each runtime's
/// plugins run on, and the updates waiting for them.
///
/// Updates wait here for an admin. An interpreter update restarts every plugin
/// on that series, so nothing applies one on its own.
pub async fn list_interpreters(
    State(state): State<AppState>,
    _: crate::auth_middleware::PluginsRead,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let Some(reg) = state.registry.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no plugin registry is configured",
        );
    };
    let store = store(&state);
    let (runtimes, placements) = match (store.list(), store.all_placements()) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"))
        }
    };
    let index = match reg.index().await {
        Ok(i) => i,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("registry lookup failed: {e:#}"),
            )
        }
    };
    let uses: Vec<InterpreterUse> = runtimes
        .iter()
        .flat_map(|r| interpreter_uses(&r.runtime_id, r.arch.as_deref(), &placements, &index))
        .collect();
    (StatusCode::OK, Json(json!({ "interpreters": uses }))).into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateInterpreterRequest {
    /// e.g. `python`.
    pub kind: String,
    /// e.g. `cp313`.
    pub series: String,
}

/// `POST /plugin-runtimes/{id}/interpreters/update`: move every plugin on this
/// runtime that runs on one series to its newest patch.
///
/// Each placement keeps its plugin version and config and gets the new
/// interpreter. The runtime builds new environments beside the old ones from
/// the wheels it already has, then restarts each plugin on its new one.
pub async fn update_interpreter(
    State(state): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
    Json(req): Json<UpdateInterpreterRequest>,
) -> impl IntoResponse {
    if !feature_enabled(&state) {
        return err(StatusCode::NOT_FOUND, "plugin runtimes are disabled");
    }
    let Some(reg) = state.registry.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no plugin registry is configured",
        );
    };
    let store = store(&state);
    let Ok(Some(rec)) = store.get(&id) else {
        return err(StatusCode::NOT_FOUND, "unknown runtime");
    };
    let Some(arch) = rec.arch.as_deref() else {
        return err(
            StatusCode::CONFLICT,
            "this runtime has not said what it runs on yet; it does when it connects",
        );
    };
    let index = match reg.index().await {
        Ok(i) => i,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("registry lookup failed: {e:#}"),
            )
        }
    };
    let Some(newest) = index.newest_interpreter(&req.kind, &req.series, arch) else {
        return err(
            StatusCode::NOT_FOUND,
            format!(
                "the registry lists no {} {} for {arch}",
                req.kind, req.series
            ),
        );
    };
    let placements = match store.placements_for(&id) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}")),
    };
    let mut moved = Vec::new();
    for mut p in placements {
        let Some(i) = &p.interpreter else { continue };
        if i.kind != req.kind || i.abi != req.series || i.version == newest.version {
            continue;
        }
        let from = i.version.clone();
        p.interpreter = Some(PlacedInterpreter {
            kind: newest.kind.clone(),
            version: newest.version.clone(),
            abi: newest.abi.clone(),
            sha256: newest.sha256.clone(),
        });
        if let Err(e) = store.place(&p) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"));
        }
        moved.push(json!({ "plugin_id": p.plugin_id, "from": from }));
    }
    if !moved.is_empty() {
        nudge_runtime(&state, &id);
        let e = crate::audit::entry_from_claims(&claims, "plugin_runtime.interpreter_updated")
            .with_target("plugin_runtime", id.clone());
        crate::audit::emit(&state, e).await;
        tracing::info!(
            runtime_id = %id, kind = %req.kind, version = %newest.version,
            plugins = moved.len(), "interpreter update placed"
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "runtime_id": id,
            "kind": newest.kind,
            "version": newest.version,
            "plugins": moved,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod interpreter_tests {
    /// A runtime's key is verified once and then known, until it changes.
    #[test]
    fn a_runtime_key_is_known_until_it_changes() {
        let key = hc_auth::api_key::generate().unwrap();
        let other = hc_auth::api_key::generate().unwrap();
        let id = format!("rt-test-{}", uuid::Uuid::new_v4());
        assert!(super::verified(&id, &key.full_token, &key.hash));
        assert!(
            super::verified(&id, &key.full_token, &key.hash),
            "known now"
        );
        assert!(!super::verified(&id, &other.full_token, &key.hash));
        // A Reset login: the old key no longer passes, from the cache or not.
        assert!(!super::verified(&id, &key.full_token, &other.hash));
        assert!(super::verified(&id, &other.full_token, &other.hash));
    }

    use super::*;
    use hc_state::plugin_runtime_store::PlacementRecord;

    fn placed(rt: &str, plugin: &str, python: Option<&str>) -> PlacementRecord {
        PlacementRecord {
            runtime_id: rt.into(),
            plugin_id: plugin.into(),
            version: "0.1.0".into(),
            artifact_url: String::new(),
            sha256: String::new(),
            config: String::new(),
            placed_at: Utc::now(),
            interpreter: python.map(|v| PlacedInterpreter {
                kind: "python".into(),
                version: v.into(),
                abi: format!("cp{}", v.split('.').take(2).collect::<String>()),
                sha256: String::new(),
            }),
        }
    }

    fn index(newest_313: &str, eol: bool) -> crate::registry::RegistryIndex {
        serde_json::from_value(json!({
            "interpreters": [
                {"kind": "python", "version": "3.12.16", "abi": "cp312", "os": "linux",
                 "arch": "x86_64", "url": "i", "sha256": "1", "eol": eol},
                {"kind": "python", "version": newest_313, "abi": "cp313", "os": "linux",
                 "arch": "x86_64", "url": "i", "sha256": "2"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn uses_are_grouped_by_series_and_say_what_would_restart() {
        let placements = vec![
            placed("rt-a", "plugin.b", Some("3.13.17")),
            placed("rt-a", "plugin.a", Some("3.13.17")),
            placed("rt-a", "plugin.c", Some("3.12.16")),
            placed("rt-a", "plugin.old", None),
            placed("rt-b", "plugin.d", Some("3.13.17")),
        ];
        let uses = interpreter_uses("rt-a", Some("x86_64"), &placements, &index("3.13.18", true));
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].series, "cp312");
        assert_eq!(uses[0].update, None);
        assert!(uses[0].eol);
        assert_eq!(uses[1].series, "cp313");
        assert_eq!(uses[1].versions, vec!["3.13.17"]);
        assert_eq!(uses[1].update.as_deref(), Some("3.13.18"));
        assert_eq!(uses[1].plugins, vec!["plugin.a", "plugin.b"]);
    }

    #[test]
    fn a_runtime_on_the_newest_patch_has_no_update() {
        let placements = vec![placed("rt-a", "plugin.a", Some("3.13.18"))];
        let uses = interpreter_uses(
            "rt-a",
            Some("x86_64"),
            &placements,
            &index("3.13.18", false),
        );
        assert_eq!(uses[0].update, None);
        // Before it has said what it runs on, nothing can be offered.
        let placements = vec![placed("rt-a", "plugin.a", Some("3.13.17"))];
        let uses = interpreter_uses("rt-a", None, &placements, &index("3.13.18", false));
        assert_eq!(uses[0].update, None);
    }
}

#[cfg(test)]
mod hosted_tests {
    use super::as_seen;

    fn rec(id: &str) -> crate::PluginRecord {
        serde_json::from_value(serde_json::json!({
            "plugin_id": id,
            "registered_at": "2026-11-16T00:00:00Z",
            "status": "active",
            "enabled": false,
        }))
        .expect("a minimal plugin record")
    }

    #[test]
    fn a_hosted_plugin_reads_as_turned_on_and_others_are_left_alone() {
        let hosted = ["plugin.mcp".to_string()].into_iter().collect();
        assert!(as_seen(&rec("plugin.mcp"), &hosted).enabled);
        assert!(!as_seen(&rec("plugin.hue"), &hosted).enabled);
    }
}
