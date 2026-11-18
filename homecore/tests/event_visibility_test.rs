//! A caller is sent only the events they may read through each event's own
//! resource, on the live stream and in the recent-events log alike.
//!
//! Both used to hand everything to anyone signed in. A key made for one
//! narrow job (`devices:read`, as a wall panel or a plugin holds) could read
//! every rule as it was saved and every webhook call, whose path is the
//! webhook's secret: knowing it is enough to fire the rules behind it.

use hc_api::{AppState, AppStateParams};
use hc_auth::{JwtService, Role};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use hc_types::event::Event;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_tungstenite::tungstenite::Message;

const SECRET_PATH: &str = "front-gate-7f3a9c";

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Harness {
    port: u16,
    admin: String,
    bus: EventBus,
    _db: tempfile::TempDir,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

async fn harness() -> anyhow::Result<Harness> {
    let db = tempfile::tempdir()?;
    let store = StateStore::open(
        db.path().join("state.redb").to_str().unwrap(),
        db.path().join("history.db").to_str().unwrap(),
    )
    .await?;
    let bus = EventBus::new(256);
    let jwt = JwtService::new_hs256(b"event-visibility-test-secret-32b!", 24);
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
    let admin = jwt.issue(&uid.to_string(), "root", Role::Admin, 0)?;
    let state = AppState::new(AppStateParams::new(store, bus.clone(), jwt));

    let port = free_port();
    let (shutdown_tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if let Err(e) = hc_api::serve("127.0.0.1", port, state, rx, 5, None, None).await {
            eprintln!("serve failed: {e:?}");
        }
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(Harness {
        port,
        admin,
        bus,
        _db: db,
        _shutdown: shutdown_tx,
    })
}

impl Harness {
    async fn key(&self, scopes: &[&str]) -> String {
        let resp: Value = reqwest::Client::new()
            .post(format!(
                "http://127.0.0.1:{}/api/v1/auth/api-keys",
                self.port
            ))
            .bearer_auth(&self.admin)
            .json(&json!({ "label": "narrow", "scopes": scopes }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp["token"].as_str().expect("a key is made").to_string()
    }

    async fn events(&self, token: &str) -> Vec<Value> {
        let resp = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/api/v1/events?limit=100",
                self.port
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    async fn stream(
        &self,
        token: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!(
            "ws://127.0.0.1:{}/api/v1/events/stream?token={token}",
            self.port
        );
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("stream opens");
        ws
    }

    /// A webhook call, a saved rule, then a device change, in that order.
    fn publish(&self) {
        self.bus
            .publish(Event::Custom {
                timestamp: chrono::Utc::now(),
                event_type: "webhook".into(),
                payload: json!({ "path": SECRET_PATH, "body": null, "query": null }),
            })
            .unwrap();
        self.bus
            .publish(Event::RuleRemoved {
                timestamp: chrono::Utc::now(),
                rule_id: uuid::Uuid::new_v4(),
            })
            .unwrap();
        self.bus
            .publish(Event::DeviceStateChanged {
                timestamp: chrono::Utc::now(),
                device_id: "light.porch".into(),
                device_name: None,
                previous: Default::default(),
                current: Default::default(),
                changed: vec![],
                change: DeviceChange::unknown(),
            })
            .unwrap();
    }
}

/// The frames a stream is sent, up to and including the device change that
/// [`Harness::publish`] sends last.
async fn read_through_device_event<S>(ws: &mut S) -> Vec<Value>
where
    S: tokio_stream::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut seen = Vec::new();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame arrives")
            .expect("the stream stays open")
            .expect("a good frame");
        if let Message::Text(text) = msg {
            let v: Value = serde_json::from_str(&text).unwrap();
            let last = v["type"] == "device_state_changed";
            seen.push(v);
            if last {
                return seen;
            }
        }
    }
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_device_reading_key_gets_device_events_and_no_rules_or_webhooks() {
    let h = harness().await.unwrap();
    let key = h.key(&["devices:read"]).await;

    let mut narrow = h.stream(&key).await;
    let mut admin = h.stream(&h.admin).await;
    // Ask for the rule and webhook kinds by name too: naming them must not
    // get round who may see them.
    let mut asking = {
        let url = format!(
            "ws://127.0.0.1:{}/api/v1/events/stream?token={key}&type=custom,rule_removed,device_state_changed",
            h.port
        );
        tokio_tungstenite::connect_async(url).await.unwrap().0
    };
    // The streams subscribe once upgraded; give them a moment to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.publish();

    for (who, ws) in [("narrow key", &mut narrow), ("asking key", &mut asking)] {
        let seen = read_through_device_event(ws).await;
        assert_eq!(types(&seen), ["device_state_changed"], "{who}");
        assert!(
            !serde_json::to_string(&seen).unwrap().contains(SECRET_PATH),
            "{who} was sent the webhook's path"
        );
    }
    let seen = read_through_device_event(&mut admin).await;
    assert_eq!(
        types(&seen),
        ["custom", "rule_removed", "device_state_changed"],
        "the admin"
    );
    assert_eq!(seen[0]["payload"]["path"], SECRET_PATH);

    // The log answers the same way.
    let logged = h.events(&key).await;
    assert_eq!(
        logged
            .iter()
            .map(|e| e["event_type"].clone())
            .collect::<Vec<_>>(),
        [json!("device_state_changed")]
    );
    assert!(!serde_json::to_string(&logged)
        .unwrap()
        .contains(SECRET_PATH));

    let logged = h.events(&h.admin).await;
    let kinds: Vec<Value> = logged.iter().map(|e| e["event_type"].clone()).collect();
    assert_eq!(
        kinds,
        [
            json!("device_state_changed"),
            json!("rule_removed"),
            json!("custom")
        ]
    );
}

#[tokio::test]
async fn a_rule_reading_key_gets_rules_and_webhooks() {
    let h = harness().await.unwrap();
    let key = h.key(&["devices:read", "automations:read"]).await;
    let mut ws = h.stream(&key).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.publish();
    let seen = read_through_device_event(&mut ws).await;
    assert_eq!(
        types(&seen),
        ["custom", "rule_removed", "device_state_changed"]
    );
}
