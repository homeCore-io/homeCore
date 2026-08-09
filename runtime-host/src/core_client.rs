//! Asking core what this runtime should be running, and fetching it.
//!
//! Core holds the desired state; the runtime pulls it. Pull rather than push
//! because the container is the side that may vanish and come back on a
//! different address — a runtime that lost its volume re-enrolls and asks again,
//! and core does not need to have noticed anything.
//!
//! Both calls authenticate with the runtime's own API key, which identifies
//! exactly one runtime. There is no request here that could name someone else's.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// One plugin core says belongs on this runtime.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Desired {
    pub plugin_id: String,
    pub version: String,
    /// Hex sha256 of the artifact bytes, as core verified them.
    pub sha256: String,
    /// The plugin's config, already rendered by core, including its minted
    /// broker credential. Written verbatim — the runtime does not interpret it.
    pub config: String,
}

#[derive(Debug, Deserialize)]
struct PlacementsBody {
    #[serde(default)]
    placements: Vec<Desired>,
}

pub struct CoreClient {
    http: reqwest::Client,
    base_url: String,
    runtime_id: String,
    api_key: String,
}

impl CoreClient {
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        runtime_id: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            runtime_id: runtime_id.into(),
            api_key: api_key.into(),
        }
    }

    /// What core says should be running here.
    pub async fn placements(&self) -> Result<Vec<Desired>> {
        let url = format!(
            "{}/api/v1/plugin-runtimes/{}/placements",
            self.base_url, self.runtime_id
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .context("asking core for placements")?;

        // A 404 here is core saying it does not know this runtime — the record
        // was denied or removed while the container kept running. Worth its own
        // message: the generic "unexpected status" reads like a bug in the
        // endpoint rather than a decision an administrator made.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!(
                "homeCore does not recognise this runtime — it may have been removed. \
                 Delete the identity file and restart to enroll again."
            );
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("core answered {status} for placements: {body}");
        }
        Ok(resp
            .json::<PlacementsBody>()
            .await
            .context("parsing the placement list")?
            .placements)
    }

    /// Fetch one placement's artifact, verified against the digest core gave.
    ///
    /// The check is deliberately only the sha256. Index signatures are verified
    /// in core, once, rather than reimplemented in every language a runtime is
    /// written in — see `docs/pluginRuntimesPlan.md`. What this catches is a
    /// truncated or corrupted transfer, which is the failure that actually
    /// happens between two processes on the same network.
    pub async fn artifact(&self, plugin_id: &str, expected_sha256: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/api/v1/plugin-runtimes/{}/artifacts/{}",
            self.base_url, self.runtime_id, plugin_id
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .with_context(|| format!("fetching the artifact for {plugin_id}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("core answered {status} for {plugin_id}'s artifact: {body}");
        }
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading {plugin_id}'s artifact"))?
            .to_vec();

        verify_sha256(&bytes, expected_sha256).with_context(|| {
            format!(
                "verifying {plugin_id} {}",
                &expected_sha256[..8.min(expected_sha256.len())]
            )
        })?;
        Ok(bytes)
    }
}

/// Compare the bytes against a hex digest, case-insensitively.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        bail!(
            "artifact digest mismatch: expected {expected}, received {actual} ({} bytes) — \
             the transfer was corrupted or truncated",
            bytes.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_digest_passes() {
        // sha256("hello")
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(b"hello", expected).is_ok());
    }

    /// Digests are written both ways by different tools; a case difference is
    /// not a corrupted download and must not be reported as one.
    #[test]
    fn case_does_not_matter() {
        let expected = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(b"hello", expected).is_ok());
    }

    /// The message has to say what happened, because the operator's next
    /// question is whether to retry or to look at the registry.
    #[test]
    fn a_mismatch_says_both_digests_and_the_size() {
        let err = verify_sha256(b"hello", &"a".repeat(64)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2cf24dba"), "names what arrived: {msg}");
        assert!(msg.contains("aaaa"), "names what was expected: {msg}");
        assert!(
            msg.contains("5 bytes"),
            "size makes truncation obvious: {msg}"
        );
    }

    /// A truncated body is the realistic failure — the first bytes match and
    /// nothing else notices.
    #[test]
    fn a_truncated_body_is_caught() {
        let full = b"the whole artifact";
        let digest = hex::encode(Sha256::digest(full));
        assert!(verify_sha256(&full[..5], &digest).is_err());
    }

    /// Trailing whitespace round-trips through headers and files easily enough
    /// that it should not read as corruption.
    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let expected = "  2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\n";
        assert!(verify_sha256(b"hello", expected).is_ok());
    }

    #[test]
    fn the_base_url_keeps_no_trailing_slash() {
        let c = CoreClient::new(
            reqwest::Client::new(),
            "http://core:8080/",
            "rt-a",
            "hc_sk_x",
        );
        assert_eq!(c.base_url, "http://core:8080");
    }

    #[test]
    fn placements_parse_from_cores_shape() {
        let body: PlacementsBody = serde_json::from_value(serde_json::json!({
            "placements": [{
                "plugin_id": "plugin.foo",
                "version": "0.2.1",
                "sha256": "abc",
                "config": "id = \"plugin.foo\"\n",
                "placed_at": "2026-08-08T00:00:00Z",
            }]
        }))
        .expect("core's placement shape");
        assert_eq!(body.placements[0].plugin_id, "plugin.foo");
        assert_eq!(body.placements[0].version, "0.2.1");
    }

    /// A runtime with nothing on it yet gets an empty list, not an error.
    #[test]
    fn an_empty_list_parses() {
        let body: PlacementsBody =
            serde_json::from_value(serde_json::json!({ "placements": [] })).unwrap();
        assert!(body.placements.is_empty());
    }
}
