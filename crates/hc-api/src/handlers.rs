//! axum route handlers for all REST endpoints.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use hc_state::StateStore;
use hc_types::device::{Area, DeviceState};
use hc_types::rule::{Action, Condition, Rule, Scene, Trigger};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

use crate::auth_middleware::{
    AreasRead, AreasWrite, AutomationsRead, AutomationsWrite, DevicesRead, DevicesWrite,
    PluginsRead, PluginsWrite, ScenesRead, ScenesWrite,
};
use crate::group_store::RuleGroup;
use crate::AppState;

// ---------- Health ----------

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ---------- System status ----------

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
        let state_sz = std::fs::metadata(&bp.state_db_path).map(|m| m.len()).unwrap_or(0);
        let hist_sz  = std::fs::metadata(&bp.history_db_path).map(|m| m.len()).unwrap_or(0);
        (state_sz, hist_sz)
    } else {
        (0, 0)
    };

    Json(json!({
        "version":           env!("CARGO_PKG_VERSION"),
        "uptime_seconds":    uptime_secs,
        "started_at":        s.started_at,
        "rules_total":       rules_total,
        "rules_enabled":     rules_enabled,
        "devices_total":     devices_total,
        "plugins_active":    plugins_active,
        "state_db_bytes":    state_db_bytes,
        "history_db_bytes":  history_db_bytes,
    }))
}

// ---------- Devices ----------

