//! Phase 2a streaming-contract integration tests.
//!
//! Drives the `hc-captest` reference plugin end-to-end through the real
//! SDK against a broker, subscribing to the stream topic directly over
//! MQTT. The core-side SSE bridge and concurrency tracker (Phase 2a
//! plumbing) land in a follow-up commit; this file exercises the
//! SDK + MQTT contract that those features sit on top of.

use anyhow::Result;
use hc_broker::{Broker, BrokerConfig};
use plugin_sdk_rs::{PluginClient, PluginConfig};
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Spin up an embedded broker + an in-process hc-captest plugin. Returns
/// (broker_port, observer_client, observer_events, pub_client) where:
/// - `observer_client` is subscribed to `homecore/#`, so stream events
///   land on `observer_events`.
/// - `pub_client` is used to inject `manage/cmd` messages that drive the
///   plugin.
async fn boot_captest() -> Result<(
    u16,
    AsyncClient,
    mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    AsyncClient,
)> {
    let port = free_port();

    Broker::new(BrokerConfig {
        host: "127.0.0.1".into(),
        port,
        ..Default::default()
    })
    .spawn()?;

    // hc-captest plugin client (in-process).
    let plugin_config = PluginConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        plugin_id: hc_captest::PLUGIN_ID.to_string(),
        password: String::new(),
    };
    let plugin = PluginClient::connect(plugin_config).await?;
    let mgmt = plugin
        .enable_management(60, Some("0.1.0-test".into()), None, None)
        .await?;
    let mgmt = hc_captest::register_actions(mgmt);
    tokio::spawn(async move {
        let _ = plugin
            .run_managed(|_dev, _pl| { /* no device commands */ }, mgmt)
            .await;
    });

    // Observer client — subscribes to homecore/# and forwards every
    // publish into a channel so tests can await specific topics/events.
    let mut obs_opts = MqttOptions::new(format!("obs-{}", Uuid::new_v4()), "127.0.0.1", port);
    obs_opts.set_keep_alive(Duration::from_secs(10));
    let (obs_client, mut obs_ev) = AsyncClient::new(obs_opts, 256);
    let (tx, rx) = mpsc::unbounded_channel::<(String, Vec<u8>)>();
    tokio::spawn(async move {
        loop {
            match obs_ev.poll().await {
                Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                    let _ = tx.send((p.topic.clone(), p.payload.to_vec()));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    obs_client.subscribe("homecore/#", QoS::AtLeastOnce).await?;

    // Publisher client — drives the plugin's management cmd topic.
    let mut pub_opts = MqttOptions::new(format!("pub-{}", Uuid::new_v4()), "127.0.0.1", port);
    pub_opts.set_keep_alive(Duration::from_secs(10));
    let (pub_client, mut pub_ev) = AsyncClient::new(pub_opts, 64);
    tokio::spawn(async move {
        loop {
            if pub_ev.poll().await.is_err() {
                break;
            }
        }
    });

    // Give everyone time to connect + subscribe.
    tokio::time::sleep(Duration::from_millis(400)).await;

    Ok((port, obs_client, rx, pub_client))
}

/// Collect stream events on a given topic until a terminal stage is
/// seen. Returns the full ordered list.
async fn collect_until_terminal(
    rx: &mut mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    stream_topic: &str,
    deadline: Duration,
) -> Vec<Value> {
    let mut out = Vec::new();
    let _ = timeout(deadline, async {
        while let Some((topic, payload)) = rx.recv().await {
            if topic != stream_topic {
                continue;
            }
            // Empty retained clear — ignore, it's the post-terminal cleanup.
            if payload.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            let stage = val
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(val);
            if matches!(
                stage.as_str(),
                "complete" | "error" | "canceled" | "timeout"
            ) {
                return;
            }
        }
    })
    .await;
    out
}

async fn publish_cmd(pub_client: &AsyncClient, plugin_id: &str, payload: Value) -> Result<()> {
    let topic = format!("homecore/plugins/{plugin_id}/manage/cmd");
    pub_client
        .publish(
            &topic,
            QoS::AtLeastOnce,
            false,
            payload.to_string().as_bytes(),
        )
        .await?;
    Ok(())
}

/// Wait for the management sync reply matching `request_id`.
async fn await_mgmt_reply(
    rx: &mut mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    plugin_id: &str,
    request_id: &str,
    deadline: Duration,
) -> Option<Value> {
    let topic = format!("homecore/plugins/{plugin_id}/manage/response");
    timeout(deadline, async {
        while let Some((t, payload)) = rx.recv().await {
            if t != topic {
                continue;
            }
            let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            if val.get("request_id").and_then(Value::as_str) == Some(request_id) {
                return Some(val);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn happy_path_item_stream() -> Result<()> {
    let (_port, _obs, mut rx, pub_client) = boot_captest().await?;

    let request_id = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    let stream_topic = format!(
        "homecore/plugins/{}/commands/{}/events",
        hc_captest::PLUGIN_ID,
        request_id
    );

    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "demo_item_stream",
            "request_id": request_id,
            "item_count": 3,
        }),
    )
    .await?;

    // Sync reply should be accepted and advertise the stream topic.
    let reply = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &request_id,
        Duration::from_secs(5),
    )
    .await
    .expect("management reply never arrived");
    assert_eq!(
        reply.get("status").and_then(Value::as_str),
        Some("accepted")
    );
    assert_eq!(
        reply.get("stream_topic").and_then(Value::as_str),
        Some(stream_topic.as_str())
    );

    let events = collect_until_terminal(&mut rx, &stream_topic, Duration::from_secs(5)).await;

    // Expect ordered: progress(0) → (item add, update, progress)*3 → complete
    let stages: Vec<&str> = events
        .iter()
        .map(|e| e.get("stage").and_then(Value::as_str).unwrap_or(""))
        .collect();
    assert_eq!(stages.first(), Some(&"progress"));
    assert_eq!(stages.last(), Some(&"complete"));

    let item_events: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("stage").and_then(Value::as_str) == Some("item"))
        .collect();
    // 3 adds + 3 updates.
    assert_eq!(
        item_events.len(),
        6,
        "expected 6 item events; got {}",
        item_events.len()
    );

    let add_count = item_events
        .iter()
        .filter(|e| e.get("op").and_then(Value::as_str) == Some("add"))
        .count();
    let update_count = item_events
        .iter()
        .filter(|e| e.get("op").and_then(Value::as_str) == Some("update"))
        .count();
    assert_eq!(add_count, 3);
    assert_eq!(update_count, 3);

    // Terminal complete carries the ids_added array.
    let complete = events.last().unwrap();
    let ids = complete["data"]["ids_added"].as_array().unwrap();
    assert_eq!(ids.len(), 3);

    Ok(())
}

#[tokio::test]
async fn cancel_round_trip() -> Result<()> {
    let (_port, _obs, mut rx, pub_client) = boot_captest().await?;

    let request_id = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    let stream_topic = format!(
        "homecore/plugins/{}/commands/{}/events",
        hc_captest::PLUGIN_ID,
        request_id
    );

    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "demo_cancelable",
            "request_id": request_id,
        }),
    )
    .await?;

    let _accepted = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &request_id,
        Duration::from_secs(5),
    )
    .await
    .expect("accepted reply missing");

    // Let the progress loop emit a few iterations.
    tokio::time::sleep(Duration::from_millis(60)).await;

    let cancel_rid = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "cancel",
            "request_id": cancel_rid,
            "target_request_id": request_id,
        }),
    )
    .await?;

    let cancel_reply = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &cancel_rid,
        Duration::from_secs(2),
    )
    .await
    .expect("cancel reply missing");
    assert_eq!(
        cancel_reply.get("status").and_then(Value::as_str),
        Some("ok")
    );

    let events = collect_until_terminal(&mut rx, &stream_topic, Duration::from_secs(3)).await;
    let terminal = events
        .last()
        .unwrap_or_else(|| panic!("no stream events on {stream_topic}"));
    assert_eq!(
        terminal.get("stage").and_then(Value::as_str),
        Some("canceled"),
        "expected canceled terminal; got {terminal:?}"
    );

    Ok(())
}

