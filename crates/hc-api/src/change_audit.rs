//! Who changed what (A6): an audit record for every change made through the
//! API to how the house works — rules, devices, rooms, scenes, helpers, modes,
//! alert rules and plugins.
//!
//! The audit used to hold only sign-ins, accounts and keys: a person asking
//! "who turned the porch rule off?" found nothing. Rather than an `emit` call in
//! each of some fifty handlers (and the next handler forgetting it), one layer
//! over the protected routes records a change by the route it went through,
//! once it has succeeded. Routes whose handler already writes its own, richer
//! record (accounts, keys, displays, calendars, backups, homeCore's settings,
//! uninstalling a plugin, turning one on or off, its log level) are left to
//! it.
//!
//! An entry names what changed — the rule, device, room or scene as it was
//! called when it changed, so a removed one can still be told apart — and
//! which fields a change touched. Field *values* are not recorded (a
//! plugin's settings can hold passwords), except `enabled`, which is the
//! change people most often ask about.
//!
//! Commanding a device, running a scene and testing a rule are uses, not
//! changes, and aren't recorded here.

use axum::{
    body::{to_bytes, Body},
    extract::{MatchedPath, Request, State},
    http::{header, Method},
    middleware::Next,
    response::Response,
};
use hc_auth::Claims;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{audit, AppState};

/// The largest request or response body read to find a name. Bigger ones (an
/// import of every rule, say) are recorded without one.
const MAX_PEEK: usize = 256 * 1024;

/// What a route changes: the audit's event type and the kind of thing its
/// `{id}` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub event: &'static str,
    pub kind: &'static str,
}

const fn c(event: &'static str, kind: &'static str) -> Option<Change> {
    Some(Change { event, kind })
}

/// The change a request makes, by its method and matched route — or `None`
/// for a read, a use, or a route whose handler records its own.
pub fn change_of(method: &Method, route: &str) -> Option<Change> {
    let route = route.strip_prefix("/api/v1").unwrap_or(route);
    let m = method.as_str();
    match (m, route) {
        // Rules
        ("POST", "/automations") => c("automation.created", "automation"),
        ("PATCH", "/automations") => c("automation.bulk_changed", "automation"),
        ("PUT", "/automations/{id}") => c("automation.updated", "automation"),
        ("PATCH", "/automations/{id}") => c("automation.updated", "automation"),
        ("DELETE", "/automations/{id}") => c("automation.deleted", "automation"),
        ("POST", "/automations/{id}/clone") => c("automation.copied", "automation"),
        ("POST", "/automations/import") => c("automation.imported", "automation"),
        ("POST", "/automations/groups") => c("automation_group.created", "automation_group"),
        ("PATCH", "/automations/groups/{id}") => c("automation_group.updated", "automation_group"),
        ("DELETE", "/automations/groups/{id}") => c("automation_group.deleted", "automation_group"),
        ("POST", "/automations/groups/{id}/{action}") => {
            c("automation_group.switched", "automation_group")
        }
        // Devices, helpers and modes (every one is a device)
        ("PATCH", "/devices") => c("device.bulk_updated", "device"),
        ("PATCH", "/devices/{id}") => c("device.updated", "device"),
        ("DELETE", "/devices/{id}") => c("device.deleted", "device"),
        ("POST", "/timers") | ("POST", "/switches") | ("POST", "/glue") => {
            c("helper.created", "device")
        }
        ("PATCH", "/glue/{id}") => c("helper.updated", "device"),
        ("DELETE", "/glue/{id}") => c("helper.deleted", "device"),
        ("POST", "/modes") => c("mode.created", "device"),
        ("DELETE", "/modes/{id}") => c("mode.deleted", "device"),
        ("PUT", "/modes/{id}/definition") | ("DELETE", "/modes/{id}/definition") => {
            c("mode.updated", "device")
        }
        // Rooms
        ("POST", "/areas") => c("area.created", "area"),
        ("PATCH", "/areas/{id}") => c("area.updated", "area"),
        ("DELETE", "/areas/{id}") => c("area.deleted", "area"),
        ("PUT", "/areas/{id}/devices") => c("area.devices_set", "area"),
        ("POST", "/areas/{id}/merge") => c("area.merged", "area"),
        // Scenes
        ("POST", "/scenes") => c("scene.created", "scene"),
        ("PUT", "/scenes/{id}") => c("scene.updated", "scene"),
        ("DELETE", "/scenes/{id}") => c("scene.deleted", "scene"),
        ("POST", "/scenes/import") => c("scene.imported", "scene"),
        // Alert rules
        ("POST", "/alert-rules") => c("alert_rule.created", "alert_rule"),
        ("PUT", "/alert-rules/{id}") => c("alert_rule.updated", "alert_rule"),
        ("DELETE", "/alert-rules/{id}") => c("alert_rule.deleted", "alert_rule"),
        // Plugins
        ("POST", "/plugins/install") => c("plugin.installed", "plugin"),
        ("POST", "/plugins/{id}/start") => c("plugin.started", "plugin"),
        ("POST", "/plugins/{id}/stop") => c("plugin.stopped", "plugin"),
        ("POST", "/plugins/{id}/restart") => c("plugin.restarted", "plugin"),
        ("PUT", "/plugins/{id}/config") => c("plugin.config_updated", "plugin"),
        ("POST", "/plugins/{id}/command") => c("plugin.action_run", "plugin"),
        ("DELETE", "/plugins/{id}/devices") => c("plugin.devices_removed", "plugin"),
        ("POST", "/plugin-runtimes/{id}/approve") => c("plugin_runtime.approved", "runtime"),
        ("POST", "/plugin-runtimes/{id}/deny") => c("plugin_runtime.denied", "runtime"),
        ("POST", "/plugin-runtimes/tokens") => c("plugin_runtime.token_issued", "runtime"),
        // homeCore itself (its settings write their own record)
        ("PUT", "/system/log-level") => c("system.log_level_changed", "system"),
        _ => None,
    }
}

