//! Remote plugin **registry** (Phase C) — a static, signed JSON index describing
//! installable plugins, hosted anywhere (self-hostable). This slice is the
//! security foundation: the index model + **detached ed25519 signature
//! verification**. Fetch (http/file), browse endpoints, and install-from-registry
//! build on it.
//!
//! Trust (R4): a single project key signs the index; core ships its public key
//! (config `[registry].public_key`, base64). The signature is **detached** —
//! over the exact bytes of `index.json` (served alongside as `index.json.sig`)
//! — so there is no JSON-canonicalization ambiguity. Each artifact also carries
//! a `sha256` and a `key_id`, so per-publisher trust is an additive change later.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One downloadable build of a plugin version, for a specific OS/arch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub os: String,
    pub arch: String,
    /// Absolute URL of the `.tar.zst` artifact.
    pub url: String,
    /// Hex sha256 of the artifact bytes.
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    /// Signing key id — single project key for now; enables per-publisher later.
    #[serde(default)]
    pub key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginVersion {
    pub version: String,
    #[serde(default)]
    pub min_core: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryPlugin {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub versions: Vec<PluginVersion>,
}

/// The signed index document (the exact bytes of `index.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryIndex {
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub plugins: Vec<RegistryPlugin>,
}

impl RegistryIndex {
    pub fn plugin(&self, id: &str) -> Option<&RegistryPlugin> {
        self.plugins.iter().find(|p| p.id == id)
    }
}

impl RegistryPlugin {
    /// Latest version (last in the list; the registry publishes in order).
    pub fn latest(&self) -> Option<&PluginVersion> {
        self.versions.last()
    }
    pub fn version(&self, v: &str) -> Option<&PluginVersion> {
        self.versions.iter().find(|pv| pv.version == v)
    }
}

impl PluginVersion {
    /// The artifact matching this host's os/arch, if any.
    pub fn artifact_for(&self, os: &str, arch: &str) -> Option<&ArtifactRef> {
        self.artifacts.iter().find(|a| a.os == os && a.arch == arch)
    }
}

/// Verify a detached ed25519 signature over `bytes`, then parse the index.
/// The whole point of the registry: nothing is trusted (and no binary is ever
/// installed) unless this passes.
pub fn verify_and_parse(
    bytes: &[u8],
    signature: &[u8],
    public_key_b64: &str,
) -> Result<RegistryIndex> {
    verify_detached(bytes, signature, public_key_b64)
        .context("verifying registry index signature")?;
    serde_json::from_slice(bytes).context("parsing registry index JSON")
}

/// Verify a detached ed25519 signature (`signature`, raw 64 bytes) over `bytes`
/// using a base64-encoded 32-byte ed25519 public key.
pub fn verify_detached(bytes: &[u8], signature: &[u8], public_key_b64: &str) -> Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64.trim())
        .context("decoding registry public key (base64)")?;
    let pk_arr: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("registry public key must be 32 bytes"))?;
    let vk = VerifyingKey::from_bytes(&pk_arr).context("invalid registry public key")?;

    let sig_arr: [u8; 64] = signature
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes"))?;
    let sig = Signature::from_bytes(&sig_arr);

    vk.verify(bytes, &sig)
        .map_err(|_| anyhow::anyhow!("signature verification failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair(seed: u8) -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pk = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        (sk, pk)
    }

    fn sample() -> Vec<u8> {
        br#"{"schema":"1","plugins":[{"id":"plugin.lutron","name":"Lutron","versions":[{"version":"1.2.0","artifacts":[{"os":"linux","arch":"x86_64","url":"https://x/hc-lutron.tar.zst","sha256":"ab"}]}]}]}"#.to_vec()
    }

    #[test]
    fn verify_and_parse_accepts_a_valid_signature() {
        let (sk, pk) = keypair(7);
        let bytes = sample();
        let sig = sk.sign(&bytes).to_bytes();

        let index = verify_and_parse(&bytes, &sig, &pk).unwrap();
        assert_eq!(index.schema, "1");
        let p = index.plugin("plugin.lutron").unwrap();
        let v = p.latest().unwrap();
        assert_eq!(v.version, "1.2.0");
        assert!(v.artifact_for("linux", "x86_64").is_some());
        assert!(v.artifact_for("linux", "aarch64").is_none());
    }

    #[test]
    fn verify_rejects_tampered_bytes() {
        let (sk, pk) = keypair(7);
        let bytes = sample();
        let sig = sk.sign(&bytes).to_bytes();
        let mut tampered = bytes.clone();
        tampered[10] ^= 0xff;
        assert!(verify_and_parse(&tampered, &sig, &pk).is_err());
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let (sk, _) = keypair(7);
        let bytes = sample();
        let sig = sk.sign(&bytes).to_bytes();
        let (_, other_pk) = keypair(9);
        assert!(verify_and_parse(&bytes, &sig, &other_pk).is_err());
    }
}
