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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// A photo's caption: Telegram takes up to 1024 characters.
pub fn caption(title: &str, message: &str) -> String {
    let text = if title.is_empty() {
        message.to_string()
    } else {
        format!("{title}\n{message}")
    };
    if text.chars().count() <= 1024 {
        return text;
    }
    let mut out: String = text.chars().take(1023).collect();
    out.push('…');
    out
}

#[async_trait]
impl NotifyChannel for TelegramChannel {
    async fn send_picture(
        &self,
        title: &str,
        message: &str,
        picture: &crate::Picture,
    ) -> Result<()> {
        let url = format!(
            "https://api.telegram.org/bot{}/sendPhoto",
            self.cfg.bot_token
        );
        let photo = reqwest::multipart::Part::bytes(picture.bytes.clone())
            .file_name(picture.name.clone())
            .mime_str(&picture.mime)
            .context("Telegram: the picture's type")?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", self.cfg.chat_id.clone())
            .text("caption", caption(title, message))
            .part("photo", photo);
        if self.cfg.markdown {
            form = form.text("parse_mode", "MarkdownV2");
        }
        let resp = self
            .client
            .post(&url)
            .multipart(form)
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
            parse_mode: if self.cfg.markdown {
                Some("MarkdownV2")
            } else {
                None
            },
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

#[cfg(test)]
mod caption_tests {
    use super::caption;

    #[test]
    fn a_caption_fits_telegrams_limit() {
        assert_eq!(caption("Front Deck", "a person"), "Front Deck\na person");
        assert_eq!(caption("", "a person"), "a person");
        let long = caption("t", &"x".repeat(2000));
        assert_eq!(long.chars().count(), 1024);
        assert!(long.ends_with('…'));
    }
}
