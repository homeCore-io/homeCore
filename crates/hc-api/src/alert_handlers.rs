//! Alerts (H4): the active list, and the time rules an admin writes.
//!
//! Reading alerts takes `devices:read` — they are about devices, and anyone
//! who can see the house can see what needs attention in it. Rules are
//! authoring, like automations, so they take `automations:read` / `:write`.
//! Every rule change wakes the engine, which works the alerts out again and
//! announces the difference on the event stream.
//!
//! Acknowledging takes `devices:command` — whoever may act in the house,
//! a display included, may say they've seen what needs attention. Ignoring
//! hides alerts from everyone, so it is authoring: `automations:write`.
//!
//! Plugins' kinds of alert are listed to anyone who can read devices, and
//! turned on or off, or sent somewhere, with `automations:write` like an
//! ignore. Pressing an alert's button is a command: `devices:command` for a
//! device's action, and `plugins:write` besides for a plugin's.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use hc_auth::{Actor, Claims};
use hc_types::alert::{AlertActionTarget, AlertIgnore, AlertRule, AlertTopicSetting};
use serde_json::json;
use uuid::Uuid;

use crate::auth_middleware::{
    AutomationsRead, AutomationsWrite, DevicesCommand, DevicesRead, PluginsWrite,
};
use crate::AppState;

fn wake(s: &AppState) {
    if let Some(alerts) = &s.alerts {
        alerts.rules_changed();
    }
}

fn failed(e: anyhow::Error) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

fn invalid(msg: String) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

/// `GET /alerts` — what needs attention now, most serious first.
pub async fn list_alerts(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    Json(s.alerts.as_ref().map(|a| a.list()).unwrap_or_default())
}

/// `GET /alert-rules`
pub async fn list_alert_rules(State(s): State<AppState>, _: AutomationsRead) -> impl IntoResponse {
    match s.store.list_alert_rules().await {
        Ok(rules) => Json(rules).into_response(),
        Err(e) => failed(e),
    }
}

/// `POST /alert-rules` — homeCore names it.
pub async fn create_alert_rule(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Json(mut rule): Json<AlertRule>,
) -> impl IntoResponse {
    rule.id = Uuid::new_v4().to_string();
    if let Err(msg) = rule.validate() {
        return invalid(msg);
    }
    if let Err(e) = s.store.upsert_alert_rule(rule.clone()).await {
        return failed(e);
    }
    wake(&s);
    (StatusCode::CREATED, Json(rule)).into_response()
}

/// `PUT /alert-rules/{id}` — the whole rule, replaced.
pub async fn put_alert_rule(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<String>,
    Json(mut rule): Json<AlertRule>,
) -> impl IntoResponse {
    match s.store.get_alert_rule(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such rule" })),
            )
                .into_response()
        }
        Err(e) => return failed(e),
    }
    rule.id = id;
    if let Err(msg) = rule.validate() {
        return invalid(msg);
    }
    if let Err(e) = s.store.upsert_alert_rule(rule.clone()).await {
        return failed(e);
    }
    wake(&s);
    Json(rule).into_response()
}

