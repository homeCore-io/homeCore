//! `hc-mqtt-client` — async MQTT client and internal event bridge.
//!
//! Connects to the broker via `rumqttc`, subscribes to `homecore/#`, converts
//! incoming publishes into typed [`Event`] values, and broadcasts them on a
//! Tokio `broadcast` channel.  A lightweight [`PublishHandle`] is exposed so
//! other crates can send commands without depending on `rumqttc` directly.

use anyhow::{Context, Result};
use hc_types::event::Event;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, error, info, warn};

/// Configuration for the internal MQTT client.
#[derive(Debug, Clone)]
pub struct MqttClientConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_packet(stream: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
        let header = stream.read_u8().await.unwrap();
        let mut length = 0usize;
        let mut multiplier = 1usize;
        loop {
            let byte = stream.read_u8().await.unwrap();
            length += (byte as usize & 127) * multiplier;
            if byte & 128 == 0 {
                break;
            }
            multiplier *= 128;
            assert!(multiplier <= 128 * 128 * 128);
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        (header, body)
    }

    async fn check_readiness(accepted: bool, delayed_broker: bool, extra_ack: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let listener = if delayed_broker {
            drop(listener);
            None
        } else {
            Some(listener)
        };
        let (subscribed_tx, subscribed_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let listener = match listener {
                Some(listener) => listener,
                None => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    tokio::net::TcpListener::bind(("127.0.0.1", port))
                        .await
                        .unwrap()
                }
            };
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(read_packet(&mut stream).await.0 >> 4, 1);
            stream.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            let (header, body) = read_packet(&mut stream).await;
            assert_eq!(header >> 4, 8);
            assert!(body.windows(10).any(|part| part == b"homecore/#"));
            if extra_ack {
                let (_, extra) = read_packet(&mut stream).await;
                // Another subscription's rejection must not resolve readiness.
                stream
                    .write_all(&[0x90, 3, extra[0], extra[1], 0x80])
                    .await
                    .unwrap();
            }
            subscribed_tx.send(()).unwrap();
            ack_rx.await.unwrap();
            stream
                .write_all(&[0x90, 3, body[0], body[1], if accepted { 1 } else { 0x80 }])
                .await
                .unwrap();
            // Keep the connection alive while the client processes SUBACK.
            let _ = read_packet(&mut stream).await;
        });
        let (mut client, _) = MqttClient::new(MqttClientConfig {
            broker_port: port,
            ..Default::default()
        });
        let (ready_tx, mut ready_rx) = oneshot::channel();
        client.set_ready_notify(ready_tx);
        if extra_ack {
            client.add_subscription("other/#");
        }
        let task = tokio::spawn(client.run());
        tokio::time::timeout(std::time::Duration::from_secs(1), subscribed_rx)
            .await
            .unwrap()
            .unwrap();
        if extra_ack {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut ready_rx)
                    .await
                    .is_err()
            );
        }
        assert!(
            matches!(
                ready_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "CONNACK/SUBSCRIBE enqueue must not declare readiness"
        );
        ack_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), ready_rx)
            .await
            .unwrap();
        assert_eq!(result.is_ok(), accepted);
        task.abort();
        server.abort();
    }

    #[tokio::test]
    async fn waits_for_broker_to_accept_subscription() {
        check_readiness(true, false, false).await;
    }
    #[tokio::test]
    async fn rejected_subscription_does_not_start_plugins() {
        check_readiness(false, false, false).await;
    }
    #[tokio::test]
    async fn retries_promptly_when_broker_binds_after_client_starts() {
        check_readiness(true, true, false).await;
    }
    #[tokio::test]
    async fn another_subscriptions_acknowledgement_does_not_resolve_readiness() {
        check_readiness(true, false, true).await;
    }
}

impl Default for MqttClientConfig {
    fn default() -> Self {
        Self {
            broker_host: "127.0.0.1".into(),
            broker_port: 1883,
            client_id: "internal.core".into(),
            username: None,
            password: None,
        }
    }
}

/// Cheap-to-clone handle for publishing to the broker.
#[derive(Clone)]
pub struct PublishHandle {
    client: AsyncClient,
}

impl PublishHandle {
    /// Publish raw bytes to `topic` at QoS 0, non-retained.
    pub async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.client
            .publish(topic, QoS::AtMostOnce, false, payload)
            .await
            .context("MQTT publish failed")
    }

    /// Publish a retained message at QoS 1.
    pub async fn publish_retained(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("MQTT retained publish failed")
    }

    /// Serialise `value` to JSON and publish it.
    pub async fn publish_json<T: serde::Serialize>(
        &self,
        topic: &str,
        value: &T,
        retain: bool,
    ) -> Result<()> {
        let payload = serde_json::to_vec(value).context("JSON serialisation failed")?;
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload)
            .await
            .context("MQTT publish_json failed")
    }
}

