//! The runtime's durable identity.
//!
//! An ed25519 keypair generated on first start and persisted to the container's
//! volume. It is the one piece of state the host genuinely owns: credentials can
//! be re-obtained by enrolling again, and installed plugins are a cache of what
//! core already knows, but an identity that cannot prove continuity is a new
//! runtime.
//!
//! The `runtime_id` is derived from the public key rather than generated
//! separately, so the two cannot disagree. An id that appears in a log is inert
//! on its own — core refuses an enrollment that presents a known id with a
//! different key.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use std::path::{Path, PathBuf};

/// Filename inside the data directory.
const KEY_FILE: &str = "identity.key";

pub struct Identity {
    signing: SigningKey,
    pub runtime_id: String,
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl Identity {
    /// Load the identity from `dir`, generating and persisting one if absent.
    ///
    /// The key file is written `0600` where the platform supports it. It is the
    /// only secret the host stores, and anyone who can read it can impersonate
    /// this runtime to core.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        if path.exists() {
            let encoded = std::fs::read_to_string(&path)
                .with_context(|| format!("reading identity at {}", path.display()))?;
            return Self::from_encoded(encoded.trim());
        }

        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data directory {}", dir.display()))?;
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
        let encoded = b64().encode(seed);
        write_private(&path, &encoded)?;
        tracing::info!(path = %path.display(), "generated a new runtime identity");
        Self::from_encoded(&encoded)
    }

    fn from_encoded(encoded: &str) -> Result<Self> {
        let raw = b64()
            .decode(encoded)
            .context("identity file is not valid base64")?;
        let seed: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("identity file must hold a 32-byte key"))?;
        let signing = SigningKey::from_bytes(&seed);
        let runtime_id = derive_runtime_id(&signing);
        Ok(Self {
            signing,
            runtime_id,
        })
    }

    pub fn public_key_b64(&self) -> String {
        b64().encode(self.signing.verifying_key().to_bytes())
    }

    pub fn sign_b64(&self, payload: &str) -> String {
        b64().encode(self.signing.sign(payload.as_bytes()).to_bytes())
    }
}

/// `rt-` plus the first 16 hex characters of the public key.
///
/// Derived rather than random so an identity file is the whole identity — there
/// is no second value to lose, and a restored key always yields the same id.
fn derive_runtime_id(signing: &SigningKey) -> String {
    let bytes = signing.verifying_key().to_bytes();
    let hex: String = bytes
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    format!("rt-{hex}")
}

#[cfg(unix)]
fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating identity file {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("creating identity file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the whole design leans on: restarting a container with its
    /// volume intact is the *same* runtime, so it does not need re-approving.
    #[test]
    fn an_identity_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path()).unwrap();
        let second = Identity::load_or_create(dir.path()).unwrap();

        assert_eq!(first.runtime_id, second.runtime_id);
        assert_eq!(first.public_key_b64(), second.public_key_b64());
    }

    /// ...and losing the volume makes it a new runtime, which is correct rather
    /// than unfortunate: continuity it cannot prove is continuity it does not have.
    #[test]
    fn a_lost_volume_is_a_new_runtime() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            Identity::load_or_create(a.path()).unwrap().runtime_id,
            Identity::load_or_create(b.path()).unwrap().runtime_id
        );
    }

    #[test]
    fn the_id_is_derived_from_the_key_not_stored_separately() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        // Only the key file exists — no second file holding an id that could
        // disagree with it.
        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec![KEY_FILE.to_string()]);
        assert!(id.runtime_id.starts_with("rt-"), "{}", id.runtime_id);
    }

    /// Signatures must verify with the key core is given, or every enrollment
    /// fails at the far end with nothing local to look at.
    #[test]
    fn signatures_verify_against_the_published_public_key() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();

        let sig_b64 = id.sign_b64("hello");
        let key_bytes: [u8; 32] = b64()
            .decode(id.public_key_b64())
            .unwrap()
            .try_into()
            .unwrap();
        let sig_bytes: [u8; 64] = b64().decode(sig_b64).unwrap().try_into().unwrap();

        let vk = VerifyingKey::from_bytes(&key_bytes).unwrap();
        assert!(vk
            .verify(b"hello", &Signature::from_bytes(&sig_bytes))
            .is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "identity is the one secret the host stores");
    }
}