/// `DELETE /alert-rules/{id}` — its alerts clear on the engine's next pass.
pub async fn delete_alert_rule(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.delete_alert_rule(&id).await {
        Ok(true) => {
            wake(&s);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such rule" })),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

/// A person's name for an alert's record: who they signed in as, or the
/// display's name.
fn who(claims: &Claims) -> String {
    match claims.actor() {
        Actor::User { username, .. } => username,
        Actor::ApiKey { label, .. } => label,
        Actor::LocalAdmin { .. } => "local admin".into(),
    }
}

/// `POST /alerts/{id}/acknowledge` — seen; it stays until its condition ends.
pub async fn acknowledge_alert(
    State(s): State<AppState>,
    DevicesCommand(claims): DevicesCommand,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(alerts) = &s.alerts else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such alert" })),
        )
            .into_response();
    };
    match alerts.acknowledge(&id, &who(&claims)) {
        Some(alert) => {
            // An active alert that now says more: announced as raised, as any
            // change to what an alert says is.
            let _ = s.event_bus.publish(hc_types::event::Event::AlertRaised {
                timestamp: Utc::now(),
                alert: alert.clone(),
                update: true,
            });
            // …and as seen, for an automation waiting on that.
            let _ = s
                .event_bus
                .publish(hc_types::event::Event::AlertAcknowledged {
                    timestamp: Utc::now(),
                    alert: alert.clone(),
                });
            Json(alert).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such alert — it may have cleared" })),
        )
            .into_response(),
    }
}

/// `GET /alert-ignores` — including any that have run out, until removed.
pub async fn list_alert_ignores(
    State(s): State<AppState>,
    _: AutomationsRead,
) -> impl IntoResponse {
    match s.store.list_alert_ignores().await {
        Ok(all) => Json(all).into_response(),
        Err(e) => failed(e),
    }
}

/// `POST /alert-ignores` — homeCore names it and says who asked.
pub async fn create_alert_ignore(
    State(s): State<AppState>,
    AutomationsWrite(claims): AutomationsWrite,
    Json(mut ignore): Json<AlertIgnore>,
) -> impl IntoResponse {
    ignore.id = Uuid::new_v4().to_string();
    ignore.by = Some(who(&claims));
    ignore.created_at = Utc::now();
    if let Err(msg) = ignore.validate() {
        return invalid(msg);
    }
    if let Err(e) = s.store.upsert_alert_ignore(ignore.clone()).await {
        return failed(e);
    }
    wake(&s);
    (StatusCode::CREATED, Json(ignore)).into_response()
}

/// `DELETE /alert-ignores/{id}` — what it hid is shown again at once.
pub async fn delete_alert_ignore(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match s.store.delete_alert_ignore(&id).await {
        Ok(true) => {
            wake(&s);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such ignore" })),
        )
            .into_response(),
        Err(e) => failed(e),
    }
}

/// `GET /alerts/kinds` — every kind of alert a plugin declared, with what the
/// house decided about it: `enabled` is the setting or the plugin's default,
/// `notify` the channel it is sent to, if any.
pub async fn list_alert_kinds(State(s): State<AppState>, _: DevicesRead) -> impl IntoResponse {
    let kinds = s
        .alerts
        .as_ref()
        .map(|a| a.plugin_kinds())
        .unwrap_or_default();
    let settings = match s.store.list_alert_topic_settings().await {
        Ok(t) => t,
        Err(e) => return failed(e),
    };
    let mut out = Vec::new();
    for (plugin_id, list) in kinds {
        for k in list {
            let setting = settings
                .iter()
                .find(|t| t.plugin_id == plugin_id && t.topic == k.topic);
            out.push(json!({
                "plugin_id": plugin_id,
                "topic": k.topic,
                "label": k.label,
                "description": k.description,
                "severity": hc_types::alert::plugin_severity(k.severity),
                "default_on": k.default_on,
                "enabled": setting.and_then(|t| t.enabled).unwrap_or(k.default_on),
                "notify": setting.and_then(|t| t.notify.clone()),
            }));
        }
    }
    Json(out).into_response()
}

/// The body of `PUT /alerts/kinds/{plugin_id}/{topic}`: `null` (or left out)
/// is the plugin's default.
#[derive(serde::Deserialize)]
pub struct TopicSettingBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    notify: Option<String>,
}

/// `PUT /alerts/kinds/{plugin_id}/{topic}` — on or off, and where it's sent.
pub async fn put_alert_kind(
    State(s): State<AppState>,
    _: AutomationsWrite,
    Path((plugin_id, topic)): Path<(String, String)>,
    Json(body): Json<TopicSettingBody>,
) -> impl IntoResponse {
    let notify = body
        .notify
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    let setting = AlertTopicSetting {
        plugin_id,
        topic,
        enabled: body.enabled,
        notify,
    };
    if let Err(e) = s.store.put_alert_topic_setting(setting.clone()).await {
        return failed(e);
    }
    wake(&s);
    Json(setting).into_response()
}

/// `POST /alerts/{id}/press` — run the alert's button: the device action it
/// names on the alert's device, or the plugin action. Accepted, like any
/// command; the alert clears when the plugin says it's done.
pub async fn press_alert(
    State(s): State<AppState>,
    DevicesCommand(claims): DevicesCommand,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(alert) = s.alerts.as_ref().and_then(|a| a.get(&id)) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such alert — it may have cleared" })),
        )
            .into_response();
    };
    let Some(action) = alert.action.clone() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "this alert has no button" })),
        )
            .into_response();
    };
    let mut body = match action.params {
        serde_json::Value::Object(m) => serde_json::Value::Object(m),
        _ => json!({}),
    };
    body["action"] = json!(action.id);
    match action.target {
        AlertActionTarget::Device => {
            let change = hc_types::device::DeviceChange::homecore("alert")
                .with_actor(Some(claims.uid), Some(claims.sub))
                .with_correlation_id(Some(Uuid::new_v4().to_string()));
            match crate::handlers::publish_device_command(&s, &alert.device_id, body, change).await
            {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    Json(json!({ "status": "accepted", "alert": alert.id })),
                )
                    .into_response(),
                Err(e) => failed(e),
            }
        }
        AlertActionTarget::Plugin => {
            if !claims.has_scope("plugins:write") {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "scope 'plugins:write' required" })),
                )
                    .into_response();
            }
            let Some(plugin_id) = alert.plugin_id.clone() else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "this alert names no plugin" })),
                )
                    .into_response();
            };
            crate::handlers::post_plugin_command(
                State(s),
                PluginsWrite(claims),
                Path(plugin_id),
                Json(body),
            )
            .await
            .into_response()
        }
    }
}
