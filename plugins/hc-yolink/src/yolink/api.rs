use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;
use uuid::Uuid;

use super::types::{Bddp, Budp, DeviceInfo};
use crate::auth::TokenManager;

pub struct YolinkApi {
    /// Base URL: "https://api.yosmart.com" (cloud) or "http://{ip}:{port}" (local)
    base_url: String,
    tokens: Arc<TokenManager>,
    http: Client,
}

impl YolinkApi {
    pub fn new(base_url: String, tokens: Arc<TokenManager>) -> Self {
        Self {
            base_url,
            tokens,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client build"),
        }
    }

    // -----------------------------------------------------------------------
    // Low-level caller
    // -----------------------------------------------------------------------

    async fn call(
        &self,
        method: &str,
        target_device: Option<&str>,
        net_token: Option<&str>,
        params: Option<Value>,
    ) -> Result<Value> {
        let user_token = self.tokens.get_token().await?;

        let body = Bddp {
            time: now_ms(),
            method,
            msgid: Some(Uuid::new_v4().to_string()),
            target_device,
            token: net_token,
            params,
        };

        debug!(method, ?target_device, "YoLink API call");

        let url = format!("{}/open/yolink/v2/api", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&user_token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("HTTP request failed for method {method}"))?
            .error_for_status()
            .with_context(|| format!("HTTP error for method {method}"))?
            .json::<Budp>()
            .await
            .context("Response JSON parse failed")?;

        resp.into_data()
    }

    // -----------------------------------------------------------------------
    // Home-level methods
    // -----------------------------------------------------------------------

    /// Get the YoLink Home ID (required to subscribe to the correct MQTT topics).
    pub async fn get_home_id(&self) -> Result<String> {
        let data = self.call("Home.getGeneralInfo", None, None, None).await?;
        data["id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Home.getGeneralInfo response missing 'id' field"))
    }

    /// List all devices associated with this account / home.
    pub async fn get_device_list(&self) -> Result<Vec<DeviceInfo>> {
        let data = self.call("Home.getDeviceList", None, None, None).await?;
        serde_json::from_value::<Vec<DeviceInfo>>(data["devices"].clone())
            .context("Failed to parse device list")
    }

    // -----------------------------------------------------------------------
    // Device-level methods
    // -----------------------------------------------------------------------

    /// Fetch the current full state for a device.
    /// The method is `{DeviceType}.getState`, e.g. "Outlet.getState".
    pub async fn get_device_state(&self, info: &DeviceInfo) -> Result<Value> {
        let method = format!("{}.getState", info.device_type);
        self.call(&method, Some(&info.device_id), Some(&info.token), None)
            .await
    }

    /// Send a state-change command to a device.
    /// The method is `{DeviceType}.setState`, e.g. "Outlet.setState".
    pub async fn set_device_state(&self, info: &DeviceInfo, params: Value) -> Result<()> {
        let method = format!("{}.setState", info.device_type);
        self.call(
            &method,
            Some(&info.device_id),
            Some(&info.token),
            Some(params),
        )
        .await?;
        Ok(())
    }

    /// Send an arbitrary method to a device (for future device types that don't use setState).
    #[allow(dead_code)]
    pub async fn device_call(
        &self,
        info: &DeviceInfo,
        method_suffix: &str,
        params: Option<Value>,
    ) -> Result<Value> {
        let method = format!("{}.{}", info.device_type, method_suffix);
        self.call(&method, Some(&info.device_id), Some(&info.token), params)
            .await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
