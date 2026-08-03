use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::PluginNotices;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::types::YolinkReport;
use crate::auth::TokenManager;

pub struct YolinkMqtt {
    host: String,
    port: u16,
    /// YoLink Client ID — used as the MQTT *username* (token is the password).
    yolink_client_id: String,
    tokens: Arc<TokenManager>,
    /// What the operator sees when the YoLink stream is down. Reconnection is
    /// silent apart from a log line, and a plugin with a dead event stream
    /// still reads "active" while every sensor quietly stops updating.
    notices: PluginNotices,
}

impl YolinkMqtt {
    pub fn new(
        host: String,
        port: u16,
        yolink_client_id: String,
        tokens: Arc<TokenManager>,
        notices: PluginNotices,
    ) -> Self {
        Self {
            host,
            port,
            yolink_client_id,
            tokens,
            notices,
        }
    }

    /// Drive the MQTT event loop forever, reconnecting on error.
    /// Parsed device reports are sent on `tx`.
    ///
    /// `topic_prefix` is the mode-specific namespace:
    /// - Cloud: `yl-home/{home_id}`
    /// - Local: `ylsubnet/{net_id}`
    pub async fn run(self, topic_prefix: String, tx: mpsc::Sender<YolinkReport>) {
        const MIN_BACKOFF: u64 = 10;
        const MAX_BACKOFF: u64 = 60;
        /// A connection lasting this long resets the backoff.
        const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

        let mut backoff_secs = MIN_BACKOFF;

        loop {
            let started = std::time::Instant::now();
            match self.run_once(&topic_prefix, &tx).await {
                Ok(()) => {
                    // tx closed — shutting down
                    return;
                }
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
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    async fn run_once(&self, topic_prefix: &str, tx: &mpsc::Sender<YolinkReport>) -> Result<()> {
        let token = self.tokens.get_token().await?;
        // Local hub (and cloud): username = YoLink Client ID, password = access token
        let session_id = format!("hc-yolink-{}", &Uuid::new_v4().to_string()[..8]);

        let mut opts = MqttOptions::new(&session_id, &self.host, self.port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        opts.set_credentials(&self.yolink_client_id, &token);

        let (client, mut eventloop) = AsyncClient::new(opts, 128);
        // A token was obtained and the client is built — the credentials are
        // good and we are past the failure this notice describes.
        self.notices.clear("stream_disconnected");

        // Cloud:  yl-home/{home_id}/+/report
        // Local:  ylsubnet/{net_id}/+/report
        let topic = format!("{topic_prefix}/+/report");
        client.subscribe(&topic, QoS::AtLeastOnce).await?;

        info!(
            host = %self.host,
            port = self.port,
            %topic,
            "YoLink MQTT subscribed"
        );

        loop {
            match eventloop.poll().await? {
                Event::Incoming(Packet::ConnAck(_)) => {
                    info!("YoLink MQTT ConnAck received");
                }
                Event::Incoming(Packet::Publish(p)) => {
                    // Cloud: yl-home/{home_id}/{device_id}/report  → parts[2] = device_id
                    // Local: ylsubnet/{net_id}/{device_id}/report   → parts[2] = device_id
                    let parts: Vec<&str> = p.topic.split('/').collect();
                    if parts.len() < 4 || parts[3] != "report" {
                        continue;
                    }
                    let device_id = parts[2].to_string();

                    match serde_json::from_slice::<Value>(&p.payload) {
                        Ok(payload) => {
                            let event = payload["event"].as_str().unwrap_or("Report").to_string();
                            let data = payload["data"].clone();
                            let report = YolinkReport {
                                device_id,
                                event,
                                data,
                            };
                            if tx.send(report).await.is_err() {
                                // Receiver dropped — plugin is shutting down
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
