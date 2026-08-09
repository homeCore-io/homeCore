//! The enrollment client: ask homeCore to join, then wait for an answer.
//!
//! The host has no credential until this completes, so it is the one part of the
//! runtime's life that speaks HTTP rather than MQTT.

use anyhow::{bail, Context, Result};
use hc_api_types::plugin_runtimes::{
    EnrollRequest, EnrollResponse, RuntimeCapabilities, RuntimeCredentials, RuntimeStatus,
};
use std::time::Duration;

use crate::identity::Identity;

/// How often to ask again while pending. Short enough that an approval feels
/// immediate, long enough that a fifteen-minute wait is a few hundred requests
/// rather than a flood — core deliberately does not rate-limit this endpoint.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

pub struct EnrollConfig {
    /// Base URL of homeCore, e.g. `http://10.0.10.150:8080`.
    pub base_url: String,
    pub capabilities: RuntimeCapabilities,
    pub host_version: String,
    pub sdk_version: String,
    pub hostname: String,
    pub network_mode: String,
    pub token: Option<String>,
}

fn api(base: &str, path: &str) -> String {
    format!("{}/api/v1{}", base.trim_end_matches('/'), path)
}

pub fn build_request(id: &Identity, cfg: &EnrollConfig) -> EnrollRequest {
    let mut req = EnrollRequest {
        runtime_id: id.runtime_id.clone(),
        public_key: id.public_key_b64(),
        signature: String::new(),
        capabilities: cfg.capabilities.clone(),
        host_version: cfg.host_version.clone(),
        sdk_version: cfg.sdk_version.clone(),
        hostname: cfg.hostname.clone(),
        network_mode: cfg.network_mode.clone(),
        token: cfg.token.clone(),
    };
    // Signed over the canonical payload, not the serialised struct, so the
    // signature does not depend on how this client happens to encode JSON.
    req.signature = id.sign_b64(&req.signing_payload());
    req
}

/// Enroll and wait until homeCore answers.
///
/// Returns credentials on approval. A denial or an expiry is an error rather
/// than a retry loop: both mean a human decided something, and hammering the
/// endpoint would neither change their mind nor tell them anything.
pub async fn enroll_and_wait(
    http: &reqwest::Client,
    id: &Identity,
    cfg: &EnrollConfig,
) -> Result<RuntimeCredentials> {
    let req = build_request(id, cfg);
    let url = api(&cfg.base_url, "/plugin-runtimes/enroll");

    let resp = http
        .post(&url)
        .json(&req)
        .send()
        .await
        .with_context(|| format!("POST {url} — is HOMECORE_URL reachable?"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("enrollment refused ({status}): {}", body.trim());
    }
    let first: EnrollResponse =
        serde_json::from_str(&body).context("enrollment response was not the expected shape")?;

    match first.status {
        RuntimeStatus::Approved => {
            let creds = first
                .credentials
                .context("core approved the runtime but sent no credentials")?;
            tracing::info!(plugin_id = %creds.plugin_id, "enrolled and approved");
            return Ok(creds);
        }
        RuntimeStatus::Denied => bail!("this runtime was denied"),
        RuntimeStatus::Pending => {}
    }

    announce_pending(&first);
    let secret = first
        .enrollment_secret
        .context("core left the runtime pending but sent no enrollment secret")?;

    let poll_url = api(
        &cfg.base_url,
        &format!("/plugin-runtimes/{}", id.runtime_id),
    );
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = match http.get(&poll_url).bearer_auth(&secret).send().await {
            Ok(r) => r,
            Err(e) => {
                // A blip while waiting is not a decision. Keep waiting.
                tracing::warn!(error = %e, "could not reach homeCore; still waiting");
                continue;
            }
        };
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::GONE {
            bail!("the enrollment expired before it was approved; restart to try again");
        }
        if !status.is_success() {
            tracing::warn!(%status, "unexpected reply while waiting");
            continue;
        }
        let Ok(next) = serde_json::from_str::<EnrollResponse>(&body) else {
            tracing::warn!("unreadable reply while waiting");
            continue;
        };
        match next.status {
            RuntimeStatus::Approved => {
                let creds = next
                    .credentials
                    .context("core approved the runtime but sent no credentials")?;
                tracing::info!(plugin_id = %creds.plugin_id, "approved");
                return Ok(creds);
            }
            RuntimeStatus::Denied => bail!("an administrator denied this runtime"),
            RuntimeStatus::Pending => {}
        }
    }
}

/// Put the code where a human will actually find it.
///
/// The operator has to compare this against what homeCore shows before
/// approving — that comparison is the entire security of open enrollment — so it
/// is worth more than one line of `INFO` scrolling past.
fn announce_pending(resp: &EnrollResponse) {
    let code = resp.code.as_deref().unwrap_or("(none)");
    let expires = resp
        .expires_at
        .map(|e| e.to_rfc3339())
        .unwrap_or_else(|| "unknown".into());
    tracing::info!(
        "\n\
         ╭──────────────────────────────────────────────╮\n\
         │  Waiting for approval in homeCore            │\n\
         │                                              │\n\
         │      confirmation code:   {code:<18} │\n\
         │                                              │\n\
         │  Approve this runtime in homeCore and check  │\n\
         │  the code shown there matches the one above. │\n\
         │  If it does not match, deny it.              │\n\
         ╰──────────────────────────────────────────────╯\n\
         expires at {expires}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn cfg() -> EnrollConfig {
        EnrollConfig {
            base_url: "http://core:8080/".into(),
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

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        assert_eq!(
            api("http://core:8080/", "/plugin-runtimes/enroll"),
            "http://core:8080/api/v1/plugin-runtimes/enroll"
        );
        assert_eq!(
            api("http://core:8080", "/plugin-runtimes/enroll"),
            "http://core:8080/api/v1/plugin-runtimes/enroll"
        );
    }

    /// The request this host builds must satisfy the verification core performs.
    /// These are the two halves of the seam, and nothing else checks that they
    /// agree until an enrollment is attempted for real.
    #[test]
    fn the_request_verifies_the_way_core_will_verify_it() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        let req = build_request(&id, &cfg());

        let b64 = base64::engine::general_purpose::STANDARD;
        let key: [u8; 32] = b64.decode(&req.public_key).unwrap().try_into().unwrap();
        let sig: [u8; 64] = b64.decode(&req.signature).unwrap().try_into().unwrap();

        assert!(VerifyingKey::from_bytes(&key)
            .unwrap()
            .verify(
                req.signing_payload().as_bytes(),
                &Signature::from_bytes(&sig)
            )
            .is_ok());
    }

    #[test]
    fn the_runtime_id_matches_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(build_request(&id, &cfg()).runtime_id, id.runtime_id);
    }
}
