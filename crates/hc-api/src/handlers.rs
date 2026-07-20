//! axum route handlers for all REST endpoints.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use hc_core::{device_naming, rule_resolver};
use hc_state::StateStore;
use hc_topic_map::canonical_device_type_name;
use hc_types::dashboard::{DashboardDefinition, DashboardResponse};
use hc_types::device::{with_command_change_metadata, Area, DeviceChange, DeviceState};
use hc_types::rule::{Action, Condition, Rule, Scene, Trigger};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

use crate::audit;
use crate::auth_middleware::{
    AreasRead, AreasWrite, AuthUser, AutomationsRead, AutomationsWrite, DashboardsRead,
    DashboardsWrite, DevicesRead, DevicesWrite, PluginsRead, PluginsWrite, ScenesRead, ScenesWrite,
};
use crate::group_store::RuleGroup;
use crate::managed_modes::{
    build_managed_rules, install_managed_rules, managed_rule_owner, remove_managed_rules,
    validate_definition,
};
use crate::mode_definition_store::{
    mode_definitions_path, CriteriaModeConfig, CriteriaOffBehavior, ModeDefinition,
    ModeDefinitionStore,
};
use crate::AppState;

const MATTER_CONTROLLER_DEVICE_ID: &str = "matter_controller";

fn mode_definition_store_for(state: &AppState) -> Option<ModeDefinitionStore> {
    state
        .modes_path
        .as_ref()
        .map(|path| ModeDefinitionStore::new(mode_definitions_path(path.as_ref())))
}

fn load_mode_definitions(state: &AppState) -> anyhow::Result<Vec<ModeDefinition>> {
    match mode_definition_store_for(state) {
        Some(store) => store.load(),
        None => Ok(Vec::new()),
    }
}

fn save_mode_definitions(state: &AppState, definitions: &[ModeDefinition]) -> anyhow::Result<()> {
    if let Some(store) = mode_definition_store_for(state) {
        store.save(definitions)?;
    }
    Ok(())
}

fn managed_rule_response(mode_id: &str, rule_id: Uuid) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": format!(
                "rule '{rule_id}' is managed by criteria-driven mode '{mode_id}' and cannot be edited directly"
            )
        })),
    )
        .into_response()
}

fn load_mode_definitions_response(
    state: &AppState,
) -> Result<Vec<ModeDefinition>, Box<axum::response::Response>> {
    load_mode_definitions(state).map_err(|e| {
        Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response(),
        )
    })
}

fn normalize_native_device_type(mut device: DeviceState) -> DeviceState {
    if device.device_type.is_none() {
        // Legacy devices that haven't been migrated yet.
        if device.plugin_id == "core.switch" {
            device.device_type = Some("switch".to_string());
        } else if device.plugin_id == "core.timer" {
            device.device_type = Some("timer".to_string());
        } else if device.device_id.starts_with("switch_") {
            device.device_type = Some("switch".to_string());
        } else if device.device_id.starts_with("timer_") {
            device.device_type = Some("timer".to_string());
        }
    } else if let Some(device_type) = device.device_type.as_deref() {
        device.device_type = Some(canonical_device_type_name(device_type));
    }
    device
}

// ---------- Health ----------

pub async fn health(State(s): State<AppState>) -> impl IntoResponse {
    Json(hc_api_types::health::HealthResponse {
        status: "ok".into(),
        // Read from AppState rather than `env!()` so this reports the
        // binary crate's version (homecore), not the hosting handler
        // crate's version (hc-api). Production `main.rs` injects via
        // `with_homecore_version`. See HEALTH-VERSION-SOURCE-1.
        version: s.homecore_version.into(),
    })
}

// ---------- System versions ----------

/// `GET /system/versions` — bill-of-materials for the running install.
///
/// **Appliance install:** reads `/opt/homecore/versions.json` written by the
/// appliance build pipeline. The file records the exact `vX.Y.Z` of every
/// bundled component (core, SDK, 10 plugins, hc-web-leptos, hc-tui) plus
/// the appliance image's own version and build timestamp. See
/// `claude-notes/plans/component_versioning.md` Phase A for the writer.
/// Path follows the existing `/opt/homecore/` convention used by
/// `Dockerfile.core` for build-time-baked files (config defaults, UI bundle,
/// profile examples). Operator-mutable state lives separately under
/// `$HOMECORE_HOME` (`/homecore`).
///
/// **Non-appliance install:** falls back to `{"core": "<CARGO_PKG_VERSION>"}`
/// — the binary's own compile-time version, same source as `/health`.
///
/// Public (no auth required). Version info is not sensitive — it's the same
/// shape `/health` already exposes, just expanded for multi-component
/// reporting. Public access also lets the login screen and pre-auth client
/// version-check (CLIENT-VER-1, planned 0.1.3) consume it.
pub async fn system_versions(State(s): State<AppState>) -> impl IntoResponse {
    const VERSIONS_PATH: &str = "/opt/homecore/versions.json";

    // Try the appliance-written file. If it exists and parses as JSON,
    // return it verbatim — the appliance build pipeline owns the schema.
    if let Ok(bytes) = std::fs::read(VERSIONS_PATH) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            return Json(json);
        }
        // File exists but isn't valid JSON — fall through to the binary's
        // own version rather than 500-ing. Operator can fix the file.
        tracing::warn!(
            path = VERSIONS_PATH,
            "versions.json present but not valid JSON; falling back to core version"
        );
    }

    // Fallback: report the binary crate's version (injected via
    // `with_homecore_version`) rather than `env!(CARGO_PKG_VERSION)`
    // which would resolve to hc-api's version. See
    // HEALTH-VERSION-SOURCE-1.
    Json(serde_json::json!({
        "core": s.homecore_version,
    }))
}

// ---------- System status ----------

// ---------- Log Level ----------

pub async fn get_log_level(State(s): State<AppState>) -> impl IntoResponse {
    match &s.log_level_handle {
        Some(handle) => {
            let level = handle.current_level();
            (StatusCode::OK, Json(json!({ "level": level }))).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "dynamic log level not available" })),
        )
            .into_response(),
    }
}

pub async fn set_log_level(
    State(s): State<AppState>,
    _: PluginsWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref handle) = s.log_level_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "dynamic log level not available" })),
        )
            .into_response();
    };
    let Some(level) = body["level"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing 'level' field" })),
        )
            .into_response();
    };
    match handle.set_level(level) {
        Ok(()) => {
            tracing::info!(level, "Log level changed via API");
            (StatusCode::OK, Json(json!({ "ok": true, "level": level }))).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

pub async fn battery_settings(State(s): State<AppState>) -> impl IntoResponse {
    let cfg = s
        .battery_config
        .as_ref()
        .map(|tx| tx.borrow().clone())
        .unwrap_or_default();
    Json(json!({
        "threshold_pct": cfg.threshold_pct,
        "recover_band_pct": cfg.recover_band_pct,
        "notify_channel": cfg.notify_channel,
        "notify_on_recovered": cfg.notify_on_recovered,
    }))
}

pub async fn system_status(State(s): State<AppState>) -> impl IntoResponse {
    let uptime_secs = (chrono::Utc::now() - s.started_at).num_seconds().max(0);

    let (rules_total, rules_enabled) = if let Some(rh) = &s.rules_handle {
        let rules = rh.read().await;
        let total = rules.len();
        let enabled = rules.iter().filter(|r| r.enabled).count();
        (total, enabled)
    } else {
        (0, 0)
    };

    let devices_total = s.store.list_devices().await.map(|d| d.len()).unwrap_or(0);

    let plugins_active = {
        let map = s.plugins.read().await;
        map.values().filter(|p| p.status == "active").count()
    };

    let (state_db_bytes, history_db_bytes) = if let Some(bp) = &s.backup_paths {
        let state_sz = std::fs::metadata(&bp.state_db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let hist_sz = std::fs::metadata(&bp.history_db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        (state_sz, hist_sz)
    } else {
        (0, 0)
    };

    Json(json!({
        // Read from AppState rather than `env!()` so this reports the
        // binary crate's version (homecore), not hc-api's. See
        // HEALTH-VERSION-SOURCE-1.
        "version":           s.homecore_version,
        "uptime_seconds":    uptime_secs,
        "started_at":        s.started_at,
        "rules_total":       rules_total,
        "rules_enabled":     rules_enabled,
        "devices_total":     devices_total,
        "plugins_active":    plugins_active,
        "state_db_bytes":    state_db_bytes,
        "history_db_bytes":  history_db_bytes,
        "timezone":          hc_time::configured_tz().to_string(),
    }))
}

// ---------- WebSocket connections (OPS-1 piece 3) ----------

/// `GET /api/v1/ws/connections` — list every live WebSocket connection
/// (events_stream + logs_stream). Admin-only. Surfaces during reconnect-
/// storm investigations to distinguish "one looping client" from "many
/// churning clients" — exactly the question that took ~30 min to answer
/// manually during the 0.1.2 deploy debugging.
///
/// Sorted newest-connection-first so an operator scrolling sees the
/// freshly-spawned ones at the top.
pub async fn list_ws_connections(
    State(s): State<AppState>,
    Extension(claims): Extension<hc_auth::Claims>,
) -> impl IntoResponse {
    if !matches!(claims.role, hc_auth::user::Role::Admin) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "admin role required"})),
        )
            .into_response();
    }
    let list = crate::ws::snapshot_connections(&s.ws_connections);
    Json(json!({
        "count": list.len(),
        "connections": list,
    }))
    .into_response()
}

// ---------- Devices ----------

#[derive(Deserialize, Default)]
pub struct DeviceListQuery {
    #[serde(default)]
    pub include_schema: bool,
    /// Optional device type filter (e.g. `media_player`).
    pub device_type: Option<String>,
    /// Maximum number of devices to return (default: all).
    pub limit: Option<usize>,
    /// Number of devices to skip before returning results (default: 0).
    pub offset: Option<usize>,
}

pub async fn list_devices(
    State(s): State<AppState>,
    _: DevicesRead,
    Query(params): Query<DeviceListQuery>,
) -> impl IntoResponse {
    let wanted_type = params
        .device_type
        .as_deref()
        .map(canonical_device_type_name);
    let all_devices = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let filtered: Vec<_> = all_devices
        .into_iter()
        .map(normalize_native_device_type)
        .filter(|device| {
            wanted_type
                .as_deref()
                .map(|wanted| device.device_type.as_deref() == Some(wanted))
                .unwrap_or(true)
        })
        .collect();

    let total = filtered.len();
    let offset = params.offset.unwrap_or(0);
    let page: Vec<_> = filtered
        .into_iter()
        .skip(offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .collect();

    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
        headers.insert("X-Total-Count", v);
    }

    if !params.include_schema {
        return (StatusCode::OK, headers, Json(json!(page))).into_response();
    }

    // Build augmented list with optional schema field.
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(page.len());
    for device in page {
        let mut entry = serde_json::to_value(&device).unwrap_or(json!({}));
        let schema = s
            .store
            .get_device_schema(&device.device_id)
            .await
            .ok()
            .flatten();
        entry["schema"] = serde_json::to_value(&schema).unwrap_or(serde_json::Value::Null);
        out.push(entry);
    }
    (StatusCode::OK, headers, Json(json!(out))).into_response()
}

pub async fn get_device(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.get_device(&id).await {
        Ok(Some(device)) => (
            StatusCode::OK,
            Json(json!(normalize_native_device_type(device))),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "device not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// Attribute keys a plugin may publish artwork under, in preference order.
///
/// `art_url` is the canonical media-contract name; `media_image_url` is what
/// hc-sonos has published all along. Both are accepted so the plugin needs no
/// change and new plugins get a name that reads right.
const ART_URL_KEYS: [&str; 2] = ["art_url", "media_image_url"];

/// Artwork is fetched from devices on the LAN, which is fast; a stuck speaker
/// must not tie up a connection.
const ART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Album art is a few hundred KB at most. The cap stops a misbehaving or
/// impersonated device from streaming us something enormous.
const ART_MAX_BYTES: usize = 8 * 1024 * 1024;

/// `GET /devices/:id/media/art` — proxies a media player's artwork.
///
/// Plugins publish artwork as an absolute URL pointing at the device itself
/// (hc-sonos gives `http://10.0.10.28:1400/getaa?...`, the speaker's own LAN
/// address). A browser usually cannot load that: it is unreachable from outside
/// the LAN, and on an HTTPS page it is blocked outright as mixed content. So the
/// art is published but unusable by the very frontends it exists for.
///
/// Core fetches it server-side and streams it back same-origin, which settles
/// mixed content, remote access and CORS in one move.
///
/// **Why this is not an SSRF gadget:** the URL is read from *the device's own
/// state*, never from the caller. A client can only ask us to fetch a URL that a
/// plugin already published for that device. The scheme check below is a
/// belt-and-braces guard against a compromised plugin publishing `file://` or
/// similar.
pub async fn device_media_art(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> axum::response::Response {
    let err = |code: StatusCode, msg: &str| -> axum::response::Response {
        (code, Json(json!({ "error": msg }))).into_response()
    };

    let device = match s.store.get_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "device not found"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let Some(url) = ART_URL_KEYS
        .iter()
        .find_map(|k| device.attributes.get(*k).and_then(Value::as_str))
        .filter(|u| !u.is_empty())
    else {
        // Nothing playing, or a track with no artwork. Not an error — the UI
        // simply falls back to a placeholder.
        return err(StatusCode::NOT_FOUND, "no artwork for this device");
    };

    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "device published an unusable art url",
            )
        }
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return err(StatusCode::BAD_GATEWAY, "art url scheme not allowed");
    }

    let client = match reqwest::Client::builder().timeout(ART_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let upstream = match client.get(parsed).send().await {
        Ok(r) => r,
        Err(_) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "could not reach the device for artwork",
            )
        }
    };
    if !upstream.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, "device returned no artwork");
    }

    // Trust the upstream's type only if it actually claims to be an image;
    // otherwise a compromised device could have us serve HTML or a script under
    // our own origin.
    let content_type = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|t| t.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::BAD_GATEWAY, "artwork download failed"),
    };
    if bytes.len() > ART_MAX_BYTES {
        return err(StatusCode::BAD_GATEWAY, "artwork too large");
    }

    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    // Art changes with the track, so this must not be cached for long — but a
    // wall panel re-rendering the now-playing card should not refetch every
    // frame either.
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=60"),
    );
    // Never let a proxied blob be sniffed into something executable.
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );

    (StatusCode::OK, headers, bytes).into_response()
}

pub async fn get_device_schema(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.get_device_schema(&id).await {
        Ok(Some(schema)) => (StatusCode::OK, Json(json!(schema))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "schema not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn command_device(
    State(s): State<AppState>,
    DevicesWrite(claims): DevicesWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let change = DeviceChange::homecore("api")
        .with_actor(Some(claims.uid), Some(claims.sub))
        .with_correlation_id(Some(Uuid::new_v4().to_string()));
    if let Err(e) = publish_device_command(&s, &id, body, change).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        );
    }

    (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" })))
}

async fn publish_device_command(
    s: &AppState,
    device_id: &str,
    body: Value,
    change: DeviceChange,
) -> anyhow::Result<()> {
    let topic = format!("homecore/devices/{device_id}/cmd");
    let body = with_command_change_metadata(body, &change);
    let payload = serde_json::to_vec(&body)?;

    if let Some(ph) = &s.publish {
        ph.publish(&topic, payload.clone()).await?;
    }

    let ev = hc_types::event::Event::MqttMessage {
        timestamp: chrono::Utc::now(),
        topic,
        payload,
        retain: false,
    };
    let _ = s.event_bus.publish(ev);

    Ok(())
}

pub async fn update_device(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    match s.store.get_device(&id).await {
        Ok(Some(mut device)) => {
            let all_devices = match s.store.list_devices().await {
                Ok(devices) => devices,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            };

            // `name`/`area` name the *user's* intent, so they set the override —
            // never the plugin-delivered value, which registration overwrites on
            // every restart (a rename written to `device.name` silently reverted).
            // Null, or an empty string, clears the override and hands the device
            // back to whatever the bridge calls it.
            if let Some(name) = body.get("name") {
                if name.is_null() {
                    device.name_override = None;
                } else if let Some(value) = name.as_str() {
                    let trimmed = value.trim();
                    device.name_override = if trimmed.is_empty() || trimmed == device.name {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                } else {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": "name must be a string or null" })),
                    )
                        .into_response();
                }
            }
            if let Some(status_icon) = body.get("status_icon") {
                if status_icon.is_null() {
                    device.status_icon = None;
                } else if let Some(value) = status_icon.as_str() {
                    let trimmed = value.trim();
                    device.status_icon = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                } else {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": "status_icon must be a string or null" })),
                    )
                        .into_response();
                }
            }
            if let Some(area) = body.get("area") {
                let requested = if area.is_null() {
                    None
                } else {
                    area.as_str()
                        .map(normalize_area_name)
                        .filter(|value| !value.is_empty())
                };
                // Same contract as `name`: setting the area the plugin already
                // reports is not an override, it is agreement — so don't pin it.
                device.area_override = match requested {
                    Some(a) if Some(a.as_str()) == device.area.as_deref() => None,
                    other => other,
                };
            }
            if let Some(ui_hint) = body.get("ui_hint") {
                if ui_hint.is_null() {
                    device.ui_hint = None;
                } else if let Some(value) = ui_hint.as_str() {
                    let trimmed = value.trim();
                    device.ui_hint = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                }
            }
            if let Some(canonical_name) = body.get("canonical_name") {
                if canonical_name.is_null() {
                    device.canonical_name = None;
                } else if let Some(value) = canonical_name.as_str() {
                    match device_naming::validate_or_generate_canonical_name(
                        &device,
                        &all_devices,
                        Some(value),
                    ) {
                        Ok(name) => device.canonical_name = Some(name),
                        Err(e) => {
                            return (
                                StatusCode::UNPROCESSABLE_ENTITY,
                                Json(json!({ "error": e.to_string() })),
                            )
                                .into_response();
                        }
                    }
                } else {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": "canonical_name must be a string or null" })),
                    )
                        .into_response();
                }
            }

            if device.canonical_name.is_none() {
                match device_naming::validate_or_generate_canonical_name(
                    &device,
                    &all_devices,
                    None,
                ) {
                    Ok(name) => device.canonical_name = Some(name),
                    Err(e) => {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(json!({ "error": e.to_string() })),
                        )
                            .into_response();
                    }
                }
            }
            match s.store.upsert_device(&device).await {
                Ok(_) => (StatusCode::OK, Json(json!(device))),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                ),
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "device not found" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
    .into_response()
}

pub async fn delete_device(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let devices_before_delete = match s.store.list_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match s.store.delete_device(&id).await {
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "device not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Ok(true) => {}
    }

    // Nullify references to this device in all rule files, then return a summary.
    let affected_rules = if let Some(rfs) = &s.rule_file_store {
        match crate::rule_file_store::nullify_device_refs(&rfs.dir, &id, &devices_before_delete) {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!(device_id = %id, error = %e, "delete_device: failed to nullify rule refs");
                vec![]
            }
        }
    } else {
        vec![]
    };

    (
        StatusCode::OK,
        Json(json!({
            "deleted": true,
            "affected_rules": affected_rules,
        })),
    )
        .into_response()
}

/// `PATCH /api/v1/devices`
///
/// Bulk update device metadata.  Currently supports bulk area assignment.
///
/// Body: `{ "ids": ["device_id_1", ...], "area": "living_room" }`
///
/// - `ids` — required, list of device IDs to update.
/// - `area` — set the area for all listed devices. Pass `null` to clear.
///
/// Returns `{ "updated": N, "not_found": ["id", ...] }`.
pub async fn bulk_patch_devices(
    State(s): State<AppState>,
    _: DevicesWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let ids: Vec<String> = match body.get("ids").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "ids array required" })),
            )
                .into_response();
        }
    };

    let new_area: Option<Option<String>> = if body.get("area").is_some() {
        Some(match body["area"].as_str() {
            Some(a) => Some(a.to_string()),
            None if body["area"].is_null() => None,
            _ => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": "area must be a string or null" })),
                )
                    .into_response();
            }
        })
    } else {
        None // no area key — nothing to do yet (future: other bulk fields)
    };

    let mut updated = 0usize;
    let mut not_found: Vec<String> = Vec::new();

    for id in &ids {
        match s.store.get_device(id).await {
            Ok(Some(mut device)) => {
                if let Some(ref area) = new_area {
                    device.area = area.clone();
                }
                if let Err(e) = s.store.upsert_device(&device).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
                updated += 1;
            }
            Ok(None) => not_found.push(id.clone()),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "updated": updated, "not_found": not_found })),
    )
        .into_response()
}