#[derive(Deserialize, Default)]
pub struct DeviceListQuery {
    #[serde(default)]
    pub include_schema: bool,
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
    let all_devices = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), Json(json!({ "error": e.to_string() }))).into_response(),
    };

    let total = all_devices.len();
    let offset = params.offset.unwrap_or(0);
    let page: Vec<_> = all_devices.into_iter().skip(offset).take(params.limit.unwrap_or(usize::MAX)).collect();

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
        let schema = s.store.get_device_schema(&device.device_id).await.ok().flatten();
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
        Ok(Some(device)) => (StatusCode::OK, Json(json!(device))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "device not found" }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn get_device_schema(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.get_device_schema(&id).await {
        Ok(Some(schema)) => (StatusCode::OK, Json(json!(schema))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "schema not found" }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn command_device(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let topic = format!("homecore/devices/{id}/cmd");
    let payload = match serde_json::to_vec(&body) {
        Ok(p) => p,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };

    // Publish to MQTT so plugins and external subscribers receive the command.
    if let Some(ph) = &s.publish {
        if let Err(e) = ph.publish(&topic, payload.clone()).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
        }
    }

    // Inject directly into the event bus so the state bridge routes the cmd to
    // the native device topic without depending on the broker echoing the publish
    // back to the internal client.
    let ev = hc_types::event::Event::MqttMessage {
        timestamp: chrono::Utc::now(),
        topic,
        payload,
        retain: false,
    };
    let _ = s.event_bus.publish(ev);

    (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" })))
}

pub async fn update_device(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    match s.store.get_device(&id).await {
        Ok(Some(mut device)) => {
            if let Some(name) = body.get("name").and_then(|v| v.as_str()) {
                device.name = name.to_string();
            }
            if let Some(area) = body.get("area") {
                device.area = if area.is_null() {
                    None
                } else {
                    area.as_str().map(|s| s.to_string())
                };
            }
            match s.store.upsert_device(&device).await {
                Ok(_) => (StatusCode::OK, Json(json!(device))),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
            }
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "device not found" }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn delete_device(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.delete_device(&id).await {
        Ok(false) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "device not found" }))).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
        Ok(true) => {}
    }

    // Nullify references to this device in all rule files, then return a summary.
    let affected_rules = if let Some(rfs) = &s.rule_file_store {
        match crate::rule_file_store::nullify_device_refs(&rfs.dir, &id) {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!(device_id = %id, error = %e, "delete_device: failed to nullify rule refs");
                vec![]
            }
        }
    } else {
        vec![]
    };

    (StatusCode::OK, Json(json!({
        "deleted": true,
        "affected_rules": affected_rules,
    }))).into_response()
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
        Some(arr) => arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        None => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "ids array required" }))).into_response(),
    };

    let new_area: Option<Option<String>> = if body.get("area").is_some() {
        Some(match body["area"].as_str() {
            Some(a) => Some(a.to_string()),
            None if body["area"].is_null() => None,
            _ => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "area must be a string or null" }))).into_response(),
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
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
                }
                updated += 1;
            }
            Ok(None) => not_found.push(id.clone()),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
        }
    }

    (StatusCode::OK, Json(json!({ "updated": updated, "not_found": not_found }))).into_response()
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
    _: DevicesWrite,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let ids: Vec<String> = match body.get("ids").and_then(|v| v.as_array()) {
        Some(arr) => arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        None => return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "ids array required" }))).into_response(),
    };

    let mut deleted = 0usize;
    let mut not_found: Vec<String> = Vec::new();
    let mut affected_rules: Vec<String> = Vec::new();

    for id in &ids {
        match s.store.delete_device(id).await {
            Ok(false) => not_found.push(id.clone()),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
            Ok(true) => {
                deleted += 1;
                if let Some(rfs) = &s.rule_file_store {
                    match crate::rule_file_store::nullify_device_refs(&rfs.dir, id) {
                        Ok(names) => {
                            for name in names {
                                if !affected_rules.contains(&name) {
                                    affected_rules.push(name);
                                }
                            }
                        }
                        Err(e) => tracing::warn!(device_id = %id, error = %e, "bulk_delete_devices: failed to nullify rule refs"),
                    }
                }
            }
        }
    }

    (StatusCode::OK, Json(json!({
        "deleted": deleted,
        "not_found": not_found,
        "affected_rules": affected_rules,
    }))).into_response()
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
    let from = params.from.unwrap_or_else(|| now - chrono::Duration::hours(24));
    let to = params.to.unwrap_or(now);
    let limit = params.limit.unwrap_or(500).min(5_000);

    match s.store.query_history(&id, from, to, params.attribute.as_deref(), limit).await {
        Ok(entries) => (StatusCode::OK, Json(json!(entries.iter().map(|e| json!({
            "attribute":   e.attribute,
            "value":       e.value,
            "recorded_at": e.recorded_at,
        })).collect::<Vec<_>>()))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
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
        return (StatusCode::CONFLICT, Json(json!({ "error": "timer already exists" }))).into_response();
    }

    let display_name = body.label.as_deref().unwrap_or(&device_id).to_string();
    let mut dev = hc_types::device::DeviceState::new(&device_id, &display_name, "core.timer");
    dev.available = true;
    dev.attributes.insert("state".into(), json!("idle"));
    dev.attributes.insert("duration_secs".into(), json!(0_u64));
    dev.attributes.insert("remaining_secs".into(), json!(0_u64));
    dev.attributes.insert("repeat".into(), json!(false));

    match s.store.upsert_device(&dev).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(dev))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn list_timers(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => {
            let timers: Vec<_> = devices
                .into_iter()
                .filter(|d| d.plugin_id == "core.timer")
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
        Ok(Some(dev)) => (StatusCode::OK, Json(json!(compute_timer_remaining(dev)))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "timer not found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// For a running timer, recompute `remaining_secs` from `started_at` + `duration_secs`
/// so callers always see an accurate countdown without requiring periodic store writes.
fn compute_timer_remaining(mut dev: hc_types::device::DeviceState) -> hc_types::device::DeviceState {
    let is_running = dev.attributes.get("state").and_then(Value::as_str) == Some("running");
    if !is_running {
        return dev;
    }
    let duration_secs = dev.attributes.get("duration_secs").and_then(Value::as_u64).unwrap_or(0);
    let started_at = dev
        .attributes
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    if let Some(started) = started_at {
        let elapsed = (chrono::Utc::now() - started).num_seconds().max(0) as u64;
        let remaining = duration_secs.saturating_sub(elapsed);
        dev.attributes.insert("remaining_secs".into(), json!(remaining));
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
        return (StatusCode::CONFLICT, Json(json!({ "error": "switch already exists" }))).into_response();
    }

    let display_name = body.label.as_deref().unwrap_or(&device_id).to_string();
    let mut dev = hc_types::device::DeviceState::new(&device_id, &display_name, "core.switch");
    dev.available = true;
    dev.attributes.insert("on".into(), json!(false));

    match s.store.upsert_device(&dev).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(dev))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn list_switches(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => {
            let switches: Vec<_> = devices
                .into_iter()
                .filter(|d| d.plugin_id == "core.switch")
                .collect();
            (StatusCode::OK, Json(json!(switches)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

// ---------- Modes ----------

/// `GET /api/v1/modes` — list all mode configs + live device state.
pub async fn list_modes(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "modes not configured" }))),
    };
    let configs = match hc_core::mode_manager::load_modes(&path) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };
    let devices = s.store.list_devices().await.unwrap_or_default();
    let result: Vec<Value> = configs.into_iter().map(|cfg| {
        let state = devices.iter().find(|d| d.device_id == cfg.id);
        json!({ "config": cfg, "state": state })
    }).collect();
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
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "modes not configured" }))).into_response(),
    };
    let configs = match hc_core::mode_manager::load_modes(&path) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    match configs.into_iter().find(|c| c.id == id) {
        Some(cfg) => {
            let state = s.store.get_device(&id).await.ok().flatten();
            (StatusCode::OK, Json(json!({ "config": cfg, "state": state }))).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "mode not found" }))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct CreateModeBody {
    pub id:   String,
    pub name: String,
    pub kind: hc_core::mode_manager::ModeKind,
}