/// Owns the MQTT event loop.  Drive it by calling [`MqttClient::run`].
pub struct MqttClient {
    config: MqttClientConfig,
    tx: broadcast::Sender<Event>,
    client: AsyncClient,
    eventloop: EventLoop,
    /// Additional topic filters to subscribe to on connect (beyond `homecore/#`).
    extra_subscriptions: Vec<String>,
    /// Optional one-shot sender that fires once homecore/# is acknowledged on
    /// the first connect.  Lets the caller know it is safe to launch plugins.
    ready_tx: Option<oneshot::Sender<()>>,
}

impl MqttClient {
    /// Build the client.  Returns `(MqttClient, broadcast::Receiver<Event>)`.
    pub fn new(config: MqttClientConfig) -> (Self, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(1024);

        let mut opts = MqttOptions::new(&config.client_id, &config.broker_host, config.broker_port);
        opts.set_keep_alive(std::time::Duration::from_secs(30));
        opts.set_clean_session(true);
        // Accept large payloads (up to the broker's 256 KiB max) so big retained
        // capability manifests (rich config schema + actions) aren't dropped on
        // the way in — the rumqttc default incoming limit is only ~10 KiB.
        opts.set_max_packet_size(256 * 1024, 256 * 1024);

        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            opts.set_credentials(u, p);
        }

        let (client, eventloop) = AsyncClient::new(opts, 256);
        (
            Self {
                config,
                tx,
                client,
                eventloop,
                extra_subscriptions: Vec::new(),
                ready_tx: None,
            },
            rx,
        )
    }

    /// Register a one-shot sender that will be signalled once the first
    /// `homecore/#` subscription is confirmed.  Use this to delay plugin
    /// launch until the internal client is actually listening.
    pub fn set_ready_notify(&mut self, tx: oneshot::Sender<()>) {
        self.ready_tx = Some(tx);
    }

    /// Add extra topic filters to subscribe to on (re)connect.
    /// Call before [`run`] to ensure the subscriptions are in place from the start.
    pub fn add_subscription(&mut self, filter: impl Into<String>) {
        self.extra_subscriptions.push(filter.into());
    }

    /// Returns a publish handle that can be cloned and shared freely.
    pub fn publish_handle(&self) -> PublishHandle {
        PublishHandle {
            client: self.client.clone(),
        }
    }

    /// Connect, subscribe to `homecore/#`, and drive the event loop.
    /// Spawn this in a dedicated `tokio::task`.
    pub async fn run(mut self) -> Result<()> {
        info!(
            broker = %self.config.broker_host,
            port   = self.config.broker_port,
            id     = %self.config.client_id,
            "MQTT client connecting"
        );

        let mut awaiting_subscription = false;
        let mut subscription_id = None;
        // The embedded broker binds in parallel with startup. Retry that initial
        // race promptly, backing off to the normal reconnect interval if needed.
        let mut retry_delay = std::time::Duration::from_millis(10);
        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    retry_delay = std::time::Duration::from_secs(2);
                    info!("MQTT connected; subscribing to homecore/#");
                    self.client
                        .subscribe("homecore/#", QoS::AtLeastOnce)
                        .await
                        .context("subscribe failed")?;
                    awaiting_subscription = true;
                    subscription_id = None;
                    for filter in &self.extra_subscriptions {
                        info!(%filter, "Subscribing to ecosystem topic filter");
                        self.client
                            .subscribe(filter, QoS::AtLeastOnce)
                            .await
                            .with_context(|| format!("subscribe to {filter} failed"))?;
                    }
                }

                Ok(rumqttc::Event::Outgoing(rumqttc::Outgoing::Subscribe(id)))
                    if awaiting_subscription && subscription_id.is_none() =>
                {
                    // homecore/# is first in the outgoing subscription queue.
                    subscription_id = Some(id);
                }
                Ok(rumqttc::Event::Incoming(Packet::SubAck(ack)))
                    if awaiting_subscription && subscription_id == Some(ack.pkid) =>
                {
                    // Match the actual packet id, even if other SUBACKs arrive first.
                    awaiting_subscription = false;
                    if ack.return_codes.len() != 1
                        || ack
                            .return_codes
                            .iter()
                            .any(|code| matches!(code, rumqttc::SubscribeReasonCode::Failure))
                    {
                        error!(
                            "Broker rejected homecore/# subscription; plugins cannot safely start"
                        );
                        self.ready_tx.take();
                    } else if let Some(tx) = self.ready_tx.take() {
                        let _ = tx.send(());
                    }
                }

                Ok(rumqttc::Event::Incoming(Packet::Publish(p))) => {
                    debug!(topic = %p.topic, bytes = p.payload.len(), "MQTT rx");
                    let ev = Event::MqttMessage {
                        timestamp: chrono::Utc::now(),
                        topic: p.topic.clone(),
                        payload: p.payload.to_vec(),
                        retain: p.retain,
                    };
                    let _ = self.tx.send(ev);
                }

                Ok(rumqttc::Event::Incoming(Packet::Disconnect)) => {
                    warn!("Broker sent DISCONNECT; will reconnect");
                }

                Ok(_) => {}

                Err(e) => {
                    warn!(error = %e, retry_ms = retry_delay.as_millis(), "MQTT poll error; reconnecting");
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(2));
                }
            }
        }
    }
}
