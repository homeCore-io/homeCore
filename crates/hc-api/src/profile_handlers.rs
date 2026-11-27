//! Devices over MQTT: the ecosystem profiles, from the app.
//!
//! - `GET    /mqtt`                          what's on, what's off, what the broker carries unmatched
//! - `GET    /mqtt/profiles/{name}`          one profile and the devices it takes in
//! - `PATCH  /mqtt/profiles/{name}/devices`  say what devices are (type, name)
//! - `GET    /mqtt/device-types`             the catalog's types, for choosing one
//! - `POST   /mqtt/catalog`                  add homeCore's device type catalog
//! - `POST   /mqtt/reload`                   read the profiles folder again
//!
//! Admin only: these show raw broker traffic and change files in the config.
//! Changes apply at once (see `hc_core::profiles`); nothing restarts.

use crate::auth_middleware::AuthUser;
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use hc_auth::Claims;
use hc_core::profiles::{DeviceEntryChange, ProfileFileStatus, ProfileSet};
use hc_types::device::DeviceState;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// The profile set, for an admin; otherwise the response to give.
fn admin_profiles(state: &AppState, claims: &Claims) -> Result<Arc<ProfileSet>, Box<Response>> {
    if !claims.is_admin() {
        return Err(Box::new(err(StatusCode::FORBIDDEN, "admin role required")));
    }
    state.profiles.clone().ok_or_else(|| {
        Box::new(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "homeCore has no profiles folder",
        ))
    })
}

/// Which loaded profile a device belongs to: the one it came through this
/// session, else the one whose prefix its ID starts with (the longest).
/// Only devices no plugin owns: a profile device is filed under its prefix
/// word (`tasmota`), never `plugin.…` or `core.…`.
fn profile_for(set: &ProfileSet, files: &[ProfileFileStatus], d: &DeviceState) -> Option<String> {
    if d.plugin_id.starts_with("plugin.") || d.plugin_id.starts_with("core.") {
        return None;
    }
    if let Some(p) = set.profile_of(&d.device_id) {
        return Some(p.to_string());
    }
    files
        .iter()
        .filter_map(|f| Some((f.name.as_deref()?, f.prefix.as_deref()?)))
        .filter(|(_, prefix)| !prefix.is_empty() && d.device_id.starts_with(prefix))
        .max_by_key(|(_, prefix)| prefix.len())
        .map(|(name, _)| name.to_string())
}

/// `GET /mqtt`
pub async fn overview(State(s): State<AppState>, AuthUser(claims): AuthUser) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let files = set.files();
    let devices = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut profiles = Vec::new();
    let mut untyped_total = 0;
    for f in &files {
        let mine: Vec<&DeviceState> = match &f.name {
            Some(name) => devices
                .iter()
                .filter(|d| profile_for(&set, &files, d).as_deref() == Some(name))
                .collect(),
            None => Vec::new(),
        };
        let untyped = mine.iter().filter(|d| d.device_type.is_none()).count();
        untyped_total += untyped;
        let last_heard = mine.iter().map(|d| d.last_seen).max();
        let mut v = serde_json::to_value(f).unwrap_or_default();
        v["devices"] = json!(mine.len());
        v["untyped"] = json!(untyped);
        v["last_heard"] = json!(last_heard);
        profiles.push(v);
    }
    let catalog = set.catalog();
    Json(json!({
        "folder": set.dir(),
        "catalog": {
            "present": catalog.is_some(),
            "types": catalog.as_ref().map(|c| c.type_names().count()).unwrap_or(0),
            "error": set.catalog_error(),
        },
        "profiles": profiles,
        "untyped": untyped_total,
        "shipped_off": set.shipped_off(),
        "off": set.off_files(),
        "heard": set.heard_since(Utc::now() - Duration::hours(1)),
    }))
    .into_response()
}

/// The readings worth showing beside a device: a few, in name order.
fn readings(d: &DeviceState) -> Value {
    let mut keys: Vec<&String> = d.attributes.keys().collect();
    keys.sort();
    let mut out = Map::new();
    for k in keys.into_iter().take(8) {
        out.insert(k.clone(), d.attributes[k].clone());
    }
    Value::Object(out)
}