/// `POST /api/v1/modes` — create a new mode (appends to modes.toml).
pub async fn create_mode(
    State(s): State<AppState>,
    _: DevicesWrite,
    Json(body): Json<CreateModeBody>,
) -> impl IntoResponse {
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "modes not configured" }))),
    };
    if !body.id.starts_with("mode_") {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "id must start with 'mode_'" })));
    }
    let cfg = hc_core::mode_manager::ModeConfig {
        id:   body.id,
        name: body.name,
        kind: body.kind,
        on_event:           None,
        off_event:          None,
        on_offset_minutes:  0,
        off_offset_minutes: 0,
    };
    match hc_core::mode_manager::append_mode(&path, cfg.clone()) {
        Ok(_) => (StatusCode::CREATED, Json(json!(cfg))),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e.to_string() }))),
    }
}

/// `DELETE /api/v1/modes/:id` — remove a mode.
/// Rejects `mode_night` (built-in) with 400.
pub async fn delete_mode(
    State(s): State<AppState>,
    _: DevicesWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if id == hc_core::mode_manager::MODE_NIGHT_ID {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "mode_night is a built-in mode and cannot be deleted"
        }))).into_response();
    }
    let path = match s.modes_path.as_ref() {
        Some(p) => p.as_ref().clone(),
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "modes not configured" }))).into_response(),
    };
    if let Err(e) = hc_core::mode_manager::remove_mode(&path, &id) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))).into_response();
    }
    if let Err(e) = s.store.delete_device(&id).await {
        tracing::warn!(mode_id = %id, error = %e, "delete_mode: failed to remove device from store");
    }
    (StatusCode::NO_CONTENT, Json(json!({}))).into_response()
}

// ---------- Areas ----------

fn normalize_area_name(name: &str) -> String {
    name.trim().to_string()
}

fn area_id_from_name(name: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("homecore:area:{}", name).as_bytes())
}

fn derive_areas_from_devices(devices: &[DeviceState]) -> Vec<Area> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for device in devices {
        let Some(area) = device.area.as_deref() else {
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

async fn find_area_by_id(store: &StateStore, id: Uuid) -> Result<Option<Area>, String> {
    let devices = store.list_devices().await.map_err(|e| e.to_string())?;
    Ok(derive_areas_from_devices(&devices)
        .into_iter()
        .find(|a| a.id == id))
}

pub async fn list_areas(State(s): State<AppState>, _: AreasRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => (StatusCode::OK, Json(json!(derive_areas_from_devices(&devices)))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
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
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "area name cannot be empty" })));
    }

    // Canonical model: areas are derived from device.area.
    // This endpoint validates/declares the area name and returns its stable ID.
    match s.store.list_devices().await {
        Ok(_) => {
            let area = Area {
                id: area_id_from_name(&name),
                name,
                device_ids: vec![],
            };
            (StatusCode::CREATED, Json(json!(area)))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
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
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "area name cannot be empty" }))).into_response();
    }

    let area = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };

    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    for device in &mut devices {
        if device
            .area
            .as_deref()
            .map(normalize_area_name)
            .as_deref()
            == Some(area.name.as_str())
        {
            device.area = Some(new_name.clone());
            if let Err(e) = s.store.upsert_device(device).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!(Area {
            id: area_id_from_name(&new_name),
            name: new_name,
            device_ids: area.device_ids,
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
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };

    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    for device in &mut devices {
        if device
            .area
            .as_deref()
            .map(normalize_area_name)
            .as_deref()
            == Some(area.name.as_str())
        {
            device.area = None;
            if let Err(e) = s.store.upsert_device(device).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
            }
        }
    }

    StatusCode::NO_CONTENT.into_response()
}

// ---------- Automations (Rules) ----------
//
// The rules_handle (Arc<RwLock<Vec<Rule>>>) is the single in-memory source of
// truth.  All reads come from it; all writes go to the rule_file_store (which
// writes a TOML file on disk) *and* directly update the handle so the change
// is immediately visible — no need to wait for the hot-reload watcher.

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
    match &s.rules_handle {
        Some(rh) => {
            let rules = rh.read().await;
            let filtered: Vec<_> = rules.iter().filter(|r| {
                if let Some(ref tag) = params.tag {
                    if !r.tags.contains(tag) { return false; }
                }
                if let Some(ref trig) = params.trigger {
                    if trigger_type_name(&r.trigger) != trig.as_str() { return false; }
                }
                if let Some(ref did) = params.device_id {
                    if !rule_references_device(r, did) { return false; }
                }
                if params.stale == Some(true) && r.error.is_none() {
                    return false;
                }
                true
            }).cloned().collect();

            let total = filtered.len();
            let offset = params.offset.unwrap_or(0);
            let page: Vec<_> = filtered.into_iter().skip(offset).take(params.limit.unwrap_or(usize::MAX)).collect();

            let mut headers = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
                headers.insert("X-Total-Count", v);
            }
            (StatusCode::OK, headers, Json(json!(page))).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new(), Json(json!({ "error": "rule engine not available" }))).into_response(),
    }
}

