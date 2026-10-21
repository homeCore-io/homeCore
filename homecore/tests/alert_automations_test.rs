//! Alerts that start automations, through the API: what each alert rule
//! starts, an automation waiting while its rule is off, and deleting a rule
//! that starts automations — refused until the caller says what becomes of
//! them, then either deleting them or keeping them turned off, with one that
//! has another trigger only losing the rule's.

use hc_api::rule_file_store::RuleFileStore;
use hc_api::{AppState, AppStateParams};
use hc_auth::{JwtService, Role};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceState;
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
    source: Arc<RwLock<Vec<Rule>>>,
    compiled: Arc<RwLock<Vec<Rule>>>,
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

    async fn automation(&self, name: &str, triggers: Value) -> String {
        let list = triggers.as_array().unwrap();
        let (status, body) = self
            .call(
                "POST",
                "/automations",
                Some(json!({
                    "id": uuid::Uuid::new_v4(), "name": name, "enabled": true, "priority": 0,
                    "tags": [], "cancel_on_false": false, "log_actions": false,
                    "log_events": false, "log_triggers": false,
                    "trigger": list[0], "triggers": list[1..], "conditions": [],
                    "actions": [{ "enabled": true, "action": { "LogMessage": { "message": "{alert.message}" } } }]
                })),
            )
            .await;
        assert!(status < 300, "create {name}: {status} {body}");
        body["id"].as_str().unwrap().to_string()
    }

    async fn links(&self) -> Vec<Value> {
        let (status, body) = self.call("GET", "/alerts/automations", None).await;
        assert_eq!(status, 200, "{body}");
        body.as_array().unwrap().clone()
    }
}

async fn harness() -> anyhow::Result<Harness> {
    let db = tempfile::tempdir()?;
    let store = StateStore::open(
        db.path().join("state.redb").to_str().unwrap(),
        db.path().join("history.db").to_str().unwrap(),
    )
    .await?;
    let mut freezer = DeviceState::new("freezer", "Garage Freezer", "plugin.yolink");
    freezer.available = true;
    store.upsert_device(&freezer).await?;

    let bus = EventBus::new(64);
    let jwt = JwtService::new_hs256(b"alert-automations-test-secret", 24);
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
    })
    .with_alerts(hc_core::alerts::AlertsHandle::new());

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
        source,
        compiled,
        _rules_dir: rules_dir,
        _db: db,
        _shutdown: shutdown_tx,
    })
}

async fn freezer_rule(h: &Harness, name: &str) -> String {
    let (status, body) = h
        .call(
            "POST",
            "/alert-rules",
            Some(json!({
                "name": name, "device_ids": ["freezer"], "attribute": "temperature",
                "value": -10, "op": "above", "for_minutes": 15
            })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    body["id"].as_str().unwrap().to_string()
}

fn by_name<'a>(list: &'a [Value], name: &str) -> &'a Value {
    list.iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("{name} not listed"))
}

#[tokio::test]
async fn a_rule_lists_what_it_starts_and_its_automations_wait_while_it_is_off() {
    let h = harness().await.unwrap();
    let r = freezer_rule(&h, "Freezer warming up").await;
    h.automation(
        "Flash",
        json!([{ "AlertRaised": { "filter": { "kind": "rule", "rule_id": r } } }]),
    )
    .await;
    h.automation(
        "Valve",
        json!([{ "DeviceStateChanged": { "device_id": "freezer" } }]),
    )
    .await;

    let list = h.links().await;
    assert_eq!(list.len(), 1, "only the alert's: {list:?}");
    let flash = by_name(&list, "Flash");
    assert_eq!(flash["links"][0]["sources"], json!([format!("rule:{r}")]));
    assert_eq!(flash["waiting"], false);

    let (_, starts) = h
        .call("GET", &format!("/alert-rules/{r}/automations"), None)
        .await;
    assert_eq!(starts[0]["name"], "Flash");
    assert_eq!(starts[0]["keeps_running"], false);

    // Turned off: its automation waits.
    let (_, mut rule) = h.call("GET", "/alert-rules", None).await;
    rule[0]["enabled"] = json!(false);
    let (status, _) = h
        .call("PUT", &format!("/alert-rules/{r}"), Some(rule[0].clone()))
        .await;
    assert_eq!(status, 200);
    let list = h.links().await;
    let flash = by_name(&list, "Flash");
    assert_eq!(flash["waiting"], true);
    assert_eq!(flash["links"][0]["waiting"], "off");

    // Ignored instead: still waiting, for that reason.
    rule[0]["enabled"] = json!(true);
    h.call("PUT", &format!("/alert-rules/{r}"), Some(rule[0].clone()))
        .await;
    let (status, _) = h
        .call("POST", "/alert-ignores", Some(json!({ "rule_id": r })))
        .await;
    assert_eq!(status, 201);
    let list = h.links().await;
    assert_eq!(by_name(&list, "Flash")["links"][0]["waiting"], "ignored");
}

