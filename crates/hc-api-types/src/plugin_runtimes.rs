//! Plugin-runtime enrollment types.
//!
//! A **plugin runtime** is a container the operator runs themselves, hosting
//! plugins written in something other than Rust. It has to reach homeCore
//! before it has any credentials, so enrollment is REST rather than MQTT —
//! everything afterwards is the ordinary plugin management protocol.
//!
//! These types are the seam between core and the runtime host, and they are
//! deliberately the first thing written: once they are fixed, the two sides
//! can be built independently. See `docs/pluginRuntimesPlan.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a runtime can host, advertised at enrollment and matched against an
/// artifact at placement time.
///
/// The triple is the whole matching key. `abi` pins the interpreter and its
/// platform tag together (`cp312-manylinux_2_28`) because for a hermetic
/// artifact those two are never independently true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    /// Runtime kind: `python`, `node`, `dotnet`.
    pub kind: String,
    /// ABI tag artifacts must match, e.g. `cp312-manylinux_2_28`.
    pub abi: String,
    /// `x86_64` | `aarch64`.
    pub arch: String,
}

/// `POST /api/v1/plugin-runtimes/enroll`.
///
/// Sent before the runtime has credentials of any kind, so it is
/// self-describing and none of it is trusted until an admin approves it (open
/// mode) or a token proves the operator initiated it (token mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// Stable identity, generated once and persisted to the runtime's volume.
    pub runtime_id: String,
    /// base64 ed25519 public key. The runtime signs this request with the
    /// matching private key, so a `runtime_id` read out of a log is not enough
    /// to impersonate it on re-enrollment.
    pub public_key: String,
    /// base64 ed25519 signature over the canonical signing payload — see
    /// [`EnrollRequest::signing_payload`].
    pub signature: String,
    pub capabilities: RuntimeCapabilities,
    /// Version of the runtime host binary.
    pub host_version: String,
    /// Version of the language SDK baked into the image.
    pub sdk_version: String,
    /// Reported for the operator's benefit when approving; never trusted.
    pub hostname: String,
    /// `host` | `bridge` | `macvlan`. Surfaced at approval because a bridged
    /// runtime cannot do mDNS or SSDP discovery, which is the failure that
    /// looks like a broken plugin rather than a network choice.
    pub network_mode: String,
    /// One-time enrollment token, when the deployment runs in token mode.
    #[serde(default)]
    pub token: Option<String>,
}

impl EnrollRequest {
    /// The exact bytes covered by `signature`.
    ///
    /// Field order is fixed and the separator cannot appear in any component,
    /// so two different requests cannot produce the same payload. Signing a
    /// serialised struct instead would make the signature depend on serde's
    /// field order and on how the client's JSON encoder spaces things.
    pub fn signing_payload(&self) -> String {
        format!(
            "hc-runtime-enroll:v1\n{}\n{}\n{}\n{}\n{}",
            self.runtime_id,
            self.public_key,
            self.capabilities.kind,
            self.capabilities.abi,
            self.capabilities.arch,
        )
    }
}

/// Where an enrollment stands. The runtime polls until this stops being
/// `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeStatus {
    Pending,
    Approved,
    Denied,
}

/// Response to `POST /enroll`, and to `GET /plugin-runtimes/{id}` while the
/// runtime is polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub runtime_id: String,
    pub status: RuntimeStatus,
    /// Bearer for the status poll. Returned once, at enrollment, and never
    /// again — full-strength random, unlike `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_secret: Option<String>,
    /// Short, human-comparable, and **not** a credential. It exists so the
    /// admin can confirm *this* container against the code in its logs rather
    /// than approving whatever happened to ask. Absent in token mode, where
    /// the operator already proved intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Set once approved: everything the runtime needs to join the broker and
    /// call back into core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<RuntimeCredentials>,
    /// Why a denial happened, when there is something useful to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When a pending record expires, so the runtime can stop polling instead
    /// of hammering a record that is already gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Handed over exactly once, on approval.
///
/// This is the runtime's *own* credential, covering its management channel
/// only. Plugins it hosts get their own minted credentials at placement time,
/// which is what preserves the per-plugin `[[broker.clients]]` ACL model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCredentials {
    pub broker_host: String,
    pub broker_port: u16,
    /// The plugin id the runtime registers under: `plugin.<kind>-<short-id>`.
    pub plugin_id: String,
    pub mqtt_password: String,
    /// For pulling artifacts and configs back from core over HTTP.
    pub api_key: String,
}

/// Admin-facing view. Never carries the secret or the credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSummary {
    pub runtime_id: String,
    pub status: RuntimeStatus,
    pub capabilities: RuntimeCapabilities,
    pub host_version: String,
    pub sdk_version: String,
    pub hostname: String,
    pub network_mode: String,
    /// Shown while pending so the admin can match it against container logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Where the enrollment came from. Part of the same judgement as the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Registered plugin id, once approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> EnrollRequest {
        EnrollRequest {
            runtime_id: "rt-abc".into(),
            public_key: "PUBKEY".into(),
            signature: "SIG".into(),
            capabilities: RuntimeCapabilities {
                kind: "python".into(),
                abi: "cp312-manylinux_2_28".into(),
                arch: "x86_64".into(),
            },
            host_version: "0.1.0".into(),
            sdk_version: "0.2.0".into(),
            hostname: "pyhost".into(),
            network_mode: "host".into(),
            token: None,
        }
    }

    /// The signature covers identity and capabilities. If capabilities were
    /// outside it, an attacker who replayed a captured enrollment could claim
    /// to host a different ABI and be handed placements meant for elsewhere.
    #[test]
    fn signing_payload_covers_identity_and_capabilities() {
        let base = req().signing_payload();
        for mutate in [
            |r: &mut EnrollRequest| r.runtime_id = "rt-other".into(),
            |r: &mut EnrollRequest| r.public_key = "OTHER".into(),
            |r: &mut EnrollRequest| r.capabilities.kind = "node".into(),
            |r: &mut EnrollRequest| r.capabilities.abi = "cp311-manylinux_2_28".into(),
            |r: &mut EnrollRequest| r.capabilities.arch = "aarch64".into(),
        ] {
            let mut r = req();
            mutate(&mut r);
            assert_ne!(
                base,
                r.signing_payload(),
                "mutation must change the payload"
            );
        }
    }

    /// The signature is over identity, not over the self-reported trivia an
    /// operator reads at approval time. Those can change between restarts
    /// without invalidating the identity.
    #[test]
    fn signing_payload_ignores_cosmetic_fields() {
        let mut r = req();
        r.hostname = "renamed".into();
        r.host_version = "9.9.9".into();
        assert_eq!(req().signing_payload(), r.signing_payload());
    }

    /// A pending response must not carry credentials, and the wire form must
    /// omit them rather than send nulls a client could misread as "approved
    /// with no password".
    #[test]
    fn pending_response_omits_credentials_entirely() {
        let resp = EnrollResponse {
            runtime_id: "rt-abc".into(),
            status: RuntimeStatus::Pending,
            enrollment_secret: Some("hc_sk_x".into()),
            code: Some("482-193".into()),
            credentials: None,
            reason: None,
            expires_at: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("credentials"), "{json}");
        assert!(json.contains("\"status\":\"pending\""), "{json}");
    }
}