/// Snake-case name of a `Trigger` variant — matches the serde `type` field value.
fn trigger_type_name(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChanged { .. }        => "device_state_changed",
        Trigger::MqttMessage { .. }               => "mqtt_message",
        Trigger::TimeOfDay { .. }                 => "time_of_day",
        Trigger::SunEvent { .. }                  => "sun_event",
        Trigger::WebhookReceived { .. }           => "webhook_received",
        Trigger::ManualTrigger                    => "manual_trigger",
        Trigger::CustomEvent { .. }               => "custom_event",
        Trigger::SystemStarted                    => "system_started",
        Trigger::Cron { .. }                      => "cron",
        Trigger::DeviceAvailabilityChanged { .. } => "device_availability_changed",
        Trigger::ButtonEvent { .. }               => "button_event",
        Trigger::NumericThreshold { .. }          => "numeric_threshold",
        Trigger::Periodic { .. }                  => "periodic",
    }
}

/// Returns `true` if `device_id` appears anywhere in the rule's trigger,
/// conditions, or actions (including nested action groups).
fn rule_references_device(rule: &Rule, device_id: &str) -> bool {
    let in_trigger = match &rule.trigger {
        Trigger::DeviceStateChanged { device_id: d, .. } => d == device_id,
        Trigger::DeviceAvailabilityChanged { device_id: d, .. } => d == device_id,
        _ => false,
    };
    if in_trigger { return true; }

    for cond in &rule.conditions {
        if condition_references_device(cond, device_id) {
            return true;
        }
    }

    actions_reference_device(&rule.actions, device_id)
}

fn condition_references_device(cond: &Condition, device_id: &str) -> bool {
    match cond {
        Condition::DeviceState { device_id: d, .. }  => d == device_id,
        Condition::TimeElapsed { device_id: d, .. }  => d == device_id,
        Condition::Not { condition }                 => condition_references_device(condition, device_id),
        _ => false,
    }
}

fn actions_reference_device(actions: &[Action], device_id: &str) -> bool {
    for action in actions {
        let found = match action {
            Action::SetDeviceState { device_id: d, .. } => d == device_id,
            Action::Parallel { actions: inner }          => actions_reference_device(inner, device_id),
            Action::RepeatUntil { actions: inner, .. }   => actions_reference_device(inner, device_id),
            Action::Conditional { then_actions, else_actions, .. } =>
                actions_reference_device(then_actions, device_id)
                || actions_reference_device(else_actions, device_id),
            _ => false,
        };
        if found { return true; }
    }
    false
}

pub async fn create_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(mut rule): Json<Rule>,
) -> impl IntoResponse {
    // Validate priority is within practical range.
    if rule.priority < -1000 || rule.priority > 1000 {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({
            "error": "priority must be between -1000 and 1000"
        }))).into_response();
    }

    rule.id = Uuid::new_v4();

    // Write file first — if this fails the in-memory state is unchanged.
    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule(&rule) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    }

    // Update live engine handle immediately (don't wait for watcher).
    if let Some(rh) = &s.rules_handle {
        rh.write().await.push(rule.clone());
    }

    (StatusCode::CREATED, Json(json!(rule))).into_response()
}

pub async fn get_automation(
    State(s): State<AppState>,
    _: AutomationsRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };
    let rules = rh.read().await;
    match rules.iter().find(|r| r.id == id).cloned() {
        Some(rule) => (StatusCode::OK, Json(json!(rule))).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
    }
}

