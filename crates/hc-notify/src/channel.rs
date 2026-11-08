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

/// A picture sent with a notification: a camera's snapshot, say.
#[derive(Debug, Clone)]
pub struct Picture {
    pub bytes: Vec<u8>,
    /// `image/jpeg`, `image/png`…
    pub mime: String,
    /// What it's called where a name shows (an email's attachment).
    pub name: String,
}

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

    /// Deliver one with a picture. A provider that can't send pictures sends
    /// the words alone, which is what this does unless it's overridden.
    async fn send_picture(&self, title: &str, message: &str, picture: &Picture) -> Result<()> {
        let _ = picture;
        self.send(title, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A provider with no pictures: it gets the words alone.
    struct Words(Mutex<Vec<String>>);

    #[async_trait]
    impl NotifyChannel for Words {
        async fn send(&self, title: &str, message: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("{title}: {message}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_provider_without_pictures_still_gets_the_words() {
        let w = Words(Mutex::new(Vec::new()));
        let p = Picture {
            bytes: vec![0xff, 0xd8],
            mime: "image/jpeg".into(),
            name: "snapshot.jpg".into(),
        };
        w.send_picture("Front Deck", "a person", &p).await.unwrap();
        assert_eq!(
            *w.0.lock().unwrap(),
            vec!["Front Deck: a person".to_string()]
        );
    }
}
