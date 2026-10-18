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
/// retry     = 60            # emergency only: seconds between repeats (≥ 30). Default 60.
/// expire    = 3600          # emergency only: stop repeating after (≤ 10800). Default 3600.
/// ```
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PushoverConfig {
    pub api_token: String,
    pub user_key: String,
    /// Target a specific device name.  Omit to send to all of the user's devices.
    #[serde(default)]
    pub device: Option<String>,
    /// Pushover message priority (-2..=2).  Default 0 (normal).
    #[serde(default)]
    pub priority: Option<i8>,
    /// Emergency (priority 2) only: how often Pushover repeats it until it's
    /// acknowledged, in seconds. Pushover requires it and refuses under 30.
    /// Default 60.
    #[serde(default)]
    pub retry: Option<u32>,
    /// Emergency only: when Pushover stops repeating, in seconds. Pushover
    /// requires it and caps it at 10800 (3 hours). Default 3600.
    #[serde(default)]
    pub expire: Option<u32>,
}

/// Pushover's limits for an emergency message.
const RETRY_MIN: u32 = 30;
const EXPIRE_MAX: u32 = 10_800;

impl PushoverConfig {
    /// `retry` and `expire` for an emergency message — which Pushover
    /// refuses without them — kept inside Pushover's limits. None otherwise.
    fn emergency(&self) -> Option<(u32, u32)> {
        (self.priority == Some(2)).then(|| {
            (
                self.retry.unwrap_or(60).max(RETRY_MIN),
                self.expire.unwrap_or(3600).clamp(RETRY_MIN, EXPIRE_MAX),
            )
        })
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expire: Option<u32>,
}

#[async_trait]
impl NotifyChannel for PushoverChannel {
    async fn send(&self, title: &str, message: &str) -> Result<()> {
        let payload = PushoverPayload {
            token: &self.cfg.api_token,
            user: &self.cfg.user_key,
            title,
            message,
            device: self.cfg.device.as_deref(),
            priority: self.cfg.priority,
            retry: self.cfg.emergency().map(|e| e.0),
            expire: self.cfg.emergency().map(|e| e.1),
        };

        let resp = self
            .client
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(priority: Option<i8>, retry: Option<u32>, expire: Option<u32>) -> PushoverConfig {
        PushoverConfig {
            api_token: "t".into(),
            user_key: "u".into(),
            device: None,
            priority,
            retry,
            expire,
        }
    }

    #[test]
    fn an_emergency_always_says_how_to_repeat_within_pushovers_limits() {
        assert_eq!(cfg(Some(2), None, None).emergency(), Some((60, 3600)));
        assert_eq!(
            cfg(Some(2), Some(5), Some(99_999)).emergency(),
            Some((30, 10_800))
        );
        assert_eq!(cfg(Some(1), Some(60), None).emergency(), None);
        assert_eq!(cfg(None, None, None).emergency(), None);
    }
}
