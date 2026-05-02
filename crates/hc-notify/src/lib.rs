//! `hc-notify` — pluggable notification channels for HomeCore rules.
//!
//! ## Built-in providers
//!
//! | Type         | Module               | Description                         |
//! |--------------|----------------------|-------------------------------------|
//! | `"email"`    | [`email::EmailChannel`]     | SMTP with STARTTLS or implicit TLS  |
//! | `"pushover"` | [`pushover::PushoverChannel`] | Pushover push notifications       |
//!
//! ## Adding a new provider
//!
//! 1. Create `crates/hc-notify/src/<name>.rs`.
//! 2. Implement [`NotifyChannel`] for your provider struct.
//! 3. Add a `Deserialize`-able config struct and a variant to [`ProviderConfig`].
//! 4. Add a build arm to [`NotificationService::from_configs`].
//!
//! The rule executor dispatches via channel name — **no changes needed there**.
//!
//! ## Rule usage
//!
//! ```json
//! {
//!   "Notify": {
//!     "channel": "phone",
//!     "title":   "Motion detected",
//!     "message": "Front door sensor triggered at 22:15"
//!   }
//! }
//! ```

pub mod channel;
pub mod email;
pub mod pushover;
pub mod telegram;

pub use channel::NotifyChannel;
pub use email::{EmailChannel, EmailConfig};
pub use pushover::{PushoverChannel, PushoverConfig};
pub use telegram::{TelegramChannel, TelegramConfig};

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

/// Sentinel value used in the shipped example configs for Telegram
/// `bot_token` / `chat_id`. Channels carrying this sentinel are rejected
/// at boot rather than silently dropping every notification at runtime.
const TELEGRAM_PLACEHOLDER: &str = "CHANGE_ME";

// ---------------------------------------------------------------------------
// TOML config types
// ---------------------------------------------------------------------------

/// Top-level TOML block: `[[notify.channels]]`
#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    /// Name used by `Notify { channel: "..." }` rules.
    pub name: String,
    #[serde(flatten)]
    pub provider: ProviderConfig,
}

/// Tagged union — `type = "email"` / `type = "pushover"`.
///
/// To add a provider: add a variant here and a build arm in
/// [`NotificationService::from_configs`].
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    Email(EmailConfig),
    Pushover(PushoverConfig),
    Telegram(TelegramConfig),
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Registry of named notification channels.
///
/// Channels are registered by name at startup from config.  The rule executor
/// calls [`NotificationService::notify`] with the channel name from the
/// `Notify` action; the service looks up and dispatches to the right provider.
///
/// [`NotificationService::register`] accepts any type that implements
/// [`NotifyChannel`], making it easy to add providers in tests or at runtime.
#[derive(Clone, Default)]
pub struct NotificationService {
    channels: HashMap<String, Arc<dyn NotifyChannel>>,
}

impl NotificationService {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Register any [`NotifyChannel`] implementation under a name.
    ///
    /// This is the extension point — call this to add custom providers that
    /// are not part of the built-in config-driven pipeline.
    pub fn register(&mut self, name: impl Into<String>, channel: impl NotifyChannel + 'static) {
        self.channels.insert(name.into(), Arc::new(channel));
    }

