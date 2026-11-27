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
    /// Optional one-shot sender that fires once subscriptions are confirmed on
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

        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    info!("MQTT connected; subscribing");
                    for filter in subscription_filters(&self.extra_subscriptions) {
                        info!(%filter, "Subscribing to topic filter");
                        self.client
                            .subscribe(&filter, QoS::AtLeastOnce)
                            .await
                            .with_context(|| format!("subscribe to {filter} failed"))?;
                    }
                    // Signal that the client is subscribed and ready.
                    // Only fires on the first connect; ignored on reconnects.
                    if let Some(tx) = self.ready_tx.take() {
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
                    error!(error = %e, "MQTT poll error; retrying in 2 s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}

/// Push connections off the broker by client id, now.
///
/// The broker checks a login only when a connection opens, so a client whose
/// login was just removed (a plugin runtime removed or reset in homeCore, and
/// the plugins placed on it) would otherwise stay connected until it next
/// reconnected. MQTT replaces a connection when another arrives with the same
/// client id, so core connects briefly as each one, with its own login, and
/// leaves at once. The client's reconnect then meets the login check and is
/// refused.
///
/// Best effort: an id that is not connected costs one short connection, and a
/// broker that does not answer is logged rather than failing the caller.
pub async fn take_over(cfg: &MqttClientConfig, client_ids: &[String]) {
    for id in client_ids {
        match take_over_one(cfg, id).await {
            Ok(()) => info!(client_id = %id, "disconnected from the broker"),
            Err(e) => {
                warn!(client_id = %id, error = %format!("{e:#}"), "could not disconnect a client from the broker")
            }
        }
    }
}

async fn take_over_one(cfg: &MqttClientConfig, client_id: &str) -> Result<()> {
    let mut opts = MqttOptions::new(client_id, cfg.broker_host.clone(), cfg.broker_port);
    opts.set_keep_alive(std::time::Duration::from_secs(5));
    opts.set_clean_session(true);
    if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
        opts.set_credentials(u.clone(), p.clone());
    }
    let (client, mut events) = AsyncClient::new(opts, 4);
    let connected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
    })
    .await
    .context("the broker did not answer")?;
    connected?;
    let _ = client.disconnect().await;
    // Let the DISCONNECT go out before the connection is dropped.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while events.poll().await.is_ok() {}
    })
    .await;
    Ok(())
}

/// The filters to subscribe to: `homecore/#` and the extra ones, leaving out
/// any another one already covers. A broker delivers a message once for each
/// subscribed filter it matches, so overlapping filters (`homecore/#` and
/// `#`) would hand core every message twice: a toggle command relayed twice
/// undoes itself.
fn subscription_filters(extra: &[String]) -> Vec<String> {
    let mut all: Vec<String> = std::iter::once("homecore/#".to_string())
        .chain(extra.iter().cloned())
        .collect();
    all.dedup();
    let mut kept: Vec<String> = Vec::new();
    for (i, f) in all.iter().enumerate() {
        let covered = all
            .iter()
            .enumerate()
            .any(|(j, g)| j != i && filter_covers(g, f) && (!filter_covers(f, g) || j < i));
        if !covered && !kept.contains(f) {
            kept.push(f.clone());
        }
    }
    kept
}

/// Whether every topic `inner` matches is matched by `outer` too.
fn filter_covers(outer: &str, inner: &str) -> bool {
    let o: Vec<&str> = outer.split('/').collect();
    let n: Vec<&str> = inner.split('/').collect();
    for (k, seg) in o.iter().enumerate() {
        if *seg == "#" {
            // `#` matches the level above it too, and anything below. A `$`
            // topic is matched only by a filter that names `$` itself.
            return !(k == 0 && n.first().is_some_and(|s| s.starts_with('$')));
        }
        let Some(other) = n.get(k) else {
            return false;
        };
        match (*seg, *other) {
            ("+", "#") => return false,
            ("+", _) => {}
            (a, b) if a == b => {}
            _ => return false,
        }
    }
    o.len() == n.len()
}

#[cfg(test)]
mod subscription_tests {
    use super::*;

    #[test]
    fn everything_takes_the_place_of_homecore() {
        assert_eq!(subscription_filters(&["#".into()]), vec!["#".to_string()]);
    }

    #[test]
    fn filters_that_do_not_overlap_are_all_kept() {
        assert_eq!(
            subscription_filters(&["stat/#".into(), "zigbee2mqtt/+".into()]),
            vec!["homecore/#", "stat/#", "zigbee2mqtt/+"]
        );
    }

    #[test]
    fn a_filter_inside_another_goes() {
        assert_eq!(
            subscription_filters(&[
                "homecore/devices/+/state".into(),
                "stat/#".into(),
                "stat/+/POWER".into()
            ]),
            vec!["homecore/#", "stat/#"]
        );
        assert_eq!(
            subscription_filters(&["homecore/#".into()]),
            vec!["homecore/#"]
        );
    }

    #[test]
    fn covering() {
        assert!(filter_covers("a/#", "a"));
        assert!(filter_covers("a/+/c", "a/b/c"));
        assert!(!filter_covers("a/+/c", "a/#"));
        assert!(!filter_covers("a/b", "a/b/c"));
        assert!(!filter_covers("#", "$SYS/#"));
    }
}
