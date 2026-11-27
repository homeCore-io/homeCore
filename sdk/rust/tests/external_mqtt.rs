use plugin_sdk_rs::{types::Capabilities, DevicePublisher};
use rumqttc::{AsyncClient, QoS, Request};
use serde_json::{json, Value};

#[tokio::test]
async fn an_existing_mqtt_session_can_publish_the_legacy_device_contract() {
    let (sender, requests) = flume::unbounded();
    let mqtt = AsyncClient::from_senders(sender);
    let publisher =
        DevicePublisher::from_mqtt_client(mqtt, "plugin.example").with_retained_registration();

    publisher
        .register_device_full("tv", "TV", Some("media_player"), None, None)
        .await
        .unwrap();
    publisher
        .publish_capabilities(&Capabilities {
            spec: "1".into(),
            plugin_id: "plugin.example".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    publisher
        .register_device_schema_json("tv", &json!({"attributes": {}}))
        .await
        .unwrap();
    publisher
        .publish_state("tv", &json!({"muted": false}))
        .await
        .unwrap();
    publisher.publish_availability("tv", true).await.unwrap();
    publisher
        .publish_heartbeat("0.2.0", 42, &[], QoS::AtLeastOnce)
        .await
        .unwrap();

    let expected = [
        (
            "homecore/plugins/plugin.example/register",
            true,
            json!({
                "device_id": "tv", "plugin_id": "plugin.example", "name": "TV", "device_type": "media_player"
            }),
        ),
        (
            "homecore/plugins/plugin.example/capabilities",
            true,
            json!({
                "spec": "1", "plugin_id": "plugin.example", "actions": []
            }),
        ),
        (
            "homecore/devices/tv/schema",
            true,
            json!({"attributes": {}}),
        ),
        ("homecore/devices/tv/state", true, json!({"muted": false})),
        ("homecore/devices/tv/availability", true, json!("online")),
        (
            "homecore/plugins/plugin.example/heartbeat",
            false,
            json!({
                "version": "0.2.0", "protocol_version": "0.1.5", "uptime_secs": 42, "notices": []
            }),
        ),
    ];
    for (topic, retained, payload) in expected {
        let Request::Publish(message) = requests.try_recv().unwrap() else {
            panic!("expected publication for {topic}");
        };
        assert_eq!(message.topic, topic);
        assert_eq!(message.retain, retained);
        assert_eq!(message.qos, QoS::AtLeastOnce);
        let actual = if topic.ends_with("/availability") {
            json!(std::str::from_utf8(&message.payload).unwrap())
        } else {
            serde_json::from_slice::<Value>(&message.payload).unwrap()
        };
        assert_eq!(actual, payload);
    }
    assert!(requests.is_empty());
}