/// The first `{…}` parameter of `route` as it appears in `path`. Matched from
/// the end: inside the nested `/api/v1` router the path has lost that prefix
/// while the matched route still carries it.
fn param_of(route: &str, path: &str) -> Option<String> {
    let r: Vec<&str> = route.trim_end_matches('/').split('/').collect();
    let p: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    let at = r.iter().position(|seg| seg.starts_with('{'))?;
    let from_end = r.len() - at;
    let i = p.len().checked_sub(from_end)?;
    // Every literal segment after the parameter must line up too.
    let tail_matches = r[at + 1..]
        .iter()
        .zip(&p[i + 1..])
        .all(|(a, b)| a.starts_with('{') || a == b);
    tail_matches.then(|| percent_decode(p[i]))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the thing is called now, if homeCore can say.
async fn name_of(s: &AppState, kind: &str, id: &str) -> Option<String> {
    match kind {
        "automation" => {
            let handle = s.source_rules_handle.as_ref().or(s.rules_handle.as_ref())?;
            let rules = handle.read().await;
            rules
                .iter()
                .find(|r| r.id.to_string() == id)
                .map(|r| r.name.clone())
        }
        "device" => s
            .store
            .get_device(id)
            .await
            .ok()
            .flatten()
            .map(|d| d.effective_name().to_string()),
        "area" => {
            let uuid = Uuid::parse_str(id).ok()?;
            s.store.get_area(uuid).await.ok().flatten().map(|a| a.name)
        }
        "scene" => {
            let uuid = Uuid::parse_str(id).ok()?;
            s.store.get_scene(uuid).await.ok().flatten().map(|x| x.name)
        }
        "plugin" => {
            let plugins = s.plugins.read().await;
            plugins.get(id).map(|_| id.to_string())
        }
        _ => None,
    }
}

/// A name in a JSON body: `name`, else `label`.
fn named(v: &Value) -> Option<String> {
    ["name", "label"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

/// What a change touched, from its body: the top-level fields, how many ids a
/// bulk change named, `enabled`, and an action's name — never other values.
fn touched(body: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = body.as_object() {
        let mut fields: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "version")
            .collect();
        fields.sort_unstable();
        out.insert("fields".into(), json!(fields));
        if let Some(e) = obj.get("enabled").and_then(Value::as_bool) {
            out.insert("enabled".into(), json!(e));
        }
        if let Some(ids) = obj.get("ids").and_then(Value::as_array) {
            out.insert("count".into(), json!(ids.len()));
        }
        if let Some(l) = obj.get("level").and_then(Value::as_str) {
            out.insert("level".into(), json!(l));
        }
        if let Some(a) = obj.get("action").and_then(Value::as_str) {
            out.insert("action".into(), json!(a));
        }
    } else if let Some(items) = body.as_array() {
        out.insert("count".into(), json!(items.len()));
    }
    Value::Object(out)
}

fn is_json(req_or_resp: &axum::http::HeaderMap) -> bool {
    req_or_resp
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// The layer itself — on the protected routes, inside authentication.
pub async fn record(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let Some(route) = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
    else {
        return next.run(req).await;
    };
    let Some(change) = change_of(&method, &route) else {
        return next.run(req).await;
    };
    let Some(claims) = req.extensions().get::<Claims>().cloned() else {
        return next.run(req).await;
    };
    let id = param_of(&route, req.uri().path());
    let was = match &id {
        Some(id) => name_of(&s, change.kind, id).await,
        None => None,
    };

    // Read a small JSON body for its name and fields, then hand it on.
    let (parts, body) = req.into_parts();
    let mut sent = Value::Null;
    // Only a body that says it is small: a big import passes untouched, and
    // is recorded without a name.
    let small = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n <= MAX_PEEK);
    let req = if is_json(&parts.headers) && small {
        match to_bytes(body, MAX_PEEK).await {
            Ok(bytes) => {
                sent = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                Request::from_parts(parts, Body::from(bytes))
            }
            // It said it was small and wasn't; it's consumed now.
            Err(_) => {
                return Response::builder()
                    .status(axum::http::StatusCode::PAYLOAD_TOO_LARGE)
                    .body(Body::from("request body too large"))
                    .unwrap_or_default();
            }
        }
    } else {
        Request::from_parts(parts, body)
    };

    let resp = next.run(req).await;
    if !resp.status().is_success() {
        return resp;
    }

    // A creation's id and name come back in its answer.
    let (resp, made) = if id.is_none() && is_json(resp.headers()) {
        let (parts, body) = resp.into_parts();
        match to_bytes(body, MAX_PEEK).await {
            Ok(bytes) => {
                let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                (Response::from_parts(parts, Body::from(bytes)), v)
            }
            Err(_) => (Response::from_parts(parts, Body::empty()), Value::Null),
        }
    } else {
        (resp, Value::Null)
    };

    let target = id.clone().or_else(|| {
        ["id", "device_id", "installed"]
            .iter()
            .find_map(|k| made.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    });
    let mut detail = touched(&sent);
    let name = was
        .clone()
        .or_else(|| named(&made))
        .or_else(|| named(&sent));
    if let Some(n) = &name {
        detail["name"] = json!(n);
    }
    if let (Some(old), Some(new)) = (&was, named(&sent)) {
        if *old != new {
            detail["renamed_to"] = json!(new);
        }
    }

    let mut entry = audit::entry_from_claims(&claims, change.event);
    if let Some(t) = target {
        entry = entry.with_target(change.kind, t);
    }
    entry.detail = detail;
    audit::emit(&s, entry).await;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_known_by_route_and_uses_are_not() {
        let at = |m: Method, r: &str| change_of(&m, r).map(|c| c.event);
        assert_eq!(
            at(Method::PATCH, "/api/v1/automations/{id}"),
            Some("automation.updated")
        );
        assert_eq!(
            at(Method::POST, "/api/v1/areas/{id}/merge"),
            Some("area.merged")
        );
        assert_eq!(at(Method::POST, "/api/v1/glue"), Some("helper.created"));
        // Reads, uses and tests are not changes.
        assert_eq!(at(Method::GET, "/api/v1/automations/{id}"), None);
        assert_eq!(at(Method::PATCH, "/api/v1/devices/{id}/state"), None);
        assert_eq!(at(Method::POST, "/api/v1/scenes/{id}/activate"), None);
        assert_eq!(at(Method::POST, "/api/v1/automations/{id}/test"), None);
        // Handlers that record their own aren't recorded twice.
        assert_eq!(at(Method::DELETE, "/api/v1/plugins/{id}"), None);
        assert_eq!(at(Method::PATCH, "/api/v1/plugins/{id}"), None);
        assert_eq!(at(Method::PATCH, "/api/v1/auth/users/{id}/role"), None);
    }

    #[test]
    fn the_id_comes_from_the_path() {
        assert_eq!(
            param_of("/api/v1/devices/{id}", "/api/v1/devices/hue_1%2Fa"),
            Some("hue_1/a".into())
        );
        assert_eq!(
            param_of("/api/v1/areas/{id}/merge", "/areas/abc/merge"),
            Some("abc".into())
        );
        assert_eq!(param_of("/api/v1/areas", "/areas"), None);
    }

    #[test]
    fn only_harmless_values_are_kept() {
        let d = touched(&json!({"enabled": false, "password": "hunter2", "name": "x"}));
        assert_eq!(d["fields"], json!(["enabled", "name", "password"]));
        assert_eq!(d["enabled"], json!(false));
        assert!(!d.to_string().contains("hunter2"));
        assert_eq!(
            touched(&json!({"ids": ["a", "b"], "enabled": true}))["count"],
            2
        );
    }

    // ---- through the real router ----

    use axum::extract::ConnectInfo;
    use axum::http::{Request as HttpRequest, StatusCode};
    use chrono::Utc;
    use hc_auth::{Role, User};
    use tower::ServiceExt;

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let text = body.to_string();
        let mut req = HttpRequest::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("content-length", text.len())
            .body(Body::from(text))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [10, 9, 8, 7],
                1234,
            ))));
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn a_room_made_renamed_and_removed_is_in_the_audit_by_name() {
        let base = std::env::temp_dir().join(format!("hc_api_change_audit_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = hc_state::StateStore::open(
            &base.join("state.redb").to_string_lossy(),
            &base.join("history.sqlite").to_string_lossy(),
        )
        .await
        .unwrap();
        let ada = User {
            id: Uuid::new_v4(),
            username: "ada".into(),
            password_hash: hc_auth::hash_pin("correct horse").unwrap(),
            role: Role::Admin,
            created_at: Utc::now(),
            token_version: 0,
            pin_hash: None,
        };
        store.create_user(&ada).await.unwrap();
        const SECRET: &[u8] = b"change-audit-secret-32-bytes-min!";
        let token = hc_auth::JwtService::new_hs256(SECRET, 24)
            .issue(&ada.id.to_string(), "ada", Role::Admin, 0)
            .unwrap();
        let state = crate::AppState::new(crate::AppStateParams::new(
            store,
            hc_core::EventBus::new(16),
            hc_auth::JwtService::new_hs256(SECRET, 24),
        ));
        let app = crate::router(state, None);

        let (st, made) = call(&app, "POST", "/areas", &token, json!({"name": "Den"})).await;
        assert!(st.is_success(), "{st} {made}");
        let id = made["id"].as_str().unwrap().to_string();
        let (st, _) = call(
            &app,
            "PATCH",
            &format!("/areas/{id}"),
            &token,
            json!({"name": "Snug"}),
        )
        .await;
        assert!(st.is_success());
        let (st, _) = call(&app, "GET", "/areas", &token, Value::Null).await;
        assert!(st.is_success(), "a read is not a change");
        let (st, _) = call(&app, "DELETE", &format!("/areas/{id}"), &token, Value::Null).await;
        assert!(st.is_success());

        let (_, audit) = call(&app, "GET", "/audit?target_kind=area", &token, Value::Null).await;
        let rows: Vec<(String, String)> = audit
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["event_type"].as_str().unwrap().to_string(),
                    e["detail"]["name"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("area.deleted".into(), "Snug".into()),
                ("area.updated".into(), "Den".into()),
                ("area.created".into(), "Den".into()),
            ]
        );
        let renamed = &audit[1]["detail"];
        assert_eq!(renamed["renamed_to"], "Snug");
        assert_eq!(audit[0]["actor_label"], "user:ada");
        assert_eq!(audit[0]["target_id"], id.as_str());
    }
}
