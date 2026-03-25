//! Telegram Bot notification channel.
//!
//! Uses the Telegram Bot API `sendMessage` method.
//! Create a bot via @BotFather, get the token, then find your chat_id
//! by sending a message to the bot and calling `getUpdates`.
//!
//! ```toml
//! [[notify.channels]]
//! name      = "telegram"
//! type      = "telegram"
//! bot_token = "123456:ABC-DEF1234..."
//! chat_id   = "-100123456789"   # group/channel (negative) or user ID (positive)
//! markdown  = true              # optional — enable MarkdownV2 formatting
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::channel::NotifyChannel;

/// TOML configuration for the Telegram channel.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
    /// When true, messages are sent with `parse_mode = "MarkdownV2"`.
    #[serde(default)]
    pub markdown: bool,
}

pub struct TelegramChannel {
    cfg: TelegramConfig,
    client: reqwest::Client,
}

impl TelegramChannel {
    pub fn new(cfg: TelegramConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("HomeCore/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build Telegram HTTP client");
        Self { cfg, client }
    }
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'static str>,
}

#[async_trait]
impl NotifyChannel for TelegramChannel {
    async fn send(&self, title: &str, message: &str) -> Result<()> {
        let text = if title.is_empty() {
            message.to_string()
        } else {
            format!("{title}\n{message}")
        };

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.cfg.bot_token
        );

        let body = SendMessage {
            chat_id: &self.cfg.chat_id,
            text: &text,
            parse_mode: if self.cfg.markdown { Some("MarkdownV2") } else { None },
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Telegram HTTP request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram API returned {status}: {body}");
        }
        Ok(())
    }
}
