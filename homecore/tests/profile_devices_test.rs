//! A device that speaks its own MQTT, through an ecosystem profile, end to end.
//!
//! A real broker, core wired as `main.rs` wires it when profiles are loaded
//! (the shipped Tasmota profile, its transforms and the example device type
//! catalog, and the subscription to every topic), and a stand-in Tasmota on
//! the broker. The stand-in reports its relay and its LWT; homeCore should
//! hold a typed device with a schema, available and on. A homeCore command
//! should reach the stand-in on its own topic, in its own words.

use anyhow::Result;
use hc_broker::{Broker, BrokerConfig};
use hc_core::{Core, EventBus};
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_state::StateStore;
use hc_topic_map::loader::{load_profiles_from_dir, load_transforms_from_dir};
use hc_topic_map::{DeviceTypeRegistry, EcosystemRouter};
use rumqttc::{AsyncClient, Event as MqttEvent, Incoming, MqttOptions, QoS};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Wait until `check` holds, or fail after a few seconds saying what didn't.
async fn eventually<F, Fut>(what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    for _ in 0..50 {
        if check().await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("{what} never happened")
}

#[tokio::test]
async fn a_tasmota_through_its_profile_is_a_device_that_obeys() -> Result<()> {
    let port = free_port();
    Broker::new(BrokerConfig {
        host: "127.0.0.1".into(),
        port,
        v5_port: None,
        ..Default::default()
    })
    .spawn()?;

    let tmp = tempfile::tempdir()?;
    let store = StateStore::open(
        &tmp.path().join("s.redb").to_string_lossy(),
        &tmp.path().join("h.db").to_string_lossy(),
    )
    .await?;

    // Core, as main.rs builds it with a profiles directory.
    let profiles_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../config/profiles/examples");
    let profiles: Vec<_> = load_profiles_from_dir(profiles_dir)?
        .into_iter()
        .filter(|p| p.name == "tasmota")
        .collect();
    assert_eq!(profiles.len(), 1, "the Tasmota example profile loads");
    let router = EcosystemRouter::with_scripts(profiles, &load_transforms_from_dir(profiles_dir));
    let types = DeviceTypeRegistry::from_file(&format!("{profiles_dir}/device-types.toml"))?;

    let (mut mqtt_client, mut mqtt_rx) = MqttClient::new(MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        client_id: "internal.core".into(),
        username: None,
        password: None,
    });
    mqtt_client.add_subscription("#");
    let publish = mqtt_client.publish_handle();
    let bus = EventBus::new(512);
    {
        let bus = bus.clone();
        tokio::spawn(async move {
            while let Ok(ev) = mqtt_rx.recv().await {
                let _ = bus.publish(ev);
            }
        });
    }
    tokio::spawn(async move {
        let _ = mqtt_client.run().await;
    });
    let core = Core::new(
        bus.clone(),
        bus.clone(),
        store.clone(),
        Some(publish.clone()),
    )
    .with_router(router)
    .with_device_types(std::sync::Arc::new(types));
    core.start(Vec::new()).await?;
    tokio::time::sleep(Duration::from_millis(600)).await;

    // The stand-in Tasmota "plug": it hears its command topic.
    let mut opts = MqttOptions::new("tasmota-plug", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(10));
    let (plug, mut plug_loop) = AsyncClient::new(opts, 64);
    let (heard_tx, mut heard) = mpsc::unbounded_channel::<(String, String)>();
    tokio::spawn(async move {
        while let Ok(ev) = plug_loop.poll().await {
            if let MqttEvent::Incoming(Incoming::Publish(p)) = ev {
                let _ = heard_tx.send((p.topic, String::from_utf8_lossy(&p.payload).into()));
            }
        }
    });
    plug.subscribe("cmnd/plug/#", QoS::AtLeastOnce).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // It reports itself, as a Tasmota does.
    plug.publish("tele/plug/LWT", QoS::AtLeastOnce, true, "Online")
        .await?;
    plug.publish("stat/plug/POWER", QoS::AtLeastOnce, false, "ON")
        .await?;

    eventually("the plug becoming a typed device that is on", || {
        let store = store.clone();
        async move {
            Ok(store.get_device("tasmota_plug").await?.is_some_and(|d| {
                d.device_type.as_deref() == Some("switch")
                    && d.available
                    && d.attributes.get("on") == Some(&serde_json::json!(true))
            }))
        }
    })
    .await?;
    let plug_device = store.get_device("tasmota_plug").await?.unwrap();
    assert_eq!(plug_device.plugin_id, "tasmota");
    let schema = store
        .get_device_schema("tasmota_plug")
        .await?
        .expect("the switch schema from the catalog");
    assert!(
        schema.attributes.contains_key("on"),
        "the schema offers `on`"
    );

    // A command from homeCore reaches the plug, in Tasmota's words.
    publish
        .publish(
            "homecore/devices/tasmota_plug/cmd",
            br#"{"on":false}"#.to_vec(),
        )
        .await?;
    let (topic, payload) = timeout(Duration::from_secs(5), async {
        loop {
            let (t, p) = heard.recv().await.expect("the plug's loop runs");
            if t.starts_with("cmnd/plug/") {
                return (t, p);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("the command never reached the plug"))?;
    assert_eq!(topic, "cmnd/plug/Backlog");
    assert_eq!(payload, "POWER OFF");

    // It goes offline the way a Tasmota says so.
    plug.publish("tele/plug/LWT", QoS::AtLeastOnce, true, "Offline")
        .await?;
    eventually("the plug going offline", || {
        let store = store.clone();
        async move {
            Ok(store
                .get_device("tasmota_plug")
                .await?
                .is_some_and(|d| !d.available))
        }
    })
    .await?;
    Ok(())
}
