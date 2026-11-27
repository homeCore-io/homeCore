//! Every retained message reaches core when it connects, however many there
//! are.
//!
//! rumqttd hands a new subscription its retained messages in one batch and
//! drops whatever doesn't fit: for QoS 1 the batch is its in-flight window, a
//! constant 100. Core subscribed at QoS 1, so a house with more than 100
//! retained device states, availabilities and plugin manifests came back from
//! a restart missing the rest: devices stale, plugins with no actions and no
//! REST access until they happened to publish again.

use anyhow::Result;
use hc_broker::{Broker, BrokerConfig};
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_types::event::Event;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::collections::HashSet;
use std::time::Duration;

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

const RETAINED: usize = 600;

#[tokio::test]
async fn core_hears_every_retained_message_when_it_connects() -> Result<()> {
    let port = free_port();
    Broker::new(BrokerConfig {
        host: "127.0.0.1".into(),
        port,
        v5_port: None,
        ..Default::default()
    })
    .spawn()?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A house's worth of retained state, published before core connects.
    let mut opts = MqttOptions::new("a-plugin", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(10));
    let (plugin, mut plugin_loop) = AsyncClient::new(opts, 1024);
    tokio::spawn(async move { while plugin_loop.poll().await.is_ok() {} });
    for i in 0..RETAINED {
        plugin
            .publish(
                format!("homecore/devices/d{i}/state"),
                QoS::AtLeastOnce,
                true,
                format!("{{\"n\":{i}}}"),
            )
            .await?;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let (core_client, mut rx) = MqttClient::new(MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        client_id: "internal.core".into(),
        username: None,
        password: None,
    });
    tokio::spawn(async move {
        let _ = core_client.run().await;
    });

    let mut heard = HashSet::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(ev) = rx.recv().await {
            if let Event::MqttMessage { topic, .. } = ev {
                if topic.ends_with("/state") {
                    heard.insert(topic);
                }
            }
            if heard.len() == RETAINED {
                break;
            }
        }
    })
    .await;
    assert_eq!(
        heard.len(),
        RETAINED,
        "retained messages core heard on connecting"
    );
    Ok(())
}