pub async fn update_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(mut rule): Json<Rule>,
) -> impl IntoResponse {
    // Validate priority is within practical range.
    if rule.priority < -1000 || rule.priority > 1000 {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({
            "error": "priority must be between -1000 and 1000"
        }))).into_response();
    }

    rule.id = id;

    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };

    // Check existence and get old name for potential rename.
    let old_name = {
        let rules = rh.read().await;
        match rules.iter().find(|r| r.id == id) {
            Some(r) => r.name.clone(),
            None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
        }
    };

    // Write file — handles rename (deletes old slug file if name changed).
    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule_renamed(&rule, &old_name) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    }

    // Update live handle.
    {
        let mut rules = rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = rule.clone();
        } else {
            rules.push(rule.clone());
        }
    }

    (StatusCode::OK, Json(json!(rule))).into_response()
}

pub async fn delete_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };

    // Verify existence before touching the filesystem.
    {
        let rules = rh.read().await;
        if !rules.iter().any(|r| r.id == id) {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response();
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
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
            }
            Ok(true) => {}
        }
    }

    // Remove from live handle.
    rh.write().await.retain(|r| r.id != id);

    StatusCode::NO_CONTENT.into_response()
}

// ---------- Scenes ----------

pub async fn list_scenes(State(s): State<AppState>, _: ScenesRead) -> impl IntoResponse {
    match s.store.list_scenes().await {
        Ok(scenes) => (StatusCode::OK, Json(json!(scenes))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn create_scene(
    State(s): State<AppState>,
    _: ScenesWrite,
    Json(mut scene): Json<Scene>,
) -> impl IntoResponse {
    scene.id = Uuid::new_v4();
    match s.store.upsert_scene(&scene).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(scene))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn activate_scene(
    State(s): State<AppState>,
    _: ScenesWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let scene = match s.store.get_scene(id).await {
        Ok(Some(sc)) => sc,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "scene not found" }))),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };

    if let Some(ph) = &s.publish {
        for (device_id, desired) in &scene.states {
            let topic = format!("homecore/devices/{device_id}/cmd");
            let payload = desired.to_string().into_bytes();
            if let Err(e) = ph.publish(&topic, payload).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
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
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
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
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
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
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" })));
    };
    let rule = match rh.read().await.iter().find(|r| r.id == id).cloned() {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))),
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

    (StatusCode::OK, Json(json!({
        "rule_id": id,
        "rule_name": rule.name,
        "conditions_pass": all_pass,
        "conditions": condition_results,
        "would_fire": all_pass,
        "actions": serde_json::to_value(&rule.actions).unwrap_or(serde_json::Value::Null),
    })))
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
        Condition::DeviceState { device_id, attribute, op, value } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                Ok(None) => return ConditionDetail {
                    condition: cond_json, passed: false, actual: None,
                    expected: Some(value.clone()), elapsed_ms: None,
                    reason: Some(format!("device '{device_id}' not found")),
                },
                Err(e) => return ConditionDetail {
                    condition: cond_json, passed: false, actual: None,
                    expected: Some(value.clone()), elapsed_ms: None,
                    reason: Some(format!("store error: {e}")),
                },
            };
            match device.attributes.get(attribute) {
                None => ConditionDetail {
                    condition: cond_json, passed: false, actual: None,
                    expected: Some(value.clone()), elapsed_ms: None,
                    reason: Some(format!("attribute '{attribute}' not present")),
                },
                Some(actual) => {
                    let passed = compare_values(actual, op, value);
                    ConditionDetail {
                        condition: cond_json, passed,
                        actual: Some(actual.clone()),
                        expected: Some(value.clone()),
                        elapsed_ms: None, reason: None,
                    }
                }
            }
        }
        Condition::TimeWindow { start, end } => {
            let now = chrono::Local::now().time();
            let passed = if start <= end {
                now >= *start && now <= *end
            } else {
                now >= *start || now <= *end
            };
            ConditionDetail {
                condition: cond_json, passed,
                actual: Some(json!(now.to_string())),
                expected: Some(json!(format!("{start}–{end}"))),
                elapsed_ms: None, reason: None,
            }
        }
        Condition::ScriptExpression { script } => {
            let script = script.clone();
            let result = tokio::task::spawn_blocking(move || {
                hc_scripting::ScriptRuntime::new().eval_condition(&script).ok()
            })
            .await
            .ok()
            .flatten();
            ConditionDetail {
                condition: cond_json,
                passed: result.unwrap_or(false),
                actual: None, expected: None, elapsed_ms: None,
                reason: if result.is_none() { Some("script error".into()) } else { None },
            }
        }
        Condition::TimeElapsed { device_id, attribute: _, duration_secs } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                Ok(None) => return ConditionDetail {
                    condition: cond_json, passed: false, actual: None, expected: None,
                    elapsed_ms: None,
                    reason: Some(format!("device '{device_id}' not found")),
                },
                Err(e) => return ConditionDetail {
                    condition: cond_json, passed: false, actual: None, expected: None,
                    elapsed_ms: None,
                    reason: Some(format!("store error: {e}")),
                },
            };
            // Dry-run uses last_seen as the conservative elapsed baseline.
            let elapsed_secs = (chrono::Utc::now() - device.last_seen).num_seconds().max(0);
            let passed = elapsed_secs as u64 >= *duration_secs;
            ConditionDetail {
                condition: cond_json, passed,
                actual: None, expected: None,
                elapsed_ms: Some(elapsed_secs * 1000),
                reason: if !passed {
                    Some(format!("only {elapsed_secs}s elapsed, need {duration_secs}s"))
                } else {
                    None
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
                    reason = Some(detail.reason.unwrap_or_else(|| "sub-condition failed".into()));
                    break;
                }
            }
            ConditionDetail { condition: cond_json, passed, actual: None, expected: None, elapsed_ms: None, reason }
        }
        Condition::Or { conditions } => {
            let mut passed = false;
            for c in conditions {
                let detail = Box::pin(eval_condition_dry_detail(c, store)).await;
                if detail.passed { passed = true; break; }
            }
            ConditionDetail {
                condition: cond_json, passed, actual: None, expected: None, elapsed_ms: None,
                reason: if !passed { Some("no sub-condition passed".into()) } else { None },
            }
        }
        Condition::Xor { conditions } => {
            let mut count = 0usize;
            for c in conditions {
                let detail = Box::pin(eval_condition_dry_detail(c, store)).await;
                if detail.passed { count += 1; }
            }
            let passed = count == 1;
            ConditionDetail {
                condition: cond_json, passed, actual: Some(json!(count)), expected: Some(json!(1)),
                elapsed_ms: None,
                reason: if !passed { Some(format!("{count} sub-conditions passed, need exactly 1")) } else { None },
            }
        }
        Condition::PrivateBooleanIs { name, value } => {
            // Dry-run cannot access live runtime state; report as indeterminate.
            ConditionDetail {
                condition: cond_json, passed: false, actual: None,
                expected: Some(json!(value)),
                elapsed_ms: None,
                reason: Some(format!("private boolean '{name}' not available in dry-run")),
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
        CompareOp::Gt => actual.as_f64().zip(expected.as_f64()).map(|(a, b)| a > b).unwrap_or(false),
        CompareOp::Gte => actual.as_f64().zip(expected.as_f64()).map(|(a, b)| a >= b).unwrap_or(false),
        CompareOp::Lt => actual.as_f64().zip(expected.as_f64()).map(|(a, b)| a < b).unwrap_or(false),
        CompareOp::Lte => actual.as_f64().zip(expected.as_f64()).map(|(a, b)| a <= b).unwrap_or(false),
    }
}

