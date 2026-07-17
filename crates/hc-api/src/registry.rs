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

use anyhow::{anyhow, bail, Context, Result};
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

/// Hex sha256 of some bytes (artifact integrity).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// A client for a remote (or local) signed registry index.
///
/// The index is fetched and signature-checked **fresh on every call** — never
/// cached. A registry can gain a plugin or a new version at any time, and a
/// stale list that only clears on a core restart is a worse failure than a
/// cheap re-fetch: browse and install are user-initiated and infrequent.
pub struct RegistryClient {
    url: String,
    public_key: String,
    http: reqwest::Client,
}

impl RegistryClient {
    pub fn new(url: String, public_key: String) -> Self {
        Self {
            url,
            public_key,
            http: reqwest::Client::new(),
        }
    }

    /// The verified index — always fetched and signature-checked fresh, so a
    /// newly-published plugin or version is visible without a core restart.
    pub async fn index(&self) -> Result<RegistryIndex> {
        self.fetch_index().await
    }

    async fn fetch_index(&self) -> Result<RegistryIndex> {
        let bytes = fetch_bytes(&self.http, &self.url)
            .await
            .with_context(|| format!("fetching registry index {}", self.url))?;
        let sig_raw = fetch_bytes(&self.http, &format!("{}.sig", self.url))
            .await
            .context("fetching registry index signature (.sig)")?;
        // The .sig is base64 of the 64-byte detached signature.
        let sig = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            String::from_utf8_lossy(&sig_raw).trim(),
        )
        .context("decoding index signature (base64)")?;
        verify_and_parse(&bytes, &sig, &self.public_key)
    }

    /// Resolve `(artifact, resolved_version)` for `id` (latest, or a specific
    /// version) matching this host's os/arch.
    pub async fn resolve(&self, id: &str, version: Option<&str>) -> Result<(ArtifactRef, String)> {
        let idx = self.index().await?;
        let plugin = idx
            .plugin(id)
            .ok_or_else(|| anyhow!("plugin `{id}` is not in the registry"))?;
        let pv = match version {
            Some(v) => plugin
                .version(v)
                .ok_or_else(|| anyhow!("version `{v}` of `{id}` not found"))?,
            None => plugin
                .latest()
                .ok_or_else(|| anyhow!("`{id}` has no published versions"))?,
        };
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let art = pv
            .artifact_for(os, arch)
            .ok_or_else(|| anyhow!("no `{id}` {} artifact for {os}/{arch}", pv.version))?;
        Ok((art.clone(), pv.version.clone()))
    }

    /// Download an artifact to a temp file and verify its sha256. Returns the
    /// temp file (kept alive by the caller until the install reads it).
    pub async fn download_artifact(&self, art: &ArtifactRef) -> Result<tempfile::NamedTempFile> {
        let bytes = fetch_bytes(&self.http, &art.url)
            .await
            .with_context(|| format!("downloading artifact {}", art.url))?;
        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(art.sha256.trim()) {
            bail!(
                "artifact sha256 mismatch: expected {}, got {got}",
                art.sha256
            );
        }
        let mut f = tempfile::NamedTempFile::new().context("creating temp artifact file")?;
        std::io::Write::write_all(&mut f, &bytes).context("writing downloaded artifact")?;
        Ok(f)
    }
}

/// Fetch bytes from an `http(s)://` URL, a `file://` URL, or a plain local path.
async fn fetch_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).with_context(|| format!("reading {path}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = http.get(url).send().await?.error_for_status()?;
        return Ok(resp.bytes().await?.to_vec());
    }
    std::fs::read(url).with_context(|| format!("reading {url}"))
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

    #[tokio::test]
    async fn client_fetches_verifies_resolves_and_downloads() {
        let tmp = tempfile::tempdir().unwrap();

        // A real artifact + its sha256.
        let artifact_bytes = b"fake .tar.zst contents".to_vec();
        let artifact_path = tmp.path().join("hc-demo.tar.zst");
        std::fs::write(&artifact_path, &artifact_bytes).unwrap();
        let sha = sha256_hex(&artifact_bytes);

        // index.json referencing it for THIS host's os/arch, then sign it.
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let index_json = format!(
            r#"{{"schema":"1","plugins":[{{"id":"plugin.demo","name":"Demo","versions":[{{"version":"1.0.0","artifacts":[{{"os":"{os}","arch":"{arch}","url":"file://{}","sha256":"{sha}"}}]}}]}}]}}"#,
            artifact_path.display()
        );
        let index_path = tmp.path().join("index.json");
        std::fs::write(&index_path, &index_json).unwrap();
        let (sk, pk) = keypair(7);
        let sig_b64 = base64::engine::general_purpose::STANDARD
            .encode(sk.sign(index_json.as_bytes()).to_bytes());
        std::fs::write(tmp.path().join("index.json.sig"), sig_b64).unwrap();

        let client = RegistryClient::new(format!("file://{}", index_path.display()), pk);

        // browse (fetch + verify)
        let idx = client.index().await.unwrap();
        assert_eq!(idx.plugin("plugin.demo").unwrap().name, "Demo");

        // resolve for this host
        let (art, ver) = client.resolve("plugin.demo", None).await.unwrap();
        assert_eq!(ver, "1.0.0");
        assert_eq!(art.sha256, sha);

        // download + verify sha256
        let f = client.download_artifact(&art).await.unwrap();
        assert_eq!(std::fs::read(f.path()).unwrap(), artifact_bytes);

        // a wrong sha256 is rejected
        let mut bad = art.clone();
        bad.sha256 = "deadbeef".into();
        assert!(client.download_artifact(&bad).await.is_err());

        // an unknown plugin / bad version fails resolution
        assert!(client.resolve("plugin.nope", None).await.is_err());
        assert!(client.resolve("plugin.demo", Some("9.9.9")).await.is_err());
    }
}
