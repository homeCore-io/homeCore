//! Persistent JWT HS256 signing secret.
//!
//! Loads a 32-byte secret from a file on startup, or generates and stores
//! one if the file does not yet exist. Using a persistent secret means that
//! issued JWTs survive core restarts — without it, every restart invalidates
//! every session.
//!
//! The file is written with mode 0600 (owner read/write only). If the file
//! exists but has the wrong length or is unreadable, startup aborts rather
//! than silently regenerating — a silent regeneration would invalidate
//! every session without operator awareness.

use anyhow::{anyhow, Context, Result};
use rand::{rngs::OsRng, RngCore};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Length of the HS256 secret in bytes.
const SECRET_BYTES: usize = 32;

/// Resolve the default path for the JWT secret file given the state DB path.
/// Places the secret alongside `state.redb` so operators who back up state
/// also back up the signing secret.
pub fn default_secret_path(state_db_path: &Path) -> PathBuf {
    state_db_path
        .parent()
        .map(|d| d.join("jwt_secret"))
        .unwrap_or_else(|| PathBuf::from("jwt_secret"))
}

/// Load the JWT signing secret, or create + persist a fresh one if the file
/// does not yet exist. Returns the raw bytes.
///
/// Precedence:
/// 1. `inline_secret` — if `Some`, use it and emit a deprecation warning.
///    This covers the legacy `[auth].jwt_secret = "..."` inline config path.
/// 2. Otherwise, read `file_path`. If the file exists, it must contain
///    exactly `SECRET_BYTES` bytes (raw, not hex/base64). If it does not
///    exist, generate a fresh 32-byte secret and write it with 0600 perms.
pub fn load_or_create(
    inline_secret: Option<&str>,
    file_path: &Path,
) -> Result<Vec<u8>> {
    if let Some(s) = inline_secret {
        tracing::warn!(
            "[auth].jwt_secret is set inline in config. This is deprecated — \
             remove it to let the core manage jwt_secret_file automatically."
        );
        return Ok(s.as_bytes().to_vec());
    }

    match fs::metadata(file_path) {
        Ok(meta) if meta.is_file() => {
            let bytes = fs::read(file_path)
                .with_context(|| format!("reading {}", file_path.display()))?;
            if bytes.len() != SECRET_BYTES {
                return Err(anyhow!(
                    "jwt_secret_file {} has {} bytes, expected {}. Delete it \
                     to regenerate (will invalidate existing sessions), or \
                     restore from backup.",
                    file_path.display(),
                    bytes.len(),
                    SECRET_BYTES
                ));
            }
            tracing::debug!(
                path = %file_path.display(),
                "Loaded persistent JWT signing secret"
            );
            Ok(bytes)
        }
        Ok(_) => Err(anyhow!(
            "jwt_secret_file path {} exists but is not a regular file",
            file_path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create(file_path)
        }
        Err(e) => Err(e).with_context(|| {
            format!("checking jwt_secret_file {}", file_path.display())
        }),
    }
}

fn create(file_path: &Path) -> Result<Vec<u8>> {
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", file_path.display()))?;
    }

    let mut bytes = vec![0u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);

    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file_path)
        .with_context(|| format!("creating {}", file_path.display()))?;
    f.write_all(&bytes)
        .with_context(|| format!("writing {}", file_path.display()))?;
    f.sync_all()
        .with_context(|| format!("syncing {}", file_path.display()))?;

    tracing::info!(
        path = %file_path.display(),
        "Generated and persisted new JWT signing secret (0600)"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn creates_file_when_missing_with_0600() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state").join("jwt_secret");
        let bytes = load_or_create(None, &path).unwrap();
        assert_eq!(bytes.len(), SECRET_BYTES);

        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn reads_existing_file_unchanged() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("jwt_secret");
        let first = load_or_create(None, &path).unwrap();
        let second = load_or_create(None, &path).unwrap();
        assert_eq!(first, second, "same file must yield the same secret");
    }

    #[test]
    fn rejects_wrong_length_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("jwt_secret");
        fs::write(&path, b"too short").unwrap();
        let err = load_or_create(None, &path).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expected 32"),
            "error should mention expected length, got: {msg}"
        );
    }

    #[test]
    fn inline_secret_takes_precedence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("jwt_secret");
        // File doesn't exist; inline should be used without touching disk.
        let bytes = load_or_create(Some("shared-secret-value"), &path).unwrap();
        assert_eq!(bytes, b"shared-secret-value");
        assert!(
            !path.exists(),
            "inline secret must not trigger file creation"
        );
    }

    #[test]
    fn default_path_derived_from_state_db() {
        let state_db = Path::new("/var/lib/homecore/state.redb");
        assert_eq!(
            default_secret_path(state_db),
            PathBuf::from("/var/lib/homecore/jwt_secret")
        );
    }

    #[test]
    fn default_path_falls_back_when_parent_missing() {
        let state_db = Path::new("state.redb");
        // No parent — should still produce a usable path.
        assert_eq!(default_secret_path(state_db), PathBuf::from("jwt_secret"));
    }
}
