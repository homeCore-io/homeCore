//! Plugin-runtime enrollment: deciding whether a container may join.
//!
//! A runtime reaches homeCore before it holds any credential, so this is the one
//! part of the plugin story that is REST rather than MQTT. Everything after
//! approval is the ordinary management protocol.
//!
//! This module is the decision logic only — no HTTP, no storage. It takes a
//! request plus the current state of the world and says what should happen, which
//! is what makes the security-relevant parts testable without a server.
//!
//! See `docs/pluginRuntimesPlan.md`, piece 1.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use hc_api_types::plugin_runtimes::{EnrollRequest, RuntimeCapabilities};
use hc_state::plugin_runtime_store::{RuntimeRecord, RuntimeStatus};

/// Length of the human-comparable code, in digits, formatted `NNN-NNN`.
const CODE_DIGITS: u32 = 6;

/// Verify the ed25519 signature on an enrollment request.
///
/// The `runtime_id` appears in logs, so identity cannot rest on it alone: without
/// proof of possession, anyone who read one could re-enroll as a known-good
/// runtime and be handed its credentials. The signature is over
/// [`EnrollRequest::signing_payload`], which covers identity *and* capabilities —
/// a replayed enrollment must not be able to claim a different ABI and collect
/// placements meant for another runtime.
pub fn verify_enrollment_signature(req: &EnrollRequest) -> Result<()> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let b64 = base64::engine::general_purpose::STANDARD;
    let key_bytes = b64
        .decode(req.public_key.trim())
        .map_err(|e| anyhow!("public_key is not base64: {e}"))?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("public_key must be 32 bytes"))?;
    let vk = VerifyingKey::from_bytes(&key_arr)
        .map_err(|_| anyhow!("public_key is not a valid ed25519 key"))?;

    let sig_bytes = b64
        .decode(req.signature.trim())
        .map_err(|e| anyhow!("signature is not base64: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature must be 64 bytes"))?;

    vk.verify(
        req.signing_payload().as_bytes(),
        &Signature::from_bytes(&sig_arr),
    )
    .map_err(|_| anyhow!("signature does not match public_key"))
}

/// Whether a re-enrolling identity is presenting the same key it first used.
///
/// A runtime that re-enrolls after losing its credentials is expected and fine.
/// A runtime that re-enrolls with a *different* key is not the same runtime, so
/// its id is being reused — by a fresh container that lost its volume, or by
/// someone who read the id somewhere. Either way the answer is the same: it does
/// not inherit the existing record's approval.
pub fn is_same_identity(existing: &RuntimeRecord, req: &EnrollRequest) -> bool {
    existing.public_key == req.public_key
}

/// What the caller should do with an enrollment request.
#[derive(Debug, PartialEq, Eq)]
pub enum EnrollDecision {
    /// Approve immediately and mint credentials — token mode, or a known
    /// identity that was already approved.
    Approve,
    /// Record as pending and show the admin a code.
    Pending,
    /// Refuse, with a reason the runtime can log.
    Reject(String),
}

/// Inputs the decision depends on, gathered by the caller.
pub struct EnrollContext<'a> {
    pub now: DateTime<Utc>,
    /// True when the deployment requires an admin-issued token.
    pub token_only: bool,
    /// Whether the presented token (if any) was valid and unused.
    pub token_valid: bool,
    /// Existing record for this `runtime_id`, if it has enrolled before.
    pub existing: Option<&'a RuntimeRecord>,
    /// Currently pending, unexpired records.
    pub pending_count: u32,
    pub max_pending: u32,
    pub max_denials: u32,
}

/// Decide what happens to an enrollment request.
///
/// Signature verification is the caller's job and must happen first — this
/// function assumes the request is authentic and decides only whether an
/// authentic request is *welcome*.
pub fn decide(ctx: &EnrollContext) -> EnrollDecision {
    // A re-enrollment that cannot prove it is the same identity is treated as a
    // new one. The caller enforces this by refusing to reuse the old record.
    if let Some(existing) = ctx.existing {
        if !existing.may_retry(ctx.now) {
            return EnrollDecision::Reject(
                "this runtime has been denied too many times; try again later".into(),
            );
        }
        if existing.denial_count >= ctx.max_denials {
            return EnrollDecision::Reject(
                "this runtime has been denied too many times; try again later".into(),
            );
        }
        // Already approved and still holding the same key: hand the credentials
        // back rather than making an operator approve a runtime they already
        // trust because its container restarted.
        if existing.status == RuntimeStatus::Approved {
            return EnrollDecision::Approve;
        }
    }

    if ctx.token_only {
        return if ctx.token_valid {
            EnrollDecision::Approve
        } else {
            EnrollDecision::Reject(
                "this homeCore requires an enrollment token; set HOMECORE_ENROLL_TOKEN".into(),
            )
        };
    }

    // Open mode. A valid token still short-circuits the wait — an operator who
    // went to the trouble of issuing one has already expressed the intent that
    // approving a code would express.
    if ctx.token_valid {
        return EnrollDecision::Approve;
    }

    if ctx.pending_count >= ctx.max_pending {
        return EnrollDecision::Reject("too many runtimes are already waiting for approval".into());
    }

    EnrollDecision::Pending
}

