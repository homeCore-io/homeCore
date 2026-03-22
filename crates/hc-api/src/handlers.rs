//! axum route handlers for all REST endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_state::StateStore;
use hc_types::device::Area;
use hc_types::rule::{Rule, Scene};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth_middleware::{
    AreasRead, AreasWrite, AutomationsRead, AutomationsWrite, DevicesRead, DevicesWrite,
    PluginsRead, PluginsWrite, ScenesRead, ScenesWrite,
};
use crate::AppState;

// ---------- Health ----------

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ---------- Devices ----------

pub async fn list_devices(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    match s.store.list_devices().await {
        Ok(devices) => (StatusCode::OK, Json(json!(devices))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
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
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({ "error": "device not found" }))).into_response(),
        Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn device_history(
    State(s): State<AppState>,
    _: DevicesRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let from = chrono::Utc::now() - chrono::Duration::hours(24);
    let to = chrono::Utc::now();
    match s.store.query_history(&id, from, to, 500).await {
        Ok(entries) => (StatusCode::OK, Json(json!(entries.iter().map(|e| json!({
            "attribute": e.attribute,
            "value": e.value,
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

// ---------- Areas ----------

pub async fn list_areas(State(s): State<AppState>, _: AreasRead) -> impl IntoResponse {
    match s.store.list_areas().await {
        Ok(areas) => (StatusCode::OK, Json(json!(areas))),
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
    let area = Area { id: Uuid::new_v4(), name: body.name, device_ids: vec![] };
    match s.store.upsert_area(&area).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(area))),
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
    let mut area = match s.store.get_area(id).await {
        Ok(Some(a)) => a,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e)    => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    area.name = body.name;
    match s.store.upsert_area(&area).await {
        Ok(_) => (StatusCode::OK, Json(json!(area))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn delete_area(
    State(s): State<AppState>,
    _: AreasWrite,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match s.store.delete_area(id).await {
        Ok(true)  => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

// ---------- Automations (Rules) ----------
//
// The rules_handle (Arc<RwLock<Vec<Rule>>>) is the single in-memory source of
// truth.  All reads come from it; all writes go to the rule_file_store (which
// writes a TOML file on disk) *and* directly update the handle so the change
// is immediately visible — no need to wait for the hot-reload watcher.

pub async fn list_automations(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    match &s.rules_handle {
        Some(rh) => (StatusCode::OK, Json(json!(rh.read().await.clone()))),
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "rule engine not available" }))),
    }
}

pub async fn create_automation(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(mut rule): Json<Rule>,
) -> impl IntoResponse {
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
        let passed = eval_condition_dry(condition, &s.store).await.unwrap_or(false);
        if !passed {
            all_pass = false;
        }
        condition_results.push(json!({
            "condition": serde_json::to_value(condition).unwrap_or(serde_json::Value::Null),
            "passed": passed,
        }));
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

async fn eval_condition_dry(
    condition: &hc_types::rule::Condition,
    store: &StateStore,
) -> Option<bool> {
    use hc_types::rule::Condition;
    match condition {
        Condition::DeviceState { device_id, attribute, op, value } => {
            let device = match store.get_device(device_id).await {
                Ok(Some(d)) => d,
                _ => return None,
            };
            let actual = device.attributes.get(attribute)?;
            Some(compare_values(actual, op, value))
        }
        Condition::TimeWindow { start, end } => {
            let now = chrono::Local::now().time();
            if start <= end {
                Some(now >= *start && now <= *end)
            } else {
                Some(now >= *start || now <= *end)
            }
        }
        Condition::ScriptExpression { script } => {
            let script = script.clone();
            tokio::task::spawn_blocking(move || {
                hc_scripting::ScriptRuntime::new().eval_condition(&script).ok()
            })
            .await
            .ok()
            .flatten()
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
    let mut area = match s.store.get_area(id).await {
        Ok(Some(a)) => a,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "area not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    area.device_ids = device_ids;
    match s.store.upsert_area(&area).await {
        Ok(_) => (StatusCode::OK, Json(json!(area))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
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
