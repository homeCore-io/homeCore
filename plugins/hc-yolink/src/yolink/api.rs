use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, trace, warn};
use uuid::Uuid;

use super::types::{is_transient, retry_delay, ApiError, Bddp, Budp, DeviceInfo};
use crate::auth::TokenManager;

pub struct YolinkApi {
    /// Base URL: "https://api.yosmart.com" (cloud) or "http://{ip}:{port}" (local)
    base_url: String,
    tokens: Arc<TokenManager>,
    http: Client,
}

/// What a call came back as, before its code is looked at.
enum Reply {
    Budp(Budp),
    /// HTTP 401: the bearer token was refused before any BUDP was written.
    Unauthorized,
}

impl YolinkApi {
    pub fn new(base_url: String, tokens: Arc<TokenManager>) -> Self {
        Self {
            base_url,
            tokens,
            http: crate::auth::http_client(),
        }
    }

    // -----------------------------------------------------------------------
    // Low-level caller
    // -----------------------------------------------------------------------

    async fn send(
        &self,
        token: &str,
        method: &str,
        target_device: Option<&str>,
        net_token: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Reply> {
        let body = Bddp {
            time: now_ms(),
            method,
            msgid: Some(Uuid::new_v4().to_string()),
            target_device,
            token: net_token,
            params: params.cloned(),
        };
        trace!(method, ?target_device, params = ?params, "YoLink API call");
        let url = format!("{}/open/yolink/v2/api", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("HTTP request failed for method {method}"))?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Ok(Reply::Unauthorized);
        }
        let budp = resp
            .error_for_status()
            .with_context(|| format!("HTTP error for method {method}"))?
            .json::<Budp>()
            .await
            .context("Response JSON parse failed")?;
        trace!(method, code = %budp.code, data = ?budp.data, "YoLink API reply");
        Ok(Reply::Budp(budp))
    }

    /// One call. A refused token (HTTP 401, or a token code in the BUDP) is
    /// renewed once and the call sent again — once, so a hub that refuses
    /// every token does not spin.
    async fn call(
        &self,
        method: &str,
        target_device: Option<&str>,
        net_token: Option<&str>,
        params: Option<Value>,
    ) -> Result<Value> {
        let token = self.tokens.get_token().await?;
        let first = self
            .send(&token, method, target_device, net_token, params.as_ref())
            .await?;
        let refused = match &first {
            Reply::Unauthorized => true,
            Reply::Budp(b) => ApiError {
                code: b.code.clone(),
                desc: String::new(),
            }
            .is_token_refused(),
        };
        let reply = if refused {
            warn!(
                method,
                "The YoLink hub refused the access token; renewing it once"
            );
            let token = self.tokens.renew_after_refusal(&token).await?;
            self.send(&token, method, target_device, net_token, params.as_ref())
                .await?
        } else {
            first
        };
        match reply {
            Reply::Budp(b) => b.into_data(),
            Reply::Unauthorized => Err(ApiError {
                code: "401".into(),
                desc: "the hub refused a freshly renewed token".into(),
            }
            .into()),
        }
    }

    /// A call retried on the errors that clear by themselves: a busy radio
    /// (1 s, 2 s) and a throttle (10 s, 20 s).
    async fn call_retrying(
        &self,
        method: &str,
        info: &DeviceInfo,
        params: Option<Value>,
    ) -> Result<Value> {
        const ATTEMPTS: u32 = 3;
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if let Some(e) = &last {
                tokio::time::sleep(retry_delay(e, attempt)).await;
            }
            match self
                .call(
                    method,
                    Some(&info.device_id),
                    Some(&info.token),
                    params.clone(),
                )
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) if is_transient(&e) => {
                    debug!(device_id = %info.device_id, method, attempt = attempt + 1, error = %e,
                        "YoLink call hit a transient error, retrying");
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.expect("loop runs at least once and only stores on error"))
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

    /// The device's state now (`{Type}.getState`), retried on a busy radio or
    /// a throttle. Exhausted retries on `000201` are what an unreachable
    /// device looks like.
    pub async fn get_device_state(&self, info: &DeviceInfo) -> Result<Value> {
        self.call_retrying(&format!("{}.getState", info.device_type), info, None)
            .await
    }

    /// `{Type}.{suffix}` with `params` — `setState`, `setAttributes`.
    pub async fn device_call(
        &self,
        info: &DeviceInfo,
        method_suffix: &str,
        params: Value,
    ) -> Result<Value> {
        self.call_retrying(
            &format!("{}.{}", info.device_type, method_suffix),
            info,
            Some(params),
        )
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
