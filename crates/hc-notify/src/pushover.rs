//! Pushover push notification channel.
//!
//! Uses the Pushover HTTP API: <https://pushover.net/api>
//! Requires a Pushover application token and a user/group key.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::channel::NotifyChannel;

const PUSHOVER_API_URL: &str = "https://api.pushover.net/1/messages.json";

/// TOML configuration for the Pushover channel.
///
/// ```toml
/// [[notify.channels]]
/// name      = "phone"
/// type      = "pushover"
/// api_token = "your-app-api-token"
/// user_key  = "your-user-or-group-key"
/// device    = "my-iphone"   # optional — omit to send to all devices
/// priority  = 0             # optional — -2 (lowest) to 2 (emergency). Default 0.
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PushoverConfig {
    pub api_token: String,
    pub user_key: String,
    /// Target a specific device name.  Omit to send to all of the user's devices.
    #[serde(default)]
    pub device: Option<String>,
    /// Pushover message priority (-2..=2).  Default 0 (normal).
    #[serde(default)]
    pub priority: Option<i8>,
}

/// Pushover notification channel.
pub struct PushoverChannel {
    cfg: PushoverConfig,
    client: reqwest::Client,
}

impl PushoverChannel {
    pub fn new(cfg: PushoverConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("HomeCore/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build Pushover HTTP client");
        Self { cfg, client }
    }
}

/// JSON payload sent to the Pushover API.
#[derive(Serialize)]
struct PushoverPayload<'a> {
    token: &'a str,
    user: &'a str,
    title: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<i8>,
}

#[async_trait]
impl NotifyChannel for PushoverChannel {
    async fn send(&self, title: &str, message: &str) -> Result<()> {
        let payload = PushoverPayload {
            token:    &self.cfg.api_token,
            user:     &self.cfg.user_key,
            title,
            message,
            device:   self.cfg.device.as_deref(),
            priority: self.cfg.priority,
        };

        let resp = self.client
            .post(PUSHOVER_API_URL)
            .json(&payload)
            .send()
            .await
            .context("Pushover HTTP request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Pushover API returned {status}: {body}");
        }
        Ok(())
    }
}
