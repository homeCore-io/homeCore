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
async fn boot_captest() -> Result<(u16, AsyncClient, mpsc::UnboundedReceiver<(String, Vec<u8>)>, AsyncClient)> {
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
    let mut obs_opts = MqttOptions::new(
        format!("obs-{}", Uuid::new_v4()),
        "127.0.0.1",
        port,
    );
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
    let mut pub_opts = MqttOptions::new(
        format!("pub-{}", Uuid::new_v4()),
        "127.0.0.1",
        port,
    );
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

async fn publish_cmd(
    pub_client: &AsyncClient,
    plugin_id: &str,
    payload: Value,
) -> Result<()> {
    let topic = format!("homecore/plugins/{plugin_id}/manage/cmd");
    pub_client
        .publish(&topic, QoS::AtLeastOnce, false, payload.to_string().as_bytes())
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
    assert_eq!(reply.get("status").and_then(Value::as_str), Some("accepted"));
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
    assert_eq!(item_events.len(), 6, "expected 6 item events; got {}", item_events.len());

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
    assert_eq!(
        last.get("stage").and_then(Value::as_str),
        Some("complete")
    );
    assert_eq!(last["data"]["accepted"], json!(true));

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
