//! Wire types for plugin runtimes.
//!
//! A runtime is a container the operator runs, which hosts plugins written in
//! another language. The admin adds it in homeCore, which makes its login on
//! the spot and shows a [`ConnectionKey`] once. The operator pastes the key
//! into the runtime's page (or sets `HC_RUNTIME_KEY`), and the runtime connects
//! with it. Nothing asks to join and nothing waits for approval: adding it in
//! homeCore is the approval.
//!
//! Shared between core and `hc-runtime-host`, so the key is written and read by
//! the same code.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a runtime can host: the language, the ABI its artifacts must match, and
/// its architecture. An artifact is placed on a runtime only when all three
/// match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    /// e.g. `python`.
    pub kind: String,
    /// e.g. `cp312-manylinux_2_28`.
    pub abi: String,
    /// e.g. `x86_64`, `aarch64`.
    pub arch: String,
}

/// The languages a runtime can be added for, with the ABI each pins. One ABI per
/// language per homeCore minor, so the matrix stays small and an ABI change is a
/// release note rather than a drift.
pub const KINDS: &[(&str, &str)] = &[("python", "cp312-manylinux_2_28")];

/// The ABI homeCore pins for a runtime kind, if it supports that kind.
pub fn abi_for(kind: &str) -> Option<&'static str> {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, abi)| *abi)
}

/// Everything a runtime needs to connect, in one string an operator copies once.
///
/// `hcrt1.` and then the fields as base64url JSON. The secret is inside, so the
/// key is shown once and is as sensitive as a password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionKey {
    /// homeCore's address as the runtime reaches it, e.g. `http://homecore.local:8080`.
    pub url: String,
    /// The broker as the runtime reaches it.
    pub broker_host: String,
    pub broker_port: u16,
    pub runtime_id: String,
    /// The runtime's broker username, which is also the plugin id it registers as.
    pub username: String,
    /// The runtime's one password, for the broker and for homeCore's API.
    pub secret: String,
    /// What it was added to host. A runtime whose image is a different kind or
    /// ABI refuses the key.
    pub kind: String,
    pub abi: String,
}

const KEY_PREFIX: &str = "hcrt1.";

impl ConnectionKey {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("a key is plain strings and a port");
        format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Read a key as pasted. Spaces and line breaks a copy may have picked up
    /// are ignored.
    pub fn decode(text: &str) -> Result<Self, String> {
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let body = compact.strip_prefix(KEY_PREFIX).ok_or_else(|| {
            "that isn't a runtime key: it should start with hcrt1. Copy it again from \
             homeCore, Settings, Plugins, Runtimes."
                .to_string()
        })?;
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| "the key is incomplete or damaged; copy it again".to_string())?;
        serde_json::from_slice(&json)
            .map_err(|_| "the key is incomplete or damaged; copy it again".to_string())
    }
}

/// `POST /plugin-runtimes`: add a runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRuntimeRequest {
    pub name: String,
    /// A kind from [`KINDS`]. Defaults to `python`.
    #[serde(default)]
    pub kind: Option<String>,
    /// homeCore's address as the runtime will reach it. The app fills it in from
    /// the address it is using; the admin can change it.
    pub homecore_url: String,
    /// The most plugins it may hold, for the machine it runs on. None or 0: no limit.
    #[serde(default)]
    pub max_plugins: Option<u32>,
}

/// `PATCH /plugin-runtimes/{id}`: rename it or change its limit. A field left
/// out stays as it is; `max_plugins: null` (or 0) removes the limit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateRuntimeRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, with = "double_option")]
    pub max_plugins: Option<Option<u32>>,
}

/// Tells a field left out (`None`) from one set to null (`Some(None)`).
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Option<u32>>, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<u32>>, D::Error> {
        Option::<u32>::deserialize(d).map(Some)
    }
}

/// The answer to adding a runtime or resetting its login: the key, once, and
/// what is inside it for anyone setting a runtime up by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedKey {
    pub runtime: RuntimeSummary,
    pub key: String,
    pub details: ConnectionKey,
}

/// `POST /plugin-runtimes/{id}/hello`: what a runtime reports about itself when
/// it connects, and every so often after. Also how its page tests a key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloRequest {
    pub capabilities: RuntimeCapabilities,
    pub host_version: String,
    pub hostname: String,
    pub network_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    pub runtime_id: String,
    pub name: String,
    pub plugin_id: String,
    /// The oldest runtime host this homeCore works with. A runtime older than
    /// this says so on its own page.
    #[serde(default)]
    pub min_host_version: Option<String>,
}

/// A runtime as the admin list shows it. Never carries the secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSummary {
    pub runtime_id: String,
    pub name: String,
    pub kind: String,
    pub abi: String,
    pub plugin_id: String,
    pub homecore_url: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_plugins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// True when the runtime's host is older than this homeCore expects. Updating
    /// it is pulling the newer image and restarting the container.
    #[serde(default)]
    pub outdated: bool,
    /// The oldest runtime host this homeCore works with.
    #[serde(default)]
    pub min_host_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ConnectionKey {
        ConnectionKey {
            url: "http://homecore.local:8080".into(),
            broker_host: "homecore.local".into(),
            broker_port: 1883,
            runtime_id: "rt-4f2a9c17".into(),
            username: "plugin.python-rt4f2a9c17".into(),
            secret: "hc_sk_abc".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
        }
    }

    #[test]
    fn a_key_round_trips() {
        let text = key().encode();
        assert!(text.starts_with("hcrt1."), "{text}");
        assert_eq!(ConnectionKey::decode(&text).unwrap(), key());
    }

    #[test]
    fn a_key_pasted_with_line_breaks_still_reads() {
        let text = key().encode();
        let (a, b) = text.split_at(20);
        assert_eq!(
            ConnectionKey::decode(&format!("  {a}\n{b} ")).unwrap(),
            key()
        );
    }

    #[test]
    fn something_else_pasted_says_what_it_should_be() {
        let err = ConnectionKey::decode("hc_sk_notakey").unwrap_err();
        assert!(err.contains("hcrt1."), "{err}");
        let err = ConnectionKey::decode("hcrt1.!!!").unwrap_err();
        assert!(err.contains("damaged"), "{err}");
    }

    #[test]
    fn only_supported_kinds_have_an_abi() {
        assert_eq!(abi_for("python"), Some("cp312-manylinux_2_28"));
        assert_eq!(abi_for("node"), None);
    }
}
