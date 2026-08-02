//! `POST /automations/import` is one operation, not N.
//!
//! The failure this guards against is real: a 42-rule export whose references
//! predated a device rename aborted partway, leaving the rules before the bad
//! one live on disk with no record of which had landed. A half-imported
//! ruleset is worse than a rejected one, because the half that ran is
//! automation nobody knowingly enabled.

use hc_api::rule_file_store::RuleFileStore;
use hc_api::{AppState, AppStateParams};
use hc_auth::{JwtService, Role};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceState;
use hc_types::rule::Rule;
use std::sync::Arc;
use tokio::sync::RwLock;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// A rule that turns `target` on when `target` changes.
///
/// Written as JSON rather than the Rule struct: this is exactly what the
/// endpoint receives, and it does not need editing every time `Rule` grows a
/// field.
fn rule_for(name: &str, target: &str) -> serde_json::Value {
    serde_json::json!({
        // Mirrors the field set a real export carries. `Rule` has several
        // required fields (id, priority, the log_* flags), so a trimmed
        // fixture is rejected by the extractor before the handler is reached
        // — which looks exactly like a handler bug and is not one.
        "id": uuid::Uuid::new_v4(),
        "name": name,
        "enabled": true,
        "priority": 10,
        "tags": [],
        "cancel_on_false": false,
        "log_actions": false,
        "log_events": false,
        "log_triggers": false,
        "trigger": { "DeviceStateChanged": { "device_id": target } },
        "conditions": [],
        "actions": [{
            "action": { "SetDeviceState": {
                "device_id": target,
                "state": { "on": true },
                "track_event_value": false
            }},
            "enabled": true
        }]
    })
}

struct Harness {
    base: String,
    token: String,
    rules_dir: tempfile::TempDir,
    source: Arc<RwLock<Vec<Rule>>>,
    compiled: Arc<RwLock<Vec<Rule>>>,
    _db: tempfile::TempDir,
    /// Held: dropping the sender signals shutdown, which closed the listener
    /// the moment the harness returned.
    _shutdown: tokio::sync::watch::Sender<bool>,
}

impl Harness {
    fn rule_files(&self) -> usize {
        std::fs::read_dir(self.rules_dir.path())
            .map(|d| d.filter_map(Result::ok).count())
            .unwrap_or(0)
    }
}

async fn harness() -> anyhow::Result<Harness> {
    let db = tempfile::tempdir()?;
    let store = StateStore::open(
        db.path().join("state.redb").to_str().unwrap(),
        db.path().join("history.db").to_str().unwrap(),
    )
    .await?;

    // One real device, so a reference to it resolves and anything else does not.
    let mut dev = DeviceState::new("light_real", "Real Light", "plugin.test");
    dev.available = true;
    store.upsert_device(&dev).await?;

    let bus = EventBus::new(64);
    let jwt = JwtService::new_hs256(b"import-atomicity-test-secret", 24);
    let uid = uuid::Uuid::new_v4();
    store
        .create_user(&hc_auth::user::User {
            id: uid,
            username: "root".into(),
            password_hash: hc_auth::hash_password("unused")?,
            role: Role::Admin,
            created_at: chrono::Utc::now(),
            token_version: 0,
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
        rules_dir,
        source,
        compiled,
        _db: db,
        _shutdown: shutdown_tx,
    })
}

async fn post_import(h: &Harness, rules: &[serde_json::Value]) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/automations/import", h.base))
        .bearer_auth(&h.token)
        .json(rules)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

#[tokio::test]
async fn a_bad_rule_midway_imports_nothing_at_all() {
    let h = harness().await.unwrap();

    // Good, good, BAD, good — the shape that used to leave two rules live.
    let batch = vec![
        rule_for("first good", "light_real"),
        rule_for("second good", "light_real"),
        rule_for("the bad one", "device_that_does_not_exist"),
        rule_for("after the bad one", "light_real"),
    ];

    let (status, body) = post_import(&h, &batch).await;
    assert_eq!(status, 422, "expected rejection, got {status}: {body}");
    assert_eq!(body["imported"], 0);

    // The point of the whole exercise: nothing landed anywhere.
    assert_eq!(h.rule_files(), 0, "rule files were written despite the 422");
    assert!(h.source.read().await.is_empty(), "rules went live anyway");
    assert!(h.compiled.read().await.is_empty(), "compiled rules leaked");
}

#[tokio::test]
async fn every_failure_is_reported_not_just_the_first() {
    let h = harness().await.unwrap();
    let batch = vec![
        rule_for("bad one", "missing_a"),
        rule_for("good one", "light_real"),
        rule_for("bad two", "missing_b"),
    ];

    let (status, body) = post_import(&h, &batch).await;
    assert_eq!(status, 422);

    // Fixing an export one round-trip per broken reference is its own kind of
    // broken; the caller should see the whole list at once.
    let failures = body["failures"].as_array().expect("failures array");
    assert_eq!(failures.len(), 2, "body: {body}");
    let names: Vec<&str> = failures
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"bad one") && names.contains(&"bad two"));
    // Index is carried so a caller can point at the offending entry.
    assert_eq!(failures[0]["index"], 0);
    assert_eq!(failures[1]["index"], 2);
}

#[tokio::test]
async fn a_fully_valid_batch_still_imports() {
    let h = harness().await.unwrap();
    let batch = vec![
        rule_for("one", "light_real"),
        rule_for("two", "light_real"),
        rule_for("three", "light_real"),
    ];

    let (status, body) = post_import(&h, &batch).await;
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["imported"], 3);
    assert_eq!(h.rule_files(), 3);
    assert_eq!(h.source.read().await.len(), 3);
    assert_eq!(h.compiled.read().await.len(), 3);

    // Import always creates, so ids from the payload are never honoured —
    // importing the same file twice is six rules, not three.
    let (status2, _) = post_import(&h, &batch).await;
    assert_eq!(status2, 201);
    assert_eq!(h.source.read().await.len(), 6);
}