/// `DELETE /api/v1/devices`
///
/// Bulk delete devices.
///
/// Body: `{ "ids": ["device_id_1", ...] }`
///
/// Each device is deleted and rule file references are nullified (same as single DELETE).
/// Returns `{ "deleted": N, "not_found": ["id", ...], "affected_rules": ["rule name", ...] }`.
pub async fn bulk_delete_devices(
    State(s): State<AppState>,
    DevicesWrite(claims): DevicesWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let ids: Vec<String> = match body.get("ids").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "ids array required" })),
            )
                .into_response();
        }
    };

    let mut deleted = 0usize;
    let mut not_found: Vec<String> = Vec::new();
    let mut affected_rules: Vec<String> = Vec::new();
    let devices_before_delete = match s.store.list_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    for id in &ids {
        match s.store.delete_device(id).await {
            Ok(false) => not_found.push(id.clone()),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
            Ok(true) => {
                deleted += 1;
                if let Some(rfs) = &s.rule_file_store {
                    match crate::rule_file_store::nullify_device_refs(
                        &rfs.dir,
                        id,
                        &devices_before_delete,
                    ) {
                        Ok(names) => {
                            for name in names {
                                if !affected_rules.contains(&name) {
                                    affected_rules.push(name);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(device_id = %id, error = %e, "bulk_delete_devices: failed to nullify rule refs")
                        }
                    }
                }
            }
        }
    }

    if deleted > 0 {
        let mut audit_e = audit::entry_from_claims(&claims, "device.bulk_deleted")
            .with_target("device", format!("{deleted} device(s)"));
        audit_e.detail = json!({
            "requested":      ids.len(),
            "deleted":        deleted,
            "not_found":      not_found.len(),
            "ids":            ids,
            "affected_rules": affected_rules,
        });
        audit::emit(&s, audit_e).await;
    }

    (
        StatusCode::OK,
        Json(json!({
            "deleted": deleted,
            "not_found": not_found,
            "affected_rules": affected_rules,
        })),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    /// Start of time window (RFC-3339 / ISO-8601 UTC). Defaults to 24 hours ago.
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    /// End of time window (RFC-3339 / ISO-8601 UTC). Defaults to now.
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    /// Filter to a single attribute name (e.g. `?attribute=on`).
    pub attribute: Option<String>,
    /// Maximum number of entries returned (default 500, max 5 000).
    pub limit: Option<u32>,
}

pub async fn device_history(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
    Query(params): Query<HistoryQuery>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();
    let from = params
        .from
        .unwrap_or_else(|| now - chrono::Duration::hours(24));
    let to = params.to.unwrap_or(now);
    let limit = params.limit.unwrap_or(500).min(5_000);

    match s
        .store
        .query_history(&id, from, to, params.attribute.as_deref(), limit)
        .await
    {
        Ok(entries) => (
            StatusCode::OK,
            Json(json!(entries
                .iter()
                .map(|e| json!({
                    "attribute":   e.attribute,
                    "value":       e.value,
                    "recorded_at": e.recorded_at,
                }))
                .collect::<Vec<_>>())),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// ---------- Timers ----------

#[derive(Deserialize)]
pub struct CreateTimerBody {
    /// Slug used to form the device_id: "garage_close" → "timer_garage_close".
    pub id: String,
    pub label: Option<String>,
}

pub async fn create_timer(
    State(s): State<AppState>,
    _: DevicesWrite,
    Json(body): Json<CreateTimerBody>,
) -> impl IntoResponse {
    // Enforce the timer_ prefix convention.
    let device_id = if body.id.starts_with("timer_") {
        body.id.clone()
    } else {
        format!("timer_{}", body.id)
    };

    if let Ok(Some(_)) = s.store.get_device(&device_id).await {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "timer already exists" })),
        )
            .into_response();
    }

    let display_name = body.label.as_deref().unwrap_or(&device_id).to_string();
    let mut dev = hc_types::device::DeviceState::new(&device_id, &display_name, "core.glue");
    dev.device_type = Some("timer".to_string());
    dev.available = true;
    dev.attributes.insert("state".into(), json!("idle"));
    dev.attributes.insert("duration_secs".into(), json!(0_u64));
    dev.attributes.insert("remaining_secs".into(), json!(0_u64));
    dev.attributes.insert("repeat".into(), json!(false));

    match s.store.upsert_device(&dev).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(dev))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_timers(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => {
            let timers: Vec<_> = devices
                .into_iter()
                .filter(|d| {
                    d.plugin_id == "core.timer"
                        || (d.plugin_id == "core.glue" && d.device_type.as_deref() == Some("timer"))
                })
                .map(normalize_native_device_type)
                .map(compute_timer_remaining)
                .collect();
            (StatusCode::OK, Json(json!(timers)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn get_timer(
    State(s): State<AppState>,
    Path(id): Path<String>,
    _: DevicesRead,
) -> impl IntoResponse {
    let device_id = if id.starts_with("timer_") {
        id.clone()
    } else {
        format!("timer_{id}")
    };
    match s.store.get_device(&device_id).await {
        Ok(Some(dev)) => (
            StatusCode::OK,
            Json(json!(compute_timer_remaining(
                normalize_native_device_type(dev)
            ))),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "timer not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// For a running timer, recompute `remaining_secs` from `started_at` + `duration_secs`
/// so callers always see an accurate countdown without requiring periodic store writes.
fn compute_timer_remaining(
    mut dev: hc_types::device::DeviceState,
) -> hc_types::device::DeviceState {
    let is_running = dev.attributes.get("state").and_then(Value::as_str) == Some("running");
    if !is_running {
        return dev;
    }
    let duration_secs = dev
        .attributes
        .get("duration_secs")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let started_at = dev
        .attributes
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    if let Some(started) = started_at {
        let elapsed = (chrono::Utc::now() - started).num_seconds().max(0) as u64;
        let remaining = duration_secs.saturating_sub(elapsed);
        dev.attributes
            .insert("remaining_secs".into(), json!(remaining));
    }
    dev
}

// ---------- Switches ----------

#[derive(Deserialize)]
pub struct CreateSwitchBody {
    /// Slug used to form the device_id: "vacation_mode" → "switch_vacation_mode".
    pub id: String,
    pub label: Option<String>,
}

pub async fn create_switch(
    State(s): State<AppState>,
    _: DevicesWrite,
    Json(body): Json<CreateSwitchBody>,
) -> impl IntoResponse {
    // Enforce the switch_ prefix convention.
    let device_id = if body.id.starts_with("switch_") {
        body.id.clone()
    } else {
        format!("switch_{}", body.id)
    };

    if let Ok(Some(_)) = s.store.get_device(&device_id).await {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "switch already exists" })),
        )
            .into_response();
    }

    let display_name = body.label.as_deref().unwrap_or(&device_id).to_string();
    let mut dev = hc_types::device::DeviceState::new(&device_id, &display_name, "core.glue");
    // Must be "switch", not "virtual_switch": that is what `GLUE_TYPES` writes,
    // what the `core.switch` → `core.glue` migration backfills, and what
    // `list_switches` filters on.  Writing "virtual_switch" here created
    // switches that GET /switches could never return.
    dev.device_type = Some("switch".to_string());
    dev.available = true;
    dev.attributes.insert("on".into(), json!(false));

    match s.store.upsert_device(&dev).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(dev))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_switches(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => {
            let switches: Vec<_> = devices
                .into_iter()
                .filter(|d| {
                    d.plugin_id == "core.switch"
                        || (d.plugin_id == "core.glue"
                            && d.device_type.as_deref() == Some("switch"))
                })
                .map(normalize_native_device_type)
                .collect();
            (StatusCode::OK, Json(json!(switches)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// ---------- Glue Devices ----------

/// Glue device type prefixes and their default attributes.
const GLUE_TYPES: &[(&str, &str, &str)] = &[
    ("switch", "switch_", "switch"),
    ("timer", "timer_", "timer"),
    ("counter", "counter_", "counter"),
    ("number", "number_", "number"),
    ("select", "select_", "select"),
    ("text", "text_", "text"),
    ("button", "button_", "button"),
    ("datetime", "datetime_", "datetime"),
    ("group", "group_", "group"),
    ("threshold", "threshold_", "threshold"),
    ("schedule", "schedule_", "schedule"),
];

#[derive(Debug, Deserialize)]
pub struct CreateGlueBody {
    /// Device type: "counter", "number", "select", "text", "button", "datetime", "group", "threshold", "schedule".
    #[serde(rename = "type")]
    pub glue_type: String,
    /// Device ID slug (prefix auto-added if missing).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Type-specific initial attributes (step, min, max, options, members, etc.).
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/v1/glue` — create a new glue device.
pub async fn create_glue(
    State(s): State<AppState>,
    _: DevicesWrite,
    Json(body): Json<CreateGlueBody>,
) -> impl IntoResponse {
    let type_info = match GLUE_TYPES.iter().find(|(t, _, _)| *t == body.glue_type) {
        Some(info) => info,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown glue type: {}", body.glue_type) })),
            )
                .into_response()
        }
    };
    let (_, prefix, device_type) = type_info;

    let device_id = if body.id.starts_with(prefix) {
        body.id.clone()
    } else {
        format!("{prefix}{}", body.id)
    };

    if let Ok(Some(_)) = s.store.get_device(&device_id).await {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "device already exists" })),
        )
            .into_response();
    }

    let mut dev = hc_types::device::DeviceState::new(&device_id, &body.name, "core.glue");
    dev.device_type = Some(device_type.to_string());
    dev.available = true;

    // Set type-specific default attributes.
    match body.glue_type.as_str() {
        "switch" => {
            dev.attributes.insert("on".into(), json!(false));
        }
        "timer" => {
            dev.attributes.insert("state".into(), json!("idle"));
            dev.attributes.insert("duration_secs".into(), json!(0_u64));
            dev.attributes.insert("remaining_secs".into(), json!(0_u64));
            dev.attributes.insert("repeat".into(), json!(false));
        }
        "counter" => {
            dev.attributes.insert("count".into(), json!(0));
            dev.attributes.insert(
                "step".into(),
                body.config.get("step").cloned().unwrap_or(json!(1)),
            );
            if let Some(v) = body.config.get("min") {
                dev.attributes.insert("min".into(), v.clone());
            }
            if let Some(v) = body.config.get("max") {
                dev.attributes.insert("max".into(), v.clone());
            }
        }
        "number" => {
            dev.attributes.insert(
                "value".into(),
                body.config.get("value").cloned().unwrap_or(json!(0.0)),
            );
            dev.attributes.insert(
                "min".into(),
                body.config.get("min").cloned().unwrap_or(json!(0.0)),
            );
            dev.attributes.insert(
                "max".into(),
                body.config.get("max").cloned().unwrap_or(json!(100.0)),
            );
            dev.attributes.insert(
                "step".into(),
                body.config.get("step").cloned().unwrap_or(json!(1.0)),
            );
            if let Some(v) = body.config.get("unit") {
                dev.attributes.insert("unit".into(), v.clone());
            }
        }
        "select" => {
            let options = body.config.get("options").cloned().unwrap_or(json!([]));
            let first = options
                .as_array()
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(json!(""));
            dev.attributes.insert("selected".into(), first);
            dev.attributes.insert("options".into(), options);
        }
        "text" => {
            dev.attributes.insert("value".into(), json!(""));
            if let Some(v) = body.config.get("max_length") {
                dev.attributes.insert("max_length".into(), v.clone());
            }
        }
        "button" => {
            dev.attributes.insert("last_pressed".into(), json!(null));
        }
        "datetime" => {
            dev.attributes.insert("value".into(), json!(""));
            dev.attributes.insert(
                "has_date".into(),
                body.config.get("has_date").cloned().unwrap_or(json!(true)),
            );
            dev.attributes.insert(
                "has_time".into(),
                body.config.get("has_time").cloned().unwrap_or(json!(true)),
            );
        }
        "group" => {
            dev.attributes.insert("on".into(), json!(false));
            dev.attributes.insert(
                "member_ids".into(),
                body.config.get("members").cloned().unwrap_or(json!([])),
            );
            dev.attributes.insert(
                "attribute".into(),
                body.config.get("attribute").cloned().unwrap_or(json!("on")),
            );
            dev.attributes.insert(
                "mode".into(),
                body.config.get("mode").cloned().unwrap_or(json!("any")),
            );
            dev.attributes.insert("active_count".into(), json!(0));
            dev.attributes.insert("member_count".into(), json!(0));
        }
        "threshold" => {
            dev.attributes.insert("above".into(), json!(false));
            dev.attributes.insert(
                "source_device_id".into(),
                body.config
                    .get("source_device_id")
                    .cloned()
                    .unwrap_or(json!("")),
            );
            dev.attributes.insert(
                "source_attribute".into(),
                body.config
                    .get("source_attribute")
                    .cloned()
                    .unwrap_or(json!("value")),
            );
            dev.attributes.insert(
                "threshold".into(),
                body.config.get("threshold").cloned().unwrap_or(json!(0.0)),
            );
            dev.attributes.insert(
                "hysteresis".into(),
                body.config.get("hysteresis").cloned().unwrap_or(json!(0.0)),
            );
        }
        "schedule" => {
            dev.attributes.insert("active".into(), json!(false));
            dev.attributes.insert(
                "blocks".into(),
                body.config.get("blocks").cloned().unwrap_or(json!([])),
            );
        }
        _ => {}
    }

    match s.store.upsert_device(&dev).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(dev))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/v1/glue` — list all glue devices.
pub async fn list_glue(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => {
            let glue: Vec<_> = devices
                .into_iter()
                .filter(|d| {
                    d.plugin_id == "core.glue"
                        || d.plugin_id == "core.timer"
                        || d.plugin_id == "core.switch"
                })
                .map(compute_timer_remaining)
                .collect();
            (StatusCode::OK, Json(json!(glue)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// `DELETE /api/v1/glue/:id` — delete a glue device.
pub async fn delete_glue(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.delete_device(&id).await {
        Ok(true) => (StatusCode::OK, Json(json!({ "deleted": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "device not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------- Modes ----------

/// `GET /api/v1/modes` — list all mode configs + live device state.
pub async fn list_modes(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "modes not configured" })),
            );
        }
    };
    let configs = match hc_core::mode_manager::load_modes(&path) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            );
        }
    };
    let devices = s.store.list_devices().await.unwrap_or_default();
    let definitions = match load_mode_definitions(&s) {
        Ok(definitions) => definitions,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            );
        }
    };
    let result: Vec<Value> = configs
        .into_iter()
        .map(|cfg| {
            let state = devices.iter().find(|d| d.device_id == cfg.id);
            let definition = definitions.iter().find(|def| def.mode_id == cfg.id);
            json!({ "config": cfg, "state": state, "definition": definition })
        })
        .collect();
    (StatusCode::OK, Json(json!(result)))
}

/// `GET /api/v1/modes/:id` — single mode config + live state.
pub async fn get_mode(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "modes not configured" })),
            )
                .into_response();
        }
    };
    let configs = match hc_core::mode_manager::load_modes(&path) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let definitions = match load_mode_definitions(&s) {
        Ok(definitions) => definitions,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    match configs.into_iter().find(|c| c.id == id) {
        Some(cfg) => {
            let state = s.store.get_device(&id).await.ok().flatten();
            let definition = definitions.iter().find(|def| def.mode_id == id);
            (
                StatusCode::OK,
                Json(json!({ "config": cfg, "state": state, "definition": definition })),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "mode not found" })),
        )
            .into_response(),
    }
}

fn default_criteria_reevaluate_minutes() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct PutModeDefinitionBody {
    pub on_condition: Condition,
    #[serde(default)]
    pub off_behavior: CriteriaOffBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_condition: Option<Condition>,
    #[serde(default = "default_criteria_reevaluate_minutes")]
    pub reevaluate_every_n_minutes: u32,
}

#[derive(Deserialize)]
pub struct CreateModeBody {
    pub id: String,
    pub name: String,
    pub kind: hc_core::mode_manager::ModeKind,
    #[serde(default)]
    pub criteria_definition: Option<PutModeDefinitionBody>,
}

/// `POST /api/v1/modes` — create a new mode (appends to modes.toml).
pub async fn create_mode(
    State(s): State<AppState>,
    DevicesWrite(claims): DevicesWrite,
    Json(body): Json<CreateModeBody>,
) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "modes not configured" })),
            )
                .into_response();
        }
    };
    if !body.id.starts_with("mode_") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "id must start with 'mode_'" })),
        )
            .into_response();
    }
    if body.criteria_definition.is_some() && !claims.has_scope("automations:write") {
        return (
            StatusCode::FORBIDDEN,
            Json(
                json!({ "error": "scope 'automations:write' required for criteria-driven modes" }),
            ),
        )
            .into_response();
    }
    let cfg = hc_core::mode_manager::ModeConfig {
        id: body.id,
        name: body.name,
        kind: body.kind,
        on_event: None,
        off_event: None,
        on_offset_minutes: 0,
        off_offset_minutes: 0,
    };
    match hc_core::mode_manager::append_mode(&path, cfg.clone()) {
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }

    if let Some(criteria) = body.criteria_definition {
        let mut definitions = match load_mode_definitions(&s) {
            Ok(definitions) => definitions,
            Err(e) => {
                let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        let mut definition = ModeDefinition {
            mode_id: cfg.id.clone(),
            criteria: CriteriaModeConfig {
                on_condition: criteria.on_condition,
                off_behavior: criteria.off_behavior,
                off_condition: criteria.off_condition,
                reevaluate_every_n_minutes: criteria.reevaluate_every_n_minutes,
            },
            generated_rule_ids: Vec::new(),
        };
        let mode_ids = match hc_core::mode_manager::load_modes(&path) {
            Ok(modes) => modes
                .into_iter()
                .map(|mode| mode.id)
                .collect::<HashSet<_>>(),
            Err(e) => {
                let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        if let Err(e) = validate_definition(&cfg, &mode_ids, &definitions, &definition) {
            let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        let rules = match build_managed_rules(&cfg, &definition) {
            Ok(rules) => rules,
            Err(e) => {
                let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        let generated_rule_ids = match install_managed_rules(&s, &[], &rules).await {
            Ok(ids) => ids,
            Err(e) => {
                let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        definition.generated_rule_ids = generated_rule_ids.clone();
        definitions.retain(|def| def.mode_id != definition.mode_id);
        definitions.push(definition);
        if let Err(e) = save_mode_definitions(&s, &definitions) {
            let _ = remove_managed_rules(&s, &generated_rule_ids).await;
            let _ = hc_core::mode_manager::remove_mode(&path, &cfg.id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    (StatusCode::CREATED, Json(json!(cfg))).into_response()
}

/// `GET /api/v1/modes/:id/definition` — criteria definition for a managed mode.
pub async fn get_mode_definition(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let definitions = match load_mode_definitions_response(&s) {
        Ok(definitions) => definitions,
        Err(resp) => return *resp,
    };
    match definitions
        .into_iter()
        .find(|definition| definition.mode_id == id)
    {
        Some(definition) => (StatusCode::OK, Json(json!(definition))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "mode definition not found" })),
        )
            .into_response(),
    }
}

/// `PUT /api/v1/modes/:id/definition` — create or replace a criteria-driven mode definition.
pub async fn put_mode_definition(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<String>,
    Json(body): Json<PutModeDefinitionBody>,
) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "modes not configured" })),
            )
                .into_response();
        }
    };

    let modes = match hc_core::mode_manager::load_modes(&path) {
        Ok(modes) => modes,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let Some(mode) = modes.iter().find(|mode| mode.id == id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "mode not found" })),
        )
            .into_response();
    };

    let mut definitions = match load_mode_definitions_response(&s) {
        Ok(definitions) => definitions,
        Err(resp) => return *resp,
    };
    let previous_rule_ids = definitions
        .iter()
        .find(|definition| definition.mode_id == id)
        .map(|definition| definition.generated_rule_ids.clone())
        .unwrap_or_default();

    let mut definition = ModeDefinition {
        mode_id: id.clone(),
        criteria: CriteriaModeConfig {
            on_condition: body.on_condition,
            off_behavior: body.off_behavior,
            off_condition: body.off_condition,
            reevaluate_every_n_minutes: body.reevaluate_every_n_minutes,
        },
        generated_rule_ids: previous_rule_ids.clone(),
    };
    let mode_ids = modes
        .into_iter()
        .map(|mode| mode.id)
        .collect::<HashSet<_>>();
    if let Err(e) = validate_definition(&mode, &mode_ids, &definitions, &definition) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let rules = match build_managed_rules(&mode, &definition) {
        Ok(rules) => rules,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let generated_rule_ids = match install_managed_rules(&s, &previous_rule_ids, &rules).await {
        Ok(ids) => ids,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    definition.generated_rule_ids = generated_rule_ids;

    let created = !definitions.iter().any(|existing| existing.mode_id == id);
    definitions.retain(|existing| existing.mode_id != id);
    definitions.push(definition.clone());
    if let Err(e) = save_mode_definitions(&s, &definitions) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    (
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!(definition)),
    )
        .into_response()
}

