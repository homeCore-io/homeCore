//! Integration test: virtual device → MQTT → rule fires → command back.
//!
//! Scenario:
//! 1. Start the embedded broker on a random port.
//! 2. Start the state bridge + rule engine wired to a temp state store.
//! 3. Create a rule: when `test_light` attribute `on` changes → publish cmd.
//! 4. Publish `homecore/devices/test_light/state` → `{"on":true}`.
//! 5. Assert `DeviceStateChanged` and `RuleFired` arrive on the bus.

use anyhow::Result;
use hc_broker::{Broker, BrokerConfig};
use hc_core::{Core, EventBus};
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_state::StateStore;
use hc_types::event::Event;
use hc_types::rule::{Action, Rule, Trigger};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test]
async fn virtual_device_triggers_rule_and_command() -> Result<()> {
    let port = free_port();

    // 1. Embedded broker.
    Broker::new(BrokerConfig { host: "127.0.0.1".into(), port, ..Default::default() }).spawn()?;

    // 2. State store (temp files unique to this port).
    let state_db = format!("/tmp/hc-test-{port}.redb");
    let history_db = format!("/tmp/hc-test-{port}.db");
    // Remove stale files from a previous run.
    let _ = std::fs::remove_file(&state_db);
    let _ = std::fs::remove_file(&history_db);
    let store = StateStore::open(&state_db, &history_db).await?;

    // 3. Rule: DeviceStateChanged on test_light.on → PublishMqtt cmd.
    let rule = Rule {
        id: Uuid::new_v4(),
        name: "test_rule".into(),
        enabled: true,
        priority: 0,
        tags: vec![],
        trigger: Trigger::DeviceStateChanged {
            device_id: "test_light".into(),
            attribute: Some("on".into()),
            to: None,
        },
        conditions: vec![],
        actions: vec![Action::PublishMqtt {
            topic: "homecore/devices/test_light/cmd".into(),
            payload: r#"{"action":"toggle_confirmed"}"#.into(),
            retain: false,
        }],
        error: None,
    };
    store.upsert_rule(&rule).await?;

    // 4. MQTT client + event bus.
    let (mqtt_client, mut mqtt_rx) = MqttClient::new(MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        client_id: "internal.core".into(),
        username: None,
        password: None,
    });
    let publish_handle = mqtt_client.publish_handle();
    let bus = EventBus::new(512);

    // Subscribe to bus BEFORE anything starts so we don't miss events.
    let mut bus_rx = bus.subscribe();

    // Forwarder: MQTT → bus.
    {
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            loop {
                match mqtt_rx.recv().await {
                    Ok(ev) => { let _ = bus_clone.publish(ev); }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    tokio::spawn(async move { let _ = mqtt_client.run().await; });

    // 5. Core: state bridge + rule engine.
    let rules = store.list_rules().await?;
    assert_eq!(rules.len(), 1, "rule should be in store");
    let core = Core::new(bus.clone(), store.clone(), Some(publish_handle.clone()));
    core.start(rules).await?;

    // Wait for the MQTT client to connect and subscribe.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 6. Virtual device client.
    let mut opts = MqttOptions::new("virtual-device", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(10));
    let (virt_client, mut virt_eventloop) = AsyncClient::new(opts, 64);
    tokio::spawn(async move {
        loop {
            if virt_eventloop.poll().await.is_err() { break; }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 7. Publish device state.
    virt_client
        .publish(
            "homecore/devices/test_light/state",
            QoS::AtLeastOnce,
            false,
            json!({"on": true}).to_string().as_bytes(),
        )
        .await?;

    // 8. Wait for both DeviceStateChanged and RuleFired on our pre-subscribed receiver.
    let mut saw_state_changed = false;
    let mut saw_rule_fired = false;

    let result = timeout(Duration::from_secs(8), async {
        loop {
            match bus_rx.recv().await {
                Ok(Event::DeviceStateChanged { device_id, .. }) if device_id == "test_light" => {
                    saw_state_changed = true;
                }
                Ok(Event::RuleFired { rule_name, .. }) if rule_name == "test_rule" => {
                    saw_rule_fired = true;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
            if saw_state_changed && saw_rule_fired {
                return true;
            }
        }
        false
    })
    .await;

    // Clean up.
    let _ = std::fs::remove_file(&state_db);
    let _ = std::fs::remove_file(&history_db);

    assert!(saw_state_changed, "DeviceStateChanged event never arrived on bus");
    assert!(saw_rule_fired, "RuleFired event never arrived — rule did not fire");
    assert!(result.is_ok(), "Timed out waiting for events");

    Ok(())
}