/// A short, human-comparable confirmation code, formatted `NNN-NNN`.
///
/// Not a credential and never checked by the server: it exists so the admin can
/// confirm *this* container against the code in its logs, rather than approving
/// whichever request happened to arrive while they were looking. Digits only,
/// grouped, because it is going to be read off one screen and compared with
/// another.
pub fn generate_code() -> Result<String> {
    let mut raw = [0u8; 4];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    let n = u32::from_be_bytes(raw) % 10u32.pow(CODE_DIGITS);
    let s = format!("{n:0width$}", width = CODE_DIGITS as usize);
    Ok(format!("{}-{}", &s[..3], &s[3..]))
}

/// The plugin id a runtime registers under once approved.
///
/// Derived from the kind and the identity so it is stable across restarts and
/// legible in a plugin list: `plugin.python-a1b2c3d4`.
pub fn plugin_id_for(capabilities: &RuntimeCapabilities, runtime_id: &str) -> Result<String> {
    let kind = capabilities.kind.trim().to_lowercase();
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("runtime kind must be alphanumeric");
    }
    let short: String = runtime_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if short.is_empty() {
        bail!("runtime_id has no usable characters");
    }
    Ok(format!("plugin.{kind}-{short}"))
}

/// When a pending record stops being answerable.
pub fn pending_expiry(now: DateTime<Utc>, ttl_mins: u32) -> DateTime<Utc> {
    now + Duration::minutes(ttl_mins.max(1) as i64)
}

/// Cooldown applied once an identity has been denied too often.
pub fn cooldown_until(now: DateTime<Utc>, mins: u32) -> DateTime<Utc> {
    now + Duration::minutes(mins.max(1) as i64)
}

