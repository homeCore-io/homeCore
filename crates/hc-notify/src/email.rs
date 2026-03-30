//! SMTP email notification channel.
//!
//! Supports STARTTLS (port 587) and implicit TLS (port 465).
//! Credentials are PLAIN via username + password.

use anyhow::{Context, Result};
use async_trait::async_trait;
use lettre::{
    message::header::ContentType, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};
use serde::Deserialize;

use crate::channel::NotifyChannel;

/// TOML configuration for the email channel.
///
/// ```toml
/// [[notify.channels]]
/// name      = "alerts"
/// type      = "email"
/// smtp_host = "smtp.example.com"
/// smtp_port = 587          # optional, default 587
/// username  = "user@example.com"
/// password  = "secret"
/// from      = "homecore@example.com"
/// to        = ["ops@example.com", "admin@example.com"]
/// starttls  = true         # optional, default true; set false for port 465
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct EmailConfig {
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    /// Envelope `From` address.
    pub from: String,
    /// One or more envelope `To` addresses.
    pub to: Vec<String>,
    /// `true`  → STARTTLS (port 587); `false` → implicit TLS (port 465).
    #[serde(default = "default_starttls")]
    pub starttls: bool,
}

fn default_smtp_port() -> u16 {
    587
}
fn default_starttls() -> bool {
    true
}

/// SMTP notification channel.
pub struct EmailChannel {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
    to: Vec<String>,
}

impl EmailChannel {
    pub fn new(cfg: &EmailConfig) -> Result<Self> {
        let creds = Credentials::new(cfg.username.clone(), cfg.password.clone());
        let builder = if cfg.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
                .context("SMTP STARTTLS relay setup failed")?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
                .context("SMTP relay setup failed")?
        };
        let transport = builder.port(cfg.smtp_port).credentials(creds).build();
        Ok(Self {
            transport,
            from: cfg.from.clone(),
            to: cfg.to.clone(),
        })
    }
}

#[async_trait]
impl NotifyChannel for EmailChannel {
    async fn send(&self, title: &str, message: &str) -> Result<()> {
        for recipient in &self.to {
            let email = Message::builder()
                .from(self.from.parse().context("Invalid SMTP from address")?)
                .to(recipient.parse().context("Invalid SMTP to address")?)
                .subject(title)
                .header(ContentType::TEXT_PLAIN)
                .body(message.to_string())
                .context("Failed to build email message")?;
            self.transport
                .send(email)
                .await
                .context("SMTP send failed")?;
        }
        Ok(())
    }
}