// ---------- Rule import / export ----------

/// `GET /api/v1/automations/export`
/// Returns all rules as a JSON array (ready to re-import).
pub async fn export_automations(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    match &s.rules_handle {
        Some(rh) => (StatusCode::OK, Json(json!(rh.read().await.clone()))),
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))),
    }
}

/// `POST /api/v1/automations/import`
/// Accepts a JSON array of rules; assigns fresh UUIDs and writes each as a TOML file.
pub async fn import_automations(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(rules): Json<Vec<Rule>>,
) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" })));
    };

    let mut saved = Vec::with_capacity(rules.len());
    for mut rule in rules {
        rule.id = Uuid::new_v4();

        if let Some(fs) = &s.rule_file_store {
            if let Err(e) = fs.write_rule(&rule) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                );
            }
        }

        rh.write().await.push(rule.clone());
        saved.push(rule);
    }
    (StatusCode::CREATED, Json(json!({ "imported": saved.len(), "rules": saved })))
}

// ---------- Plugins ----------

pub async fn list_plugins(State(s): State<AppState>, _: PluginsRead) -> impl IntoResponse {
    let map = s.plugins.read().await;
    let list: Vec<_> = map.values().cloned().collect();
    (StatusCode::OK, Json(json!(list)))
}

pub async fn deregister_plugin(
    State(s): State<AppState>,
    _: PluginsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut map = s.plugins.write().await;
    if map.remove(&id).is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({ "error": "plugin not found" }))).into_response()
    }
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
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };

    let desired: HashSet<String> = device_ids.into_iter().collect();
    let mut devices = match s.store.list_devices().await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    for device in &mut devices {
        let in_desired = desired.contains(&device.device_id);
        let in_area = device
            .area
            .as_deref()
            .map(normalize_area_name)
            .as_deref()
            == Some(area.name.as_str());

        if in_desired {
            if !in_area {
                device.area = Some(area.name.clone());
                if let Err(e) = s.store.upsert_device(device).await {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
                }
            }
        } else if in_area {
            device.area = None;
            if let Err(e) = s.store.upsert_device(device).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
            }
        }
    }

    // Return the updated derived area membership.
    let refreshed = match find_area_by_id(&s.store, id).await {
        Ok(Some(a)) => a,
        Ok(None) => Area {
            id,
            name: area.name,
            device_ids: vec![],
        },
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
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
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };

    // Read current rule from handle.
    let mut rule = match rh.read().await.iter().find(|r| r.id == id).cloned() {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
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
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    }

    // Update live handle.
    {
        let mut rules = rh.write().await;
        if let Some(pos) = rules.iter().position(|r| r.id == id) {
            rules[pos] = rule.clone();
        }
    }

    (StatusCode::OK, Json(json!(rule))).into_response()
}

