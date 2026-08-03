use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// TokenManager
// ---------------------------------------------------------------------------

pub struct TokenManager {
    token_url: String,
    client_id: String,
    client_secret: String,
    http: Client,
    cached: RwLock<Option<CachedToken>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds reported by the server
    expires_in: u64,
}

impl TokenManager {
    /// Create a new manager.  Call [`TokenManager::init`] before use.
    pub fn new(token_url: String, client_id: String, client_secret: String) -> Arc<Self> {
        Arc::new(Self {
            token_url,
            client_id,
            client_secret,
            http: Client::new(),
            cached: RwLock::new(None),
        })
    }

    /// Fetch the first token.  Must succeed before the plugin starts.
    pub async fn init(self: &Arc<Self>) -> Result<()> {
        self.fetch_and_store().await?;
        Ok(())
    }

    /// Return a valid access token, refreshing it if it expires within 60 s.
    pub async fn get_token(&self) -> Result<String> {
        {
            let guard = self.cached.read().await;
            if let Some(ref t) = *guard {
                if t.expires_at > Utc::now() + Duration::seconds(60) {
                    return Ok(t.access_token.clone());
                }
            }
        }
        self.fetch_and_store().await
    }

    async fn fetch_and_store(&self) -> Result<String> {
        debug!(url = %self.token_url, "Fetching YoLink access token");

        let resp = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
            ])
            .send()
            .await
            .context("Token request failed")?
            .error_for_status()
            .context("Token endpoint returned error")?
            .json::<TokenResponse>()
            .await
            .context("Token response parse failed")?;

        let expires_at = Utc::now() + Duration::seconds(resp.expires_in as i64);
        let token = resp.access_token.clone();

        *self.cached.write().await = Some(CachedToken {
            access_token: resp.access_token,
            expires_at,
        });

        info!(expires_at = %expires_at, "YoLink access token obtained");
        Ok(token)
    }
}
