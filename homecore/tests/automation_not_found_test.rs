//! A rule that isn't there is 404, on every endpoint that takes a rule id —
//! not 400, which says the request itself was wrong. A malformed id (not a
//! UUID) stays 400: that one is.

use hc_api::rule_file_store::RuleFileStore;
use hc_api::{AppState, AppStateParams};
use hc_auth::{JwtService, Role};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::rule::Rule;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Harness {
    base: String,
    token: String,
    _rules_dir: tempfile::TempDir,
    _db: tempfile::TempDir,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl Harness {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let c = reqwest::Client::new();
        let url = format!("{}/api/v1{path}", self.base);
        let req = match method {
            "GET" => c.get(url),
            "POST" => c.post(url),
            "PUT" => c.put(url),
            "PATCH" => c.patch(url),
            "DELETE" => c.delete(url),
            _ => unreachable!(),
        }
        .bearer_auth(&self.token);
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }
}

async fn harness() -> anyhow::Result<Harness> {
    let db = tempfile::tempdir()?;
    let store = StateStore::open(
        db.path().join("state.redb").to_str().unwrap(),
        db.path().join("history.db").to_str().unwrap(),
    )
    .await?;
    let bus = EventBus::new(64);
    let jwt = JwtService::new_hs256(b"automation-not-found-test-secret", 24);
    let uid = uuid::Uuid::new_v4();
    store
        .create_user(&hc_auth::user::User {
            id: uid,
            username: "root".into(),
            password_hash: hc_auth::hash_password("unused")?,
            role: Role::Admin,
            created_at: chrono::Utc::now(),
            token_version: 0,
            pin_hash: None,
        })
        .await?;
    let token = jwt.issue(&uid.to_string(), "root", Role::Admin, 0)?;

    let rules_dir = tempfile::tempdir()?;
    let source: Arc<RwLock<Vec<Rule>>> = Arc::new(RwLock::new(Vec::new()));
    let compiled: Arc<RwLock<Vec<Rule>>> = Arc::new(RwLock::new(Vec::new()));
    let state = AppState::new(AppStateParams {
        source_rules_handle: Some(Arc::clone(&source)),
        rules_handle: Some(Arc::clone(&compiled)),
        rule_file_store: Some(RuleFileStore::new(rules_dir.path())),
        ..AppStateParams::new(store, bus, jwt)
    });

    let port = free_port();
    let (shutdown_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if let Err(e) = hc_api::serve("127.0.0.1", port, state, rx, 5, None, None).await {
            eprintln!("serve failed: {e:?}");
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(Harness {
        base: format!("http://127.0.0.1:{port}"),
        token,
        _rules_dir: rules_dir,
        _db: db,
        _shutdown: shutdown_tx,
    })
}

fn rule_body() -> Value {
    json!({
        "name": "Ghost", "enabled": true, "priority": 0,
        "tags": [], "cancel_on_false": false, "log_actions": false,
        "log_events": false, "log_triggers": false,
        "trigger": "ManualTrigger", "conditions": [],
        "actions": [{ "enabled": true, "action": { "LogMessage": { "message": "hi" } } }]
    })
}

/// Every endpoint addressed by a rule id, with the body it needs.
fn endpoints(id: &str) -> Vec<(&'static str, String, Option<Value>)> {
    vec![
        ("GET", format!("/automations/{id}"), None),
        ("PUT", format!("/automations/{id}"), Some(rule_body())),
        (
            "PATCH",
            format!("/automations/{id}"),
            Some(json!({ "enabled": false })),
        ),
        ("DELETE", format!("/automations/{id}"), None),
        ("GET", format!("/automations/{id}/history"), None),
        ("GET", format!("/automations/{id}/ron"), None),
        ("POST", format!("/automations/{id}/test"), None),
        ("POST", format!("/automations/{id}/run"), Some(json!({}))),
        ("POST", format!("/automations/{id}/clone"), None),
        ("DELETE", format!("/automations/{id}/pause"), None),
    ]
}

#[tokio::test]
async fn a_rule_that_is_not_there_is_404_everywhere() {
    let h = harness().await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    for (method, path, body) in endpoints(&id) {
        let (status, resp) = h.call(method, &path, body).await;
        assert_eq!(status, 404, "{method} {path}: {resp}");
        assert_eq!(resp["error"], "rule not found", "{method} {path}: {resp}");
    }
}

#[tokio::test]
async fn an_id_that_is_not_a_uuid_is_400_with_an_error() {
    let h = harness().await.unwrap();
    for (method, path, body) in endpoints("not-a-uuid") {
        let (status, resp) = h.call(method, &path, body).await;
        assert_eq!(status, 400, "{method} {path}: {resp}");
        assert!(
            resp["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("not an automation id")),
            "{method} {path}: {resp}"
        );
    }
}

#[tokio::test]
async fn a_rule_that_is_there_is_still_found() {
    let h = harness().await.unwrap();
    let body = rule_body();
    let (status, resp) = h.call("POST", "/automations", Some(body)).await;
    assert_eq!(status, 201, "{resp}");
    // Core gives a new rule its own id.
    let id = resp["id"].as_str().unwrap().to_string();

    let (status, resp) = h.call("GET", &format!("/automations/{id}"), None).await;
    assert_eq!(status, 200, "{resp}");
    let (status, resp) = h
        .call("PUT", &format!("/automations/{id}"), Some(rule_body()))
        .await;
    assert_eq!(status, 200, "{resp}");
    let (status, resp) = h.call("GET", &format!("/automations/{id}/ron"), None).await;
    assert_eq!(status, 200, "{resp}");
    // Never paused, but there: answered, not refused.
    let (status, resp) = h
        .call("DELETE", &format!("/automations/{id}/pause"), None)
        .await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["was_paused"], false);
}
