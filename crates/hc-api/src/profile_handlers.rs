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