#[tokio::test]
async fn awaiting_user_with_schema_roundtrip() -> Result<()> {
    let (_port, _obs, mut rx, pub_client) = boot_captest().await?;

    let request_id = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    let stream_topic = format!(
        "homecore/plugins/{}/commands/{}/events",
        hc_captest::PLUGIN_ID,
        request_id
    );

    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "demo_awaiting_user",
            "request_id": request_id,
        }),
    )
    .await?;

    let _ = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &request_id,
        Duration::from_secs(5),
    )
    .await
    .expect("accepted reply missing");

    // Wait for the first advisory awaiting_user to land.
    let mut saw_first = false;
    let mut saw_schema = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !(saw_first && saw_schema) {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some((topic, payload))) if topic == stream_topic => {
                let val: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                if val.get("stage").and_then(Value::as_str) == Some("awaiting_user") {
                    if val.get("response_schema").is_some() {
                        saw_schema = true;
                    } else {
                        saw_first = true;
                    }
                }
            }
            _ => {}
        }
    }
    assert!(saw_first, "first advisory awaiting_user missing");
    assert!(saw_schema, "interactive awaiting_user missing");

    // Respond with proceed=true.
    let respond_rid = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "respond",
            "request_id": respond_rid,
            "target_request_id": request_id,
            "response": { "proceed": true },
        }),
    )
    .await?;

    let _resp_reply = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &respond_rid,
        Duration::from_secs(2),
    )
    .await
    .expect("respond reply missing");

    let events = collect_until_terminal(&mut rx, &stream_topic, Duration::from_secs(3)).await;
    let last = events.last().unwrap();
    assert_eq!(last.get("stage").and_then(Value::as_str), Some("complete"));
    assert_eq!(last["data"]["accepted"], json!(true));

    Ok(())
}