    /// Build a service from TOML-deserialized channel configs.
    ///
    /// Returns `Err` if a Telegram channel is declared with placeholder
    /// (`CHANGE_ME`) values for `bot_token` or `chat_id` — better to fail
    /// fast at boot than to silently drop notifications at runtime when a
    /// rule fires (release_0_1_1.md S4).
    ///
    /// Other channel-init errors (e.g. SMTP host unreachable for email)
    /// are logged and the offending channel is skipped — those happen at
    /// runtime and shouldn't block startup.
    pub fn from_configs(configs: Vec<ChannelConfig>) -> Result<Self> {
        let mut svc = Self::new();
        for cfg in configs {
            let name = cfg.name;
            match cfg.provider {
                ProviderConfig::Email(ec) => match EmailChannel::new(&ec) {
                    Ok(ch) => {
                        info!(channel = %name, "Registered email notification channel");
                        svc.register(name, ch);
                    }
                    Err(e) => {
                        warn!(channel = %name, error = %e, "Email channel init failed — skipping");
                    }
                },
                ProviderConfig::Pushover(pc) => {
                    info!(channel = %name, "Registered Pushover notification channel");
                    svc.register(name, PushoverChannel::new(pc));
                }
                ProviderConfig::Telegram(tc) => {
                    if tc.bot_token == TELEGRAM_PLACEHOLDER
                        || tc.chat_id == TELEGRAM_PLACEHOLDER
                    {
                        anyhow::bail!(
                            "[[notify.channels]] '{name}' (type = telegram) still has \
                             placeholder values — set a real bot_token (from @BotFather) \
                             and chat_id, or comment out / remove this channel from the config"
                        );
                    }
                    info!(channel = %name, "Registered Telegram notification channel");
                    svc.register(name, TelegramChannel::new(tc));
                }
            }
        }
        Ok(svc)
    }

    /// Send via the named channel.
    ///
    /// The special channel name `"all"` fans the message out to every
    /// registered channel.  Any individual channel errors are logged but do
    /// not prevent delivery to the remaining channels.
    ///
    /// Returns an error if a specific channel name is not registered, or if
    /// the underlying provider returns an error.
    pub async fn notify(&self, channel: &str, title: &str, message: &str) -> Result<()> {
        if channel == "all" {
            let mut last_err: Option<anyhow::Error> = None;
            for (name, ch) in &self.channels {
                if let Err(e) = ch.send(title, message).await {
                    warn!(channel = %name, error = %e, "notify_all: channel delivery failed");
                    last_err = Some(e);
                }
            }
            return if let Some(e) = last_err {
                Err(e.context("one or more notify_all channels failed"))
            } else {
                Ok(())
            };
        }
        let ch = self
            .channels
            .get(channel)
            .with_context(|| format!("Notification channel '{channel}' not configured"))?;
        ch.send(title, message).await
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    pub fn channel_names(&self) -> Vec<&str> {
        self.channels.keys().map(|s| s.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn telegram_cfg(name: &str, bot_token: &str, chat_id: &str) -> ChannelConfig {
        ChannelConfig {
            name: name.into(),
            provider: ProviderConfig::Telegram(TelegramConfig {
                bot_token: bot_token.into(),
                chat_id: chat_id.into(),
                markdown: false,
            }),
        }
    }

    #[test]
    fn telegram_placeholder_bot_token_refuses_boot() {
        let result = NotificationService::from_configs(vec![telegram_cfg(
            "telegram",
            TELEGRAM_PLACEHOLDER,
            "-100123456789",
        )]);
        let err = result
            .err()
            .expect("expected boot-time error for placeholder bot_token");
        let msg = err.to_string();
        assert!(msg.contains("placeholder"), "error message: {msg}");
        assert!(msg.contains("telegram"), "error message: {msg}");
    }

    #[test]
    fn telegram_placeholder_chat_id_refuses_boot() {
        let result = NotificationService::from_configs(vec![telegram_cfg(
            "telegram",
            "123456:ABC-DEF",
            TELEGRAM_PLACEHOLDER,
        )]);
        let err = result
            .err()
            .expect("expected boot-time error for placeholder chat_id");
        assert!(err.to_string().contains("placeholder"));
    }

    #[test]
    fn telegram_real_values_register_cleanly() {
        let svc = NotificationService::from_configs(vec![telegram_cfg(
            "telegram",
            "123456:ABC-DEF",
            "-100123456789",
        )])
        .expect("real values should register without error");
        assert_eq!(svc.channel_names(), vec!["telegram"]);
    }

    #[test]
    fn empty_config_is_fine() {
        let svc = NotificationService::from_configs(vec![]).expect("empty is OK");
        assert!(svc.is_empty());
    }
}