/// `DELETE /api/v1/modes/:id/definition` — remove criteria definition and generated rules.
pub async fn delete_mode_definition(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut definitions = match load_mode_definitions_response(&s) {
        Ok(definitions) => definitions,
        Err(resp) => return *resp,
    };
    let Some(pos) = definitions
        .iter()
        .position(|definition| definition.mode_id == id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "mode definition not found" })),
        )
            .into_response();
    };
    let definition = definitions.remove(pos);
    if let Err(e) = remove_managed_rules(&s, &definition.generated_rule_ids).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    if let Err(e) = save_mode_definitions(&s, &definitions) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /api/v1/modes/:id` — remove a mode.
/// Rejects built-in solar modes with 400.
pub async fn delete_mode(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if matches!(
        id.as_str(),
        hc_core::mode_manager::MODE_NIGHT_ID | hc_core::mode_manager::MODE_DAY_ID
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("{id} is a built-in mode and cannot be deleted")
            })),
        )
            .into_response();
    }
    let mut definitions = match load_mode_definitions_response(&s) {
        Ok(definitions) => definitions,
        Err(resp) => return *resp,
    };
    if let Some(pos) = definitions
        .iter()
        .position(|definition| definition.mode_id == id)
    {
        let definition = definitions.remove(pos);
        if let Err(e) = remove_managed_rules(&s, &definition.generated_rule_ids).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        if let Err(e) = save_mode_definitions(&s, &definitions) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "modes not configured" })),
            )
                .into_response();
        }
    };
    if let Err(e) = hc_core::mode_manager::remove_mode(&path, &id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    if let Err(e) = s.store.delete_device(&id).await {
        tracing::warn!(mode_id = %id, error = %e, "delete_mode: failed to remove device from store");
    }
    (StatusCode::NO_CONTENT, Json(json!({}))).into_response()
}

// ---------- Areas ----------

fn normalize_area_name(name: &str) -> String {
    device_naming::normalize_name_segment(name)
}

fn area_id_from_name(name: &str) -> Uuid {
    let normalized = normalize_area_name(name);
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("homecore:area:{}", normalized).as_bytes(),
    )
}

/// Does this (raw, possibly un-normalized) area value name the given area?
/// Area names are normalized on the way in, but device-side values arrive from
/// plugins in whatever shape the bridge uses ("Living Room"), so every
/// comparison has to normalize.
fn area_matches(value: Option<&str>, area_name: &str) -> bool {
    value.map(normalize_area_name).as_deref() == Some(area_name)
}

fn derive_areas_from_devices(devices: &[DeviceState]) -> Vec<Area> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for device in devices {
        // Effective, not raw: a device the user has assigned to a room carries
        // that room in `area_override` with `area` left as whatever the plugin
        // said (often nothing). Reading `area` alone made every user
        // assignment invisible here — the room existed and the device pointed
        // at it, but the area listed no devices, so "add to room" looked like
        // it had done nothing.
        let Some(area) = device.effective_area() else {
            continue;
        };
        let normalized = normalize_area_name(area);
        if normalized.is_empty() {
            continue;
        }
        grouped
            .entry(normalized)
            .or_default()
            .push(device.device_id.clone());
    }

    grouped
        .into_iter()
        .map(|(name, device_ids)| Area {
            id: area_id_from_name(&name),
            name,
            device_ids,
        })
        .collect()
}

fn merge_declared_and_derived_areas(declared: Vec<Area>, devices: &[DeviceState]) -> Vec<Area> {
    let mut merged: HashMap<Uuid, Area> = derive_areas_from_devices(devices)
        .into_iter()
        .map(|area| (area.id, area))
        .collect();

    for mut area in declared {
        let name = normalize_area_name(&area.name);
        if name.is_empty() {
            continue;
        }

        area.id = area_id_from_name(&name);
        area.name = name;
        area.device_ids = merged
            .remove(&area.id)
            .map(|existing| existing.device_ids)
            .unwrap_or_default();
        merged.insert(area.id, area);
    }

    let mut areas: Vec<Area> = merged.into_values().collect();
    areas.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    areas
}

async fn list_area_state(store: &StateStore) -> Result<Vec<Area>, String> {
    let devices = store.list_devices().await.map_err(|e| e.to_string())?;
    let declared = store.list_areas().await.map_err(|e| e.to_string())?;
    Ok(merge_declared_and_derived_areas(declared, &devices))
}

async fn find_area_by_id(store: &StateStore, id: Uuid) -> Result<Option<Area>, String> {
    Ok(list_area_state(store)
        .await?
        .into_iter()
        .find(|a| a.id == id))
}

pub async fn list_areas(State(s): State<AppState>, _: AreasRead) -> impl IntoResponse {
    match list_area_state(&s.store).await {
        Ok(areas) => (StatusCode::OK, Json(json!(areas))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

#[derive(Deserialize)]
pub struct CreateAreaBody {
    pub name: String,
}

pub async fn create_area(
    State(s): State<AppState>,
    _: AreasWrite,
    Json(body): Json<CreateAreaBody>,
) -> impl IntoResponse {
    let name = normalize_area_name(&body.name);
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "area name cannot be empty" })),
        );
    }

    let area = Area {
        id: area_id_from_name(&name),
        name,
        device_ids: vec![],
    };

    match s.store.upsert_area(&area).await {
        Ok(()) => (StatusCode::CREATED, Json(json!(area))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
pub struct PatchAreaBody {
    pub name: String,
}

pub async fn patch_area(
    State(s): State<AppState>,
    _: AreasWrite,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchAreaBody>,
) -> impl IntoResponse {
    let new_name = normalize_area_name(&body.name);
    if new_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "area name cannot be empty" })),
        )
            .into_response();
    }

    let area = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "area not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    let new_id = area_id_from_name(&new_name);
    if new_id != id {
        match find_area_by_id(&s.store, new_id).await {
            Ok(Some(_)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "area name already exists" })),
                )
                    .into_response();
            }
            Ok(None) => {}
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e })),
                )
                    .into_response();
            }
        }
    }

    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    for device in &mut devices {
        // Rename whichever field points at this area. A device assigned by the
        // user carries the room in `area_override`; matching only `area` left
        // those devices pointing at a room name that no longer exists.
        let in_plugin_area = area_matches(device.area.as_deref(), &area.name);
        let in_override = area_matches(device.area_override.as_deref(), &area.name);
        if in_plugin_area || in_override {
            if in_plugin_area {
                device.area = Some(new_name.clone());
            }
            if in_override {
                device.area_override = Some(new_name.clone());
            }
            if let Err(e) = s.store.upsert_device(device).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }

    let updated_area = Area {
        id: new_id,
        name: new_name.clone(),
        device_ids: vec![],
    };
    if let Err(e) = s.store.upsert_area(&updated_area).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    if id != new_id {
        let _ = s.store.delete_area(id).await;
    }

    (
        StatusCode::OK,
        Json(json!(match find_area_by_id(&s.store, new_id).await {
            Ok(Some(area)) => area,
            _ => updated_area,
        })),
    )
        .into_response()
}

pub async fn delete_area(
    State(s): State<AppState>,
    _: AreasWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let area = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "area not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    for device in &mut devices {
        // Same reasoning as rename: clear whichever field pointed at the
        // deleted area, or user-assigned devices keep a dangling room.
        let in_plugin_area = area_matches(device.area.as_deref(), &area.name);
        let in_override = area_matches(device.area_override.as_deref(), &area.name);
        if in_plugin_area || in_override {
            if in_plugin_area {
                device.area = None;
            }
            if in_override {
                device.area_override = None;
            }
            if let Err(e) = s.store.upsert_device(device).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }

    let _ = s.store.delete_area(id).await;

    StatusCode::NO_CONTENT.into_response()
}

// ---------- Automations (Rules) ----------
//
// HomeCore keeps two in-memory rule views:
// - `source_rules_handle`: the authored rule form from disk/API payloads
// - `rules_handle`: compiled rules with device references resolved to device IDs
// API reads should prefer the source handle so canonical names round-trip
// cleanly; the engine executes the compiled handle.

#[derive(Deserialize, Default)]
pub struct AutomationListQuery {
    /// Filter by tag — only rules that have this tag.
    pub tag: Option<String>,
    /// Filter by trigger type (snake_case variant name, e.g. `device_state_changed`).
    pub trigger: Option<String>,
    /// Filter to rules that reference this device_id in their trigger, conditions, or actions.
    pub device_id: Option<String>,
    /// When `true`, return only rules that have an `error` field set (broken / stale rules).
    pub stale: Option<bool>,
    /// Sort order.  Currently only `"priority"` (descending) is supported and is also
    /// the default — this field is accepted for API forward-compatibility but is a no-op.
    #[serde(default)]
    pub sort: Option<String>,
    /// Maximum number of automations to return (default: all).
    pub limit: Option<usize>,
    /// Number of automations to skip before returning results (default: 0).
    pub offset: Option<usize>,
}

pub async fn list_automations(
    State(s): State<AppState>,
    _: AutomationsRead,
    Query(params): Query<AutomationListQuery>,
) -> impl IntoResponse {
    let devices = match s.store.list_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match &s.source_rules_handle {
        Some(rh) => {
            let rules = rh.read().await;
            let filtered: Vec<_> = rules
                .iter()
                .filter(|r| {
                    if let Some(ref tag) = params.tag {
                        if !r.tags.contains(tag) {
                            return false;
                        }
                    }
                    if let Some(ref trig) = params.trigger {
                        if trigger_type_name(&r.trigger) != trig.as_str() {
                            return false;
                        }
                    }
                    if let Some(ref did) = params.device_id {
                        if !rule_references_device(r, did, &devices) {
                            return false;
                        }
                    }
                    if params.stale == Some(true) && r.error.is_none() {
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();

            let total = filtered.len();
            let offset = params.offset.unwrap_or(0);
            let page: Vec<_> = filtered
                .into_iter()
                .skip(offset)
                .take(params.limit.unwrap_or(usize::MAX))
                .collect();

            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
                headers.insert("X-Total-Count", v);
            }
            (StatusCode::OK, headers, Json(json!(page))).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            HeaderMap::new(),
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response(),
    }
}

/// Snake-case name of a `Trigger` variant — matches the serde `type` field value.
fn trigger_type_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChanged { .. } => "device_state_changed",
        Trigger::MqttMessage { .. } => "mqtt_message",
        Trigger::TimeOfDay { .. } => "time_of_day",
        Trigger::SunEvent { .. } => "sun_event",
        Trigger::WebhookReceived { .. } => "webhook_received",
        Trigger::ManualTrigger => "manual_trigger",
        Trigger::CustomEvent { .. } => "custom_event",
        Trigger::SystemStarted => "system_started",
        Trigger::Cron { .. } => "cron",
        Trigger::DeviceAvailabilityChanged { .. } => "device_availability_changed",
        Trigger::ButtonEvent { .. } => "button_event",
        Trigger::NumericThreshold { .. } => "numeric_threshold",
        Trigger::Periodic { .. } => "periodic",
        Trigger::HubVariableChanged { .. } => "hub_variable_changed",
        Trigger::CalendarEvent { .. } => "calendar_event",
        Trigger::ModeChanged { .. } => "mode_changed",
        Trigger::DeviceBatteryLow { .. } => "device_battery_low",
        Trigger::DeviceBatteryRecovered { .. } => "device_battery_recovered",
    }
}

/// Returns `true` if `device_id` appears anywhere in the rule's trigger,
/// conditions, or actions (including nested action groups).
fn rule_references_device(rule: &Rule, device_id: &str, devices: &[DeviceState]) -> bool {
    let in_trigger = match &rule.trigger {
        Trigger::DeviceStateChanged {
            device_id: d,
            device_ids,
            ..
        } => {
            rule_resolver::reference_points_to_device(d, device_id, devices)
                || device_ids
                    .iter()
                    .any(|d| rule_resolver::reference_points_to_device(d, device_id, devices))
        }
        Trigger::DeviceAvailabilityChanged { device_id: d, .. }
        | Trigger::ButtonEvent { device_id: d, .. }
        | Trigger::NumericThreshold { device_id: d, .. } => {
            rule_resolver::reference_points_to_device(d, device_id, devices)
        }
        _ => false,
    };
    if in_trigger {
        return true;
    }

    for cond in &rule.conditions {
        if condition_references_device(cond, device_id, devices) {
            return true;
        }
    }

    rule.actions
        .iter()
        .any(|ra| actions_reference_device(std::slice::from_ref(&ra.action), device_id, devices))
}

fn condition_references_device(cond: &Condition, device_id: &str, devices: &[DeviceState]) -> bool {
    match cond {
        Condition::DeviceState { device_id: d, .. }
        | Condition::TimeElapsed { device_id: d, .. }
        | Condition::DeviceLastChange { device_id: d, .. } => {
            rule_resolver::reference_points_to_device(d, device_id, devices)
        }
        Condition::Not { condition } => condition_references_device(condition, device_id, devices),
        _ => false,
    }
}

fn actions_reference_device(actions: &[Action], device_id: &str, devices: &[DeviceState]) -> bool {
    for action in actions {
        let found = match action {
            Action::SetDeviceState { device_id: d, .. }
            | Action::SetDeviceStatePerMode { device_id: d, .. }
            | Action::FadeDevice { device_id: d, .. } => {
                rule_resolver::reference_points_to_device(d, device_id, devices)
            }
            Action::CaptureDeviceState { device_ids, .. } => device_ids
                .iter()
                .any(|d| rule_resolver::reference_points_to_device(d, device_id, devices)),
            Action::WaitForEvent {
                device_id: Some(d), ..
            } => rule_resolver::reference_points_to_device(d, device_id, devices),
            Action::Parallel { actions: inner } => {
                actions_reference_device(inner, device_id, devices)
            }
            Action::RepeatUntil { actions: inner, .. } => {
                actions_reference_device(inner, device_id, devices)
            }
            Action::RepeatWhile { actions: inner, .. } => {
                actions_reference_device(inner, device_id, devices)
            }
            Action::RepeatCount { actions: inner, .. } => {
                actions_reference_device(inner, device_id, devices)
            }
            Action::Conditional {
                then_actions,
                else_if,
                else_actions,
                ..
            } => {
                actions_reference_device(then_actions, device_id, devices)
                    || else_if
                        .iter()
                        .any(|branch| actions_reference_device(&branch.actions, device_id, devices))
                    || actions_reference_device(else_actions, device_id, devices)
            }
            Action::PingHost {
                then_actions,
                else_actions,
                ..
            } => {
                actions_reference_device(then_actions, device_id, devices)
                    || actions_reference_device(else_actions, device_id, devices)
            }
            _ => false,
        };
        if found {
            return true;
        }
    }
    false
}

pub async fn create_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Server is always authoritative on ID for new rules.  Inject a fresh
    // UUID unconditionally — clients may omit `id` or send any value.
    let new_id = Uuid::new_v4();
    body["id"] = serde_json::Value::String(new_id.to_string());

    let rule: Rule = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!("invalid rule body: {e}")
                })),
            )
                .into_response();
        }
    };

    // Validate priority is within practical range.
    if rule.priority < -1000 || rule.priority > 1000 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "priority must be between -1000 and 1000"
            })),
        )
            .into_response();
    }

    // id already set above; assert it round-tripped correctly.
    debug_assert_eq!(rule.id, new_id);

    let compiled_rule = match rule_resolver::compile_rule_for_store(&s.store, &rule).await {
        Ok(rule) => rule,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Write file first — if this fails the in-memory state is unchanged.
    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule(&rule) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Update live engine handle immediately (don't wait for watcher).
    if let Some(rh) = &s.source_rules_handle {
        rh.write().await.push(rule.clone());
    }
    if let Some(rh) = &s.rules_handle {
        rh.write().await.push(compiled_rule);
    }

    (StatusCode::CREATED, Json(json!(rule))).into_response()
}

pub async fn get_automation(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };
    let rules = rh.read().await;
    match rules.iter().find(|r| r.id == id).cloned() {
        Some(rule) => (StatusCode::OK, Json(json!(rule))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "rule not found" })),
        )
            .into_response(),
    }
}

/// `GET /automations/:id/ron` — return the rule's on-disk RON file
/// content as `text/plain`. Surfaces the literal authoring artifact
/// (preserving field order and any embedded comments) to read-only
/// clients like `hc-tui` that want to display rules without
/// re-serializing through JSON. Auto-generated rules backed only by
/// the in-memory store (no `.ron` file) return 404.
/// The rule vocabulary — every variant, every field — derived from the types.
///
/// A rule editor needs to know what a rule can contain, and every client so far
/// has kept that table BY HAND. A hand-written mirror of a Rust enum always
/// cracks: core grew `HouseStatusHero`, shipped it on its own default dashboard,
/// and the Dart client's mirror had never heard of it — so it coerced the card
/// to `markdown` and would have saved it back as one.
///
/// The tripwire meant to catch that was hand-written too: the client asserted
/// its OWN table had 18 triggers in it. That measures the mirror, not the thing
/// being mirrored, and it passes happily while core grows a 19th.
///
/// So core now says what it knows, mechanically, and a client can check itself
/// against the real thing. No auth beyond read: it is a description of the
/// software, not of the house.
pub async fn get_rule_vocabulary(_: State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    Json(hc_types::vocabulary::Vocabulary::derive())
}

pub async fn get_automation_ron(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(ref store) = s.rule_file_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({ "error": "rule file store not available" }).to_string(),
        )
            .into_response();
    };

    let path = match store.find_file(id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({ "error": "rule has no .ron file backing" }).to_string(),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json!({ "error": e.to_string() }).to_string(),
            )
                .into_response();
        }
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({ "error": format!("read {}: {e}", path.display()) }).to_string(),
        )
            .into_response(),
    }
}

pub async fn update_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    match load_mode_definitions(&s) {
        Ok(definitions) => {
            if let Some(mode_id) = managed_rule_owner(&definitions, id) {
                return managed_rule_response(mode_id, id);
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Path id is authoritative — inject it so clients don't need to send it.
    body["id"] = serde_json::Value::String(id.to_string());

    let mut rule: Rule = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!("invalid rule body: {e}")
                })),
            )
                .into_response();
        }
    };

    // Validate priority is within practical range.
    if rule.priority < -1000 || rule.priority > 1000 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "priority must be between -1000 and 1000"
            })),
        )
            .into_response();
    }

    rule.id = id;

    let compiled_rule = match rule_resolver::compile_rule_for_store(&s.store, &rule).await {
        Ok(rule) => rule,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let Some(source_rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    // Check existence and get old name for potential rename.
    let old_name = {
        let rules = source_rh.read().await;
        match rules.iter().find(|r| r.id == id) {
            Some(r) => r.name.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "rule not found" })),
                )
                    .into_response();
            }
        }
    };

    // Write file — handles rename (deletes old slug file if name changed).
    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule_renamed(&rule, &old_name) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Update live handle.
    {
        let mut rules = source_rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = rule.clone();
        } else {
            rules.push(rule.clone());
        }
    }
    if let Some(rh) = &s.rules_handle {
        let mut rules = rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = compiled_rule;
        } else {
            rules.push(compiled_rule);
        }
    }

    (StatusCode::OK, Json(json!(rule))).into_response()
}

pub async fn delete_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match load_mode_definitions(&s) {
        Ok(definitions) => {
            if let Some(mode_id) = managed_rule_owner(&definitions, id) {
                return managed_rule_response(mode_id, id);
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    let Some(rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    // Verify existence before touching the filesystem.
    {
        let rules = rh.read().await;
        if !rules.iter().any(|r| r.id == id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "rule not found" })),
            )
                .into_response();
        }
    }

    // Delete the file.
    if let Some(fs) = &s.rule_file_store {
        match fs.delete_rule(id) {
            Ok(false) => {
                // File not found on disk — could have been manually deleted.
                // Still remove from live handle below.
                tracing::warn!(%id, "Rule file not found on disk during delete — removing from memory only");
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
            Ok(true) => {}
        }
    }

    // Remove from live handle.
    rh.write().await.retain(|r| r.id != id);
    if let Some(compiled_rh) = &s.rules_handle {
        compiled_rh.write().await.retain(|r| r.id != id);
    }

    StatusCode::NO_CONTENT.into_response()
}

// ---------- Dashboards ----------

/// Everyone who can log in can SEE every dashboard.
///
/// This replaces a `visibility` field (private | shared | public) that gated
/// exactly this filter. Access control for a house is not a CMS problem — the
/// answer to "who may look at the kitchen dashboard" is "the people who live
/// here", and they all have accounts. Who may CHANGE one is a real question, and
/// [`dashboard_mutable_by`] still answers it: the owner, or an admin.
fn dashboard_visible_to(_claims: &hc_auth::Claims, _dashboard: &DashboardDefinition) -> bool {
    true
}

fn dashboard_mutable_by(claims: &hc_auth::Claims, dashboard: &DashboardDefinition) -> bool {
    claims.is_admin() || dashboard.owner_user_id == claims.uid
}

fn dashboard_response_for(
    dashboard: &DashboardDefinition,
    default_id: Option<&str>,
) -> DashboardResponse {
    DashboardResponse {
        dashboard: dashboard.clone(),
        is_default: default_id == Some(dashboard.id.as_str()),
    }
}

fn dashboard_copy_name(base: &str, existing_names: &HashSet<String>) -> String {
    if !existing_names.contains(&format!("{base} Copy")) {
        return format!("{base} Copy");
    }
    for index in 2..1000 {
        let candidate = format!("{base} Copy {index}");
        if !existing_names.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base} Copy {}", Uuid::new_v4().simple())
}

fn default_dashboard_layout(
    placements: &[(&str, i32, i32, i32, i32)],
) -> Vec<hc_types::dashboard::DashboardLayout> {
    use hc_types::dashboard::{DashboardBreakpoint, DashboardLayout, DashboardWidgetPlacement};

    let build = |breakpoint, columns, row_height, gap| DashboardLayout {
        breakpoint,
        columns,
        row_height,
        gap,
        placements: placements
            .iter()
            .enumerate()
            .map(
                |(index, (widget_id, x, y, w, h))| DashboardWidgetPlacement {
                    widget_id: (*widget_id).to_string(),
                    x: if matches!(breakpoint, DashboardBreakpoint::Mobile) {
                        0
                    } else {
                        *x
                    },
                    y: if matches!(breakpoint, DashboardBreakpoint::Mobile) {
                        index as i32
                    } else {
                        *y
                    },
                    w: if matches!(breakpoint, DashboardBreakpoint::Mobile) {
                        1
                    } else {
                        *w
                    },
                    h: *h,
                },
            )
            .collect(),
    };

    vec![
        build(DashboardBreakpoint::Mobile, 1, 150.0, 12.0),
        build(DashboardBreakpoint::Tablet, 12, 150.0, 12.0),
        build(DashboardBreakpoint::Desktop, 12, 150.0, 12.0),
        build(DashboardBreakpoint::Tv, 12, 180.0, 16.0),
    ]
}

fn dashboard_templates_for(owner_user_id: &str) -> Vec<DashboardDefinition> {
    use hc_types::dashboard::{DashboardBreakpoint, DashboardDefinition, DashboardWidget};

    let now = chrono::Utc::now();
    let widget = |id: &str, r#type: &str, title: &str, subtitle: Option<&str>, config: Value| {
        DashboardWidget {
            id: id.to_string(),
            r#type: r#type.to_string(),
            title: title.to_string(),
            subtitle: subtitle.map(str::to_string),
            config,
        }
    };

    vec![
        DashboardDefinition {
            id: "starter_getting_started".to_string(),
            name: "Getting Started".to_string(),
            description: Some("Your home at a glance.".to_string()),
            owner_user_id: owner_user_id.to_string(),
            tags: vec!["starter".into(), "home".into(), "overview".into()],
            // Not "home": that renders the same house glyph as the Home nav
            // item. "rocket" reads as onboarding and stays distinct.
            icon: "rocket".into(),
            created_at: now,
            updated_at: now,
            widgets: vec![
                widget("hero", "house_status_hero", "Home", None, json!({})),
                widget(
                    "media",
                    "media_player",
                    "Now Playing",
                    None,
                    json!({"selection_mode": "query", "query": "", "show_offline": true, "limit": 4}),
                ),
                widget(
                    "devices",
                    "device_grid",
                    "Devices",
                    None,
                    json!({"selection_mode": "query", "query": "", "show_offline": true, "limit": 12}),
                ),
                widget("log", "event_feed", "Activity", None, json!({"limit": 16})),
                widget("modes", "mode_chips", "Modes", None, json!({})),
            ],
            layouts: vec![
                hc_types::dashboard::DashboardLayout {
                    breakpoint: DashboardBreakpoint::Mobile,
                    columns: 1,
                    row_height: 100.0,
                    gap: 12.0,
                    placements: vec![
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "hero".into(),
                            x: 0,
                            y: 0,
                            w: 1,
                            h: 2,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "media".into(),
                            x: 0,
                            y: 2,
                            w: 1,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "devices".into(),
                            x: 0,
                            y: 5,
                            w: 1,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "log".into(),
                            x: 0,
                            y: 8,
                            w: 1,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "modes".into(),
                            x: 0,
                            y: 11,
                            w: 1,
                            h: 2,
                        },
                    ],
                },
                hc_types::dashboard::DashboardLayout {
                    breakpoint: DashboardBreakpoint::Tablet,
                    columns: 12,
                    row_height: 100.0,
                    gap: 12.0,
                    placements: vec![
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "hero".into(),
                            x: 0,
                            y: 0,
                            w: 12,
                            h: 2,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "media".into(),
                            x: 0,
                            y: 2,
                            w: 5,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "devices".into(),
                            x: 5,
                            y: 2,
                            w: 7,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "log".into(),
                            x: 0,
                            y: 5,
                            w: 8,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "modes".into(),
                            x: 8,
                            y: 5,
                            w: 4,
                            h: 2,
                        },
                    ],
                },
                hc_types::dashboard::DashboardLayout {
                    breakpoint: DashboardBreakpoint::Desktop,
                    columns: 12,
                    row_height: 100.0,
                    gap: 12.0,
                    placements: vec![
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "hero".into(),
                            x: 0,
                            y: 0,
                            w: 12,
                            h: 2,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "media".into(),
                            x: 0,
                            y: 2,
                            w: 4,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "devices".into(),
                            x: 4,
                            y: 2,
                            w: 8,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "log".into(),
                            x: 0,
                            y: 5,
                            w: 8,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "modes".into(),
                            x: 8,
                            y: 5,
                            w: 4,
                            h: 2,
                        },
                    ],
                },
                hc_types::dashboard::DashboardLayout {
                    breakpoint: DashboardBreakpoint::Tv,
                    columns: 12,
                    row_height: 120.0,
                    gap: 16.0,
                    placements: vec![
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "hero".into(),
                            x: 0,
                            y: 0,
                            w: 12,
                            h: 2,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "media".into(),
                            x: 0,
                            y: 2,
                            w: 4,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "devices".into(),
                            x: 4,
                            y: 2,
                            w: 8,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "log".into(),
                            x: 0,
                            y: 5,
                            w: 8,
                            h: 3,
                        },
                        hc_types::dashboard::DashboardWidgetPlacement {
                            widget_id: "modes".into(),
                            x: 8,
                            y: 5,
                            w: 4,
                            h: 2,
                        },
                    ],
                },
            ],
        },
        DashboardDefinition {
            id: "template_security".to_string(),
            name: "Security".to_string(),
            description: Some("Entry points, alerts, and camera placeholders.".to_string()),
            owner_user_id: owner_user_id.to_string(),
            tags: vec!["security".into()],
            icon: "shield".into(),
            created_at: now,
            updated_at: now,
            widgets: vec![
                widget(
                    "summary",
                    "stat_summary",
                    "Security Summary",
                    None,
                    json!({"metrics": ["doors_open", "motion_active", "offline"]}),
                ),
                widget(
                    "devices",
                    "device_list",
                    "Security Devices",
                    None,
                    json!({"selection_mode": "query", "query": "door,motion,lock,camera", "show_offline": true, "limit": 16}),
                ),
                widget(
                    "events",
                    "event_feed",
                    "Alerts",
                    None,
                    json!({"limit": 12, "types": ["device_state_changed", "system_alert"], "group_by": "device"}),
                ),
                widget(
                    "notes",
                    "markdown",
                    "Camera Setup",
                    None,
                    json!({"markdown": "Add camera widgets after configuring approved sources and embed policy."}),
                ),
            ],
            layouts: default_dashboard_layout(&[
                ("summary", 0, 0, 12, 2),
                ("devices", 0, 2, 7, 3),
                ("events", 7, 2, 5, 3),
                ("notes", 0, 5, 12, 1),
            ]),
        },
        DashboardDefinition {
            id: "template_living_room".to_string(),
            name: "Living Room".to_string(),
            description: Some("A room-focused dashboard with devices and media.".to_string()),
            owner_user_id: owner_user_id.to_string(),
            tags: vec!["room".into(), "living_room".into()],
            icon: "chair".into(),
            created_at: now,
            updated_at: now,
            widgets: vec![
                widget(
                    "devices",
                    "device_grid",
                    "Living Room Devices",
                    None,
                    json!({"selection_mode": "area", "area_name": "Living Room", "show_offline": false, "limit": 8}),
                ),
                widget(
                    "media",
                    "media_player",
                    "Media",
                    None,
                    json!({"selection_mode": "query", "query": "living", "show_offline": false, "limit": 2}),
                ),
                widget("scenes", "scene_row", "Scenes", None, json!({})),
                widget(
                    "events",
                    "event_feed",
                    "Room Activity",
                    None,
                    json!({"limit": 8, "area_name": "Living Room"}),
                ),
            ],
            layouts: default_dashboard_layout(&[
                ("devices", 0, 0, 7, 3),
                ("media", 7, 0, 5, 3),
                ("scenes", 0, 3, 12, 1),
                ("events", 0, 4, 12, 2),
            ]),
        },
    ]
}

fn find_dashboard_template(template_id: &str, owner_user_id: &str) -> Option<DashboardDefinition> {
    dashboard_templates_for(owner_user_id)
        .into_iter()
        .find(|template| template.id == template_id)
}

/// Auto-populates `layouts` from `widgets` when the caller submitted an
/// empty layouts array. Stacks each widget full-width (12 cols, h=3) per
/// breakpoint so a freshly-created dashboard renders sensibly without the
/// client needing to know the per-breakpoint placement schema.
///
/// No-op when layouts are already set or when there are no widgets.
fn ensure_default_layouts(dashboard: &mut DashboardDefinition) {
    if !dashboard.layouts.is_empty() || dashboard.widgets.is_empty() {
        return;
    }
    let placements: Vec<(&str, i32, i32, i32, i32)> = dashboard
        .widgets
        .iter()
        .enumerate()
        .map(|(i, w)| (w.id.as_str(), 0, (i as i32) * 3, 12, 3))
        .collect();
    dashboard.layouts = default_dashboard_layout(&placements);
}

fn validate_dashboard(dashboard: &DashboardDefinition) -> Result<(), String> {
    if dashboard.id.trim().is_empty() {
        return Err("dashboard id cannot be empty".into());
    }
    if dashboard.name.trim().is_empty() {
        return Err("dashboard name cannot be empty".into());
    }
    if dashboard.icon.trim().is_empty() {
        return Err("dashboard icon cannot be empty".into());
    }
    if dashboard.layouts.is_empty() {
        return Err("dashboard must define at least one layout".into());
    }

    let mut widget_ids = HashSet::new();
    for widget in &dashboard.widgets {
        if widget.id.trim().is_empty() {
            return Err("widget id cannot be empty".into());
        }
        if widget.title.trim().is_empty() {
            return Err(format!("widget '{}' title cannot be empty", widget.id));
        }
        if !widget_ids.insert(widget.id.as_str()) {
            return Err(format!("duplicate widget id '{}'", widget.id));
        }
        validate_widget_config(widget)?;
    }

    let widget_id_set: HashSet<&str> = dashboard.widgets.iter().map(|w| w.id.as_str()).collect();
    let mut breakpoints = HashSet::new();
    for layout in &dashboard.layouts {
        if layout.columns <= 0 {
            return Err(format!(
                "layout {:?} must have columns > 0",
                layout.breakpoint
            ));
        }
        if layout.row_height <= 0.0 {
            return Err(format!(
                "layout {:?} must have row_height > 0",
                layout.breakpoint
            ));
        }
        if layout.gap < 0.0 {
            return Err(format!("layout {:?} must have gap >= 0", layout.breakpoint));
        }
        if !breakpoints.insert(layout.breakpoint) {
            return Err(format!(
                "duplicate layout breakpoint '{:?}'",
                layout.breakpoint
            ));
        }
        let mut layout_widget_ids = HashSet::new();
        for placement in &layout.placements {
            if !widget_id_set.contains(placement.widget_id.as_str()) {
                return Err(format!(
                    "layout {:?} references unknown widget '{}'",
                    layout.breakpoint, placement.widget_id
                ));
            }
            if !layout_widget_ids.insert(placement.widget_id.as_str()) {
                return Err(format!(
                    "layout {:?} has duplicate placement for widget '{}'",
                    layout.breakpoint, placement.widget_id
                ));
            }
            if placement.x < 0 || placement.y < 0 {
                return Err(format!(
                    "layout {:?} placement '{}' must have non-negative x/y",
                    layout.breakpoint, placement.widget_id
                ));
            }
            if placement.w <= 0 || placement.h <= 0 {
                return Err(format!(
                    "layout {:?} placement '{}' must have w/h > 0",
                    layout.breakpoint, placement.widget_id
                ));
            }
            if placement.x + placement.w > layout.columns {
                return Err(format!(
                    "layout {:?} placement '{}' exceeds column count {}",
                    layout.breakpoint, placement.widget_id, layout.columns
                ));
            }
        }
    }

    Ok(())
}

fn config_object(
    widget: &hc_types::dashboard::DashboardWidget,
) -> Result<&serde_json::Map<String, Value>, String> {
    widget
        .config
        .as_object()
        .ok_or_else(|| format!("widget '{}' config must be an object", widget.id))
}

fn require_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
    widget_id: &str,
) -> Result<String, String> {
    match map.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        _ => Err(format!(
            "widget '{}' requires non-empty string '{}'",
            widget_id, key
        )),
    }
}

fn optional_string_list(
    map: &serde_json::Map<String, Value>,
    key: &str,
    widget_id: &str,
) -> Result<(), String> {
    if let Some(value) = map.get(key) {
        let Some(items) = value.as_array() else {
            return Err(format!(
                "widget '{}' field '{}' must be a string array",
                widget_id, key
            ));
        };
        if items.iter().any(|item| item.as_str().is_none()) {
            return Err(format!(
                "widget '{}' field '{}' must be a string array",
                widget_id, key
            ));
        }
    }
    Ok(())
}

fn optional_bool(
    map: &serde_json::Map<String, Value>,
    key: &str,
    widget_id: &str,
) -> Result<(), String> {
    if let Some(value) = map.get(key) {
        if !value.is_boolean() {
            return Err(format!(
                "widget '{}' field '{}' must be a boolean",
                widget_id, key
            ));
        }
    }
    Ok(())
}

fn optional_i64_min(
    map: &serde_json::Map<String, Value>,
    key: &str,
    min: i64,
    widget_id: &str,
) -> Result<(), String> {
    if let Some(value) = map.get(key) {
        match value.as_i64() {
            Some(v) if v >= min => {}
            _ => {
                return Err(format!(
                    "widget '{}' field '{}' must be an integer >= {}",
                    widget_id, key, min
                ));
            }
        }
    }
    Ok(())
}

fn validate_selection_widget_config(
    widget: &hc_types::dashboard::DashboardWidget,
    require_limit: bool,
) -> Result<(), String> {
    let map = config_object(widget)?;
    let selection_mode = require_string(map, "selection_mode", &widget.id)?;
    match selection_mode.as_str() {
        "manual" => optional_string_list(map, "device_ids", &widget.id)?,
        "area" => {
            require_string(map, "area_name", &widget.id)?;
        }
        "query" => {
            if let Some(value) = map.get("query") {
                if value.as_str().is_none() {
                    return Err(format!(
                        "widget '{}' field 'query' must be a string",
                        widget.id
                    ));
                }
            }
        }
        _ => {
            return Err(format!(
                "widget '{}' has unsupported selection_mode '{}'",
                widget.id, selection_mode
            ));
        }
    }
    optional_i64_min(map, "limit", 1, &widget.id)?;
    if require_limit && !map.contains_key("limit") {
        return Err(format!(
            "widget '{}' requires integer field 'limit'",
            widget.id
        ));
    }
    optional_bool(map, "show_offline", &widget.id)?;
    Ok(())
}

/// Validates the config of the widget types core happens to know about.
///
/// An UNKNOWN type is accepted, not rejected. That is the point of `type` being
/// a string: the client's registry decides what can be drawn, and core stays out
/// of the business of knowing which cards exist. Rejecting unknown types is what
/// forced every new card — including plugin cards — through a core release.
fn validate_widget_config(widget: &hc_types::dashboard::DashboardWidget) -> Result<(), String> {
    match widget.r#type.as_str() {
        "device_grid" | "device_list" | "device_tile" | "media_player" => {
            validate_selection_widget_config(widget, false)
        }
        "stat_summary" => {
            let map = config_object(widget)?;
            let metrics = map
                .get("metrics")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("widget '{}' requires string array 'metrics'", widget.id))?;
            if metrics.is_empty() || metrics.iter().any(|item| item.as_str().is_none()) {
                return Err(format!(
                    "widget '{}' requires string array 'metrics'",
                    widget.id
                ));
            }
            Ok(())
        }
        "event_feed" => {
            let map = config_object(widget)?;
            optional_i64_min(map, "limit", 1, &widget.id)?;
            optional_string_list(map, "types", &widget.id)?;
            optional_string_list(map, "device_ids", &widget.id)?;
            if let Some(value) = map.get("area_name") {
                if value.as_str().is_none() {
                    return Err(format!(
                        "widget '{}' field 'area_name' must be a string",
                        widget.id
                    ));
                }
            }
            if let Some(value) = map.get("group_by") {
                let Some(group_by) = value.as_str() else {
                    return Err(format!(
                        "widget '{}' field 'group_by' must be a string",
                        widget.id
                    ));
                };
                match group_by {
                    "none" | "type" | "device" | "area" => {}
                    _ => {
                        return Err(format!(
                            "widget '{}' field 'group_by' is unsupported",
                            widget.id
                        ));
                    }
                }
            }
            Ok(())
        }
        "camera_video" => {
            let map = config_object(widget)?;
            let source_type = require_string(map, "source_type", &widget.id)?;
            match source_type.as_str() {
                "image_refresh" | "mjpeg" | "hls" | "webrtc" => {}
                _ => {
                    return Err(format!(
                        "widget '{}' field 'source_type' is unsupported",
                        widget.id
                    ));
                }
            }
            require_string(map, "url", &widget.id)?;
            optional_i64_min(map, "refresh_secs", 1, &widget.id)?;
            Ok(())
        }
        "web_embed" => {
            let map = config_object(widget)?;
            require_string(map, "url", &widget.id)?;
            if let Some(value) = map.get("sandbox_profile") {
                let Some(profile) = value.as_str() else {
                    return Err(format!(
                        "widget '{}' field 'sandbox_profile' must be a string",
                        widget.id
                    ));
                };
                match profile {
                    "readonly_embed" | "trusted_internal" | "strict_isolated" => {}
                    _ => {
                        return Err(format!(
                            "widget '{}' field 'sandbox_profile' is unsupported",
                            widget.id
                        ));
                    }
                }
            }
            Ok(())
        }
        "markdown" => {
            let map = config_object(widget)?;
            if let Some(value) = map.get("markdown") {
                if value.as_str().is_none() {
                    return Err(format!(
                        "widget '{}' field 'markdown' must be a string",
                        widget.id
                    ));
                }
            } else {
                return Err(format!(
                    "widget '{}' requires string field 'markdown'",
                    widget.id
                ));
            }
            Ok(())
        }
        "history_chart" => {
            let map = config_object(widget)?;
            require_string(map, "device_id", &widget.id)?;
            require_string(map, "attribute", &widget.id)?;
            optional_i64_min(map, "limit", 1, &widget.id)?;
            optional_i64_min(map, "timeframe_hours", 1, &widget.id)?;
            Ok(())
        }
        "dashboard_link" => {
            let map = config_object(widget)?;
            optional_string_list(map, "dashboard_ids", &widget.id)?;
            Ok(())
        }
        "mode_chips" | "scene_row" => {
            let _ = config_object(widget)?;
            Ok(())
        }
        // Hero is rendered client-side from the live device map. Config is
        // optional (defaults to all 6 systems); accept any object shape and
        // let the renderer ignore unknown fields.
        "house_status_hero" => {
            if !widget.config.is_object() && !widget.config.is_null() {
                return Err(format!("widget '{}' config must be an object", widget.id));
            }
            Ok(())
        }
        // A plugin-contributed card. Core validates only the two keys that
        // identify it and treats the rest as opaque — it has no business knowing
        // what a given plugin's card needs, and guessing would make every new
        // card a core release.
        "plugin_widget" => {
            let map = config_object(widget)?;
            require_string(map, "plugin_id", &widget.id)?;
            require_string(map, "widget_id", &widget.id)?;
            Ok(())
        }
        // An unknown card is NOT an error.
        //
        // This is the whole reason `type` stopped being an enum. Core has no
        // business knowing which cards exist — it stores the type verbatim and
        // the client's registry decides what can be drawn. Rejecting here would
        // put every new card, including every plugin card, behind a core
        // release, and it is what let core and the Dart client drift until
        // `house_status_hero` — shipped on core's OWN default dashboard — was a
        // type the client had never heard of.
        _ => Ok(()),
    }
}

pub async fn list_dashboards(State(s): State<AppState>, user: DashboardsRead) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let data = handle.read().await;
    let default_id = data.user_defaults.get(&user.0.uid).map(String::as_str);
    let mut dashboards: Vec<_> = data
        .dashboards
        .iter()
        .filter(|dashboard| dashboard_visible_to(&user.0, dashboard))
        .map(|dashboard| dashboard_response_for(dashboard, default_id))
        .collect();
    dashboards.sort_by(|a, b| a.dashboard.name.cmp(&b.dashboard.name));
    Json(dashboards).into_response()
}

pub async fn list_dashboard_templates(
    _: State<AppState>,
    user: DashboardsRead,
) -> impl IntoResponse {
    let mut templates = dashboard_templates_for(&user.0.uid);
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    Json(templates).into_response()
}

pub async fn get_dashboard(
    State(s): State<AppState>,
    user: DashboardsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let data = handle.read().await;
    match data.dashboards.iter().find(|dashboard| dashboard.id == id) {
        Some(dashboard) if dashboard_visible_to(&user.0, dashboard) => {
            Json(dashboard_response_for(
                dashboard,
                data.user_defaults.get(&user.0.uid).map(String::as_str),
            ))
            .into_response()
        }
        Some(_) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "dashboard access denied" })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "dashboard not found" })),
        )
            .into_response(),
    }
}

pub async fn create_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Json(mut dashboard): Json<DashboardDefinition>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let now = chrono::Utc::now();
    if dashboard.id.trim().is_empty() {
        dashboard.id = format!("dashboard_{}", Uuid::new_v4().simple());
    }
    dashboard.owner_user_id = user.0.uid.clone();
    dashboard.created_at = now;
    dashboard.updated_at = now;

    ensure_default_layouts(&mut dashboard);

    if let Err(error) = validate_dashboard(&dashboard) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    }

    let response = {
        let mut data = handle.write().await;
        if data.dashboards.iter().any(|item| item.id == dashboard.id) {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "dashboard id already exists" })),
            )
                .into_response();
        }
        data.dashboards.push(dashboard.clone());
        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        dashboard_response_for(
            &dashboard,
            data.user_defaults.get(&user.0.uid).map(String::as_str),
        )
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

pub async fn create_dashboard_from_template(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(template) = find_dashboard_template(&id, &user.0.uid) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "dashboard template not found" })),
        )
            .into_response();
    };
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let response = {
        let mut data = handle.write().await;
        let existing_names: HashSet<String> = data
            .dashboards
            .iter()
            .map(|item| item.name.clone())
            .collect();
        let now = chrono::Utc::now();
        let mut dashboard = template.clone();
        dashboard.id = format!("dashboard_{}", Uuid::new_v4().simple());
        dashboard.owner_user_id = user.0.uid.clone();
        dashboard.created_at = now;
        dashboard.updated_at = now;
        dashboard.name = if template.id == "starter_getting_started"
            && !existing_names.contains(&template.name)
        {
            template.name
        } else {
            dashboard_copy_name(&template.name, &existing_names)
        };

        data.dashboards.push(dashboard.clone());
        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }

        dashboard_response_for(
            &dashboard,
            data.user_defaults.get(&user.0.uid).map(String::as_str),
        )
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

pub async fn update_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Path(id): Path<String>,
    Json(mut dashboard): Json<DashboardDefinition>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let response = {
        let mut data = handle.write().await;
        let Some(existing) = data.dashboards.iter().find(|item| item.id == id).cloned() else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "dashboard not found" })),
            )
                .into_response();
        };
        if !dashboard_mutable_by(&user.0, &existing) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "dashboard access denied" })),
            )
                .into_response();
        }

        dashboard.id = existing.id.clone();
        dashboard.owner_user_id = existing.owner_user_id.clone();
        dashboard.created_at = existing.created_at;
        dashboard.updated_at = chrono::Utc::now();

        ensure_default_layouts(&mut dashboard);

        if let Err(error) = validate_dashboard(&dashboard) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }

        if let Some(pos) = data.dashboards.iter().position(|item| item.id == id) {
            data.dashboards[pos] = dashboard.clone();
        }

        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }

        dashboard_response_for(
            &dashboard,
            data.user_defaults.get(&user.0.uid).map(String::as_str),
        )
    };

    Json(response).into_response()
}

pub async fn duplicate_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let response = {
        let mut data = handle.write().await;
        let Some(existing) = data.dashboards.iter().find(|item| item.id == id).cloned() else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "dashboard not found" })),
            )
                .into_response();
        };
        if !dashboard_visible_to(&user.0, &existing) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "dashboard access denied" })),
            )
                .into_response();
        }

        let existing_names: HashSet<String> = data
            .dashboards
            .iter()
            .map(|item| item.name.clone())
            .collect();
        let now = chrono::Utc::now();
        let mut duplicate = existing.clone();
        duplicate.id = format!("dashboard_{}", Uuid::new_v4().simple());
        duplicate.owner_user_id = user.0.uid.clone();
        duplicate.name = dashboard_copy_name(&existing.name, &existing_names);
        duplicate.created_at = now;
        duplicate.updated_at = now;

        data.dashboards.push(duplicate.clone());
        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }

        dashboard_response_for(
            &duplicate,
            data.user_defaults.get(&user.0.uid).map(String::as_str),
        )
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

pub async fn export_dashboard(
    State(s): State<AppState>,
    user: DashboardsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let data = handle.read().await;
    match data.dashboards.iter().find(|dashboard| dashboard.id == id) {
        Some(dashboard) if dashboard_visible_to(&user.0, dashboard) => {
            Json(dashboard).into_response()
        }
        Some(_) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "dashboard access denied" })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "dashboard not found" })),
        )
            .into_response(),
    }
}

pub async fn import_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Json(mut dashboard): Json<DashboardDefinition>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    if let Err(error) = validate_dashboard(&dashboard) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    }

    let response = {
        let mut data = handle.write().await;
        let existing_names: HashSet<String> = data
            .dashboards
            .iter()
            .map(|item| item.name.clone())
            .collect();
        let now = chrono::Utc::now();
        dashboard.id = format!("dashboard_{}", Uuid::new_v4().simple());
        dashboard.owner_user_id = user.0.uid.clone();
        dashboard.name = if existing_names.contains(&dashboard.name) {
            dashboard_copy_name(&dashboard.name, &existing_names)
        } else {
            dashboard.name.clone()
        };
        dashboard.created_at = now;
        dashboard.updated_at = now;

        data.dashboards.push(dashboard.clone());
        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }

        dashboard_response_for(
            &dashboard,
            data.user_defaults.get(&user.0.uid).map(String::as_str),
        )
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

pub async fn reload_dashboards(
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

    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let store = store.clone();
    let loaded = match tokio::task::spawn_blocking(move || store.load()).await {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("dashboard reload task failed: {error}") })),
            )
                .into_response();
        }
    };

    let dashboards_total = loaded.dashboards.len();
    let user_defaults_total = loaded.user_defaults.len();
    *handle.write().await = loaded;

    Json(json!({
        "status": "reloaded",
        "dashboards_total": dashboards_total,
        "user_defaults_total": user_defaults_total
    }))
    .into_response()
}

pub async fn delete_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    {
        let mut data = handle.write().await;
        let Some(existing) = data.dashboards.iter().find(|item| item.id == id).cloned() else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "dashboard not found" })),
            )
                .into_response();
        };
        if !dashboard_mutable_by(&user.0, &existing) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "dashboard access denied" })),
            )
                .into_response();
        }

        data.dashboards.retain(|item| item.id != id);
        data.user_defaults
            .retain(|_, dashboard_id| dashboard_id != &id);

        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    StatusCode::NO_CONTENT.into_response()
}

pub async fn set_default_dashboard(
    State(s): State<AppState>,
    user: DashboardsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.dashboards else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };
    let Some(store) = &s.dashboard_store else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "dashboards unavailable" })),
        )
            .into_response();
    };

    let response = {
        let mut data = handle.write().await;
        let Some(existing) = data.dashboards.iter().find(|item| item.id == id).cloned() else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "dashboard not found" })),
            )
                .into_response();
        };
        if !dashboard_visible_to(&user.0, &existing) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "dashboard access denied" })),
            )
                .into_response();
        }

        data.user_defaults.insert(user.0.uid.clone(), id.clone());
        if let Err(e) = store.save(&data) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }

        dashboard_response_for(&existing, Some(id.as_str()))
    };

    Json(response).into_response()
}

// ---------- Scenes ----------

#[derive(Deserialize)]
pub struct SceneUpsertBody {
    pub name: String,
    pub states: HashMap<String, Value>,
}

pub async fn list_scenes(State(s): State<AppState>, _: ScenesRead) -> impl IntoResponse {
    match s.store.list_scenes().await {
        Ok(scenes) => (StatusCode::OK, Json(json!(scenes))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn create_scene(
    State(s): State<AppState>,
    _: ScenesWrite,
    Json(body): Json<SceneUpsertBody>,
) -> impl IntoResponse {
    let scene = Scene {
        id: Uuid::new_v4(),
        name: body.name,
        states: body.states,
    };
    match s.store.upsert_scene(&scene).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(scene))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn get_scene(
    State(s): State<AppState>,
    _: ScenesRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match s.store.get_scene(id).await {
        Ok(Some(scene)) => (StatusCode::OK, Json(json!(scene))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "scene not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn update_scene(
    State(s): State<AppState>,
    _: ScenesWrite,
    Path(id): Path<Uuid>,
    Json(body): Json<SceneUpsertBody>,
) -> impl IntoResponse {
    let scene = Scene {
        id,
        name: body.name,
        states: body.states,
    };

    match s.store.upsert_scene(&scene).await {
        Ok(_) => (StatusCode::OK, Json(json!(scene))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_scene(
    State(s): State<AppState>,
    _: ScenesWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match s.store.delete_scene(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "scene not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn activate_scene(
    State(s): State<AppState>,
    ScenesWrite(_claims): ScenesWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let scene = match s.store.get_scene(id).await {
        Ok(Some(sc)) => sc,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "scene not found" })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            );
        }
    };

    if let Some(ph) = &s.publish {
        let change = DeviceChange::homecore("scene")
            .with_actor(Some(id.to_string()), Some(scene.name.clone()))
            .with_correlation_id(Some(Uuid::new_v4().to_string()));
        for (device_id, desired) in &scene.states {
            let topic = format!("homecore/devices/{device_id}/cmd");
            // If payload is {"actions":[...]}, publish each item in sequence.
            let items: Vec<&Value> = desired
                .get("actions")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().collect())
                .unwrap_or_else(|| vec![desired]);
            for item in items {
                let payload =
                    serde_json::to_vec(&with_command_change_metadata(item.clone(), &change))
                        .unwrap_or_else(|_| item.to_string().into_bytes());
                if let Err(e) = ph.publish(&topic, payload).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    );
                }
            }
        }
        // Emit scene activated event.
        let ev = hc_types::event::Event::SceneActivated {
            timestamp: chrono::Utc::now(),
            scene_id: id.to_string(),
            scene_name: scene.name.clone(),
        };
        let _ = s.event_bus.publish(ev);
    }

    (StatusCode::OK, Json(json!({ "activated": scene.name })))
}

// ---------- Scene export / import ----------

/// `GET /api/v1/scenes/export`
/// Returns all scenes as a JSON array (ready to re-import).
pub async fn export_scenes(State(s): State<AppState>, _: ScenesRead) -> impl IntoResponse {
    match s.store.list_scenes().await {
        Ok(scenes) => (StatusCode::OK, Json(json!(scenes))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /api/v1/scenes/import`
