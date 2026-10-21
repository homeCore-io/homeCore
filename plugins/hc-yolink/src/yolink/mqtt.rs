//! The hub's (or YoLink cloud's) event feed: device reports over MQTT.
//!
//! **Credentials, per YoLink's docs.** Local hub: username = the Local API
//! client id, password = the client **secret** (the docs allow the secret or
//! an access token; the secret never expires, so renewing the HTTP token no
//! longer ends the feed). Cloud (Open API V2): username = the **access token**,
//! no password — read afresh on every (re)connect.
//!
//! **Up is ConnAck**, not "a client was built": the "stream down" notice is
//! cleared, and the bridge told, only when the broker has accepted us.

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::PluginNotices;
use rumqttc::{AsyncClient, ConnectReturnCode, Event, MqttOptions, Packet, QoS};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, trace, warn};
use uuid::Uuid;

use super::types::YolinkReport;
use crate::auth::TokenManager;
use crate::config::Mode;

/// What the feed tells the bridge.
#[derive(Debug, Clone)]
pub enum FeedEvent {
    /// The broker accepted us and the subscription is in place.
    Up,
    /// The feed dropped; it is reconnecting.
    Down(String),
    Report(YolinkReport),
}

pub struct YolinkMqtt {
    host: String,
    port: u16,
    mode: Mode,
    client_id: String,
    client_secret: String,
    tokens: Arc<TokenManager>,
    notices: PluginNotices,
}

/// The MQTT username and password for a mode: local `client_id` + the client
/// secret; cloud the access token and no password.
pub fn credentials(mode: &Mode, client_id: &str, secret: &str, token: &str) -> (String, String) {
    match mode {
        Mode::Local => (client_id.to_string(), secret.to_string()),
        Mode::Cloud => (token.to_string(), String::new()),
    }
}

impl YolinkMqtt {
    pub fn new(
        host: String,
        port: u16,
        mode: Mode,
        client_id: String,
        client_secret: String,
        tokens: Arc<TokenManager>,
        notices: PluginNotices,
    ) -> Self {
        Self {
            host,
            port,
            mode,
            client_id,
            client_secret,
            tokens,
            notices,
        }
    }

    /// Drive the MQTT event loop forever, reconnecting on error.
    ///
    /// `topic_prefix` is the mode-specific namespace:
    /// - Cloud: `yl-home/{home_id}`
    /// - Local: `ylsubnet/{net_id}`
    pub async fn run(self, topic_prefix: String, tx: mpsc::Sender<FeedEvent>) {
        const MIN_BACKOFF: u64 = 10;
        const MAX_BACKOFF: u64 = 60;
        /// A connection lasting this long resets the backoff.
        const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

        let mut backoff_secs = MIN_BACKOFF;
        loop {
            let started = std::time::Instant::now();
            match self.run_once(&topic_prefix, &tx).await {
                Ok(()) => return, // tx closed — shutting down
                Err(e) => {
                    if started.elapsed() >= HEALTHY_UPTIME {
                        backoff_secs = MIN_BACKOFF;
                    }
                    error!(error = %e, backoff_secs, "YoLink MQTT connection lost; reconnecting");
                    self.notices.raise(
                        PluginNotice::error(
                            "stream_disconnected",
                            format!(
                                "The YoLink event stream at {}:{} is down — {e}. Device \
                                 states will not update until it reconnects.",
                                self.host, self.port
                            ),
                        )
                        .with_remedy(
                            "If this persists, check the Client ID and Secret under \
                             Configuration — a rejected credential looks like a dropped \
                             connection here. Cloud mode also needs outbound internet.",
                        ),
                    );
                    if tx.send(FeedEvent::Down(e.to_string())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    async fn run_once(&self, topic_prefix: &str, tx: &mpsc::Sender<FeedEvent>) -> Result<()> {
        let token = match self.mode {
            Mode::Cloud => self.tokens.get_token().await?,
            Mode::Local => String::new(),
        };
        let (user, pass) = credentials(&self.mode, &self.client_id, &self.client_secret, &token);
        let session_id = format!("hc-yolink-{}", Uuid::new_v4());

        let mut opts = MqttOptions::new(&session_id, &self.host, self.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        opts.set_credentials(user, pass);

        let (client, mut eventloop) = AsyncClient::new(opts, 128);
        // Cloud:  yl-home/{home_id}/+/report
        // Local:  ylsubnet/{net_id}/+/report
        let topic = format!("{topic_prefix}/+/report");

        loop {
            match eventloop.poll().await? {
                Event::Incoming(Packet::ConnAck(ack)) => {
                    if ack.code != ConnectReturnCode::Success {
                        anyhow::bail!("the broker refused the connection ({:?})", ack.code);
                    }
                    client.subscribe(&topic, QoS::AtLeastOnce).await?;
                    info!(host = %self.host, port = self.port, "YoLink event feed connected");
                    self.notices.clear("stream_disconnected");
                    if tx.send(FeedEvent::Up).await.is_err() {
                        return Ok(());
                    }
                }
                Event::Incoming(Packet::Publish(p)) => {
                    // {prefix}/{device_id}/report
                    let parts: Vec<&str> = p.topic.split('/').collect();
                    if parts.len() < 4 || parts[3] != "report" {
                        continue;
                    }
                    let device_id = parts[2].to_string();
                    match serde_json::from_slice::<Value>(&p.payload) {
                        Ok(payload) => {
                            trace!(topic = %p.topic, payload = %payload, "YoLink report");
                            let report = YolinkReport {
                                device_id,
                                event: payload["event"].as_str().unwrap_or("Report").to_string(),
                                time: payload["time"].as_i64(),
                                data: payload["data"].clone(),
                            };
                            if tx.send(FeedEvent::Report(report)).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(e) => {
                            warn!(topic = %p.topic, error = %e, "Non-JSON YoLink payload, skipping");
                        }
                    }
                }
                Event::Incoming(Packet::Disconnect) => {
                    anyhow::bail!("YoLink MQTT broker sent Disconnect");
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local: client id + secret (a token refresh leaves the feed alone).
    /// Cloud: the access token as the username, and no password.
    #[test]
    fn credentials_follow_the_docs_per_mode() {
        assert_eq!(
            credentials(&Mode::Local, "cid", "secret", "tok"),
            ("cid".into(), "secret".into())
        );
        assert_eq!(
            credentials(&Mode::Cloud, "ua_x", "sec_x", "tok"),
            ("tok".into(), String::new())
        );
    }
}
