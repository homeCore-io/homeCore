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

use crate::AppState;

// ---------- Health ----------

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

// ---------- Devices ----------

pub async fn list_devices(State(s): State<AppState>) -> impl IntoResponse {
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
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let topic = format!("homecore/devices/{id}/cmd");
    if let Some(ph) = &s.publish {
        match ph.publish_json::<Value>(&topic, &body, false).await {
            Ok(_) => (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        }
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "MQTT not connected" })))
    }
}

pub async fn device_history(
    State(s): State<AppState>,
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

// ---------- Areas ----------

pub async fn list_areas(State(s): State<AppState>) -> impl IntoResponse {
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
    Json(body): Json<CreateAreaBody>,
) -> impl IntoResponse {
    let area = Area { id: Uuid::new_v4(), name: body.name, device_ids: vec![] };
    match s.store.upsert_area(&area).await {
        Ok(_) => (StatusCode::CREATED, Json(json!(area))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

// ---------- Automations (Rules) ----------

pub async fn list_automations(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.list_rules().await {
        Ok(rules) => (StatusCode::OK, Json(json!(rules))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn create_automation(
    State(s): State<AppState>,
    Json(mut rule): Json<Rule>,
) -> impl IntoResponse {
    rule.id = Uuid::new_v4();
    match s.store.upsert_rule(&rule).await {
        Ok(_) => {
            // Reload rules into the engine via the rules_handle.
            if let Some(rh) = &s.rules_handle {
                let mut rules = rh.write().await;
                rules.push(rule.clone());
            }
            (StatusCode::CREATED, Json(json!(rule)))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn get_automation(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match s.store.get_rule(id).await {
        Ok(Some(rule)) => (StatusCode::OK, Json(json!(rule))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn update_automation(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(mut rule): Json<Rule>,
) -> impl IntoResponse {
    rule.id = id;
    match s.store.upsert_rule(&rule).await {
        Ok(_) => {
            if let Some(rh) = &s.rules_handle {
                let mut rules = rh.write().await;
                if let Some(pos) = rules.iter().position(|r| r.id == id) {
                    rules[pos] = rule.clone();
                } else {
                    rules.push(rule.clone());
                }
            }
            (StatusCode::OK, Json(json!(rule)))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn delete_automation(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    match s.store.delete_rule(id).await {
        Ok(true) => {
            if let Some(rh) = &s.rules_handle {
                let mut rules = rh.write().await;
                rules.retain(|r| r.id != id);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

// ---------- Scenes ----------

pub async fn list_scenes(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.list_scenes().await {
        Ok(scenes) => (StatusCode::OK, Json(json!(scenes))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

pub async fn create_scene(
    State(s): State<AppState>,
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
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let rule = match s.store.get_rule(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
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
pub async fn export_automations(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.list_rules().await {
        Ok(rules) => (StatusCode::OK, Json(json!(rules))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// `POST /api/v1/automations/import`
/// Accepts a JSON array of rules; assigns fresh UUIDs and saves them all.
pub async fn import_automations(
    State(s): State<AppState>,
    Json(rules): Json<Vec<Rule>>,
) -> impl IntoResponse {
    let mut saved = Vec::with_capacity(rules.len());
    for mut rule in rules {
        rule.id = Uuid::new_v4();
        match s.store.upsert_rule(&rule).await {
            Ok(_) => {
                if let Some(rh) = &s.rules_handle {
                    rh.write().await.push(rule.clone());
                }
                saved.push(rule);
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                );
            }
        }
    }
    (StatusCode::CREATED, Json(json!({ "imported": saved.len(), "rules": saved })))
}

// ---------- Plugins ----------

pub async fn list_plugins(State(s): State<AppState>) -> impl IntoResponse {
    let map = s.plugins.read().await;
    let list: Vec<_> = map.values().cloned().collect();
    (StatusCode::OK, Json(json!(list)))
}

pub async fn deregister_plugin(
    State(s): State<AppState>,
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
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchAutomationBody>,
) -> impl IntoResponse {
    let mut rule = match s.store.get_rule(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "rule not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    if let Some(enabled) = patch.enabled {
        rule.enabled = enabled;
    }
    if let Some(priority) = patch.priority {
        rule.priority = priority;
    }
    match s.store.upsert_rule(&rule).await {
        Ok(_) => {
            if let Some(rh) = &s.rules_handle {
                let mut rules = rh.write().await;
                if let Some(pos) = rules.iter().position(|r| r.id == id) {
                    rules[pos] = rule.clone();
                }
            }
            (StatusCode::OK, Json(json!(rule))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
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
    axum::extract::Query(query): axum::extract::Query<crate::event_log::EventLogQuery>,
) -> impl IntoResponse {
    let entries = s.event_log.query(&query);
    (StatusCode::OK, Json(json!(entries)))
}