/// Accepts a JSON array of scenes; assigns fresh UUIDs and saves each one.
/// Returns `{ imported: N }`.
pub async fn import_scenes(
    State(s): State<AppState>,
    _: ScenesWrite,
    Json(scenes): Json<Vec<hc_types::rule::Scene>>,
) -> impl IntoResponse {
    let mut count = 0usize;
    for mut scene in scenes {
        scene.id = Uuid::new_v4();
        if let Err(e) = s.store.upsert_scene(&scene).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        count += 1;
    }
    (StatusCode::CREATED, Json(json!({ "imported": count }))).into_response()
}

// ---------- Automation dry-run ----------

/// `POST /api/v1/automations/{id}/test`
///
/// Evaluates all conditions for the rule and returns whether they pass and
/// which actions *would* fire — without executing them.
pub async fn test_automation(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        );
    };
    let rule = match rh.read().await.iter().find(|r| r.id == id).cloned() {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "rule not found" })),
            );
        }
    };

    // Evaluate each condition independently and collect results.
    let mut condition_results = Vec::new();
    let mut all_pass = true;

    for condition in &rule.conditions {
        let detail = eval_condition_dry_detail(condition, &s.store).await;
        if !detail.passed {
            all_pass = false;
        }
        condition_results.push(serde_json::to_value(&detail).unwrap_or(serde_json::Value::Null));
    }

    (
        StatusCode::OK,
        Json(json!({
            "rule_id": id,
            "rule_name": rule.name,
            "conditions_pass": all_pass,
            "conditions": condition_results,
            "would_fire": all_pass,
            "actions": serde_json::to_value(&rule.actions).unwrap_or(serde_json::Value::Null),
        })),
    )
}

#[derive(serde::Serialize)]
struct ConditionDetail {
    condition: serde_json::Value,
    passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

async fn eval_condition_dry_detail(
    condition: &hc_types::rule::Condition,
    store: &StateStore,
) -> ConditionDetail {
    use hc_types::rule::Condition;
    let cond_json = serde_json::to_value(condition).unwrap_or(serde_json::Value::Null);

    match condition {
        Condition::DeviceState {
            device_id,
            attribute,
            op,
            value,
        } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: Some(value.clone()),
                        elapsed_ms: None,
                        reason: Some(format!("device '{device_id}' not found")),
                    };
                }
                Err(e) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: Some(value.clone()),
                        elapsed_ms: None,
                        reason: Some(format!("store error: {e}")),
                    };
                }
            };
            match device.attributes.get(attribute) {
                None => ConditionDetail {
                    condition: cond_json,
                    passed: false,
                    actual: None,
                    expected: Some(value.clone()),
                    elapsed_ms: None,
                    reason: Some(format!("attribute '{attribute}' not present")),
                },
                Some(actual) => {
                    let passed = compare_values(actual, op, value);
                    ConditionDetail {
                        condition: cond_json,
                        passed,
                        actual: Some(actual.clone()),
                        expected: Some(value.clone()),
                        elapsed_ms: None,
                        reason: None,
                    }
                }
            }
        }
        Condition::TimeWindow { start, end } => {
            let now = hc_time::now_local().time();
            let passed = if start <= end {
                now >= *start && now <= *end
            } else {
                now >= *start || now <= *end
            };
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: Some(json!(now.to_string())),
                expected: Some(json!(format!("{start}–{end}"))),
                elapsed_ms: None,
                reason: None,
            }
        }
        Condition::ScriptExpression { script } => {
            let script = script.clone();
            let result = tokio::task::spawn_blocking(move || {
                hc_scripting::ScriptRuntime::new()
                    .eval_condition(&script)
                    .ok()
            })
            .await
            .ok()
            .flatten();
            ConditionDetail {
                condition: cond_json,
                passed: result.unwrap_or(false),
                actual: None,
                expected: None,
                elapsed_ms: None,
                reason: if result.is_none() {
                    Some("script error".into())
                } else {
                    None
                },
            }
        }
        Condition::TimeElapsed {
            device_id,
            attribute: _,
            duration_secs,
        } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: None,
                        elapsed_ms: None,
                        reason: Some(format!("device '{device_id}' not found")),
                    };
                }
                Err(e) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: None,
                        elapsed_ms: None,
                        reason: Some(format!("store error: {e}")),
                    };
                }
            };
            // Dry-run uses last_seen as the conservative elapsed baseline.
            let elapsed_secs = (chrono::Utc::now() - device.last_seen).num_seconds().max(0);
            let passed = elapsed_secs as u64 >= *duration_secs;
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: None,
                expected: None,
                elapsed_ms: Some(elapsed_secs * 1000),
                reason: if !passed {
                    Some(format!(
                        "only {elapsed_secs}s elapsed, need {duration_secs}s"
                    ))
                } else {
                    None
                },
            }
        }
        Condition::DeviceLastChange {
            device_id,
            kind,
            source,
            actor_id,
            actor_name,
        } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: None,
                        elapsed_ms: None,
                        reason: Some(format!("device '{device_id}' not found")),
                    };
                }
                Err(e) => {
                    return ConditionDetail {
                        condition: cond_json,
                        passed: false,
                        actual: None,
                        expected: None,
                        elapsed_ms: None,
                        reason: Some(format!("store error: {e}")),
                    };
                }
            };

            let Some(change) = device.last_change else {
                return ConditionDetail {
                    condition: cond_json,
                    passed: false,
                    actual: None,
                    expected: None,
                    elapsed_ms: None,
                    reason: Some(format!("device '{device_id}' has no last_change metadata")),
                };
            };

            let passed = kind.as_ref().map(|v| *v == change.kind).unwrap_or(true)
                && source
                    .as_deref()
                    .map(|v| change.source.as_deref() == Some(v))
                    .unwrap_or(true)
                && actor_id
                    .as_deref()
                    .map(|v| change.actor_id.as_deref() == Some(v))
                    .unwrap_or(true)
                && actor_name
                    .as_deref()
                    .map(|v| change.actor_name.as_deref() == Some(v))
                    .unwrap_or(true);

            ConditionDetail {
                condition: cond_json,
                passed,
                actual: Some(json!({
                    "kind": change.kind,
                    "source": change.source,
                    "actor_id": change.actor_id,
                    "actor_name": change.actor_name,
                    "correlation_id": change.correlation_id,
                    "changed_at": change.changed_at,
                })),
                expected: Some(json!({
                    "kind": kind,
                    "source": source,
                    "actor_id": actor_id,
                    "actor_name": actor_name,
                })),
                elapsed_ms: None,
                reason: if passed {
                    None
                } else {
                    Some("last_change metadata did not match requested filters".into())
                },
            }
        }
        Condition::Not { condition: inner } => {
            let mut inner_detail = Box::pin(eval_condition_dry_detail(inner, store)).await;
            inner_detail.passed = !inner_detail.passed;
            inner_detail.condition = cond_json;
            if inner_detail.reason.is_none() && !inner_detail.passed {
                inner_detail.reason = Some("negated condition passed (outer Not fails)".into());
            }
            inner_detail
        }
        Condition::And { conditions } => {
            let mut passed = true;
            let mut reason = None;
            for c in conditions {
                let detail = Box::pin(eval_condition_dry_detail(c, store)).await;
                if !detail.passed {
                    passed = false;
                    reason = Some(
                        detail
                            .reason
                            .unwrap_or_else(|| "sub-condition failed".into()),
                    );
                    break;
                }
            }
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: None,
                expected: None,
                elapsed_ms: None,
                reason,
            }
        }
        Condition::Or { conditions } => {
            let mut passed = false;
            for c in conditions {
                let detail = Box::pin(eval_condition_dry_detail(c, store)).await;
                if detail.passed {
                    passed = true;
                    break;
                }
            }
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: None,
                expected: None,
                elapsed_ms: None,
                reason: if !passed {
                    Some("no sub-condition passed".into())
                } else {
                    None
                },
            }
        }
        Condition::Xor { conditions } => {
            let mut count = 0usize;
            for c in conditions {
                let detail = Box::pin(eval_condition_dry_detail(c, store)).await;
                if detail.passed {
                    count += 1;
                }
            }
            let passed = count == 1;
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: Some(json!(count)),
                expected: Some(json!(1)),
                elapsed_ms: None,
                reason: if !passed {
                    Some(format!("{count} sub-conditions passed, need exactly 1"))
                } else {
                    None
                },
            }
        }
        Condition::PrivateBooleanIs { name, value } => {
            // Dry-run cannot access live runtime state; report as indeterminate.
            ConditionDetail {
                condition: cond_json,
                passed: false,
                actual: None,
                expected: Some(json!(value)),
                elapsed_ms: None,
                reason: Some(format!("private boolean '{name}' not available in dry-run")),
            }
        }
        Condition::HubVariable { name, value, .. } => {
            // Dry-run cannot access live hub variable state; report as indeterminate.
            ConditionDetail {
                condition: cond_json,
                passed: false,
                actual: None,
                expected: Some(value.clone()),
                elapsed_ms: None,
                reason: Some(format!("hub variable '{name}' not available in dry-run")),
            }
        }
        Condition::ModeIs { mode_id, on } => {
            // Dry-run checks the persisted device state.
            let actual_on = match store.get_device(mode_id).await {
                Ok(Some(d)) => d
                    .attributes
                    .get("on")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                _ => false,
            };
            let passed = actual_on == *on;
            ConditionDetail {
                condition: cond_json,
                passed,
                actual: Some(json!(actual_on)),
                expected: Some(json!(on)),
                elapsed_ms: None,
                reason: if !passed {
                    Some(format!(
                        "mode '{mode_id}' is {} (expected {})",
                        actual_on, on
                    ))
                } else {
                    None
                },
            }
        }
        Condition::CalendarActive { .. } => {
            // Dry-run cannot access live calendar state.
            ConditionDetail {
                condition: cond_json,
                passed: false,
                actual: None,
                expected: None,
                elapsed_ms: None,
                reason: Some("calendar state not available in dry-run".into()),
            }
        }
    }
}

fn compare_values(
    actual: &serde_json::Value,
    op: &hc_types::rule::CompareOp,
    expected: &serde_json::Value,
) -> bool {
    use hc_types::rule::CompareOp;
    match op {
        CompareOp::Eq => actual == expected,
        CompareOp::Ne => actual != expected,
        CompareOp::Gt => actual
            .as_f64()
            .zip(expected.as_f64())
            .map(|(a, b)| a > b)
            .unwrap_or(false),
        CompareOp::Gte => actual
            .as_f64()
            .zip(expected.as_f64())
            .map(|(a, b)| a >= b)
            .unwrap_or(false),
        CompareOp::Lt => actual
            .as_f64()
            .zip(expected.as_f64())
            .map(|(a, b)| a < b)
            .unwrap_or(false),
        CompareOp::Lte => actual
            .as_f64()
            .zip(expected.as_f64())
            .map(|(a, b)| a <= b)
            .unwrap_or(false),
    }
}

// ---------- Rule import / export ----------

/// `GET /api/v1/automations/export`
/// Returns all rules as a JSON array (ready to re-import).
pub async fn export_automations(
    State(s): State<AppState>,
    _: AutomationsRead,
) -> impl IntoResponse {
    match &s.source_rules_handle {
        Some(rh) => (StatusCode::OK, Json(json!(rh.read().await.clone()))),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        ),
    }
}

/// `POST /api/v1/automations/import`
/// Accepts a JSON array of rules; assigns fresh UUIDs and writes each as a RON file.
pub async fn import_automations(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(rules): Json<Vec<Rule>>,
) -> impl IntoResponse {
    let Some(source_rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        );
    };

    let mut saved = Vec::with_capacity(rules.len());
    for mut rule in rules {
        rule.id = Uuid::new_v4();
        let compiled_rule = match rule_resolver::compile_rule_for_store(&s.store, &rule).await {
            Ok(rule) => rule,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": e.to_string() })),
                );
            }
        };

        if let Some(fs) = &s.rule_file_store {
            if let Err(e) = fs.write_rule(&rule) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                );
            }
        }

        source_rh.write().await.push(rule.clone());
        if let Some(rh) = &s.rules_handle {
            rh.write().await.push(compiled_rule);
        }
        saved.push(rule);
    }
    (
        StatusCode::CREATED,
        Json(json!({ "imported": saved.len(), "rules": saved })),
    )
}

// ---------- Plugins ----------

pub async fn list_plugins(State(s): State<AppState>, _: PluginsRead) -> impl IntoResponse {
    let map = s.plugins.read().await;
    let list: Vec<_> = map.values().cloned().collect();
    (StatusCode::OK, Json(json!(list)))
}

/// Delete every device belonging to `plugin_id`, emitting a `device_deleted`
/// event for each so live clients drop the tile without a manual refresh (the
/// same event shape the plugin-unregister path in state_bridge emits), and
/// nullifying any rule references. Shared by plugin uninstall and the
/// `/plugins/:id/devices` wipe. Returns `(deleted, device_ids, affected_rules)`.
async fn remove_plugin_devices(s: &AppState, plugin_id: &str) -> (usize, Vec<String>, Vec<String>) {
    let devices_before = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(plugin_id, error = %e, "remove_plugin_devices: list_devices failed");
            return (0, Vec::new(), Vec::new());
        }
    };
    let target_ids: Vec<String> = devices_before
        .iter()
        .filter(|d| d.plugin_id == plugin_id)
        .map(|d| d.device_id.clone())
        .collect();

    let mut deleted = 0usize;
    let mut affected_rules: Vec<String> = Vec::new();
    for id in &target_ids {
        match s.store.delete_device(id).await {
            Ok(true) => {
                deleted += 1;
                let _ = s.event_bus.publish(hc_types::event::Event::Custom {
                    timestamp: chrono::Utc::now(),
                    event_type: "device_deleted".to_string(),
                    payload: json!({
                        "device_id": id,
                        "plugin_id": plugin_id,
                        "source": "plugin_uninstall",
                    }),
                });
                if let Some(rfs) = &s.rule_file_store {
                    if let Ok(names) =
                        crate::rule_file_store::nullify_device_refs(&rfs.dir, id, &devices_before)
                    {
                        for name in names {
                            if !affected_rules.contains(&name) {
                                affected_rules.push(name);
                            }
                        }
                    }
                }
            }
            Ok(false) => {} // race: gone between list and delete — not an error
            Err(e) => {
                tracing::warn!(device_id = %id, error = %e, "remove_plugin_devices: delete failed");
            }
        }
    }
    (deleted, target_ids, affected_rules)
}