#[tokio::test]
async fn deleting_a_rule_that_starts_automations_asks_first() {
    let h = harness().await.unwrap();
    let r = freezer_rule(&h, "Freezer warming up").await;
    let flash = h
        .automation(
            "Flash",
            json!([{ "AlertRaised": { "filter": { "kind": "rule", "rule_id": r } } }]),
        )
        .await;
    let say = h
        .automation(
            "Say it",
            json!([
                { "AlertActiveFor": { "filter": { "rule_id": r }, "minutes": 10 } },
                { "AlertRaised": { "filter": { "kind": "offline" } } }
            ]),
        )
        .await;

    // No choice: nothing changes.
    let (status, body) = h.call("DELETE", &format!("/alert-rules/{r}"), None).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["automations"].as_array().unwrap().len(), 2);
    let (_, rules) = h.call("GET", "/alert-rules", None).await;
    assert_eq!(
        rules.as_array().unwrap().len(),
        1,
        "the rule is still there"
    );
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/alert-rules/{r}?automations=maybe"),
            None,
        )
        .await;
    assert_eq!(status, 400);

    // Kept, turned off: the one it alone started is off and marked; the one
    // with another trigger runs on with only that.
    let (status, body) = h
        .call(
            "DELETE",
            &format!("/alert-rules/{r}?automations=disable"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["disabled"][0]["name"], "Flash");
    assert_eq!(body["kept"][0]["name"], "Say it");
    let (_, rules) = h.call("GET", "/alert-rules", None).await;
    assert!(rules.as_array().unwrap().is_empty());

    let (_, f) = h.call("GET", &format!("/automations/{flash}"), None).await;
    assert_eq!(f["enabled"], false);
    assert_eq!(
        f["error"],
        "its alert was deleted — choose another trigger or delete it"
    );
    assert!(f["trigger"]["AlertRaised"]["filter"]["rule_id"]
        .as_str()
        .unwrap()
        .starts_with("DELETED:"));
    let (_, s) = h.call("GET", &format!("/automations/{say}"), None).await;
    assert_eq!(s["enabled"], true);
    assert!(s["trigger"]["AlertRaised"].is_object());
    assert!(s
        .get("triggers")
        .is_none_or(|t| t.as_array().unwrap().is_empty()));

    let list = h.links().await;
    assert_eq!(by_name(&list, "Flash")["alert_deleted"], true);

    // It can't be turned on until it has another trigger…
    let (status, _) = h
        .call(
            "PATCH",
            &format!("/automations/{flash}"),
            Some(json!({ "enabled": true })),
        )
        .await;
    assert_eq!(status, 422);
    let compiled = h.compiled.read().await;
    let live = compiled.iter().find(|x| x.id.to_string() == flash).unwrap();
    assert!(!live.enabled, "the refused patch changed nothing");
    drop(compiled);
    // …but its steps can be changed while it is off.
    let mut changed = f.clone();
    changed["name"] = json!("Flash, renamed");
    let (status, body) = h
        .call(
            "PUT",
            &format!("/automations/{flash}"),
            Some(changed.clone()),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    // Given another trigger, it may run again, and says nothing is wrong.
    changed["trigger"] = json!({ "AlertRaised": { "filter": { "kind": "wet" } } });
    changed["enabled"] = json!(true);
    let (status, body) = h
        .call("PUT", &format!("/automations/{flash}"), Some(changed))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("error").is_none_or(Value::is_null), "{body}");
}

#[tokio::test]
async fn deleting_a_rule_and_its_automations_keeps_one_with_another_trigger() {
    let h = harness().await.unwrap();
    let r = freezer_rule(&h, "Freezer warming up").await;
    let power = freezer_rule(&h, "Power cut").await;
    let flash = h
        .automation(
            "Flash",
            json!([{ "AlertRaised": { "filter": { "rule_id": r } } }]),
        )
        .await;
    let say = h
        .automation(
            "Say it",
            json!([
                { "AlertRaised": { "filter": { "rule_id": r } } },
                { "AlertRaised": { "filter": { "rule_id": power } } }
            ]),
        )
        .await;
    let (status, body) = h
        .call(
            "DELETE",
            &format!("/alert-rules/{r}?automations=delete"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"][0]["name"], "Flash");
    assert_eq!(body["kept"][0]["name"], "Say it");

    let (status, _) = h.call("GET", &format!("/automations/{flash}"), None).await;
    assert_eq!(status, 404);
    assert!(!h
        .source
        .read()
        .await
        .iter()
        .any(|x| x.id.to_string() == flash));
    let (_, s) = h.call("GET", &format!("/automations/{say}"), None).await;
    assert_eq!(
        s["trigger"]["AlertRaised"]["filter"]["rule_id"], power,
        "only the deleted rule's trigger went"
    );

    // A rule that starts nothing is deleted as before.
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/alert-rules/{power}?automations=delete"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let lone = freezer_rule(&h, "Lone").await;
    let (status, _) = h
        .call("DELETE", &format!("/alert-rules/{lone}"), None)
        .await;
    assert_eq!(status, 204);

    // Deleting an automation leaves its alert rule alone.
    let keep = freezer_rule(&h, "Keep me").await;
    let auto = h
        .automation(
            "Mine",
            json!([{ "AlertRaised": { "filter": { "rule_id": keep } } }]),
        )
        .await;
    let (status, _) = h
        .call("DELETE", &format!("/automations/{auto}"), None)
        .await;
    assert_eq!(status, 204);
    let (_, rules) = h.call("GET", "/alert-rules", None).await;
    assert!(rules.as_array().unwrap().iter().any(|x| x["id"] == keep));
}