// ---------- Bulk automation PATCH ----------

#[derive(Deserialize, Default)]
pub struct BulkPatchQuery {
    /// Apply only to rules that have this tag.  Ignored when `ids` is present in the body.
    pub tag: Option<String>,
}

#[derive(Deserialize)]
pub struct BulkPatchBody {
    /// When present, apply only to these specific rule IDs (overrides `?tag=`).
    #[serde(default)]
    pub ids: Option<Vec<Uuid>>,
    pub enabled: Option<bool>,
}

/// `PATCH /api/v1/automations[?tag=<tag>]`
///
/// Bulk enable/disable rules, selecting targets in priority order:
/// 1. `ids` field in body — explicit list of rule UUIDs (ignores `?tag=`)
/// 2. `?tag=<tag>` query param — all rules with that tag
/// 3. No selector — all rules
///
/// Returns `{ "updated": N, "rules": [...] }`.
pub async fn bulk_patch_automations(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Query(params): Query<BulkPatchQuery>,
    Json(patch): Json<BulkPatchBody>,
) -> impl IntoResponse {
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };

    let mut updated = Vec::new();
    {
        let mut rules = rh.write().await;
        for rule in rules.iter_mut() {
            let selected = if let Some(ref ids) = patch.ids {
                ids.contains(&rule.id)
            } else if let Some(ref tag) = params.tag {
                rule.tags.contains(tag)
            } else {
                true
            };
            if selected {
                if let Some(enabled) = patch.enabled {
                    rule.enabled = enabled;
                }
                updated.push(rule.clone());
            }
        }
    }

    // Persist each changed rule to its TOML file.
    if let Some(fs) = &s.rule_file_store {
        for rule in &updated {
            if let Err(e) = fs.write_rule(rule) {
                tracing::warn!(rule_id = %rule.id, error = %e, "bulk_patch: failed to write rule file");
            }
        }
    }

    (StatusCode::OK, Json(json!({ "updated": updated.len(), "rules": updated }))).into_response()
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
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "fire history not available" }))).into_response();
    };

    // Verify rule exists.
    if let Some(rh) = &s.rules_handle {
        let rules = rh.read().await;
        if !rules.iter().any(|r| r.id == id) {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response();
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
    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
    };

    let original = {
        let rules = rh.read().await;
        match rules.iter().find(|r| r.id == id).cloned() {
            Some(r) => r,
            None => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
        }
    };

    let mut cloned = original.clone();
    cloned.id      = Uuid::new_v4();
    cloned.name    = format!("Copy of {}", original.name);
    cloned.enabled = false; // disabled until operator reviews
    cloned.error   = None;

    if let Some(fs) = &s.rule_file_store {
        if let Err(e) = fs.write_rule(&cloned) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    }

    rh.write().await.push(cloned.clone());
    (StatusCode::CREATED, Json(json!(cloned))).into_response()
}

// ---------- Rule groups ----------

/// `GET /api/v1/automations/groups`
pub async fn list_groups(
    State(s): State<AppState>,
    _: AutomationsRead,
) -> impl IntoResponse {
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
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "group store not available" }))).into_response();
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
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    let groups = rg.read().await;
    match groups.iter().find(|g| g.id == id).cloned() {
        Some(g) => (StatusCode::OK, Json(json!(g))).into_response(),
        None    => (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response(),
    }
}