/// `DELETE /api/v1/plugins/:id` — **full uninstall** (Phase A).
///
/// Was a soft "deregister" that only forgot the in-memory record, leaving the
/// process running and its devices behind as ghosts. Now it: (1) stops the
/// managed child, (2) unregisters all of the plugin's devices (emitting
/// `device_deleted` so clients update live), (3) drops it from the registry and
/// clears its learned state + retained topic.
///
/// NOTE (Phase A limit): a plugin still declared in `[[plugins]]` respawns on the
/// next core restart — removing the *declaration* needs the managed-plugin store
/// (Phase A slice 2). Returns a summary of what was torn down.
pub async fn deregister_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // 1. Stop the managed child if we supervise it (best-effort). Stopping first
    //    prevents the plugin re-registering devices we're about to delete.
    let stopped = {
        let cmds = s.plugin_commands.read().await;
        match cmds.get(&id) {
            Some(tx) => tx.send(crate::PluginCommand::Stop).await.is_ok(),
            None => false,
        }
    };

    // 2. Unregister its devices (no ghosts; emits device_deleted per device).
    let (devices_removed, device_ids, affected_rules) = remove_plugin_devices(&s, &id).await;

    // 3. Drop from the registry + clear learned state + retained topic.
    let removed = {
        let mut map = s.plugins.write().await;
        map.remove(&id).is_some()
    };
    if removed {
        if let Err(e) = s.store.plugin_state_delete(&id).await {
            tracing::warn!(plugin_id = %id, error = %e, "Failed to delete plugin learned state");
        }
        if let Some(ref publish) = s.publish {
            let _ = publish
                .publish_retained(&format!("homecore/plugins/{id}/state"), Vec::new())
                .await;
        }
    }

    if !removed && !stopped && devices_removed == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found" })),
        )
            .into_response();
    }

    // 4. Persist the uninstall: drop any managed record + tombstone the id so a
    //    plugin still declared statically in homecore.toml doesn't respawn on the
    //    next core restart.
    if let Some(mp) = &s.managed_plugins {
        if let Err(e) = mp.uninstall(&id) {
            tracing::warn!(plugin_id = %id, error = %e, "Failed to record uninstall in managed store");
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "uninstalled": id,
            "stopped": stopped,
            "devices_removed": devices_removed,
            "device_ids": device_ids,
            "affected_rules": affected_rules,
        })),
    )
        .into_response()
}

/// `GET /api/v1/registry/plugins` — browse the (verified, cached) registry index.
pub async fn browse_registry(State(s): State<AppState>, _: PluginsRead) -> impl IntoResponse {
    let Some(reg) = &s.registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no plugin registry is configured" })),
        )
            .into_response();
    };
    match reg.index().await {
        Ok(idx) => (StatusCode::OK, Json(json!({ "plugins": idx.plugins }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("registry unavailable: {e:#}") })),
        )
            .into_response(),
    }
}

/// `GET /api/v1/registry/plugins/:id` — one plugin's registry detail.
pub async fn get_registry_plugin(
    State(s): State<AppState>,
    _: PluginsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(reg) = &s.registry else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no plugin registry is configured" })),
        )
            .into_response();
    };
    match reg.index().await {
        Ok(idx) => match idx.plugin(&id) {
            Some(p) => (StatusCode::OK, Json(json!(p))).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin not in registry" })),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("registry unavailable: {e:#}") })),
        )
            .into_response(),
    }
}

/// `POST /api/v1/plugins/install` — install a plugin, then activate it.
///
/// Two sources: `{ "id": "...", "version": "..."? }` resolves + downloads +
/// verifies (sha256, over a signature-verified index) from the remote registry
/// (Phase C); `{ "path": "/abs/x.tar.zst" }` installs a local artifact (Phase B).
/// Either way it unpacks the `.tar.zst`, installs the binary under
/// `HOMECORE_HOME/plugins/`, mints an MQTT credential + seeds operator config,
/// records a managed entry (clearing any uninstall tombstone), and dynamically
/// spawns it (no restart).
pub async fn install_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (Some(mp), Some(ctx)) = (s.managed_plugins.clone(), s.plugin_install.clone()) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "plugin install is not configured on this server" })),
        )
            .into_response();
    };

    // Resolve the artifact path: registry `{id, version?}` or local `{path}`.
    // A downloaded temp file must outlive the blocking install that reads it.
    let mut _download_guard: Option<tempfile::NamedTempFile> = None;
    let archive_path = if let Some(id) = body.get("id").and_then(Value::as_str) {
        let Some(reg) = s.registry.clone() else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "no plugin registry is configured" })),
            )
                .into_response();
        };
        let version = body.get("version").and_then(Value::as_str);
        let (art, _ver) = match reg.resolve(id, version).await {
            Ok(x) => x,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("registry resolve failed: {e:#}") })),
                )
                    .into_response();
            }
        };
        let f = match reg.download_artifact(&art).await {
            Ok(f) => f,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("artifact download failed: {e:#}") })),
                )
                    .into_response();
            }
        };
        let p = f.path().to_string_lossy().into_owned();
        _download_guard = Some(f);
        p
    } else if let Some(p) = body
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
    {
        p.to_string()
    } else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "provide `id` (registry) or `path` (local artifact)" })),
        )
            .into_response();
    };

    // Unpack + copy + write config off the async runtime.
    let ctx_inner = (*ctx).clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::plugin_install::install_from_archive(std::path::Path::new(&archive_path), &ctx_inner)
    })
    .await;

    let outcome = match result {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("install failed: {e:#}") })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("install task panicked: {e}") })),
            )
                .into_response();
        }
    };

    let mut record = outcome.record;
    record.installed_at = chrono::Utc::now().to_rfc3339();
    if let Err(e) = mp.install(record.clone()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("installed but failed to record: {e}") })),
        )
            .into_response();
    }

    // Seed or refresh the registry record — keeping binary/config/version current
    // so a reinstall/upgrade points the (record-driven) supervisor at the new
    // binary — then activate.
    {
        let mut map = s.plugins.write().await;
        let entry = map.entry(record.id.clone()).or_insert_with(|| {
            crate::PluginRecord::managed_seed(
                record.id.clone(),
                Some(record.config.clone()),
                Some(record.binary.clone()),
                record.enabled,
                Some(record.version.clone()),
            )
        });
        entry.binary_path = Some(record.binary.clone());
        entry.config_path = Some(record.config.clone());
        entry.installed_version = Some(record.version.clone());
        entry.enabled = record.enabled;
    }
    // Already supervised (reinstall / upgrade) → Restart relaunches the new
    // binary (the supervisor reads binary/config from the record). Fresh install
    // → dynamic spawn into the supervisor.
    let already_supervised = s.plugin_commands.read().await.contains_key(&record.id);
    let activated = if !record.enabled {
        false
    } else if already_supervised {
        let cmds = s.plugin_commands.read().await;
        match cmds.get(&record.id) {
            Some(tx) => tx.send(crate::PluginCommand::Restart).await.is_ok(),
            None => false,
        }
    } else {
        match &s.plugin_spawn {
            Some(tx) => tx
                .send(crate::InstalledPlugin {
                    id: record.id.clone(),
                    binary: record.binary.clone(),
                    config: record.config.clone(),
                    enabled: record.enabled,
                })
                .await
                .is_ok(),
            None => false,
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "installed": record.id,
            "name": record.name,
            "version": record.version,
            "binary": record.binary,
            "config": record.config,
            "reinstall": outcome.reinstall,
            "activated": activated,
            "note": if activated {
                "installed and activated"
            } else {
                "installed; activate on next core start"
            },
        })),
    )
        .into_response()
}

/// `DELETE /api/v1/plugins/:id/devices`
///
/// Bulk-wipe every device whose `plugin_id` matches `:id`. The plugin
/// itself stays registered — only the devices it had registered are
/// removed. On the plugin's next sync cycle, devices it still cares
/// about will be re-registered automatically.
///
/// Use this to clean up zombies left over from development churn,
/// from a plugin's pre-cleanup behavior, or after rearranging device
/// IDs in a plugin's config.
///
/// Reuses the same `delete_device` path as the single + bulk device
/// endpoints, including rule-reference nullification, so any rule
/// that referenced one of the wiped devices is patched (and disabled
/// if its trigger device was the deleted one) on the way out.
///
/// Returns `{ "deleted": N, "device_ids": ["id1", ...], "affected_rules": ["rule name", ...] }`.
pub async fn delete_plugin_devices(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    let devices_before_delete = match s.store.list_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let target_ids: Vec<String> = devices_before_delete
        .iter()
        .filter(|d| d.plugin_id == plugin_id)
        .map(|d| d.device_id.clone())
        .collect();

    if target_ids.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "deleted": 0,
                "device_ids": [],
                "affected_rules": [],
                "note": format!("no devices found with plugin_id = '{plugin_id}'"),
            })),
        )
            .into_response();
    }

    let mut deleted = 0usize;
    let mut affected_rules: Vec<String> = Vec::new();
    for id in &target_ids {
        match s.store.delete_device(id).await {
            Ok(true) => {
                deleted += 1;
                if let Some(rfs) = &s.rule_file_store {
                    if let Ok(names) = crate::rule_file_store::nullify_device_refs(
                        &rfs.dir,
                        id,
                        &devices_before_delete,
                    ) {
                        for name in names {
                            if !affected_rules.contains(&name) {
                                affected_rules.push(name);
                            }
                        }
                    } else {
                        tracing::warn!(
                            device_id = %id,
                            "delete_plugin_devices: failed to nullify rule refs"
                        );
                    }
                }
            }
            Ok(false) => {
                // Race: device disappeared between list and delete. Not an error.
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": e.to_string(),
                        "deleted_so_far": deleted,
                    })),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "deleted": deleted,
            "device_ids": target_ids,
            "affected_rules": affected_rules,
        })),
    )
        .into_response()
}

pub async fn get_plugin(
    State(s): State<AppState>,
    _: PluginsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let map = s.plugins.read().await;
    match map.get(&id) {
        Some(rec) => (StatusCode::OK, Json(json!(rec))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found" })),
        )
            .into_response(),
    }
}

pub async fn get_plugin_capabilities(
    State(s): State<AppState>,
    _: PluginsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let map = s.plugins.read().await;
    match map.get(&id).and_then(|r| r.capabilities.as_ref()) {
        Some(caps) => (StatusCode::OK, Json(json!(caps))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "capabilities not published" })),
        )
            .into_response(),
    }
}

/// The plugin's operator-config JSON Schema, published on its capability
/// manifest. The config editor renders a typed form from this; a 404 means the
/// plugin published no schema and the editor should fall back to raw TOML. This
/// is non-secret shape information (no config values), so `plugins:read` is
/// sufficient — same as capabilities.
pub async fn get_plugin_config_schema(
    State(s): State<AppState>,
    _: PluginsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let map = s.plugins.read().await;
    match map.get(&id).and_then(|r| r.config_schema.as_ref()) {
        Some(schema) => (
            StatusCode::OK,
            Json(json!({ "plugin_id": id, "schema": schema })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no config schema published for this plugin" })),
        )
            .into_response(),
    }
}

/// `GET /plugins/:id/config/descriptor` — the plugin's own config descriptor
/// (sections, field kinds, conditionals, data sources), as published on its
/// capability manifest. 404 when the plugin ships none; the client then
/// auto-derives a baseline descriptor from `config/schema`.
pub async fn get_plugin_config_descriptor(
    State(s): State<AppState>,
    _: PluginsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let map = s.plugins.read().await;
    match map.get(&id).and_then(|r| r.config_descriptor.as_ref()) {
        Some(descriptor) => (
            StatusCode::OK,
            Json(json!({ "plugin_id": id, "descriptor": descriptor })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no config descriptor published for this plugin" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct PluginStreamQuery {
    /// JWT token — browsers can't set `Authorization` on `EventSource`, so
    /// the stream endpoint lives on the public router and validates the
    /// token from the query string (same pattern as `/events/stream`).
    pub token: Option<String>,
}

/// SSE bridge for streaming-action events. Subscribes to the core event
/// bus and forwards every `MqttMessage` on
/// `homecore/plugins/{id}/commands/{request_id}/events` as a Server-Sent
/// Event. Closes on the first terminal stage
/// (`complete`/`error`/`canceled`/`timeout`).
///
/// Public route: auth is enforced here rather than via the Bearer
/// middleware, because `EventSource` can't send custom headers. Accepts
/// `?token=<jwt>` (query) or `Authorization: Bearer <jwt>` (header) or
/// falls back to the IP whitelist. Requires `plugins:read` scope.
pub async fn get_plugin_stream_sse(
    State(s): State<AppState>,
    Path((plugin_id, request_id)): Path<(String, String)>,
    Query(query): Query<PluginStreamQuery>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvent, KeepAlive};
    use axum::response::IntoResponse;
    use std::convert::Infallible;

    // Inline auth: whitelist → Admin; otherwise require a valid JWT with
    // `plugins:read` (from ?token= or Authorization: Bearer). Mirrors
    // `ws::authenticate_ws` so the streaming endpoints behave similarly,
    // plus header fallback so programmatic clients can reuse their Bearer
    // token without URL-encoding it.
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);
    let claims = {
        let remote_ip = match addr.ip() {
            std::net::IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map(std::net::IpAddr::V4)
                .unwrap_or(std::net::IpAddr::V6(v6)),
            v4 => v4,
        };
        let whitelisted =
            !s.whitelist.is_empty() && s.whitelist.iter().any(|net| net.contains(&remote_ip));
        if whitelisted {
            crate::auth_middleware::whitelist_claims()
        } else {
            let token = bearer.as_deref().or(query.token.as_deref()).unwrap_or("");
            if token.is_empty() {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(
                        json!({ "error": "missing token (query ?token= or Authorization header)" }),
                    ),
                )
                    .into_response();
            }
            match s.jwt.validate(token) {
                Ok(c) => c,
                Err(_) => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "error": "invalid or expired token" })),
                    )
                        .into_response();
                }
            }
        }
    };
    if !claims.has_scope("plugins:read") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "scope 'plugins:read' required" })),
        )
            .into_response();
    }

    let target_topic = format!("homecore/plugins/{plugin_id}/commands/{request_id}/events");
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(64);
    // Subscribe BEFORE reading the cache so any event that lands between
    // the snapshot and the first recv() is still captured on the live bus.
    let mut bus_rx = s.raw_bus.subscribe();
    let cache_snapshot = s.stream_cache.snapshot(&request_id).await;

    tokio::spawn(async move {
        // Step 1: replay cached events. The plan's "retained last event
        // is the resilience floor" is implemented by this cache (rather
        // than by MQTT retained) so late subscribers see the full history
        // up to the cache limit.
        let mut last_terminal = false;
        let mut seen_ts: std::collections::HashSet<String> = std::collections::HashSet::new();
        for val in &cache_snapshot {
            if let Some(ts) = val.get("ts").and_then(|v| v.as_str()) {
                seen_ts.insert(ts.to_string());
            }
            let stage = val.get("stage").and_then(|s| s.as_str()).unwrap_or("");
            let data = serde_json::to_string(val).unwrap_or_default();
            let sse_event = SseEvent::default().event("stream").data(data);
            if tx.send(Ok(sse_event)).await.is_err() {
                return;
            }
            if matches!(stage, "complete" | "error" | "canceled" | "timeout") {
                last_terminal = true;
            }
        }
        if last_terminal {
            return;
        }

        // Step 2: forward live events. Dedup by `ts` against the cache —
        // bus_rx was subscribed before the snapshot, so any event that
        // appears in both channels shares a timestamp and is skipped.
        loop {
            match bus_rx.recv().await {
                Ok(hc_types::event::Event::MqttMessage { topic, payload, .. })
                    if topic == target_topic =>
                {
                    if payload.is_empty() {
                        continue;
                    }
                    let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                        continue;
                    };
                    if let Some(ts) = val.get("ts").and_then(|v| v.as_str()) {
                        if !seen_ts.insert(ts.to_string()) {
                            continue;
                        }
                    }
                    let stage = val
                        .get("stage")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = serde_json::to_string(&val).unwrap_or_default();
                    let sse_event = SseEvent::default().event("stream").data(text);
                    if tx.send(Ok(sse_event)).await.is_err() {
                        return;
                    }
                    if matches!(
                        stage.as_str(),
                        "complete" | "error" | "canceled" | "timeout"
                    ) {
                        return;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    axum::response::Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub async fn start_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cmds = s.plugin_commands.read().await;
    let Some(tx) = cmds.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found or not managed locally" })),
        )
            .into_response();
    };
    match tx.send(crate::PluginCommand::Start).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "action": "start" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "plugin supervisor not responding" })),
        )
            .into_response(),
    }
}

pub async fn stop_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cmds = s.plugin_commands.read().await;
    let Some(tx) = cmds.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found or not managed locally" })),
        )
            .into_response();
    };
    match tx.send(crate::PluginCommand::Stop).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "action": "stop" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "plugin supervisor not responding" })),
        )
            .into_response(),
    }
}

pub async fn restart_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let cmds = s.plugin_commands.read().await;
    let Some(tx) = cmds.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found or not managed locally" })),
        )
            .into_response();
    };
    match tx.send(crate::PluginCommand::Restart).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "action": "restart" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "plugin supervisor not responding" })),
        )
            .into_response(),
    }
}

pub async fn patch_plugin(
    State(s): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Snapshot the previous enabled state + apply updates.
    // We hold the write lock only briefly so we can release it before
    // doing potentially-blocking I/O (config write) and channel sends.
    let prev_log_level = {
        let map = s.plugins.read().await;
        map.get(&id).and_then(|r| r.log_level.clone())
    };
    let (prev_enabled, new_rec) = {
        let mut map = s.plugins.write().await;
        let Some(rec) = map.get_mut(&id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin not found" })),
            )
                .into_response();
        };
        let prev = rec.enabled;
        if let Some(enabled) = body["enabled"].as_bool() {
            rec.enabled = enabled;
        }
        if let Some(level) = body["log_level"].as_str() {
            rec.log_level = Some(level.to_string());
            // Send MQTT management command so the plugin changes level
            // immediately (if it supports the management protocol).
            if let Some(ref rpc) = s.management_rpc {
                let id = id.clone();
                let level = level.to_string();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let _ = rpc.set_log_level(&id, &level).await;
                });
            }
        }
        (prev, rec.clone())
    };

    // ── Persist + react to enabled transitions ──────────────────────────
    if let Some(new_enabled) = body["enabled"].as_bool() {
        // 1. Persist the change to homecore.toml. Best-effort: if the
        //    config path isn't known (rare; means hc-core was started
        //    in some unusual way), or the plugin isn't declared in
        //    [[plugins]] (e.g. it registered via MQTT only), we log
        //    and continue. The runtime change in `s.plugins` already
        //    landed above; we just don't survive a restart.
        if let Some(ref path) = s.homecore_config_path {
            match crate::config_writer::persist_plugin_enabled(path.as_path(), &id, new_enabled) {
                Ok(()) => {
                    tracing::info!(
                        plugin = %id,
                        enabled = new_enabled,
                        "Persisted plugin enabled to homecore.toml"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %id,
                        error = %e,
                        "Failed to persist plugin enabled (runtime change applied; \
                         will reset on next restart)"
                    );
                }
            }
        }

        // 2. Auto-start / auto-stop on transition. The seeded config
        //    expects this — flipping the toggle is the user-visible
        //    action; spinning the supervisor is a side effect.
        if new_enabled != prev_enabled {
            let cmds = s.plugin_commands.read().await;
            if let Some(tx) = cmds.get(&id) {
                let action = if new_enabled {
                    crate::PluginCommand::Start
                } else {
                    crate::PluginCommand::Stop
                };
                let _ = tx.send(action).await;
            } else {
                tracing::debug!(
                    plugin = %id,
                    enabled = new_enabled,
                    "Plugin enabled flipped but plugin is not locally managed — \
                     no start/stop side effect"
                );
            }
        }
    }

    // Audit: emit on enabled transition or log_level change.
    if let Some(new_enabled) = body["enabled"].as_bool() {
        if new_enabled != prev_enabled {
            let event = if new_enabled {
                "plugin.enabled"
            } else {
                "plugin.disabled"
            };
            let audit_e =
                audit::entry_from_claims(&claims, event).with_target("plugin", id.clone());
            audit::emit(&s, audit_e).await;
        }
    }
    if let Some(level) = body["log_level"].as_str() {
        if prev_log_level.as_deref() != Some(level) {
            let mut audit_e = audit::entry_from_claims(&claims, "plugin.log_level_changed")
                .with_target("plugin", id.clone());
            audit_e.detail = json!({
                "previous": prev_log_level,
                "current":  level,
            });
            audit::emit(&s, audit_e).await;
        }
    }

    (StatusCode::OK, Json(json!(new_rec))).into_response()
}

/// Written in place of every secret by `GET /plugins/{id}/config`.
///
/// `PUT` restores the stored value wherever it sees this, so a config editor can
/// fetch a document, edit an unrelated field, and save it back without wiping
/// out credentials it was never shown.  The admin UI does exactly that: it loads
/// `raw` into a text area and PUTs the text straight back.
const REDACTED_SECRET: &str = "__redacted__";

/// Whether a TOML/JSON key holds a credential, as opposed to merely pointing at
/// one.  `password` and `app_key` are secrets; `key_path`, `token_url`, and
/// `refresh_token_expiry_days` are not, and redacting those would corrupt the
/// config to no benefit — so the exclusions are checked first.
fn is_secret_key(key: &str) -> bool {
    let k = key.trim().to_ascii_lowercase();
    if k.ends_with("path") || k.ends_with("_url") || k.ends_with("_type") || k.contains("expiry") {
        return false;
    }
    const SECRET_PARTS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "hash",
        "api_key",
        "apikey",
        "app_key",
        "appkey",
        "access_key",
        "private_key",
        "user_key",
        "auth_key",
    ];
    SECRET_PARTS.iter().any(|p| k.contains(p))
        || matches!(k.as_str(), "key" | "pat" | "pin" | "passphrase")
}

/// Section label for each line of a TOML document.  `[[array]]` tables carry an
/// occurrence suffix so repeated tables (e.g. several `[[notify.channels]]`,
/// each with its own `token`) stay distinct instead of collapsing together.
fn toml_line_sections(src: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("[[") && t.ends_with("]]") {
            let name = t.trim_matches(|c| c == '[' || c == ']').to_string();
            let n = seen.entry(name.clone()).or_insert(0);
            current = format!("{name}#{n}");
            *n += 1;
        } else if t.starts_with('[') && t.ends_with(']') {
            current = t.trim_matches(|c| c == '[' || c == ']').to_string();
        }
        sections.push(current.clone());
    }
    sections
}

/// Split a TOML line into `(indent, key, value)` if it is a plain `key = value`
/// assignment.  Comment lines and table headers yield `None`.
fn toml_assignment(line: &str) -> Option<(&str, &str, &str)> {
    let body = line.trim_start();
    if body.starts_with('#') || body.starts_with('[') {
        return None;
    }
    let eq = line.find('=')?;
    let indent = &line[..line.len() - body.len()];
    Some((indent, line[..eq].trim(), line[eq + 1..].trim()))
}

/// Redact secrets in raw TOML text, preserving comments and layout — the admin
/// UI shows this verbatim in an editor, so re-serializing the parsed document
/// (and losing every comment) would not do.
fn redact_toml_text(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        match toml_assignment(line) {
            Some((indent, key, _)) if is_secret_key(key) => {
                out.push_str(&format!("{indent}{key} = \"{REDACTED_SECRET}\"\n"));
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Put the stored secret back wherever the client echoed the placeholder.
/// Without this, saving a config fetched from the API would overwrite every
/// credential in it with `__redacted__`.
fn restore_toml_secrets(incoming: &str, current: &str) -> String {
    let cur_sections = toml_line_sections(current);
    let mut stored: HashMap<(String, String), String> = HashMap::new();
    for (i, line) in current.lines().enumerate() {
        if let Some((_, key, value)) = toml_assignment(line) {
            if is_secret_key(key) {
                stored.insert(
                    (cur_sections[i].clone(), key.to_string()),
                    value.to_string(),
                );
            }
        }
    }

    let in_sections = toml_line_sections(incoming);
    let mut out = String::with_capacity(incoming.len());
    for (i, line) in incoming.lines().enumerate() {
        if let Some((indent, key, value)) = toml_assignment(line) {
            if is_secret_key(key) && value.trim_matches('"') == REDACTED_SECRET {
                if let Some(original) = stored.get(&(in_sections[i].clone(), key.to_string())) {
                    out.push_str(&format!("{indent}{key} = {original}\n"));
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Redact secret leaves anywhere in a parsed config tree.
fn redact_json_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_secret_key(k) && !v.is_object() && !v.is_array() {
                    *v = Value::String(REDACTED_SECRET.to_string());
                } else {
                    redact_json_secrets(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json_secrets),
        _ => {}
    }
}

/// Structural counterpart to [`restore_toml_secrets`], for the `config` (JSON)
/// form of the body.  Arrays are matched positionally.
fn restore_json_secrets(incoming: &mut Value, current: &Value) {
    match (incoming, current) {
        (Value::Object(inc), Value::Object(cur)) => {
            for (k, v) in inc.iter_mut() {
                if v.as_str() == Some(REDACTED_SECRET) {
                    if let Some(original) = cur.get(k) {
                        *v = original.clone();
                    }
                } else if let Some(c) = cur.get(k) {
                    restore_json_secrets(v, c);
                }
            }
        }
        (Value::Array(inc), Value::Array(cur)) => {
            // Match elements by a stable identity field when present, falling
            // back to positional. Pure positional matching corrupts secrets the
            // moment an array element is removed or reordered: dropping
            // `bridges[0]` would shift every following bridge up one slot, so
            // core would restore each remaining bridge's redacted `app_key` from
            // the WRONG stored element. Identity matching keeps `bridges[i]`
            // pinned to its own stored entry regardless of position.
            for (i, v) in inc.iter_mut().enumerate() {
                let matched = array_identity_match(v, cur).or_else(|| cur.get(i));
                if let Some(c) = matched {
                    restore_json_secrets(v, c);
                }
            }
        }
        _ => {}
    }
}

/// Find the element of `current` that refers to the same logical entity as
/// `incoming`, by the first identity field they share a value on. Returns
/// `None` for non-objects or when no identity field matches (caller then falls
/// back to positional). Weak keys (name/host) come last so a strong id wins.
fn array_identity_match<'a>(incoming: &Value, current: &'a [Value]) -> Option<&'a Value> {
    const ID_KEYS: [&str; 6] = ["id", "bridge_id", "uuid", "serial", "name", "host"];
    let inc = incoming.as_object()?;
    for key in ID_KEYS {
        let Some(want) = inc.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some(found) = current.iter().find(|c| {
            c.as_object()
                .and_then(|co| co.get(key))
                .and_then(Value::as_str)
                == Some(want)
        }) {
            return Some(found);
        }
    }
    None
}

/// Does this tree still carry a redaction placeholder anywhere?
fn has_redaction(value: &Value) -> bool {
    match value {
        Value::String(s) => s == REDACTED_SECRET,
        Value::Object(map) => map.values().any(has_redaction),
        Value::Array(items) => items.iter().any(has_redaction),
        _ => false,
    }
}

pub async fn get_plugin_config(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (config_path, managed) = {
        let map = s.plugins.read().await;
        let Some(rec) = map.get(&id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin not found" })),
            )
                .into_response();
        };
        (rec.config_path.clone(), rec.managed)
    };

    // Local plugin: read config file directly.
    //
    // Secrets are replaced with REDACTED_SECRET — this response used to hand
    // back every plugin credential (Hue app_key, YoLink client_secret, MQTT
    // passwords) in clear text to anyone with plugins:write. PUT restores them,
    // so an editor can still round-trip the document safely.
    if let Some(ref path) = config_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            let mut resp = json!({
                "plugin_id": id,
                "format":    "toml",
                "raw":       redact_toml_text(&content),
                "redacted":  true,
            });
            // Also include parsed JSON for clients that want structured access.
            if let Ok(parsed) = content.parse::<toml::Value>() {
                let mut cfg = serde_json::to_value(parsed).unwrap_or_default();
                redact_json_secrets(&mut cfg);
                resp["config"] = cfg;
            }
            return (StatusCode::OK, Json(resp)).into_response();
        }
    }

    // Remote plugin: use MQTT management RPC.
    if let Some(ref rpc) = s.management_rpc {
        match rpc.get_config(&id).await {
            Ok(resp) => {
                let mut cfg = resp.get("data").cloned().unwrap_or(resp);
                redact_json_secrets(&mut cfg);
                (
                    StatusCode::OK,
                    Json(json!({
                        "plugin_id": id,
                        "format":    "remote",
                        "config":    cfg,
                        "redacted":  true,
                    })),
                )
                    .into_response()
            }
            Err(e) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({ "error": e }))).into_response(),
        }
    } else if !managed {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "remote config not available — management RPC not configured" })),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no config path for this plugin" })),
        )
            .into_response()
    }
}

