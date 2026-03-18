//! MQTT credential store: per-client password + publish/subscribe ACLs.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Credential record for a single MQTT client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttCredential {
    pub client_id: String,
    /// bcrypt hash of the plaintext password.
    pub password_hash: String,
    pub allow_pub: Vec<String>,
    pub allow_sub: Vec<String>,
}

/// In-memory MQTT credential store.
#[derive(Default)]
pub struct MqttCredStore {
    entries: Vec<MqttCredential>,
}

impl MqttCredStore {
    pub fn new(entries: Vec<MqttCredential>) -> Self {
        Self { entries }
    }

    /// Hash a plaintext password with bcrypt and return the hash.
    pub fn hash_password(password: &str) -> Result<String> {
        bcrypt::hash(password, bcrypt::DEFAULT_COST)
            .map_err(|e| anyhow::anyhow!("bcrypt error: {e}"))
    }

    /// Authenticate a client by ID and plaintext password.
    pub fn authenticate(&self, client_id: &str, password: &str) -> bool {
        let Some(cred) = self.entries.iter().find(|c| c.client_id == client_id) else {
            return false;
        };
        bcrypt::verify(password, &cred.password_hash).unwrap_or(false)
    }

    /// Check whether `client_id` may publish to `topic`.
    pub fn can_publish(&self, client_id: &str, topic: &str) -> bool {
        let Some(cred) = self.entries.iter().find(|c| c.client_id == client_id) else {
            return false;
        };
        cred.allow_pub.iter().any(|pat| topic_matches(pat, topic))
    }

    /// Check whether `client_id` may subscribe to `topic_filter`.
    pub fn can_subscribe(&self, client_id: &str, topic_filter: &str) -> bool {
        let Some(cred) = self.entries.iter().find(|c| c.client_id == client_id) else {
            return false;
        };
        cred.allow_sub.iter().any(|pat| topic_matches(pat, topic_filter))
    }
}

/// MQTT wildcard matching for ACL patterns.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    let mut pparts = pattern.split('/');
    let mut tparts = topic.split('/');
    loop {
        match (pparts.next(), tparts.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => continue,
            (Some(p), Some(t)) if p == t => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}
