//! Core trait that every notification provider implements.
//!
//! ## Adding a new provider
//!
//! 1. Create a new module (e.g. `crates/hc-notify/src/slack.rs`).
//! 2. Define a config struct that derives `serde::Deserialize`.
//! 3. Implement `NotifyChannel` for your provider struct.
//! 4. Add a variant to [`crate::ProviderConfig`] and a build arm in
//!    [`crate::NotificationService::from_configs`].
//!
//! The rule engine and executor need **no changes** — they dispatch via the
//! `NotificationService` registry by channel name.

use anyhow::Result;
use async_trait::async_trait;

/// A notification delivery channel.  Implement this trait to add new providers.
///
/// All methods receive an owned `title` and `message` so implementations are
/// free to format or truncate them as needed.
#[async_trait]
pub trait NotifyChannel: Send + Sync {
    /// Deliver a notification.
    ///
    /// - `title`   — short subject line (used as email subject, Pushover title, etc.)
    /// - `message` — notification body text
    async fn send(&self, title: &str, message: &str) -> Result<()>;
}