pub async fn put_plugin_config(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (config_path, managed) = {
        let map = s.plugins.read().await;
        let Some(rec) = map.get(&id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin not found" })),
            )
                .into_response();
        };
        (rec.config_path.clone(), rec.managed)
    };

    // Local plugin: write config file directly.
    if let Some(ref path) = config_path {
        // GET redacts secrets, so a client that fetched this config never saw
        // them. Restore the stored value wherever it echoed the placeholder back
        // — otherwise round-tripping a config through the editor would silently
        // overwrite every credential in it with "__redacted__".
        let current = std::fs::read_to_string(path).unwrap_or_default();

        // Accept either { "config": {...} } (JSON→TOML) or { "raw": "..." } (raw TOML string)
        let toml_str = if let Some(raw) = body["raw"].as_str() {
            restore_toml_secrets(raw, &current)
        } else if let Some(config) = body.get("config") {
            let mut config = config.clone();
            if has_redaction(&config) {
                if let Ok(parsed) = current.parse::<toml::Value>() {
                    let current_json = serde_json::to_value(parsed).unwrap_or_default();
                    restore_json_secrets(&mut config, &current_json);
                }
            }
            let toml_val: toml::Value = match serde_json::from_value(config) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("invalid config: {e}") })),
                    )
                        .into_response()
                }
            };
            toml::to_string_pretty(&toml_val).unwrap_or_default()
        } else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "provide 'config' (JSON object) or 'raw' (TOML string)" })),
            )
                .into_response();
        };

        return match std::fs::write(path, &toml_str) {
            Ok(()) => {
                (StatusCode::OK, Json(json!({ "ok": true, "plugin_id": id }))).into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to write config: {e}") })),
            )
                .into_response(),
        };
    }

    // Remote plugin: use MQTT management RPC.
    if let Some(ref rpc) = s.management_rpc {
        let mut config = body.get("config").cloned().unwrap_or(body.clone());
        // Same restore as the local path — fetch what the plugin currently holds
        // and put the real secrets back before pushing the document to it.
        if has_redaction(&config) {
            if let Ok(current) = rpc.get_config(&id).await {
                let current = current.get("data").cloned().unwrap_or(current);
                restore_json_secrets(&mut config, &current);
            }
        }
        match rpc.set_config(&id, config).await {
            Ok(_) => (StatusCode::OK, Json(json!({ "ok": true, "plugin_id": id }))).into_response(),
            Err(e) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({ "error": e }))).into_response(),
        }
    } else if !managed {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "remote config not available" })),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no config path for this plugin" })),
        )
            .into_response()
    }
}

/// `POST /plugins/{id}/command` — send a plugin-specific management command.
///
/// Body: `{ "action": "...", ...extra fields }`.  Forwarded verbatim (plus a
/// generated `request_id`) to the plugin via the management RPC.  Used for
/// plugin-defined actions beyond the built-in `get_config`/`set_config`/
/// `set_log_level` set (e.g. yolink's `rescan_devices`).
//
// SAFETY (W2-tail): authorization here is defense-in-depth across two
// layers, but only Layer 1 covers actions whose capability manifest
// is unknown:
//
//   Layer 1 — `PluginsWrite(claims)` extractor rejects with 403 before
//             this body runs, for any caller missing the `plugins:write`
//             scope.
//   Layer 2 — `requires_admin` check below rejects when the *manifest*
//             declares `requires_role: Admin` and the caller's role
//             isn't Admin. Skipped (`requires_admin == false`) for any
//             action `action_meta == None`.
//
// Layer 2 is a refinement of Layer 1 today — it's harmless that it skips
// for unknown actions, because Layer 1 already restricts to admins (the
// only role currently granted `plugins:write` is `Role::Admin`, asserted
// by the `user_can_command_but_not_manage` test in `hc_auth::user`).
//
// If a future role is granted `plugins:write` (e.g. a hypothetical
// `PluginOperator`), unknown manifest actions would silently bypass the
// role check entirely. When that change lands, either:
//   (a) require all plugin actions to be in the manifest before they're
//       dispatched (close the unknown-action path), or
//   (b) default `requires_admin = true` for `action_meta.is_none()`
//       (fail closed when we don't know what an action does).
pub async fn post_plugin_command(
    State(s): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(action) = body["action"].as_str().map(str::to_string) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing 'action' field" })),
        )
            .into_response();
    };

    // Resolve the manifest action (if any) to pick up requires_role,
    // concurrency, stream, and timeout_ms. Unknown actions keep the
    // pre-capabilities default (user / multi / non-streaming / no timeout).
    let action_meta = {
        let map = s.plugins.read().await;
        match map.get(&id) {
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "plugin not found" })),
                )
                    .into_response();
            }
            Some(rec) => rec
                .capabilities
                .as_ref()
                .and_then(|c| c.actions.iter().find(|a| a.id == action))
                .cloned(),
        }
    };

    let requires_admin = matches!(
        action_meta.as_ref().map(|a| a.requires_role),
        Some(hc_types::RequiresRole::Admin)
    );
    if requires_admin && !matches!(claims.role, hc_auth::user::Role::Admin) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "action requires admin role",
                "requires_role": "admin",
            })),
        )
            .into_response();
    }

    let Some(ref rpc) = s.management_rpc else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "management RPC not configured" })),
        )
            .into_response();
    };

    // Forward all fields except `action` as params.
    let mut params = body.clone();
    if let Some(obj) = params.as_object_mut() {
        obj.remove("action");
    }

    let is_streaming = action_meta.as_ref().map(|a| a.stream).unwrap_or(false);
    let concurrency = action_meta
        .as_ref()
        .map(|a| a.concurrency)
        .unwrap_or(hc_types::Concurrency::Multi);
    let timeout_ms = action_meta.as_ref().and_then(|a| a.timeout_ms);

    if is_streaming && matches!(concurrency, hc_types::Concurrency::Single) {
        if let Some(active_rid) = s.streaming_registry.active_single(&id, &action).await {
            let stream_topic = format!("homecore/plugins/{id}/commands/{active_rid}/events",);
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "busy",
                    "active_request_id": active_rid,
                    "stream_topic": stream_topic,
                })),
            )
                .into_response();
        }
    }

    // Mint the request_id here so streaming slot reservation races cleanly.
    let request_id = uuid::Uuid::new_v4().to_string();

    if is_streaming {
        s.streaming_registry
            .reserve(
                request_id.clone(),
                crate::streaming::StreamingEntry {
                    plugin_id: id.clone(),
                    action_id: action.clone(),
                    concurrency,
                },
            )
            .await;
        if let (Some(ms), Some(pub_handle)) = (timeout_ms, s.publish.clone()) {
            crate::streaming::schedule_timeout(
                &s.streaming_registry,
                pub_handle,
                id.clone(),
                request_id.clone(),
                ms,
            )
            .await;
        }
    }

    // Streaming actions keep the convention-applying path: a `status:"error"`
    // ack means the stream never started, so release the reserved slot and
    // report 504. For non-streaming actions, a plugin reply of ANY status is
    // the action's result — return it verbatim (HTTP 200). Only a genuine
    // no-response timeout is a 504; a plugin's own business error (e.g. "no
    // devices configured to discover") is not a gateway timeout.
    let outcome = if is_streaming {
        rpc.send_command_with_id(&id, &action, &request_id, timeout_ms, params)
            .await
    } else {
        rpc.send_command_raw_with_id(&id, &action, &request_id, timeout_ms, params)
            .await
    };
    match outcome {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            if is_streaming {
                s.streaming_registry.release(&request_id).await;
            }
            (StatusCode::GATEWAY_TIMEOUT, Json(json!({ "error": e }))).into_response()
        }
    }
}

pub async fn matter_commission(
    State(s): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(obj) = body.as_object() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "request body must be a JSON object" })),
        )
            .into_response();
    };

    let mut payload = serde_json::Map::new();
    payload.insert(
        "action".to_string(),
        Value::String("commission".to_string()),
    );
    for (k, v) in obj {
        payload.insert(k.clone(), v.clone());
    }

    let change = DeviceChange::homecore("api")
        .with_actor(Some(claims.uid), Some(claims.sub))
        .with_correlation_id(Some(Uuid::new_v4().to_string()));
    if let Err(e) = publish_device_command(
        &s,
        MATTER_CONTROLLER_DEVICE_ID,
        Value::Object(payload),
        change,
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "action": "commission" })),
    )
        .into_response()
}

pub async fn matter_reinterview(
    State(s): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(obj) = body.as_object() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "request body must be a JSON object" })),
        )
            .into_response();
    };

    let mut payload = serde_json::Map::new();
    payload.insert(
        "action".to_string(),
        Value::String("reinterview".to_string()),
    );
    for (k, v) in obj {
        payload.insert(k.clone(), v.clone());
    }

    let change = DeviceChange::homecore("api")
        .with_actor(Some(claims.uid), Some(claims.sub))
        .with_correlation_id(Some(Uuid::new_v4().to_string()));
    if let Err(e) = publish_device_command(
        &s,
        MATTER_CONTROLLER_DEVICE_ID,
        Value::Object(payload),
        change,
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "action": "reinterview" })),
    )
        .into_response()
}

pub async fn list_matter_nodes(State(s): State<AppState>, _: PluginsRead) -> impl IntoResponse {
    let device = match s.store.get_device(MATTER_CONTROLLER_DEVICE_ID).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "matter controller device not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let nodes = device
        .attributes
        .get("commissioned_nodes")
        .cloned()
        .unwrap_or_else(|| json!([]));

    (StatusCode::OK, Json(json!({ "nodes": nodes }))).into_response()
}

pub async fn remove_matter_node(
    State(s): State<AppState>,
    PluginsWrite(claims): PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if id.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "node id is required" })),
        )
            .into_response();
    }

    let payload = json!({
        "action": "remove_node",
        "node_id": id,
    });

    let change = DeviceChange::homecore("api")
        .with_actor(Some(claims.uid), Some(claims.sub))
        .with_correlation_id(Some(Uuid::new_v4().to_string()));
    if let Err(e) = publish_device_command(&s, MATTER_CONTROLLER_DEVICE_ID, payload, change).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "action": "remove_node" })),
    )
        .into_response()
}

// ---------- Area device assignment ----------

/// `PUT /api/v1/areas/{id}/devices`
/// Body: `["device_id_1", "device_id_2", ...]`
pub async fn set_area_devices(
    State(s): State<AppState>,
    _: AreasWrite,
    Path(id): Path<Uuid>,
    Json(device_ids): Json<Vec<String>>,
) -> impl IntoResponse {
    let area = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "area not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    let desired: HashSet<String> = device_ids.into_iter().collect();
    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    for device in &mut devices {
        let in_desired = desired.contains(&device.device_id);
        // Membership is by *effective* area — a user-assigned device is in the
        // room even though the plugin never said so.
        let in_area = area_matches(device.effective_area(), &area.name);

        if in_desired {
            if !in_area {
                // This is a user assigning a device to a room, so it writes the
                // override — the same contract as PATCH /devices. Writing the
                // plugin-owned `area` here meant the next registration silently
                // reverted the assignment.
                device.area_override = Some(area.name.clone());
                if let Err(e) = s.store.upsert_device(device).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        } else if in_area {
            // Dropping a device from the room clears the user's assignment. If
            // the plugin itself still reports this room the device stays in it
            // — the bridge owns that, per the bridge-wins rule.
            device.area_override = None;
            if area_matches(device.area.as_deref(), &area.name) {
                device.area = None;
            }
            if let Err(e) = s.store.upsert_device(device).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }

    if let Err(e) = s
        .store
        .upsert_area(&Area {
            id: area.id,
            name: area.name.clone(),
            device_ids: vec![],
        })
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // Return the updated derived area membership.
    let refreshed = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => Area {
            id,
            name: area.name,
            device_ids: vec![],
        },
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    (StatusCode::OK, Json(json!(refreshed))).into_response()
}

// ---------- Automation PATCH (enable/disable/priority) ----------

#[derive(Deserialize)]
pub struct PatchAutomationBody {
    pub enabled: Option<bool>,
    pub priority: Option<i32>,
}

/// `PATCH /api/v1/automations/{id}`
/// Allows partial update of `enabled` and/or `priority` without replacing the whole rule.
pub async fn patch_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchAutomationBody>,
) -> impl IntoResponse {
    match load_mode_definitions(&s) {
        Ok(definitions) => {
            if let Some(mode_id) = managed_rule_owner(&definitions, id) {
                return managed_rule_response(mode_id, id);
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    let Some(source_rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    // Read current rule from handle.
    let mut rule = match source_rh.read().await.iter().find(|r| r.id == id).cloned() {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "rule not found" })),
            )
                .into_response();
        }
    };

    if let Some(enabled) = patch.enabled {
        rule.enabled = enabled;
    }
    if let Some(priority) = patch.priority {
        rule.priority = priority;
    }

    // Persist to file.
    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule(&rule) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Update live handle.
    {
        let mut rules = source_rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = rule.clone();
        }
    }

    if let Some(rh) = &s.rules_handle {
        let compiled_rule = match rule_resolver::compile_rule_for_store(&s.store, &rule).await {
            Ok(rule) => rule,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };
        let mut rules = rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = compiled_rule;
        }
    }

    (StatusCode::OK, Json(json!(rule))).into_response()
}

// ---------- Bulk automation PATCH ----------

#[derive(Deserialize, Default)]
pub struct BulkPatchQuery {
    /// Apply only to rules that have this tag.  Ignored when `ids` is present in the body.
    pub tag: Option<String>,
    /// Opt in to patching *every* rule.  Required to hit all rules; without it,
    /// a request carrying no selector is rejected rather than assumed to mean
    /// "all".
    #[serde(default)]
    pub all: bool,
}

#[derive(Deserialize)]
pub struct BulkPatchBody {
    /// When present, apply only to these specific rule IDs (overrides `?tag=`).
    #[serde(default)]
    pub ids: Option<Vec<Uuid>>,
    pub enabled: Option<bool>,
}

/// `PATCH /api/v1/automations?tag=<tag>|all=true`
///
/// Bulk enable/disable rules.  Exactly one selector is required:
/// 1. `ids` field in body — explicit list of rule UUIDs (takes precedence)
/// 2. `?tag=<tag>` — all rules carrying that tag
/// 3. `?all=true` — every rule, stated deliberately
///
/// A request with no selector is a **400**.  It used to mean "all rules", so
/// `PATCH /automations {"enabled": false}` — a plausible typo, or a client that
/// forgot to send `ids` — silently disabled every automation in the house.  The
/// blast radius of the default was the whole system, so the default is now to
/// refuse.
///
/// Returns `{ "updated": N, "rules": [...] }`.
pub async fn bulk_patch_automations(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Query(params): Query<BulkPatchQuery>,
    Json(patch): Json<BulkPatchBody>,
) -> impl IntoResponse {
    // Refuse a request that names no target.  Hitting every rule must be said
    // out loud, not arrived at by omission.
    if patch.ids.is_none() && params.tag.is_none() && !params.all {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "no selector — send `ids` in the body, or ?tag=<tag>, or ?all=true to patch every rule"
            })),
        )
            .into_response();
    }

    let Some(source_rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    let managed_rule_ids = match load_mode_definitions(&s) {
        Ok(definitions) => definitions
            .iter()
            .flat_map(|definition| definition.generated_rule_ids.iter().copied())
            .collect::<HashSet<_>>(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let mut updated = Vec::new();
    {
        let mut rules = source_rh.write().await;
        for rule in rules.iter_mut() {
            let selected = if let Some(ref ids) = patch.ids {
                ids.contains(&rule.id)
            } else if let Some(ref tag) = params.tag {
                rule.tags.contains(tag)
            } else {
                // Only reachable with ?all=true — the no-selector case was
                // rejected above.
                params.all
            };
            if selected && !managed_rule_ids.contains(&rule.id) {
                if let Some(enabled) = patch.enabled {
                    rule.enabled = enabled;
                }
                updated.push(rule.clone());
            }
        }
    }

    // Persist each changed rule to its RON file.
    if let Some(fs) = &s.rule_file_store {
        for rule in &updated {
            if let Err(e) = fs.write_rule(rule) {
                tracing::warn!(rule_id = %rule.id, error = %e, "bulk_patch: failed to write rule file");
            }
        }
    }

    if let Some(rh) = &s.rules_handle {
        let mut compiled_updates = Vec::with_capacity(updated.len());
        for rule in &updated {
            match rule_resolver::compile_rule_for_store(&s.store, rule).await {
                Ok(rule) => compiled_updates.push(rule),
                Err(e) => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        }

        let mut compiled_rules = rh.write().await;
        for compiled_rule in compiled_updates {
            if let Some(pos) = compiled_rules.iter().position(|r| r.id == compiled_rule.id) {
                compiled_rules[pos] = compiled_rule;
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "updated": updated.len(), "rules": updated })),
    )
        .into_response()
}

// ---------- Automation fire history ----------

/// `GET /api/v1/automations/{id}/history`
///
/// Returns the last 20 evaluation records for the rule (newest last).
/// Each record contains `timestamp`, `trigger_type`, `trigger_context`,
/// `outcome` (fired/condition_failed/cooldown/paused/…), `conditions[]`
/// (per-condition pass/fail with actual vs expected values), `actions[]`
/// (per-action type, description, outcome, duration_ms), and `eval_ms`.
pub async fn automation_history(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(fh) = &s.fire_history else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "fire history not available" })),
        )
            .into_response();
    };

    // Verify rule exists.
    if let Some(rh) = &s.source_rules_handle {
        let rules = rh.read().await;
        if !rules.iter().any(|r| r.id == id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "rule not found" })),
            )
                .into_response();
        }
    }

    let history: Vec<_> = fh
        .get(&id)
        .map(|buf| buf.iter().cloned().collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(json!(history))).into_response()
}

// ---------- Clone automation ----------

/// `POST /api/v1/automations/{id}/clone`
///
/// Duplicates a rule with a new UUID.  The clone is disabled by default
/// to prevent accidental double-firing until the operator reviews it.
/// Returns `201 Created` with the new rule body.
pub async fn clone_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(source_rh) = &s.source_rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    let original = {
        let rules = source_rh.read().await;
        match rules.iter().find(|r| r.id == id).cloned() {
            Some(r) => r,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "rule not found" })),
                )
                    .into_response();
            }
        }
    };

    let mut cloned = original.clone();
    cloned.id = Uuid::new_v4();
    cloned.name = format!("Copy of {}", original.name);
    cloned.enabled = false; // disabled until operator reviews
    cloned.error = None;

    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule(&cloned) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    source_rh.write().await.push(cloned.clone());
    if let Some(rh) = &s.rules_handle {
        match rule_resolver::compile_rule_for_store(&s.store, &cloned).await {
            Ok(compiled) => rh.write().await.push(compiled),
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        }
    }
    (StatusCode::CREATED, Json(json!(cloned))).into_response()
}

// ---------- Stale device references ----------

/// `GET /api/v1/automations/stale-refs`
///
/// Pick the likely orphans among one plugin's devices, given how many that
/// plugin says it manages.  `devices` is reordered (oldest `last_seen` first).
///
/// `None` when the counts agree, or when the plugin claims more devices than
/// homeCore holds — that is a plugin mid-registration, not an orphan.
fn orphan_suspects<'a>(
    devices: &mut Vec<&'a DeviceState>,
    plugin_reports: usize,
) -> Option<Vec<&'a DeviceState>> {
    let core_holds = devices.len();
    if core_holds <= plugin_reports {
        return None;
    }
    // Orphans are the devices the plugin stopped republishing, so they fall to
    // the back of the pack as its live devices keep being refreshed.
    devices.sort_by_key(|d| d.last_seen);
    Some(
        devices
            .iter()
            .take(core_holds - plugin_reports)
            .copied()
            .collect(),
    )
}

/// `GET /api/v1/devices/orphaned`
///
/// An *orphaned* device is one homeCore still holds a registration and retained
/// state for, but which the owning plugin no longer manages — e.g. a Hue grouped
/// light after `publish_grouped_lights` is turned off.  Nothing else in the
/// system notices: the plugin subscribes to command topics per-device, so
/// commands published for an orphan have no subscriber and are dropped by the
/// broker, while its state stays frozen at whatever it last was and rule
/// conditions keep reading that as truth.  A toggle pair
/// (`on == false` → turn on / `on == true` → turn off) built on a frozen
/// attribute wedges permanently: one rule can never pass, the other always
/// does, and the rule engine records `fired: success` either way.
///
/// Detection is a **count comparison, not a staleness threshold**.  Every
/// management-capable plugin self-reports how many devices it manages
/// (`device_count`); if homeCore holds more devices for that plugin than the
/// plugin claims, the difference is exactly the orphan count.  A `last_seen`-age
/// rule was the obvious approach and is wrong: plugins that publish only on
/// change leave healthy devices untouched for months (a Caseta switch nobody
/// flips is not orphaned), so it produces false positives on precisely the
/// quiet devices operators care least about.
///
/// Identifying *which* devices are orphaned is necessarily a heuristic — the
/// plugin reports a count, not an id list — so the least-recently-seen
/// `orphan_count` devices are returned as `suspects`.  On real data the orphans
/// are frozen months behind their siblings and the ranking is unambiguous, but
/// callers should confirm before deleting.
///
/// Only `active` plugins with `supports_management` are checked: a stopped
/// plugin legitimately reports zero devices while homeCore still holds them,
/// and a plugin that never reports `device_count` would otherwise look
/// entirely orphaned.
pub async fn orphaned_devices(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    let devices = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let plugins = s.plugins.read().await;
    let now = chrono::Utc::now();
    let mut report = Vec::new();
    let mut total_orphans: usize = 0;

    for plugin in plugins.values() {
        if plugin.status != "active" || !plugin.supports_management {
            continue;
        }

        let mut owned: Vec<_> = devices
            .iter()
            .filter(|d| d.plugin_id == plugin.plugin_id)
            .collect();

        let core_holds = owned.len();
        let plugin_reports = plugin.device_count as usize;
        let Some(orphans) = orphan_suspects(&mut owned, plugin_reports) else {
            continue;
        };
        let orphan_count = orphans.len();
        total_orphans += orphan_count;

        let suspects: Vec<_> = orphans
            .iter()
            .map(|d| {
                json!({
                    "device_id":  d.device_id,
                    "name":       d.name,
                    "device_type": d.device_type,
                    "last_seen":  d.last_seen,
                    "stale_secs": (now - d.last_seen).num_seconds().max(0),
                })
            })
            .collect();

        report.push(json!({
            "plugin_id":      plugin.plugin_id,
            "status":         plugin.status,
            "plugin_reports": plugin_reports,
            "core_holds":     core_holds,
            "orphan_count":   orphan_count,
            "suspects":       suspects,
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "total_orphans": total_orphans,
            "plugins":       report,
        })),
    )
        .into_response()
}

/// Returns rules that reference device IDs not currently registered in the
/// device store.  Useful for finding automations broken by device
/// renames or deletions.
///
/// Response: `[{ rule_id, rule_name, stale_device_ids: [String] }]`
pub async fn stale_refs(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    let known_ids: HashSet<String> = match s.store.list_devices().await {
        Ok(devices) => devices.into_iter().map(|d| d.device_id).collect(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let rules = rh.read().await;
    let result: Vec<serde_json::Value> = rules
        .iter()
        .filter_map(|rule| {
            let stale: Vec<String> = collect_rule_device_refs(rule)
                .into_iter()
                .filter(|id| !id.starts_with("DELETED:") && !known_ids.contains(id.as_str()))
                .collect();
            if stale.is_empty() {
                None
            } else {
                Some(json!({
                    "rule_id":         rule.id,
                    "rule_name":       rule.name,
                    "stale_device_ids": stale,
                }))
            }
        })
        .collect();

    (StatusCode::OK, Json(json!(result))).into_response()
}

/// Collect every device ID referenced by a rule (trigger, conditions, actions).
fn collect_rule_device_refs(rule: &Rule) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    collect_trigger_refs(&rule.trigger, &mut ids);
    for cond in &rule.conditions {
        collect_condition_refs(cond, &mut ids);
    }
    for ra in &rule.actions {
        collect_action_refs(&ra.action, &mut ids);
    }
    ids.sort();
    ids.dedup();
    ids
}

fn collect_trigger_refs(trigger: &Trigger, ids: &mut Vec<String>) {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id,
            device_ids,
            ..
        } => {
            ids.push(device_id.clone());
            ids.extend_from_slice(device_ids);
        }
        Trigger::DeviceAvailabilityChanged { device_id, .. } => ids.push(device_id.clone()),
        Trigger::ButtonEvent { device_id, .. } => ids.push(device_id.clone()),
        Trigger::NumericThreshold { device_id, .. } => ids.push(device_id.clone()),
        _ => {}
    }
}

fn collect_condition_refs(cond: &Condition, ids: &mut Vec<String>) {
    match cond {
        Condition::DeviceState { device_id, .. }
        | Condition::TimeElapsed { device_id, .. }
        | Condition::DeviceLastChange { device_id, .. } => ids.push(device_id.clone()),
        Condition::Not { condition } => collect_condition_refs(condition, ids),
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => {
            for c in conditions {
                collect_condition_refs(c, ids);
            }
        }
        _ => {}
    }
}

fn collect_action_refs(action: &Action, ids: &mut Vec<String>) {
    match action {
        Action::SetDeviceState { device_id, .. } => ids.push(device_id.clone()),
        Action::SetDeviceStatePerMode { device_id, .. } => ids.push(device_id.clone()),
        Action::FadeDevice { device_id, .. } => ids.push(device_id.clone()),
        Action::CaptureDeviceState { device_ids, .. } => ids.extend_from_slice(device_ids),
        Action::Parallel { actions } => {
            for a in actions {
                collect_action_refs(a, ids);
            }
        }
        Action::RepeatUntil { actions, .. } => {
            for a in actions {
                collect_action_refs(a, ids);
            }
        }
        Action::RepeatWhile { actions, .. } => {
            for a in actions {
                collect_action_refs(a, ids);
            }
        }
        Action::RepeatCount { actions, .. } => {
            for a in actions {
                collect_action_refs(a, ids);
            }
        }
        Action::Conditional {
            then_actions,
            else_actions,
            else_if,
            ..
        } => {
            for a in then_actions {
                collect_action_refs(a, ids);
            }
            for a in else_actions {
                collect_action_refs(a, ids);
            }
            for branch in else_if {
                for a in &branch.actions {
                    collect_action_refs(a, ids);
                }
            }
        }
        Action::PingHost {
            then_actions,
            else_actions,
            ..
        } => {
            for a in then_actions {
                collect_action_refs(a, ids);
            }
            for a in else_actions {
                collect_action_refs(a, ids);
            }
        }
        _ => {}
    }
}

// ---------- Rule groups ----------

/// `GET /api/v1/automations/groups`
pub async fn list_groups(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    match &s.rule_groups {
        Some(rg) => {
            let groups = rg.read().await;
            (StatusCode::OK, Json(json!(*groups))).into_response()
        }
        None => (StatusCode::OK, Json(json!([]))).into_response(),
    }
}