/// Mint the MQTT password a runtime connects with.
///
/// Same shape as the credential `plugin_install` mints for a binary plugin, for
/// the same reason: the runtime is a plugin, and there is no argument for it
/// holding a different kind of secret from every other plugin.
pub fn mint_mqtt_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use hc_api_types::plugin_runtimes::RuntimeCapabilities;

    fn caps() -> RuntimeCapabilities {
        RuntimeCapabilities {
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
        }
    }

    fn signed_request(seed: u8) -> (EnrollRequest, SigningKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut req = EnrollRequest {
            runtime_id: "rt-abcdef123456".into(),
            public_key: b64.encode(sk.verifying_key().to_bytes()),
            signature: String::new(),
            capabilities: caps(),
            host_version: "0.1.0".into(),
            sdk_version: "0.2.0".into(),
            hostname: "pyhost".into(),
            network_mode: "host".into(),
            token: None,
        };
        req.signature = b64.encode(sk.sign(req.signing_payload().as_bytes()).to_bytes());
        (req, sk)
    }

    fn record(status: RuntimeStatus) -> RuntimeRecord {
        RuntimeRecord {
            runtime_id: "rt-abcdef123456".into(),
            public_key: "PUB".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
            host_version: "0.1.0".into(),
            sdk_version: "0.2.0".into(),
            hostname: "pyhost".into(),
            network_mode: "host".into(),
            status,
            code: None,
            secret_hash: None,
            source_ip: None,
            plugin_id: None,
            denial_count: 0,
            cooldown_until: None,
            created_at: Utc::now(),
            expires_at: None,
            last_seen_at: None,
        }
    }

    fn ctx<'a>(existing: Option<&'a RuntimeRecord>) -> EnrollContext<'a> {
        EnrollContext {
            now: Utc::now(),
            token_only: false,
            token_valid: false,
            existing,
            pending_count: 0,
            max_pending: 5,
            max_denials: 3,
        }
    }

    #[test]
    fn a_genuine_signature_verifies() {
        let (req, _) = signed_request(7);
        assert!(verify_enrollment_signature(&req).is_ok());
    }

    /// The point of signing capabilities: a captured enrollment replayed with a
    /// different ABI would otherwise be handed placements built for another
    /// runtime, which would then fail to install on the operator's machine.
    #[test]
    fn tampering_with_capabilities_breaks_the_signature() {
        let (mut req, _) = signed_request(7);
        req.capabilities.abi = "cp311-manylinux_2_28".into();
        assert!(verify_enrollment_signature(&req).is_err());
    }

    #[test]
    fn a_signature_from_another_key_is_rejected() {
        let (mut req, _) = signed_request(7);
        let (other, _) = signed_request(9);
        req.signature = other.signature;
        assert!(verify_enrollment_signature(&req).is_err());
    }

    /// Cosmetic fields are outside the signed payload on purpose, so renaming a
    /// container or updating the host does not invalidate its identity.
    #[test]
    fn renaming_the_host_does_not_break_the_signature() {
        let (mut req, _) = signed_request(7);
        req.hostname = "renamed".into();
        req.host_version = "9.9.9".into();
        assert!(verify_enrollment_signature(&req).is_ok());
    }

    #[test]
    fn malformed_key_and_signature_are_errors_not_panics() {
        let (mut req, _) = signed_request(7);
        req.public_key = "not base64!!".into();
        assert!(verify_enrollment_signature(&req).is_err());

        let (mut req, _) = signed_request(7);
        req.signature = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        assert!(verify_enrollment_signature(&req).is_err());
    }

    #[test]
    fn token_mode_requires_a_valid_token() {
        let mut c = ctx(None);
        c.token_only = true;
        assert!(matches!(decide(&c), EnrollDecision::Reject(_)));

        c.token_valid = true;
        assert_eq!(decide(&c), EnrollDecision::Approve);
    }

    #[test]
    fn open_mode_leaves_a_tokenless_request_pending() {
        assert_eq!(decide(&ctx(None)), EnrollDecision::Pending);
    }

    /// An operator who issued a token has already expressed the intent that
    /// approving a code expresses, so they should not have to do both.
    #[test]
    fn a_token_short_circuits_the_wait_in_open_mode() {
        let mut c = ctx(None);
        c.token_valid = true;
        assert_eq!(decide(&c), EnrollDecision::Approve);
    }

    /// A restarted container that lost its credentials but kept its identity
    /// should not need re-approving; the operator already decided.
    #[test]
    fn an_approved_runtime_re_enrolling_is_approved_again() {
        let rec = record(RuntimeStatus::Approved);
        assert_eq!(decide(&ctx(Some(&rec))), EnrollDecision::Approve);
    }

    #[test]
    fn the_pending_cap_is_enforced() {
        let mut c = ctx(None);
        c.pending_count = 5;
        assert!(matches!(decide(&c), EnrollDecision::Reject(_)));
    }

    #[test]
    fn a_cooling_down_identity_is_refused() {
        let mut rec = record(RuntimeStatus::Denied);
        rec.cooldown_until = Some(Utc::now() + Duration::minutes(30));
        assert!(matches!(
            decide(&ctx(Some(&rec))),
            EnrollDecision::Reject(_)
        ));
    }

    /// Once the cooldown elapses the identity is welcome to ask again — the
    /// limits bound noise, they are not a permanent ban.
    #[test]
    fn an_elapsed_cooldown_lets_it_ask_again() {
        let mut rec = record(RuntimeStatus::Denied);
        rec.cooldown_until = Some(Utc::now() - Duration::minutes(1));
        rec.denial_count = 1;
        assert_eq!(decide(&ctx(Some(&rec))), EnrollDecision::Pending);
    }

    /// A re-enrollment carrying a different key is a different runtime wearing a
    /// known id, and must not inherit its approval.
    #[test]
    fn a_different_key_is_a_different_identity() {
        let (req, _) = signed_request(7);
        let mut rec = record(RuntimeStatus::Approved);
        rec.public_key = req.public_key.clone();
        assert!(is_same_identity(&rec, &req));

        rec.public_key = "SOMETHING-ELSE".into();
        assert!(!is_same_identity(&rec, &req));
    }

    #[test]
    fn codes_are_grouped_digits() {
        for _ in 0..50 {
            let c = generate_code().unwrap();
            assert_eq!(c.len(), 7, "{c}");
            assert_eq!(&c[3..4], "-", "{c}");
            assert!(
                c.chars().filter(|c| *c != '-').all(|c| c.is_ascii_digit()),
                "{c}"
            );
        }
    }

    #[test]
    fn plugin_ids_are_stable_and_legible() {
        let id = plugin_id_for(&caps(), "rt-abcdef123456").unwrap();
        assert_eq!(id, "plugin.python-rtabcdef");
        assert_eq!(
            id,
            plugin_id_for(&caps(), "rt-abcdef123456").unwrap(),
            "same inputs, same id — it is what rules and history key on"
        );
    }

    #[test]
    fn a_hostile_kind_cannot_forge_a_plugin_id() {
        let mut c = caps();
        c.kind = "python/../hue".into();
        assert!(plugin_id_for(&c, "rt-abc").is_err());
    }
}