/// `GET /mqtt/profiles/{name}`
pub async fn profile(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(name): Path<String>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let files = set.files();
    let Some(status) = files
        .iter()
        .find(|f| f.name.as_deref() == Some(&name))
        .cloned()
    else {
        return err(StatusCode::NOT_FOUND, format!("no profile named {name}"));
    };
    let devices = match s.store.list_devices().await {
        Ok(d) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let configured: std::collections::HashMap<String, hc_topic_map::profile::DeviceConfig> = set
        .router()
        .map(|r| {
            r.configured_devices()
                .into_iter()
                .filter(|(p, _, _)| **p == *name)
                .map(|(_, id, d)| (id, d))
                .collect()
        })
        .unwrap_or_default();
    let mut list: Vec<Value> = devices
        .iter()
        .filter(|d| profile_for(&set, &files, d).as_deref() == Some(&name))
        .map(|d| {
            json!({
                "device_id": d.device_id,
                "name": d.effective_name(),
                "device_type": d.device_type,
                "configured": configured.get(&d.device_id),
                "suggestion": set.suggestion(&d.device_id),
                "suggested_name": set.suggested_name(&d.device_id),
                "available": d.available,
                "last_seen": d.last_seen,
                "readings": readings(d),
            })
        })
        .collect();
    list.sort_by(|a, b| {
        let key = |v: &Value| v["name"].as_str().unwrap_or_default().to_lowercase();
        key(a).cmp(&key(b))
    });
    Json(json!({ "profile": status, "devices": list })).into_response()
}

#[derive(Deserialize)]
pub struct DeviceChanges {
    pub changes: Vec<DeviceEntryChange>,
}

/// `PATCH /mqtt/profiles/{name}/devices`
pub async fn set_devices(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(name): Path<String>,
    Json(body): Json<DeviceChanges>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if body.changes.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no changes");
    }
    let writer = Arc::clone(&set);
    let result =
        tokio::task::spawn_blocking(move || writer.set_devices(&name, &body.changes)).await;
    match result {
        Ok(Ok(())) => {
            // Answer once the devices show it, so the app reads them changed.
            set.applied(set.generation(), std::time::Duration::from_secs(3))
                .await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /mqtt/device-types`
pub async fn device_types(State(s): State<AppState>, AuthUser(claims): AuthUser) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let Some(catalog) = set.catalog() else {
        return Json(json!({ "types": [] })).into_response();
    };
    let mut names: Vec<&str> = catalog.type_names().collect();
    names.sort_unstable();
    let types: Vec<Value> = names
        .into_iter()
        .filter_map(|n| {
            let schema = catalog.get_device_schema(n)?;
            let mut attributes: Vec<Value> = schema
                .attributes
                .iter()
                .map(|(k, a)| json!({ "name": k, "writable": a.writable }))
                .collect();
            attributes.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Some(json!({
                "name": n,
                "description": catalog.get_schema(n).and_then(|v| v["description"].as_str()),
                "attributes": attributes,
            }))
        })
        .collect();
    Json(json!({ "types": types })).into_response()
}

/// `POST /mqtt/catalog`
pub async fn add_catalog(State(s): State<AppState>, AuthUser(claims): AuthUser) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match tokio::task::spawn_blocking(move || set.install_catalog()).await {
        Ok(Ok(added)) => Json(json!({ "added": added })).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /mqtt/reload`
pub async fn reload(State(s): State<AppState>, AuthUser(claims): AuthUser) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let _ = tokio::task::spawn_blocking(move || set.reload()).await;
    Json(json!({ "ok": true })).into_response()
}

// ── Round 2: turning on and off, editing, trying, logins ────────────────────

/// Run a change to the profiles folder off the async threads, then answer
/// once the state bridge has applied it.
async fn change_then_apply(
    set: Arc<ProfileSet>,
    change: impl FnOnce(&ProfileSet) -> anyhow::Result<Value> + Send + 'static,
) -> Response {
    let writer = Arc::clone(&set);
    match tokio::task::spawn_blocking(move || change(&writer)).await {
        Ok(Ok(v)) => {
            set.applied(set.generation(), std::time::Duration::from_secs(3))
                .await;
            Json(v).into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct FileBody {
    pub file: String,
}

/// `POST /mqtt/on` `{file}`: turn a shipped or turned-off profile on.
pub async fn turn_on(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<FileBody>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    change_then_apply(set, move |p| {
        p.turn_on(&body.file)?;
        Ok(json!({ "ok": true }))
    })
    .await
}

/// `POST /mqtt/off` `{file}`: turn a profile off, keeping it.
pub async fn turn_off(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<FileBody>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    change_then_apply(set, move |p| {
        p.turn_off(&body.file)?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub file: String,
}

/// `GET /mqtt/preview?file=`: what turning a profile on would take in now.
pub async fn preview(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    axum::extract::Query(q): axum::extract::Query<PreviewQuery>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match set.preview(&q.file) {
        Ok(devices) => Json(json!({ "devices": devices })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// `GET /mqtt/files/{file}`: a profile's text, and whether it is on.
pub async fn get_file(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(file): Path<String>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let status = set.files().into_iter().find(|f| f.file == file);
    match set.source(&file) {
        Ok(text) => Json(json!({
            "file": file,
            "text": text,
            "on": status.is_some(),
            "status": status,
        }))
        .into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct TextBody {
    pub text: String,
}

/// `PUT /mqtt/files/{file}` `{text}`: save a profile, new or changed.
pub async fn put_file(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(file): Path<String>,
    Json(body): Json<TextBody>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    change_then_apply(set, move |p| {
        p.save(&file, &body.text)?;
        Ok(json!({ "ok": true }))
    })
    .await
}

/// `DELETE /mqtt/files/{file}`: delete a profile. Its devices stay.
pub async fn delete_file(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(file): Path<String>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    change_then_apply(set, move |p| {
        p.delete(&file)?;
        Ok(json!({ "ok": true }))
    })
    .await
}

#[derive(Deserialize)]
pub struct TryBody {
    pub text: String,
    pub topic: String,
    #[serde(default)]
    pub payload: String,
}

/// `POST /mqtt/try` `{text, topic, payload}`: what a profile would make of a message.
pub async fn try_message(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<TryBody>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match set.try_message(&body.text, &body.topic, body.payload.as_bytes()) {
        Ok(r) => Json(r).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// The device logins, for an admin.
fn admin_logins(
    state: &AppState,
    claims: &Claims,
) -> Result<Arc<crate::mqtt_logins::DeviceLogins>, Box<Response>> {
    if !claims.is_admin() {
        return Err(Box::new(err(StatusCode::FORBIDDEN, "admin role required")));
    }
    state.mqtt_logins.clone().ok_or_else(|| {
        Box::new(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "homeCore keeps no device logins here",
        ))
    })
}

/// Whether the broker asks devices to log in: it has `[[broker.clients]]`.
/// Until it does (on this machine only), any device here gets in without one.
fn broker_asks_for_logins(state: &AppState) -> Option<bool> {
    let path = state.homecore_config_path.as_ref()?;
    let doc: toml::Value = toml::from_str(&std::fs::read_to_string(path.as_path()).ok()?).ok()?;
    Some(
        doc.get("broker")
            .and_then(|b| b.get("clients"))
            .and_then(|c| c.as_array())
            .is_some_and(|c| !c.is_empty()),
    )
}

#[derive(Deserialize)]
pub struct LoginsQuery {
    pub profile: Option<String>,
}

/// `GET /mqtt/logins?profile=`
pub async fn list_logins(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    axum::extract::Query(q): axum::extract::Query<LoginsQuery>,
) -> Response {
    let logins = match admin_logins(&s, &claims) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    Json(json!({
        "broker_asks": broker_asks_for_logins(&s),
        "logins": logins.list(q.profile.as_deref()),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct NewLogin {
    pub profile: String,
    pub user: String,
    #[serde(default)]
    pub label: String,
}

/// `POST /mqtt/logins` `{profile, user, label}`: the password, once.
pub async fn add_login(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<NewLogin>,
) -> Response {
    let logins = match admin_logins(&s, &claims) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let user = body.user.trim().to_string();
    match tokio::task::spawn_blocking(move || {
        logins
            .add(&body.profile, &user, &body.label)
            .map(|p| (user, p))
    })
    .await
    {
        Ok(Ok((user, password))) => {
            Json(json!({ "user": user, "password": password })).into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /mqtt/logins/{user}/password`: a new password, once.
pub async fn reset_login(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(user): Path<String>,
) -> Response {
    let logins = match admin_logins(&s, &claims) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    match tokio::task::spawn_blocking(move || logins.reset(&user)).await {
        Ok(Ok(password)) => Json(json!({ "password": password })).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `DELETE /mqtt/logins/{user}`
pub async fn remove_login(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(user): Path<String>,
) -> Response {
    let logins = match admin_logins(&s, &claims) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    match logins.remove(&user) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// `GET /mqtt/devices/{id}`: the ecosystem a device comes in through, for its
/// own page to link to. 404 for a device no profile takes in.
pub async fn device_profile(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let set = match admin_profiles(&s, &claims) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let device = match s.store.get_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("no device {id}")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let files = set.files();
    match profile_for(&set, &files, &device) {
        Some(name) => {
            let file = files
                .iter()
                .find(|f| f.name.as_deref() == Some(&name))
                .map(|f| f.file.clone());
            Json(json!({ "profile": name, "file": file })).into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("no ecosystem takes {id} in")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, prefix: &str) -> ProfileFileStatus {
        ProfileFileStatus {
            file: format!("{name}.toml"),
            name: Some(name.into()),
            description: None,
            prefix: Some(prefix.into()),
            shipped: true,
            error: None,
        }
    }

    /// A device belongs to the ecosystem it came through, else the one whose
    /// prefix it starts with (the longest); a plugin's or core's to none.
    #[test]
    fn a_device_belongs_to_the_ecosystem_it_comes_in_through() {
        let set = ProfileSet::fixed(None, None);
        let files = vec![
            status("shelly-gen1", "shelly_"),
            status("shelly-plus", "shelly_plus_"),
        ];
        let d = |id: &str, plugin: &str| DeviceState::new(id, id, plugin);
        assert_eq!(
            profile_for(&set, &files, &d("shelly_plus_1", "shelly")).as_deref(),
            Some("shelly-plus")
        );
        assert_eq!(
            profile_for(&set, &files, &d("shelly_abc", "shelly")).as_deref(),
            Some("shelly-gen1")
        );
        set.came_through("shelly_plus_2", &std::sync::Arc::from("shelly-gen1"));
        assert_eq!(
            profile_for(&set, &files, &d("shelly_plus_2", "shelly")).as_deref(),
            Some("shelly-gen1")
        );
        assert_eq!(
            profile_for(&set, &files, &d("shelly_x", "plugin.shelly")),
            None
        );
        assert_eq!(profile_for(&set, &files, &d("hue_1", "hue")), None);
    }
}
