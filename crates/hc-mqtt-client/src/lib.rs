//! `hc-mqtt-client` — async MQTT client and internal event bridge.
//!
//! Connects to the broker via `rumqttc`, subscribes to `homecore/#`, converts
//! incoming publishes into typed [`Event`] values, and broadcasts them on a
//! Tokio `broadcast` channel.  A lightweight [`PublishHandle`] is exposed so
//! other crates can send commands without depending on `rumqttc` directly.

use anyhow::{Context, Result};
use hc_types::event::Event;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::broadcast;
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
}

impl MqttClient {
    /// Build the client.  Returns `(MqttClient, broadcast::Receiver<Event>)`.
    pub fn new(config: MqttClientConfig) -> (Self, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(1024);

        let mut opts = MqttOptions::new(
            &config.client_id,
            &config.broker_host,
            config.broker_port,
        );
        opts.set_keep_alive(std::time::Duration::from_secs(30));
        opts.set_clean_session(true);

        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            opts.set_credentials(u, p);
        }

        let (client, eventloop) = AsyncClient::new(opts, 256);
        (Self { config, tx, client, eventloop, extra_subscriptions: Vec::new() }, rx)
    }

    /// Add extra topic filters to subscribe to on (re)connect.
    /// Call before [`run`] to ensure the subscriptions are in place from the start.
    pub fn add_subscription(&mut self, filter: impl Into<String>) {
        self.extra_subscriptions.push(filter.into());
    }

    /// Returns a publish handle that can be cloned and shared freely.
    pub fn publish_handle(&self) -> PublishHandle {
        PublishHandle { client: self.client.clone() }
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

        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    info!("MQTT connected; subscribing to homecore/#");
                    self.client
                        .subscribe("homecore/#", QoS::AtLeastOnce)
                        .await
                        .context("subscribe failed")?;
                    for filter in &self.extra_subscriptions {
                        info!(%filter, "Subscribing to ecosystem topic filter");
                        self.client
                            .subscribe(filter, QoS::AtLeastOnce)
                            .await
                            .with_context(|| format!("subscribe to {filter} failed"))?;
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
                    error!(error = %e, "MQTT poll error; retrying in 2 s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}
