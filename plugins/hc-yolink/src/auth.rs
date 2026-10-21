//! YoLink access tokens: fetched once, renewed when they are about to expire
//! or when the hub says one is no longer good.
//!
//! **One renewal at a time.** Every caller that finds the token stale used to
//! fetch its own; a poll and a command arriving together asked for two tokens,
//! and on the local hub each new token can end the session the previous one
//! opened. Renewal is behind a mutex, and a caller that waited for someone
//! else's renewal takes that token instead of asking again.
//!
//! **Refresh first.** YoLink's docs prefer `grant_type=refresh_token` to a
//! new `client_credentials` grant; the latter is only the fallback when the
//! refresh token is refused (or there is none).
//!
//! **The event feed does not depend on this** in local mode: the hub takes the
//! client secret as the MQTT password, so renewing the HTTP token no longer
//! touches the live feed. In cloud mode the MQTT username is the access token,
//! read afresh on each (re)connect.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: DateTime<Utc>,
}

pub struct TokenManager {
    token_url: String,
    client_id: String,
    client_secret: String,
    http: Client,
    cached: RwLock<Option<CachedToken>>,
    /// Held while a renewal is in flight.
    renewing: Mutex<()>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Lifetime in seconds reported by the server
    expires_in: u64,
}

/// How long before expiry a token is renewed.
const EARLY: i64 = 60;

/// An HTTP client that gives up: a hub that stops answering must not hang a
/// poll, a command or startup forever.
pub fn http_client() -> Client {
    Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client build")
}

impl TokenManager {
    /// Create a new manager.  Call [`TokenManager::init`] before use.
    pub fn new(token_url: String, client_id: String, client_secret: String) -> Arc<Self> {
        Arc::new(Self {
            token_url,
            client_id,
            client_secret,
            http: http_client(),
            cached: RwLock::new(None),
            renewing: Mutex::new(()),
        })
    }

    /// Fetch the first token.  Must succeed before the plugin starts.
    pub async fn init(self: &Arc<Self>) -> Result<()> {
        self.renew(None).await.map(|_| ())
    }

    /// A valid access token, renewed first if it expires within a minute.
    pub async fn get_token(&self) -> Result<String> {
        if let Some(t) = self.fresh().await {
            return Ok(t);
        }
        self.renew(None).await
    }

    /// The hub refused `stale` (token invalid or expired): renew it — unless
    /// someone else already has, in which case theirs is returned.
    pub async fn renew_after_refusal(&self, stale: &str) -> Result<String> {
        self.renew(Some(stale)).await
    }

    async fn fresh(&self) -> Option<String> {
        let guard = self.cached.read().await;
        guard
            .as_ref()
            .filter(|t| t.expires_at > Utc::now() + Duration::seconds(EARLY))
            .map(|t| t.access_token.clone())
    }

    /// Single-flight renewal. With `stale`, renews only if the cached token is
    /// still that one; without, only if the cached token is not fresh.
    async fn renew(&self, stale: Option<&str>) -> Result<String> {
        let _one = self.renewing.lock().await;
        // Someone may have renewed while this caller waited.
        {
            let guard = self.cached.read().await;
            if let Some(t) = guard.as_ref() {
                let fresh = t.expires_at > Utc::now() + Duration::seconds(EARLY);
                let replaced = stale.is_some_and(|s| s != t.access_token);
                if (stale.is_none() && fresh) || (replaced && fresh) {
                    return Ok(t.access_token.clone());
                }
            }
        }
        let refresh = self
            .cached
            .read()
            .await
            .as_ref()
            .and_then(|t| t.refresh_token.clone());
        let got = match refresh {
            Some(r) => match self
                .request(&[("grant_type", "refresh_token"), ("refresh_token", &r)])
                .await
            {
                Ok(t) => Ok(t),
                Err(e) => {
                    warn!(error = %e, "Refreshing the YoLink token failed; asking for a new one");
                    self.request(&[
                        ("grant_type", "client_credentials"),
                        ("client_secret", &self.client_secret),
                    ])
                    .await
                }
            },
            None => {
                self.request(&[
                    ("grant_type", "client_credentials"),
                    ("client_secret", &self.client_secret),
                ])
                .await
            }
        }?;
        let token = got.access_token.clone();
        *self.cached.write().await = Some(got);
        Ok(token)
    }

    async fn request(&self, grant: &[(&str, &str)]) -> Result<CachedToken> {
        debug!(grant = grant[0].1, "Fetching a YoLink access token");
        let mut form: Vec<(&str, &str)> = vec![("client_id", &self.client_id)];
        form.extend_from_slice(grant);
        let resp = self
            .http
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .context("Token request failed")?
            .error_for_status()
            .context("Token endpoint returned error")?
            .json::<TokenResponse>()
            .await
            .context("Token response parse failed")?;
        let expires_at = Utc::now() + Duration::seconds(resp.expires_in as i64);
        info!(expires_at = %expires_at, "YoLink access token obtained");
        Ok(CachedToken {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.filter(|r| !r.is_empty()),
            expires_at,
        })
    }
}