/// `POST /api/v1/automations/groups`
pub async fn create_group(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(mut group): Json<RuleGroup>,
) -> impl IntoResponse {
    let Some(rg) = &s.rule_groups else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "group store not available" })),
        )
            .into_response();
    };
    group.id = Uuid::new_v4();
    let mut groups = rg.write().await;
    groups.push(group.clone());
    if let Some(gs) = &s.group_store {
        if let Err(e) = gs.save(&groups) {
            tracing::warn!(error = %e, "create_group: failed to persist groups");
        }
    }
    (StatusCode::CREATED, Json(json!(group))).into_response()
}

/// `GET /api/v1/automations/groups/{id}`
pub async fn get_group(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rg) = &s.rule_groups else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    let groups = rg.read().await;
    match groups.iter().find(|g| g.id == id).cloned() {
        Some(g) => (StatusCode::OK, Json(json!(g))).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response(),
    }
}

/// `PATCH /api/v1/automations/groups/{id}`
///
/// Update group metadata (name, description, rule_ids).  Does not toggle rules.
#[derive(Deserialize)]
pub struct GroupPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub rule_ids: Option<Vec<Uuid>>,
}

pub async fn patch_group(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(patch): Json<GroupPatch>,
) -> impl IntoResponse {
    let Some(rg) = &s.rule_groups else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    let mut groups = rg.write().await;
    let Some(g) = groups.iter_mut().find(|g| g.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    if let Some(name) = patch.name {
        g.name = name;
    }
    if let Some(desc) = patch.description {
        g.description = Some(desc);
    }
    if let Some(ids) = patch.rule_ids {
        g.rule_ids = ids;
    }
    let updated = g.clone();
    if let Some(gs) = &s.group_store {
        if let Err(e) = gs.save(&groups) {
            tracing::warn!(error = %e, "patch_group: failed to persist groups");
        }
    }
    (StatusCode::OK, Json(json!(updated))).into_response()
}

/// `DELETE /api/v1/automations/groups/{id}`
pub async fn delete_group(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rg) = &s.rule_groups else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    let mut groups = rg.write().await;
    let before = groups.len();
    groups.retain(|g| g.id != id);
    if groups.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    }
    if let Some(gs) = &s.group_store {
        if let Err(e) = gs.save(&groups) {
            tracing::warn!(error = %e, "delete_group: failed to persist groups");
        }
    }
    (StatusCode::OK, Json(json!({ "deleted": true }))).into_response()
}

/// `POST /api/v1/automations/groups/{id}/enable`
/// `POST /api/v1/automations/groups/{id}/disable`
///
/// Apply `enabled = true/false` to every rule in the group.
pub async fn set_group_enabled(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path((id, action)): Path<(Uuid, String)>,
) -> impl IntoResponse {
    let enabled =
        match action.as_str() {
            "enable" => true,
            "disable" => false,
            other => return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "error": format!("unknown action '{other}'; use enable or disable") }),
                ),
            )
                .into_response(),
        };

    let Some(rg) = &s.rule_groups else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    let groups = rg.read().await;
    let Some(group) = groups.iter().find(|g| g.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "group not found" })),
        )
            .into_response();
    };
    let rule_ids = group.rule_ids.clone();
    drop(groups);

    let Some(rh) = &s.rules_handle else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "rule engine not available" })),
        )
            .into_response();
    };

    let mut updated = Vec::new();
    {
        let mut rules = rh.write().await;
        for rule in rules.iter_mut() {
            if rule_ids.contains(&rule.id) {
                rule.enabled = enabled;
                updated.push(rule.clone());
            }
        }
    }

    if let Some(fs) = &s.rule_file_store {
        for rule in &updated {
            if let Err(e) = fs.write_rule(rule) {
                tracing::warn!(rule_id = %rule.id, error = %e, "set_group_enabled: failed to write rule file");
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "enabled": enabled, "updated": updated.len(), "rules": updated })),
    )
        .into_response()
}

// ---------- Webhooks ----------

/// `POST /api/v1/webhooks/{path}`
///
/// Any POST to this endpoint fires a `Custom` event with `event_type = "webhook"` and
/// `payload = { "path": "...", "body": <request body>, "query": { ... } }`.  Rules with
/// `Trigger::WebhookReceived { path }` will match when the path matches.
///
/// In Rhai scripts:
/// - `trigger_value()` returns the request body (or `()` when empty)
/// - `trigger_extra()` returns a map of query-string parameters
pub async fn receive_webhook(
    State(s): State<AppState>,
    Path(path): Path<String>,
    Query(query_params): Query<std::collections::HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let body_value = body.map(|b| b.0).unwrap_or(Value::Null);
    let query_value: Value = if query_params.is_empty() {
        Value::Null
    } else {
        Value::Object(
            query_params
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    };
    let event = hc_types::event::Event::Custom {
        timestamp: chrono::Utc::now(),
        event_type: "webhook".into(),
        payload: json!({ "path": path, "body": body_value, "query": query_value }),
    };
    let _ = s.event_bus.publish(event);
    (
        StatusCode::OK,
        Json(json!({ "status": "accepted", "path": path })),
    )
}

// ---------- Events log ----------

/// `GET /api/v1/events`
///
/// Query parameters:
/// - `limit`     — max entries to return (default 50, max 1000)
/// - `type`      — comma-separated event type names (e.g. `device_state_changed,rule_fired`)
/// - `device_id` — only events for this device
pub async fn list_events(
    State(s): State<AppState>,
    _: DevicesRead,
    axum::extract::Query(query): axum::extract::Query<crate::event_log::EventLogQuery>,
) -> impl IntoResponse {
    let entries = s.event_log.query(&query);
    (StatusCode::OK, Json(json!(entries)))
}

// ---------- Calendars ----------

/// `GET /api/v1/calendars`
///
/// Lists all loaded calendars with metadata and event counts.
pub async fn list_calendars(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    let Some(cal_handle) = &s.calendar else {
        return (StatusCode::OK, Json(json!([]))).into_response();
    };
    let calendars = cal_handle.read().await;
    let list: Vec<Value> = calendars
        .iter()
        .map(|c| {
            json!({
                "id":            c.id,
                "event_count":   c.events.len(),
                "upcoming_count": c.upcoming_count(),
                "source_url":    c.source_url,
                "fetched_at":    c.fetched_at,
                "loaded_at":     c.loaded_at,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!(list))).into_response()
}

/// `POST /api/v1/calendars/fetch`
///
/// Fetch an ICS file from a URL, save it to the calendar directory, reload.
///
/// Body: `{ "url": "https://...", "name": "my_cal", "refresh_hours": 24 }`
/// (`name` and `refresh_hours` are optional.)
#[derive(serde::Deserialize)]
pub struct FetchCalendarBody {
    pub url: String,
    pub name: Option<String>,
    pub refresh_hours: Option<u64>,
}

pub async fn fetch_calendar(
    State(s): State<AppState>,
    AutomationsWrite(claims): AutomationsWrite,
    Json(body): Json<FetchCalendarBody>,
) -> impl IntoResponse {
    let (Some(cal_handle), Some(cal_dir)) = (&s.calendar, &s.calendar_dir) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "calendar store not configured" })),
        )
            .into_response();
    };

    let expansion_days = s.calendar_expansion_days;
    let dir = cal_dir.as_ref().clone();

    match hc_core::calendar_store::fetch_and_save(
        &body.url,
        body.name.as_deref(),
        &dir,
        expansion_days,
        body.refresh_hours,
    )
    .await
    {
        Ok(entry) => {
            let id = entry.id.clone();
            let event_count = entry.events.len();
            // Upsert into the live handle.
            let mut calendars = cal_handle.write().await;
            if let Some(slot) = calendars.iter_mut().find(|c| c.id == id) {
                *slot = entry;
            } else {
                calendars.push(entry);
            }
            drop(calendars);

            let mut audit_e = audit::entry_from_claims(&claims, "calendar.fetched")
                .with_target("calendar", id.clone());
            audit_e.detail = json!({
                "url":         body.url,
                "event_count": event_count,
            });
            audit::emit(&s, audit_e).await;

            (
                StatusCode::OK,
                Json(json!({
                    "calendar_id": id,
                    "event_count": event_count,
                    "saved_path":  dir.join(format!("{id}.ics")).display().to_string(),
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `POST /api/v1/calendars/upload`
///
/// Upload an ICS file directly as text.  Saves it to the calendar directory
/// and reloads the live store.
///
/// Body: `{ "content": "BEGIN:VCALENDAR...", "name": "my_cal" }`
/// (`name` is optional; derived from content if omitted.)
#[derive(serde::Deserialize)]
pub struct UploadCalendarBody {
    pub content: String,
    pub name: Option<String>,
}

pub async fn upload_calendar(
    State(s): State<AppState>,
    AutomationsWrite(claims): AutomationsWrite,
    Json(body): Json<UploadCalendarBody>,
) -> impl IntoResponse {
    let (Some(cal_handle), Some(cal_dir)) = (&s.calendar, &s.calendar_dir) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "calendar store not configured" })),
        )
            .into_response();
    };

    if !body.content.contains("BEGIN:VCALENDAR") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "content does not appear to be a valid ICS file (missing BEGIN:VCALENDAR)" })),
        )
            .into_response();
    }

    let cal_name = body
        .name
        .as_deref()
        .unwrap_or("uploaded_calendar")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();

    let ics_path = cal_dir.as_ref().join(format!("{cal_name}.ics"));
    if let Err(e) = std::fs::write(&ics_path, &body.content) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to write ICS file: {e}") })),
        )
            .into_response();
    }

    // Write meta sidecar (no source URL since this was uploaded).
    let meta = hc_core::calendar_store::CalendarMeta {
        source_url: None,
        fetched_at: None,
        refresh_hours: None,
    };
    let meta_path = cal_dir.as_ref().join(format!("{cal_name}.meta.json"));
    let _ = std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );

    // The file watcher will auto-reload, but also upsert into live handle immediately.
    let expansion_days = s.calendar_expansion_days;
    let dir = cal_dir.as_ref().clone();
    match tokio::task::spawn_blocking(move || {
        hc_core::calendar_store::load_dir(&dir, expansion_days)
    })
    .await
    {
        Ok(Ok(entries)) => {
            let event_count = entries
                .iter()
                .find(|e| e.id == cal_name)
                .map(|e| e.events.len())
                .unwrap_or(0);
            *cal_handle.write().await = entries;

            let mut audit_e = audit::entry_from_claims(&claims, "calendar.uploaded")
                .with_target("calendar", cal_name.clone());
            audit_e.detail = json!({
                "event_count": event_count,
                "bytes":       body.content.len(),
            });
            audit::emit(&s, audit_e).await;

            (
                StatusCode::OK,
                Json(json!({
                    "calendar_id": cal_name,
                    "event_count": event_count,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("calendar reload failed: {e}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("calendar reload task failed: {e}") })),
        )
            .into_response(),
    }
}

/// `DELETE /api/v1/calendars/:id`
///
/// Remove a calendar's `.ics` and `.meta.json` from disk and from the live
/// store.  Returns a warning list of rules that reference `calendar_id`.
pub async fn delete_calendar(
    State(s): State<AppState>,
    AutomationsWrite(claims): AutomationsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (Some(cal_handle), Some(cal_dir)) = (&s.calendar, &s.calendar_dir) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "calendar store not configured" })),
        )
            .into_response();
    };

    let dir = cal_dir.as_ref();
    let ics_path = dir.join(format!("{id}.ics"));
    let meta_path = dir.join(format!("{id}.meta.json"));

    // Check calendar exists in the live store.
    {
        let calendars = cal_handle.read().await;
        if !calendars.iter().any(|c| c.id == id) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("calendar '{}' not found", id) })),
            )
                .into_response();
        }
    }

    // Delete files (non-fatal if already missing).
    if let Err(e) = std::fs::remove_file(&ics_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to delete {}: {}", ics_path.display(), e) })),
            )
                .into_response();
        }
    }
    let _ = std::fs::remove_file(&meta_path); // best-effort

    // Remove from live store.
    {
        let mut calendars = cal_handle.write().await;
        calendars.retain(|c| c.id != id);
    }

    // Check for rules that reference this calendar_id (warn only, don't delete).
    let referencing_rules: Vec<Value> = if let Some(rh) = &s.rules_handle {
        let rules = rh.read().await;
        rules
            .iter()
            .filter_map(|r| {
                if let hc_types::rule::Trigger::CalendarEvent {
                    calendar_id: Some(cid),
                    ..
                } = &r.trigger
                {
                    if cid == &id {
                        return Some(json!({ "rule_id": r.id, "rule_name": r.name }));
                    }
                }
                None
            })
            .collect()
    } else {
        vec![]
    };

    let mut audit_e =
        audit::entry_from_claims(&claims, "calendar.deleted").with_target("calendar", id.clone());
    audit_e.detail = json!({
        "referencing_rules": referencing_rules.len(),
    });
    audit::emit(&s, audit_e).await;

    (
        StatusCode::OK,
        Json(json!({
            "deleted": id,
            "referencing_rules": referencing_rules,
        })),
    )
        .into_response()
}

/// `GET /api/v1/calendars/:id/events`
///
/// List events from a single calendar.  Query params: `from`, `to` (ISO-8601),
/// `limit` (default 100, max 1000).
#[derive(serde::Deserialize, Default)]
pub struct CalendarEventsQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<usize>,
}

pub async fn list_calendar_events(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<String>,
    Query(q): Query<CalendarEventsQuery>,
) -> impl IntoResponse {
    let Some(cal_handle) = &s.calendar else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "calendar store not configured" })),
        )
            .into_response();
    };

    let calendars = cal_handle.read().await;
    let Some(cal) = calendars.iter().find(|c| c.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("calendar '{}' not found", id) })),
        )
            .into_response();
    };

    let now = chrono::Utc::now();
    let from = q.from.unwrap_or(now);
    let to = q.to.unwrap_or_else(|| now + chrono::Duration::days(400));
    let limit = q.limit.unwrap_or(100).min(1000);

    let events: Vec<Value> = cal
        .events
        .iter()
        .filter(|e| e.start >= from && e.start <= to)
        .take(limit)
        .map(|e| {
            json!({
                "uid":        e.uid,
                "summary":    e.summary,
                "start":      e.start,
                "is_all_day": e.is_all_day,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "calendar_id": id,
            "events": events,
            "total":  events.len(),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// System config + restart  (admin-only; modify homecore.toml at runtime)
// ---------------------------------------------------------------------------

/// `GET /api/v1/system/config`
///
/// Returns the current homecore.toml — both the raw text (for the raw
/// editor in the admin UI) and a parsed JSON view (for the structured
/// per-section forms to bind against). Admin-only.
pub async fn get_system_config(
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

    let Some(path) = s.homecore_config_path.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "config path not available — hc-core started without --config" }),
            ),
        )
            .into_response();
    };

    let raw = match std::fs::read_to_string(path.as_path()) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("read failed: {e}") })),
            )
                .into_response();
        }
    };

    let parsed: Value = raw
        .parse::<toml::Value>()
        .ok()
        .and_then(|t| serde_json::to_value(t).ok())
        .unwrap_or(Value::Null);

    (
        StatusCode::OK,
        Json(json!({
            "raw":    raw,
            "parsed": parsed,
            "path":   path.display().to_string(),
        })),
    )
        .into_response()
}

/// `PUT /api/v1/system/config`
///
/// Two write modes, mutually exclusive in the body:
///
/// - `{ "patch": { "section_name": { "field": value, ... }, ... } }`
///   Surgical edit via toml_edit — preserves comments, ordering, and
///   sections you didn't touch. Use for per-section Save buttons.
///
/// - `{ "raw": "..." }` Full-file replace. Use for the raw editor.
///
/// Both modes validate that the result parses as TOML before writing.
/// Admin-only. Returns the new content + a `restart_required` flag
/// that's true unless the only thing changed is hot-reloadable
/// (rules dir, modes — both already hot-reload elsewhere; this
/// endpoint is for the static config so the answer is essentially
/// always true for v0.1.0).
pub async fn put_system_config(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }

    let Some(path) = s.homecore_config_path.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "config path not available — hc-core started without --config" }),
            ),
        )
            .into_response();
    };

    let path = path.as_path();

    // Resolve patch / raw / array_of_tables mode (mutually exclusive).
    let modes_set = [
        body.get("patch").is_some(),
        body.get("raw").is_some(),
        body.get("array_of_tables").is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if modes_set > 1 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "specify exactly one of `patch`, `raw`, or `array_of_tables`" })),
        )
            .into_response();
    }

    let new_raw: String = if let Some(patch) = body.get("patch") {
        let current = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("read failed: {e}") })),
                )
                    .into_response();
            }
        };
        match crate::config_writer::apply_section_patch(&current, patch) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": format!("patch failed: {e}") })),
                )
                    .into_response();
            }
        }
    } else if let Some(raw) = body.get("raw") {
        let Some(s) = raw.as_str() else {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "`raw` must be a string" })),
            )
                .into_response();
        };
        s.to_string()
    } else if let Some(aot) = body.get("array_of_tables") {
        let section = aot.get("section").and_then(|v| v.as_str());
        let items = aot.get("items").and_then(|v| v.as_array());
        let (Some(section), Some(items)) = (section, items) else {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": "`array_of_tables` requires `section` (string) and `items` (array)"
                })),
            )
                .into_response();
        };
        let current = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("read failed: {e}") })),
                )
                    .into_response();
            }
        };
        match crate::config_writer::replace_array_of_tables(&current, section, items) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": format!("array_of_tables write failed: {e}") })),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "body must include `patch`, `raw`, or `array_of_tables`" })),
        )
            .into_response();
    };

    // Validate the result parses as TOML. Full AppConfig deserialize
    // happens at next core start; we only catch the shallow class of
    // errors here (typos, missing quotes) so the operator gets
    // immediate feedback without rejecting valid-but-unusual configs.
    if let Err(e) = new_raw.parse::<toml::Value>() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("TOML parse error: {e}") })),
        )
            .into_response();
    }

    // Atomic write via the same helper plugin-toggle persistence uses.
    if let Err(e) = crate::config_writer::write_atomic(path, new_raw.as_bytes()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("write failed: {e}") })),
        )
            .into_response();
    }

    tracing::info!(path = %path.display(), "homecore.toml updated via PUT /system/config");

    // Audit — record what the operator changed so the audit tab can
    // surface "who flipped X at when" without leaking secrets. We
    // record the mode + section path / section list, never the field
    // values (which can include passwords, API tokens, etc.).
    let detail = if let Some(patch) = body.get("patch").and_then(|v| v.as_object()) {
        json!({
            "mode": "patch",
            "sections": patch.keys().cloned().collect::<Vec<_>>(),
        })
    } else if body.get("raw").is_some() {
        json!({
            "mode": "raw",
            "bytes": new_raw.len(),
        })
    } else if let Some(aot) = body.get("array_of_tables") {
        let section = aot.get("section").and_then(|v| v.as_str()).unwrap_or("");
        let count = aot
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        json!({
            "mode": "array_of_tables",
            "section": section,
            "items": count,
        })
    } else {
        json!({})
    };
    let mut e = audit::entry_from_claims(&claims, "system.config_updated")
        .with_target("config", "homecore.toml");
    e.detail = detail;
    audit::emit(&s, e).await;

    (
        StatusCode::OK,
        Json(json!({
            "raw": new_raw,
            "restart_required": true,
        })),
    )
        .into_response()
}