/// `PATCH /api/v1/automations/groups/{id}`
///
/// Update group metadata (name, description, rule_ids).  Does not toggle rules.
#[derive(Deserialize)]
pub struct GroupPatch {
    pub name:        Option<String>,
    pub description: Option<String>,
    pub rule_ids:    Option<Vec<Uuid>>,
}

pub async fn patch_group(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<Uuid>,
    Json(patch): Json<GroupPatch>,
) -> impl IntoResponse {
    let Some(rg) = &s.rule_groups else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    let mut groups = rg.write().await;
    let Some(g) = groups.iter_mut().find(|g| g.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    if let Some(name) = patch.name            { g.name        = name; }
    if let Some(desc) = patch.description     { g.description = Some(desc); }
    if let Some(ids)  = patch.rule_ids        { g.rule_ids    = ids; }
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
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    let mut groups = rg.write().await;
    let before = groups.len();
    groups.retain(|g| g.id != id);
    if groups.len() == before {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
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
    let enabled = match action.as_str() {
        "enable"  => true,
        "disable" => false,
        other => return (StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown action '{other}'; use enable or disable") }))).into_response(),
    };

    let Some(rg) = &s.rule_groups else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    let groups = rg.read().await;
    let Some(group) = groups.iter().find(|g| g.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "group not found" }))).into_response();
    };
    let rule_ids = group.rule_ids.clone();
    drop(groups);

    let Some(rh) = &s.rules_handle else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))).into_response();
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

    (StatusCode::OK, Json(json!({ "enabled": enabled, "updated": updated.len(), "rules": updated }))).into_response()
}

// ---------- Webhooks ----------

/// `POST /api/v1/webhooks/{path}`
///
/// Any POST to this endpoint fires a `Custom` event with `event_type = "webhook"` and
/// `payload = { "path": "...", "body": <request body> }`.  Rules with
/// `Trigger::WebhookReceived { path }` will match when the path matches.
pub async fn receive_webhook(
    State(s): State<AppState>,
    Path(path): Path<String>,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let body_value = body.map(|b| b.0).unwrap_or(Value::Null);
    let event = hc_types::event::Event::Custom {
        timestamp: chrono::Utc::now(),
        event_type: "webhook".into(),
        payload: json!({ "path": path, "body": body_value }),
    };
    let _ = s.event_bus.publish(event);
    (StatusCode::OK, Json(json!({ "status": "accepted", "path": path })))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_middleware::{whitelist_claims, AreasRead, AreasWrite};
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use chrono::Utc;
    use hc_auth::JwtService;
    use hc_core::EventBus;
    use hc_types::device::DeviceState;
    use http_body_util::BodyExt;
    use serde::de::DeserializeOwned;
    use uuid::Uuid;

    fn temp_db_paths(prefix: &str) -> (String, String) {
        let base = std::env::temp_dir().join(format!("hc_api_handlers_{prefix}_{}", Uuid::new_v4()));
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
        AppState::new(store, bus, None, None, None, jwt, vec![], None)
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
        let kitchen = areas.iter().find(|a| a.name == "Kitchen").expect("kitchen exists");
        assert_eq!(kitchen.device_ids.len(), 2);
        let office = areas.iter().find(|a| a.name == "Office").expect("office exists");
        assert_eq!(office.device_ids, vec!["d3".to_string()]);
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
        let d1 = state.store.get_device("d1").await.expect("load d1").expect("d1 exists");
        let d2 = state.store.get_device("d2").await.expect("load d2").expect("d2 exists");
        assert_eq!(d1.area.as_deref(), Some("Great Room"));
        assert_eq!(d2.area.as_deref(), Some("Great Room"));
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
        let d1 = state.store.get_device("d1").await.expect("load d1").expect("d1 exists");
        let d2 = state.store.get_device("d2").await.expect("load d2").expect("d2 exists");
        assert_eq!(d1.area, None);
        assert_eq!(d2.area, None);
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
        let d1 = state.store.get_device("d1").await.expect("load d1").expect("d1 exists");
        let d2 = state.store.get_device("d2").await.expect("load d2").expect("d2 exists");
        let d3 = state.store.get_device("d3").await.expect("load d3").expect("d3 exists");

        assert_eq!(d1.area, None);
        assert_eq!(d2.area, None);
        assert_eq!(d3.area.as_deref(), Some("Kitchen"));
    }
}