// ── HTTP-level harness ─────────────────────────────────────────────────────
//
// The tests below need the full post_plugin_command handler in the loop,
// not just MQTT. The harness below spins up AppState + axum on a free TCP
// port, pre-seeds hc-captest's capability manifest into the plugin
// registry (skipping state_bridge), and wires a minimal forwarder for
// manage/response → Event::Custom("plugin_management_response") so
// ManagementRpc can resolve requests.

struct HttpHarness {
    base_url: String,
    // Kept so a future role-restructure can add a requires_role:"user" test.
    // Admin has plugins:write today; User doesn't, so this token gets
    // blocked by the base gate before the manifest check can fire.
    #[allow(dead_code)]
    user_token: String,
    admin_token: String,
    _shutdown_tx: tokio::sync::watch::Sender<bool>,
    _keepalive: Vec<tokio::task::JoinHandle<()>>,
    _state_db: String,
    _history_db: String,
}

async fn boot_http_harness() -> Result<HttpHarness> {
    use chrono::Utc;
    use hc_api::{management_rpc::ManagementRpc, AppState, PluginRecord};
    use hc_auth::{JwtService, Role};
    use hc_core::EventBus;
    use hc_mqtt_client::{MqttClient, MqttClientConfig};
    use hc_state::StateStore;
    use hc_types::event::Event;

    let port = free_port();
    Broker::new(BrokerConfig {
        host: "127.0.0.1".into(),
        port,
        ..Default::default()
    })
    .spawn()?;

    // In-process hc-captest plugin.
    let plugin_config = PluginConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        plugin_id: hc_captest::PLUGIN_ID.to_string(),
        password: String::new(),
    };
    let plugin = PluginClient::connect(plugin_config).await?;
    let mgmt = plugin
        .enable_management(60, Some("0.1.0-test".into()), None, None)
        .await?;
    let mgmt = hc_captest::register_actions(mgmt);
    let plugin_task = tokio::spawn(async move {
        let _ = plugin
            .run_managed(|_dev, _pl| { /* no device commands */ }, mgmt)
            .await;
    });

    // Core MQTT client — bridges MQTT → event bus.
    let (mqtt_client, mut mqtt_rx) = MqttClient::new(MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        client_id: "internal.core".into(),
        username: None,
        password: None,
    });
    let publish_handle = mqtt_client.publish_handle();
    let mqtt_task = tokio::spawn(async move {
        let _ = mqtt_client.run().await;
    });

    let bus = EventBus::new(512);

    // Forwarder 1: mqtt_rx → bus (MqttMessage only — what the client emits).
    let bus_fwd = bus.clone();
    let fwd_task = tokio::spawn(async move {
        loop {
            match mqtt_rx.recv().await {
                Ok(ev) => {
                    let _ = bus_fwd.publish(ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Forwarder 2 (state_bridge shim): synthesise the events downstream
    // code expects. ManagementRpc needs Event::Custom with
    // event_type="plugin_management_response". We skip the rest of
    // state_bridge and pre-seed PluginRecord instead.
    {
        let mut rx = bus.subscribe();
        let bus_shim = bus.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Event::MqttMessage { topic, payload, .. }) => {
                        let parts: Vec<&str> = topic.split('/').collect();
                        if parts.len() == 5
                            && parts[0] == "homecore"
                            && parts[1] == "plugins"
                            && parts[3] == "manage"
                            && parts[4] == "response"
                        {
                            if let Ok(resp) = serde_json::from_slice::<Value>(&payload) {
                                let _ = bus_shim.publish(Event::Custom {
                                    timestamp: Utc::now(),
                                    event_type: "plugin_management_response".into(),
                                    payload: resp,
                                });
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // State store.
    let state_db = format!("/tmp/hc-stream-test-{port}.redb");
    let history_db = format!("/tmp/hc-stream-test-{port}.db");
    let _ = std::fs::remove_file(&state_db);
    let _ = std::fs::remove_file(&history_db);
    let store = StateStore::open(&state_db, &history_db).await?;

    // JwtService with fixed test secret.
    let jwt_secret = b"test-secret-fixed-bytes-for-streaming-tests-32b";
    let jwt = JwtService::new_hs256(jwt_secret, 24);
    let user_token = jwt.issue("uid-user", "alice", Role::User)?;
    let admin_token = jwt.issue("uid-admin", "root", Role::Admin)?;

    // AppState.
    let state = AppState::new(
        store,
        bus.clone(),
        Some(publish_handle.clone()),
        None,
        None,
        None,
        jwt,
        vec![],
        None,
    );
    // Wire management_rpc so post_plugin_command forwards commands.
    let rpc = ManagementRpc::new(publish_handle.clone(), &bus);
    let state = AppState {
        management_rpc: Some(rpc),
        ..state
    };
    // Pre-seed the plugin registry with hc-captest's manifest so the
    // role-check and concurrency-check branches in post_plugin_command
    // fire. (Skips state_bridge's PluginCapabilities decode path.)
    {
        let mut map = state.plugins.write().await;
        map.insert(
            hc_captest::PLUGIN_ID.into(),
            PluginRecord {
                plugin_id: hc_captest::PLUGIN_ID.into(),
                registered_at: chrono::Utc::now(),
                status: "active".into(),
                enabled: true,
                managed: false,
                config_path: None,
                binary_path: None,
                last_heartbeat: None,
                last_restart: None,
                restart_count: 0,
                uptime_started: None,
                device_count: 0,
                log_level: None,
                version: Some("0.1.0-test".into()),
                supports_management: true,
                capabilities: Some(hc_captest::capabilities_manifest()),
            },
        );
    }

    // HTTP listener.
    let tcp_port = free_port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state_clone = state.clone();
    let serve_task = tokio::spawn(async move {
        let _ = hc_api::serve(
            "127.0.0.1",
            tcp_port,
            state_clone,
            shutdown_rx,
            2,
            None,
            None,
        )
        .await;
    });
    drop(state);

    // Wait for listener.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", tcp_port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(HttpHarness {
        base_url: format!("http://127.0.0.1:{tcp_port}"),
        user_token,
        admin_token,
        _shutdown_tx: shutdown_tx,
        _keepalive: vec![plugin_task, mqtt_task, fwd_task, serve_task],
        _state_db: state_db,
        _history_db: history_db,
    })
}

#[tokio::test]
async fn concurrency_single_blocks_second_caller() -> Result<()> {
    let h = boot_http_harness().await?;
    let client = reqwest::Client::new();

    // Base guard on /plugins/:id/command is PluginsWrite, which only Admin
    // role has today. Use admin_token for both calls so the handler
    // actually reaches the concurrency check. A future role-restructure
    // (see project_auth_expansion.md) will let a requires_role:"user"
    // test exercise a non-admin path.
    let resp1 = client
        .post(format!(
            "{}/api/v1/plugins/{}/command",
            h.base_url,
            hc_captest::PLUGIN_ID
        ))
        .bearer_auth(&h.admin_token)
        .json(&json!({ "action": "demo_cancelable" }))
        .send()
        .await?;
    assert_eq!(resp1.status(), 200);
    let body1: Value = resp1.json().await?;
    assert_eq!(
        body1.get("status").and_then(Value::as_str),
        Some("accepted")
    );
    let first_rid = body1["request_id"].as_str().unwrap().to_string();

    // Give the tracker a moment to stabilise.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second call — should get 409 busy with the first's request_id.
    let resp2 = client
        .post(format!(
            "{}/api/v1/plugins/{}/command",
            h.base_url,
            hc_captest::PLUGIN_ID
        ))
        .bearer_auth(&h.admin_token)
        .json(&json!({ "action": "demo_cancelable" }))
        .send()
        .await?;
    assert_eq!(resp2.status(), 409, "second invocation should be 409 busy");
    let body2: Value = resp2.json().await?;
    assert_eq!(body2.get("status").and_then(Value::as_str), Some("busy"));
    assert_eq!(
        body2.get("active_request_id").and_then(Value::as_str),
        Some(first_rid.as_str())
    );

    Ok(())
}

#[tokio::test]
async fn core_injects_synthetic_timeout() -> Result<()> {
    let h = boot_http_harness().await?;
    let client = reqwest::Client::new();

    // Kick off demo_never_completes (manifest timeout_ms:300). Admin
    // token — see concurrency_single_blocks_second_caller for rationale.
    let resp = client
        .post(format!(
            "{}/api/v1/plugins/{}/command",
            h.base_url,
            hc_captest::PLUGIN_ID
        ))
        .bearer_auth(&h.admin_token)
        .json(&json!({ "action": "demo_never_completes" }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await?;
    assert_eq!(body.get("status").and_then(Value::as_str), Some("accepted"));
    let request_id = body["request_id"].as_str().unwrap().to_string();

    // Poll the HTTP SSE endpoint — if core's injection fires, an SSE
    // consumer will see the synthetic `timeout` terminal within
    // manifest.timeout_ms + a small budget.
    let sse_url = format!(
        "{}/api/v1/plugins/{}/command/{}/stream",
        h.base_url,
        hc_captest::PLUGIN_ID,
        request_id
    );
    let sse_resp = client
        .get(&sse_url)
        .bearer_auth(&h.admin_token)
        .header("accept", "text/event-stream")
        .send()
        .await?;
    assert_eq!(sse_resp.status(), 200);

    // Read SSE body until we see a data: line containing "stage":"timeout".
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut stream = sse_resp.bytes_stream();
    let mut buf = String::new();
    use tokio_stream::StreamExt;
    let mut saw_timeout = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(Duration::from_millis(500), stream.next()).await
        {
            if let Ok(s) = std::str::from_utf8(&chunk) {
                buf.push_str(s);
                if buf.contains("\"stage\":\"timeout\"") {
                    saw_timeout = true;
                    break;
                }
            }
        }
    }
    assert!(
        saw_timeout,
        "SSE bridge never delivered synthetic timeout; buf={buf}"
    );

    Ok(())
}

#[tokio::test]
async fn error_is_terminal_after_warnings() -> Result<()> {
    let (_port, _obs, mut rx, pub_client) = boot_captest().await?;

    let request_id = format!("r-{}", &Uuid::new_v4().to_string()[..8]);
    let stream_topic = format!(
        "homecore/plugins/{}/commands/{}/events",
        hc_captest::PLUGIN_ID,
        request_id
    );

    publish_cmd(
        &pub_client,
        hc_captest::PLUGIN_ID,
        json!({
            "action": "demo_error_vs_warning",
            "request_id": request_id,
            "warnings": 2,
        }),
    )
    .await?;

    let _ = await_mgmt_reply(
        &mut rx,
        hc_captest::PLUGIN_ID,
        &request_id,
        Duration::from_secs(5),
    )
    .await
    .expect("accepted reply missing");

    let events = collect_until_terminal(&mut rx, &stream_topic, Duration::from_secs(3)).await;
    let stages: Vec<&str> = events
        .iter()
        .map(|e| e.get("stage").and_then(Value::as_str).unwrap_or(""))
        .collect();
    assert_eq!(stages.iter().filter(|s| **s == "warning").count(), 2);
    assert_eq!(stages.last(), Some(&"error"));

    Ok(())
}