/// `POST /api/v1/system/restart`
///
/// Triggers graceful shutdown via the broadcast channel hc-core's
/// signal handler also uses. The runtime supervisor (systemd /
/// docker / hand-rolled) is expected to spawn the process again.
/// Returns 202 — the actual exit happens after the response flushes.
pub async fn system_restart(
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

    let Some(tx) = s.shutdown_tx.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "shutdown channel not configured" })),
        )
            .into_response();
    };

    tracing::warn!(actor = %claims.sub, "Restart requested via /system/restart");

    let audit_e = audit::entry_from_claims(&claims, "system.restart_requested")
        .with_target("system", "homecore");
    audit::emit(&s, audit_e).await;

    // Spawn the broadcast on a delay so the HTTP response can flush
    // before the server starts tearing down.
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let _ = tx.send(true);
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "message": "shutdown requested; supervisor should respawn within seconds",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_json_secrets_realigns_after_array_element_removed() {
        // Current stored config: two bridges, each with a real key.
        let current = serde_json::json!({
            "bridges": [
                { "bridge_id": "aaa", "app_key": "KEY_A" },
                { "bridge_id": "bbb", "app_key": "KEY_B" },
            ]
        });
        // Client removed bridges[0] and echoed the survivor's key as redacted.
        // Positionally, incoming[0] (bbb) would line up with current[0] (aaa)
        // and wrongly receive KEY_A — identity matching must keep KEY_B.
        let mut incoming = serde_json::json!({
            "bridges": [
                { "bridge_id": "bbb", "app_key": "__redacted__" },
            ]
        });
        restore_json_secrets(&mut incoming, &current);
        assert_eq!(incoming["bridges"][0]["app_key"], "KEY_B");
    }

    #[test]
    fn restore_json_secrets_positional_fallback_without_identity() {
        // No id fields → fall back to positional matching (unchanged behaviour).
        let current = serde_json::json!({ "keys": [{ "app_key": "K0" }, { "app_key": "K1" }] });
        let mut incoming =
            serde_json::json!({ "keys": [{ "app_key": "__redacted__" }, { "app_key": "K1b" }] });
        restore_json_secrets(&mut incoming, &current);
        assert_eq!(incoming["keys"][0]["app_key"], "K0");
        assert_eq!(incoming["keys"][1]["app_key"], "K1b");
    }

    use crate::auth_middleware::{
        whitelist_claims, AreasRead, AreasWrite, AuthUser, AutomationsWrite, DashboardsWrite,
        DevicesRead, DevicesWrite,
    };
    use crate::dashboard_store::{DashboardStore, DashboardStoreData};
    use axum::extract::{Path, Query, State};
    use axum::response::IntoResponse;
    use chrono::Utc;
    use hc_auth::{Claims, JwtService, Role};
    use hc_core::EventBus;
    use hc_types::dashboard::{
        DashboardBreakpoint, DashboardDefinition, DashboardLayout, DashboardResponse,
        DashboardWidget, DashboardWidgetPlacement,
    };
    use hc_types::device::DeviceState;
    use http_body_util::BodyExt;
    use serde::de::DeserializeOwned;
    use serde_json::json;
    use uuid::Uuid;

    fn temp_db_paths(prefix: &str) -> (String, String) {
        let base =
            std::env::temp_dir().join(format!("hc_api_handlers_{prefix}_{}", Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&base);
        (
            base.join("state.redb").to_string_lossy().to_string(),
            base.join("history.sqlite").to_string_lossy().to_string(),
        )
    }

    async fn mk_state() -> AppState {
        let (state_db, history_db) = temp_db_paths("areas");
        let store = hc_state::StateStore::open(&state_db, &history_db)
            .await
            .expect("state store opens");
        let bus = EventBus::new(128);
        let jwt = JwtService::new_hs256(b"test-secret-key-32-bytes-minimum!", 24);
        let dashboard_store = DashboardStore::new(
            std::env::temp_dir().join(format!("hc_api_dashboards_{}.json", Uuid::new_v4())),
        );
        AppState::new(store, bus, None, None, None, None, jwt, vec![], None)
            .with_dashboard_store(dashboard_store, DashboardStoreData::default())
    }

    // A config with the shapes that actually occur: a secret at top level, one
    // inside a table, decoy keys that merely *reference* a secret, and repeated
    // [[array]] tables each carrying their own distinct token.
    const SAMPLE_CONFIG: &str = r#"# Hue plugin
app_key = "REAL-HUE-APP-KEY"
verify_tls = true

[mqtt]
host = "10.0.0.1"
password = "REAL-MQTT-PASSWORD"
key_path = "/etc/homecore/broker.key"   # a path, not a secret
token_url = "https://example.test/oauth"
refresh_token_expiry_days = 30

[[notify.channels]]
kind = "pushover"
token = "TOKEN-ONE"

[[notify.channels]]
kind = "telegram"
token = "TOKEN-TWO"
"#;

    #[test]
    fn secret_keys_exclude_things_that_merely_point_at_secrets() {
        for k in [
            "password",
            "app_key",
            "client_secret",
            "api_token",
            "user_key",
            "password_hash",
        ] {
            assert!(is_secret_key(k), "{k} should be treated as a secret");
        }
        // Redacting any of these would corrupt the config and protect nothing.
        for k in [
            "key_path",
            "cert_path",
            "capath",
            "token_url",
            "token_type",
            "refresh_token_expiry_days",
            "host",
            "kind",
        ] {
            assert!(!is_secret_key(k), "{k} must NOT be redacted");
        }
    }

    #[test]
    fn redaction_hides_secrets_but_keeps_the_document_intact() {
        let out = redact_toml_text(SAMPLE_CONFIG);
        for secret in [
            "REAL-HUE-APP-KEY",
            "REAL-MQTT-PASSWORD",
            "TOKEN-ONE",
            "TOKEN-TWO",
        ] {
            assert!(!out.contains(secret), "{secret} leaked through redaction");
        }
        // Comments, layout, and non-secret values survive — the admin UI shows
        // this text verbatim in an editor.
        assert!(out.contains("# Hue plugin"));
        assert!(out.contains("/etc/homecore/broker.key"));
        assert!(out.contains("https://example.test/oauth"));
        assert!(out.contains("refresh_token_expiry_days = 30"));
        assert!(out.contains("[[notify.channels]]"));
    }

    #[test]
    fn saving_a_redacted_config_does_not_destroy_the_secrets() {
        // Exactly what the admin UI does: GET the config, edit one visible
        // field, PUT the text back. Every secret it never saw must survive.
        let shown = redact_toml_text(SAMPLE_CONFIG);
        let edited = shown.replace("host = \"10.0.0.1\"", "host = \"10.0.0.9\"");
        let written = restore_toml_secrets(&edited, SAMPLE_CONFIG);

        assert!(written.contains("app_key = \"REAL-HUE-APP-KEY\""));
        assert!(written.contains("password = \"REAL-MQTT-PASSWORD\""));
        assert!(written.contains("host = \"10.0.0.9\""), "edit was lost");
        assert!(
            !written.contains(REDACTED_SECRET),
            "placeholder was written to disk"
        );
    }

    #[test]
    fn repeated_array_tables_keep_their_own_secrets() {
        // Both channels have a key literally named `token`. Keyed on name alone
        // they would collapse and one channel would inherit the other's secret.
        let written = restore_toml_secrets(&redact_toml_text(SAMPLE_CONFIG), SAMPLE_CONFIG);
        let one = written.find("TOKEN-ONE").expect("first token restored");
        let two = written.find("TOKEN-TWO").expect("second token restored");
        assert!(one < two, "the two channels' tokens were swapped");
        assert_eq!(written.matches("TOKEN-ONE").count(), 1);
        assert_eq!(written.matches("TOKEN-TWO").count(), 1);
    }

    #[test]
    fn json_config_round_trips_secrets_too() {
        let current = json!({
            "app_key": "REAL-KEY",
            "mqtt": { "password": "REAL-PW", "host": "10.0.0.1" },
        });
        let mut shown = current.clone();
        redact_json_secrets(&mut shown);
        assert_eq!(shown["app_key"], json!(REDACTED_SECRET));
        assert_eq!(shown["mqtt"]["password"], json!(REDACTED_SECRET));
        assert_eq!(shown["mqtt"]["host"], json!("10.0.0.1"));

        assert!(has_redaction(&shown));
        let mut incoming = shown.clone();
        incoming["mqtt"]["host"] = json!("10.0.0.9");
        restore_json_secrets(&mut incoming, &current);

        assert_eq!(incoming["app_key"], json!("REAL-KEY"));
        assert_eq!(incoming["mqtt"]["password"], json!("REAL-PW"));
        assert_eq!(incoming["mqtt"]["host"], json!("10.0.0.9"));
        assert!(!has_redaction(&incoming));
    }

    fn device_seen_days_ago(id: &str, days: i64) -> DeviceState {
        let mut d = DeviceState::new(id, id, "plugin.test");
        d.last_seen = Utc::now() - chrono::Duration::days(days);
        d
    }

    #[test]
    fn orphan_suspects_is_silent_when_counts_agree() {
        let a = device_seen_days_ago("fresh", 0);
        let b = device_seen_days_ago("quiet", 90);
        let mut owned = vec![&a, &b];
        // A device untouched for 90 days is NOT an orphan while the plugin still
        // claims it — plugins that publish only on change leave healthy devices
        // cold for months. Staleness alone must never raise a flag.
        assert!(orphan_suspects(&mut owned, 2).is_none());
    }

    #[test]
    fn orphan_suspects_returns_the_least_recently_seen() {
        let live_a = device_seen_days_ago("live_a", 0);
        let live_b = device_seen_days_ago("live_b", 0);
        let ghost_old = device_seen_days_ago("ghost_old", 100);
        let ghost_older = device_seen_days_ago("ghost_older", 120);
        let mut owned = vec![&live_a, &ghost_old, &live_b, &ghost_older];

        // Plugin manages 2, core holds 4 → the 2 coldest are the orphans.
        let found = orphan_suspects(&mut owned, 2).expect("orphans");
        let ids: Vec<&str> = found.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(ids, vec!["ghost_older", "ghost_old"]);
    }

    #[test]
    fn orphan_suspects_ignores_a_plugin_still_registering() {
        let a = device_seen_days_ago("a", 0);
        let mut owned = vec![&a];
        // Plugin claims 5 but core only knows 1 — mid-registration, not orphaned.
        assert!(orphan_suspects(&mut owned, 5).is_none());
    }

    async fn seed_device(state: &AppState, id: &str, area: Option<&str>) {
        let mut d = DeviceState::new(id, id, "plugin.test");
        d.available = true;
        d.last_seen = Utc::now();
        d.area = area.map(str::to_string);
        state.store.upsert_device(&d).await.expect("seed device");
    }

    async fn parse_json<T: DeserializeOwned>(resp: axum::response::Response) -> T {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("body collect")
            .to_bytes();
        serde_json::from_slice::<T>(&bytes).expect("json parse")
    }

    fn claims_for(uid: &str, role: Role) -> Claims {
        Claims {
            sub: uid.to_string(),
            uid: uid.to_string(),
            exp: u64::MAX,
            role,
            scopes: role.scopes(),
            actor: None,
        }
    }

    fn sample_dashboard(id: &str, owner_user_id: &str) -> DashboardDefinition {
        DashboardDefinition {
            id: id.to_string(),
            name: "Home".to_string(),
            description: Some("Test dashboard".to_string()),
            owner_user_id: owner_user_id.to_string(),
            tags: vec!["home".to_string()],
            icon: "dashboard".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            layouts: vec![DashboardLayout {
                breakpoint: DashboardBreakpoint::Desktop,
                columns: 12,
                row_height: 160.0,
                gap: 12.0,
                placements: vec![DashboardWidgetPlacement {
                    widget_id: "summary".to_string(),
                    x: 0,
                    y: 0,
                    w: 12,
                    h: 1,
                }],
            }],
            widgets: vec![DashboardWidget {
                id: "summary".to_string(),
                r#type: "stat_summary".to_string(),
                title: "Summary".to_string(),
                subtitle: None,
                config: json!({"metrics":["devices"]}),
            }],
        }
    }

    async fn seed_dashboard(state: &AppState, dashboard: DashboardDefinition) {
        let handle = state.dashboards.as_ref().expect("dashboard handle");
        handle.write().await.dashboards.push(dashboard);
    }

    fn sample_web_dashboard_payload(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "name": "Getting Started",
            "description": "Starter dashboard with onboarding and status widgets.",
            "owner_user_id": "ignored-by-server",
            "visibility": "private",
            "tags": ["starter", "home", "overview"],
            "icon": "home",
            "is_default": true,
            "created_at": "2026-03-30T16:00:00Z",
            "updated_at": "2026-03-30T16:00:00Z",
            "layouts": [
                {
                    "breakpoint": "mobile",
                    "columns": 1,
                    "row_height": 140.0,
                    "gap": 12.0,
                    "placements": [
                        {"widget_id": "welcome", "x": 0, "y": 0, "w": 1, "h": 2},
                        {"widget_id": "summary", "x": 0, "y": 2, "w": 1, "h": 1},
                        {"widget_id": "links", "x": 0, "y": 3, "w": 1, "h": 1}
                    ]
                },
                {
                    "breakpoint": "desktop",
                    "columns": 12,
                    "row_height": 160.0,
                    "gap": 12.0,
                    "placements": [
                        {"widget_id": "welcome", "x": 0, "y": 0, "w": 12, "h": 2},
                        {"widget_id": "summary", "x": 0, "y": 2, "w": 7, "h": 1},
                        {"widget_id": "links", "x": 7, "y": 2, "w": 5, "h": 1}
                    ]
                }
            ],
            "widgets": [
                {
                    "id": "welcome",
                    "type": "markdown",
                    "title": "Welcome",
                    "refresh_policy": "passive",
                    "config": {"markdown": "## Welcome to HomeCore"}
                },
                {
                    "id": "summary",
                    "type": "stat_summary",
                    "title": "Home Summary",
                    "refresh_policy": "live",
                    "config": {"metrics": ["devices", "on", "offline"]}
                },
                {
                    "id": "links",
                    "type": "dashboard_link",
                    "title": "Next Steps",
                    "refresh_policy": "passive",
                    "config": {"dashboard_ids": []}
                }
            ]
        })
    }

    #[tokio::test]
    async fn bulk_patch_without_a_selector_is_refused() {
        let state = mk_state().await;

        // This exact request used to mean "every rule in the system", so a
        // client that forgot to send `ids` disabled every automation in the
        // house and got a 200 for it.
        let resp = bulk_patch_automations(
            State(state.clone()),
            AutomationsWrite(whitelist_claims()),
            Query(BulkPatchQuery {
                tag: None,
                all: false,
            }),
            Json(BulkPatchBody {
                ids: None,
                enabled: Some(false),
            }),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn bulk_patch_all_true_is_allowed_through() {
        let state = mk_state().await;

        // Hitting every rule is still possible — it just has to be said out
        // loud. Past the selector guard this state has no rule engine, so any
        // status other than 400 proves the guard let it through.
        let resp = bulk_patch_automations(
            State(state.clone()),
            AutomationsWrite(whitelist_claims()),
            Query(BulkPatchQuery {
                tag: None,
                all: true,
            }),
            Json(BulkPatchBody {
                ids: None,
                enabled: Some(false),
            }),
        )
        .await
        .into_response();

        assert_ne!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A device the user assigned to a room carries it in `area_override`,
    /// with `area` left as whatever the plugin said (often nothing). Deriving
    /// membership from `area` alone made every user assignment invisible: the
    /// room existed and the device pointed at it, but the area listed no
    /// devices, so "add to room" looked like it had done nothing.
    #[tokio::test]
    async fn area_membership_counts_user_assigned_devices() {
        let state = mk_state().await;

        let mut d = DeviceState::new("sonos_bathroom", "Bathroom", "plugin.sonos");
        d.area = None; // the plugin reports no room
        d.area_override = Some("studio_b".into()); // the user assigned one
        state.store.upsert_device(&d).await.expect("seed");

        let resp = list_areas(State(state.clone()), AreasRead(whitelist_claims()))
            .await
            .into_response();
        let areas: Vec<Area> = parse_json(resp).await;

        let studio = areas
            .iter()
            .find(|a| a.name == "studio_b")
            .expect("user-assigned room should exist");
        assert_eq!(studio.device_ids, vec!["sonos_bathroom".to_string()]);
    }

    /// The override must follow a rename, or the device is left pointing at a
    /// room name that no longer exists.
    #[tokio::test]
    async fn renaming_an_area_follows_user_assignments() {
        let state = mk_state().await;

        let mut d = DeviceState::new("sonos_bathroom", "Bathroom", "plugin.sonos");
        d.area_override = Some("studio_b".into());
        state.store.upsert_device(&d).await.expect("seed");

        let area_id = area_id_from_name("studio_b");
        patch_area(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Path(area_id),
            Json(PatchAreaBody {
                name: "Studio C".into(),
            }),
        )
        .await
        .into_response();

        let stored = state
            .store
            .get_device("sonos_bathroom")
            .await
            .expect("load")
            .expect("device");
        assert_eq!(stored.area_override.as_deref(), Some("studio_c"));
    }

    #[tokio::test]
    async fn a_created_switch_is_actually_listed() {
        let state = mk_state().await;

        let resp = create_switch(
            State(state.clone()),
            DevicesWrite(whitelist_claims()),
            Json(CreateSwitchBody {
                id: "vacation_mode".into(),
                label: Some("Vacation Mode".into()),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = list_switches(State(state.clone()), DevicesRead(whitelist_claims()))
            .await
            .into_response();
        let switches: Vec<DeviceState> = parse_json(resp).await;

        // Regression: create_switch wrote device_type "virtual_switch", which
        // list_switches filters out — so POST /switches created a device that
        // GET /switches could never return. It existed, and was invisible.
        assert_eq!(
            switches.len(),
            1,
            "the switch we just created is missing from GET /switches"
        );
        assert_eq!(switches[0].device_id, "switch_vacation_mode");
    }

    #[tokio::test]
    async fn list_areas_is_derived_from_device_assignments() {
        let state = mk_state().await;
        seed_device(&state, "d1", Some("Kitchen")).await;
        seed_device(&state, "d2", Some("Kitchen")).await;
        seed_device(&state, "d3", Some("Office")).await;

        let resp = list_areas(State(state.clone()), AreasRead(whitelist_claims()))
            .await
            .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let mut areas: Vec<Area> = parse_json(resp).await;
        areas.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(areas.len(), 2);
        let kitchen = areas
            .iter()
            .find(|a| a.name == "kitchen")
            .expect("kitchen exists");
        assert_eq!(kitchen.device_ids.len(), 2);
        let office = areas
            .iter()
            .find(|a| a.name == "office")
            .expect("office exists");
        assert_eq!(office.device_ids, vec!["d3".to_string()]);
    }

    #[tokio::test]
    async fn create_area_persists_empty_area_for_listing() {
        let state = mk_state().await;

        let create_resp = create_area(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Json(CreateAreaBody {
                name: "Library".to_string(),
            }),
        )
        .await
        .into_response();

        assert_eq!(create_resp.status(), StatusCode::CREATED);

        let list_resp = list_areas(State(state.clone()), AreasRead(whitelist_claims()))
            .await
            .into_response();
        assert_eq!(list_resp.status(), StatusCode::OK);

        let areas: Vec<Area> = parse_json(list_resp).await;
        let library = areas
            .iter()
            .find(|area| area.name == "library")
            .expect("library exists");
        assert!(library.device_ids.is_empty());
    }

    #[tokio::test]
    async fn patch_area_renames_member_devices() {
        let state = mk_state().await;
        seed_device(&state, "d1", Some("Kitchen")).await;
        seed_device(&state, "d2", Some("Kitchen")).await;
        let kitchen_id = area_id_from_name("Kitchen");

        let resp = patch_area(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Path(kitchen_id),
            Json(PatchAreaBody {
                name: "Great Room".to_string(),
            }),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let d1 = state
            .store
            .get_device("d1")
            .await
            .expect("load d1")
            .expect("d1 exists");
        let d2 = state
            .store
            .get_device("d2")
            .await
            .expect("load d2")
            .expect("d2 exists");
        assert_eq!(d1.area.as_deref(), Some("great_room"));
        assert_eq!(d2.area.as_deref(), Some("great_room"));
        let renamed = state
            .store
            .get_area(area_id_from_name("Great Room"))
            .await
            .expect("load renamed area");
        assert!(renamed.is_some());
    }

    #[tokio::test]
    async fn delete_area_unassigns_member_devices() {
        let state = mk_state().await;
        seed_device(&state, "d1", Some("Kitchen")).await;
        seed_device(&state, "d2", Some("Kitchen")).await;
        let kitchen_id = area_id_from_name("Kitchen");

        let resp = delete_area(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Path(kitchen_id),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let d1 = state
            .store
            .get_device("d1")
            .await
            .expect("load d1")
            .expect("d1 exists");
        let d2 = state
            .store
            .get_device("d2")
            .await
            .expect("load d2")
            .expect("d2 exists");
        assert_eq!(d1.area, None);
        assert_eq!(d2.area, None);
    }

    #[tokio::test]
    async fn update_device_round_trips_status_icon() {
        let state = mk_state().await;
        seed_device(&state, "lamp_1", Some("Living Room")).await;

        let resp = update_device(
            State(state.clone()),
            DevicesWrite(whitelist_claims()),
            Path("lamp_1".to_string()),
            Json(json!({ "status_icon": "lightbulb" })),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let updated: DeviceState = parse_json(resp).await;
        assert_eq!(updated.status_icon.as_deref(), Some("lightbulb"));

        let stored = state
            .store
            .get_device("lamp_1")
            .await
            .expect("load lamp_1")
            .expect("lamp_1 exists");
        assert_eq!(stored.status_icon.as_deref(), Some("lightbulb"));

        let clear_resp = update_device(
            State(state.clone()),
            DevicesWrite(whitelist_claims()),
            Path("lamp_1".to_string()),
            Json(json!({ "status_icon": null })),
        )
        .await
        .into_response();

        assert_eq!(clear_resp.status(), StatusCode::OK);
        let cleared: DeviceState = parse_json(clear_resp).await;
        assert_eq!(cleared.status_icon, None);

        let stored_cleared = state
            .store
            .get_device("lamp_1")
            .await
            .expect("reload lamp_1")
            .expect("lamp_1 still exists");
        assert_eq!(stored_cleared.status_icon, None);
    }

    #[tokio::test]
    async fn set_area_devices_reconciles_membership() {
        let state = mk_state().await;
        seed_device(&state, "d1", Some("Kitchen")).await;
        seed_device(&state, "d2", Some("Kitchen")).await;
        seed_device(&state, "d3", Some("Office")).await;
        let kitchen_id = area_id_from_name("Kitchen");

        let resp = set_area_devices(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Path(kitchen_id),
            Json(vec!["d3".to_string()]),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let d1 = state
            .store
            .get_device("d1")
            .await
            .expect("load d1")
            .expect("d1 exists");
        let d2 = state
            .store
            .get_device("d2")
            .await
            .expect("load d2")
            .expect("d2 exists");
        let d3 = state
            .store
            .get_device("d3")
            .await
            .expect("load d3")
            .expect("d3 exists");

        // Assigning a device to a room is a *user* action, so it writes the
        // override — the same contract as PATCH /devices. Writing the
        // plugin-owned `area` here meant the next registration silently
        // reverted the assignment.
        assert_eq!(d1.effective_area(), None);
        assert_eq!(d2.effective_area(), None);
        assert_eq!(d3.effective_area(), Some("kitchen"));
        assert_eq!(d3.area_override.as_deref(), Some("kitchen"));
        // The plugin's own value is left untouched — it belongs to the plugin.
        assert_eq!(d3.area.as_deref(), Some("Office"));
    }

    #[tokio::test]
    async fn set_area_devices_can_assign_declared_empty_area() {
        let state = mk_state().await;
        seed_device(&state, "d1", None).await;

        let office = Area {
            id: area_id_from_name("Office"),
            name: "office".to_string(),
            device_ids: vec![],
        };
        state
            .store
            .upsert_area(&office)
            .await
            .expect("declare area");

        let resp = set_area_devices(
            State(state.clone()),
            AreasWrite(whitelist_claims()),
            Path(office.id),
            Json(vec!["d1".to_string()]),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let updated: Area = parse_json(resp).await;
        assert_eq!(updated.name, "office");
        assert_eq!(updated.device_ids, vec!["d1".to_string()]);

        let d1 = state
            .store
            .get_device("d1")
            .await
            .expect("load d1")
            .expect("d1 exists");
        assert_eq!(d1.effective_area(), Some("office"));
        assert_eq!(d1.area_override.as_deref(), Some("office"));
    }

    #[tokio::test]
    async fn dashboard_create_sets_owner_and_default_can_be_selected() {
        let state = mk_state().await;
        let claims = whitelist_claims();
        let owner_id = claims.uid.clone();

        let resp = create_dashboard(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Json(sample_dashboard("dash_1", "ignored-owner")),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: DashboardResponse = parse_json(resp).await;
        assert_eq!(created.dashboard.owner_user_id, owner_id);
        assert!(!created.is_default);

        let resp = set_default_dashboard(
            State(state.clone()),
            DashboardsWrite(claims),
            Path("dash_1".to_string()),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let updated: DashboardResponse = parse_json(resp).await;
        assert!(updated.is_default);
    }

    #[tokio::test]
    async fn web_dashboard_payload_round_trips_through_lifecycle() {
        let state = mk_state().await;
        let claims = claims_for("web_user", Role::User);
        let dashboard: DashboardDefinition =
            serde_json::from_value(sample_web_dashboard_payload("starter_web"))
                .expect("web payload deserializes");

        let resp = create_dashboard(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Json(dashboard),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: DashboardResponse = parse_json(resp).await;
        assert_eq!(created.dashboard.id, "starter_web");
        assert_eq!(created.dashboard.owner_user_id, "web_user");
        assert_eq!(created.dashboard.layouts.len(), 2);
        assert_eq!(created.dashboard.widgets.len(), 3);
        assert!(!created.is_default);

        let resp = list_dashboards(
            State(state.clone()),
            crate::auth_middleware::DashboardsRead(claims.clone()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let listed: Vec<DashboardResponse> = parse_json(resp).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].dashboard.id, "starter_web");

        let mut updated_dashboard = created.dashboard.clone();
        updated_dashboard.name = "Getting Started Updated".to_string();
        updated_dashboard.description = Some("Updated description".to_string());
        let resp = update_dashboard(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Path("starter_web".to_string()),
            Json(updated_dashboard),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let updated: DashboardResponse = parse_json(resp).await;
        assert_eq!(updated.dashboard.name, "Getting Started Updated");
        assert_eq!(
            updated.dashboard.description.as_deref(),
            Some("Updated description")
        );

        let resp = get_dashboard(
            State(state.clone()),
            crate::auth_middleware::DashboardsRead(claims.clone()),
            Path("starter_web".to_string()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let fetched: DashboardResponse = parse_json(resp).await;
        assert_eq!(fetched.dashboard.name, "Getting Started Updated");
        assert!(!fetched.is_default);

        let resp = set_default_dashboard(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Path("starter_web".to_string()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let defaulted: DashboardResponse = parse_json(resp).await;
        assert!(defaulted.is_default);

        let resp = list_dashboards(
            State(state.clone()),
            crate::auth_middleware::DashboardsRead(claims.clone()),
        )
        .await
        .into_response();
        let listed: Vec<DashboardResponse> = parse_json(resp).await;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default);

        let resp = delete_dashboard(
            State(state.clone()),
            DashboardsWrite(claims),
            Path("starter_web".to_string()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = list_dashboards(
            State(state),
            crate::auth_middleware::DashboardsRead(claims_for("web_user", Role::User)),
        )
        .await
        .into_response();
        let listed: Vec<DashboardResponse> = parse_json(resp).await;
        assert!(listed.is_empty());
    }

    /// Everyone who can log in sees every dashboard.
    ///
    /// This REPLACES `list_dashboards_filters_by_visibility`, and the change is
    /// deliberate rather than incidental. `visibility` (private | shared |
    /// public) existed to gate exactly this list. Access control for a house is
    /// not a CMS problem: the answer to "who may look at the kitchen dashboard"
    /// is "the people who live here", and they all have accounts.
    ///
    /// Who may CHANGE one is a real question, and it is still answered — see
    /// `non_owner_cannot_update_shared_dashboard` below, which is the test that
    /// actually protects anything.
    /// `/automations/vocabulary` must not be swallowed by `/automations/:id`.
    ///
    /// It very nearly is. Against a core that does NOT have this route, the path
    /// falls through to `/automations/:id`, fails to parse "vocabulary" as a
    /// Uuid, and answers 400 — not 404. (That is what a live older core actually
    /// does, and it is why the client treats *any* failure as "could not ask"
    /// rather than looking for a 404.)
    ///
    /// So the static route winning here is load-bearing, not incidental: get it
    /// wrong and the endpoint is dead on arrival while every test that calls the
    /// handler directly still passes.
    #[tokio::test]
    async fn the_vocabulary_route_is_not_shadowed_by_the_id_route() {
        use tower::ServiceExt;

        let state = mk_state().await;
        let app = crate::router(state, None);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/automations/vocabulary")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Unauthenticated here, so 200 or 401 — either proves the route matched.
        // A 400 would mean `:id` swallowed it and tried to parse a Uuid.
        assert_ne!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "/automations/vocabulary was swallowed by /automations/:id"
        );
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "/automations/vocabulary is not routed at all"
        );
    }

    #[tokio::test]
    async fn every_user_sees_every_dashboard() {
        let state = mk_state().await;
        seed_dashboard(&state, sample_dashboard("mine", "user_a")).await;
        seed_dashboard(&state, sample_dashboard("someone_elses", "user_b")).await;

        let resp = list_dashboards(
            State(state),
            crate::auth_middleware::DashboardsRead(claims_for("user_a", Role::User)),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let dashboards: Vec<DashboardResponse> = parse_json(resp).await;
        let ids: HashSet<_> = dashboards.iter().map(|d| d.dashboard.id.as_str()).collect();
        assert!(ids.contains("mine"));
        assert!(ids.contains("someone_elses"));
    }

    #[tokio::test]
    async fn non_owner_cannot_update_shared_dashboard() {
        let state = mk_state().await;
        let mut dashboard = sample_dashboard("shared_1", "owner");
        seed_dashboard(&state, dashboard.clone()).await;

        dashboard.name = "Renamed".to_string();
        let resp = update_dashboard(
            State(state),
            DashboardsWrite(claims_for("other_user", Role::User)),
            Path("shared_1".to_string()),
            Json(dashboard),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_dashboard_clears_default_mapping() {
        let state = mk_state().await;
        let claims = claims_for("owner", Role::User);
        seed_dashboard(&state, sample_dashboard("dash_1", "owner")).await;
        {
            let handle = state.dashboards.as_ref().expect("dashboard handle");
            handle
                .write()
                .await
                .user_defaults
                .insert("owner".to_string(), "dash_1".to_string());
        }

        let resp = delete_dashboard(
            State(state.clone()),
            DashboardsWrite(claims),
            Path("dash_1".to_string()),
        )
        .await
        .into_response();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let handle = state.dashboards.as_ref().expect("dashboard handle");
        let data = handle.read().await;
        assert!(!data.user_defaults.contains_key("owner"));
        assert!(data.dashboards.is_empty());
    }

    #[tokio::test]
    async fn reload_dashboards_refreshes_in_memory_store_from_disk() {
        let state = mk_state().await;
        seed_dashboard(&state, sample_dashboard("memory_only", "owner")).await;

        let store = state
            .dashboard_store
            .as_ref()
            .expect("dashboard store")
            .clone();
        store
            .save(&DashboardStoreData {
                dashboards: vec![sample_dashboard("disk_only", "owner")],
                user_defaults: std::collections::HashMap::from([(
                    "owner".to_string(),
                    "disk_only".to_string(),
                )]),
            })
            .expect("save dashboards file");

        let resp = reload_dashboards(State(state.clone()), AuthUser(whitelist_claims()))
            .await
            .into_response();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = parse_json(resp).await;
        assert_eq!(body["status"], "reloaded");
        assert_eq!(body["dashboards_total"], 1);
        assert_eq!(body["user_defaults_total"], 1);

        let handle = state.dashboards.as_ref().expect("dashboard handle");
        let data = handle.read().await;
        assert_eq!(data.dashboards.len(), 1);
        assert_eq!(data.dashboards[0].id, "disk_only");
        assert_eq!(
            data.user_defaults.get("owner").map(String::as_str),
            Some("disk_only")
        );
    }

    #[tokio::test]
    async fn invalid_dashboard_payload_is_rejected() {
        let state = mk_state().await;
        let claims = claims_for("owner", Role::User);
        let mut dashboard = sample_dashboard("bad_1", "owner");
        dashboard.widgets.push(DashboardWidget {
            id: "summary".to_string(),
            r#type: "markdown".to_string(),
            title: "Duplicate".to_string(),
            subtitle: None,
            config: json!({"markdown":"hello"}),
        });

        let resp = create_dashboard(State(state), DashboardsWrite(claims), Json(dashboard))
            .await
            .into_response();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = parse_json(resp).await;
        assert!(body["error"]
            .as_str()
            .expect("error string")
            .contains("duplicate widget id"));
    }

    #[tokio::test]
    async fn invalid_widget_config_is_rejected() {
        let state = mk_state().await;
        let claims = claims_for("owner", Role::User);
        let mut dashboard = sample_dashboard("bad_cfg", "owner");
        dashboard.widgets[0] = DashboardWidget {
            id: "camera".to_string(),
            r#type: "camera_video".to_string(),
            title: "Camera".to_string(),
            subtitle: None,
            config: json!({"source_type":"bogus","url":"https://example.com/cam"}),
        };
        dashboard.layouts[0].placements[0].widget_id = "camera".to_string();

        let resp = create_dashboard(State(state), DashboardsWrite(claims), Json(dashboard))
            .await
            .into_response();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = parse_json(resp).await;
        assert!(body["error"]
            .as_str()
            .expect("error string")
            .contains("source_type"));
    }

    #[tokio::test]
    async fn dashboard_templates_duplicate_export_and_import_work() {
        let state = mk_state().await;
        let claims = claims_for("owner", Role::User);

        let resp = list_dashboard_templates(
            State(state.clone()),
            crate::auth_middleware::DashboardsRead(claims.clone()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let templates: Vec<DashboardDefinition> = parse_json(resp).await;
        assert!(templates
            .iter()
            .any(|template| template.id == "starter_getting_started"));

        let resp = create_dashboard_from_template(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Path("starter_getting_started".to_string()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: DashboardResponse = parse_json(resp).await;
        assert_eq!(created.dashboard.owner_user_id, "owner");
        assert_ne!(created.dashboard.id, "starter_getting_started");

        let resp = export_dashboard(
            State(state.clone()),
            crate::auth_middleware::DashboardsRead(claims.clone()),
            Path(created.dashboard.id.clone()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let exported: DashboardDefinition = parse_json(resp).await;
        assert_eq!(exported.name, created.dashboard.name);

        let resp = duplicate_dashboard(
            State(state.clone()),
            DashboardsWrite(claims.clone()),
            Path(created.dashboard.id.clone()),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let duplicated: DashboardResponse = parse_json(resp).await;
        assert_ne!(duplicated.dashboard.id, created.dashboard.id);
        assert!(duplicated.dashboard.name.contains("Copy"));

        let resp = import_dashboard(
            State(state.clone()),
            DashboardsWrite(claims),
            Json(exported),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let imported: DashboardResponse = parse_json(resp).await;
        assert_ne!(imported.dashboard.id, created.dashboard.id);
        assert_eq!(imported.dashboard.owner_user_id, "owner");
    }
}
