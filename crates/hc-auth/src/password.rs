//! Argon2id password hashing and verification.
//!
//! Uses Argon2id (PHC winner) with recommended parameters:
//! - memory: 64 MiB
//! - iterations: 3
//! - parallelism: 4

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use rand_core::OsRng;

/// Hash a plaintext password using Argon2id with a random salt.
/// This is CPU-intensive and should be called from `spawn_blocking`.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(65536, 3, 4, None).map_err(|e| anyhow!("Argon2 params error: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("Argon2 hash error: {e}"))
}

/// Verify a plaintext password against a stored Argon2id hash.
/// Returns `true` if the password matches.
/// This is CPU-intensive and should be called from `spawn_blocking`.
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Argon2id params so tests run fast.
    fn hash_fast(password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        let params = Params::new(8192, 1, 1, None).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        argon2
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    #[test]
    fn correct_password_verifies() {
        let hash = hash_fast("hunter2");
        assert!(verify_password("hunter2", &hash));
    }

    #[test]
    fn wrong_password_rejected() {
        let hash = hash_fast("hunter2");
        assert!(!verify_password("wrong", &hash));
    }

    #[test]
    fn empty_password_has_its_own_hash() {
        let hash = hash_fast("");
        assert!(verify_password("", &hash));
        assert!(!verify_password("notempty", &hash));
    }

    #[test]
    fn garbage_hash_returns_false() {
        assert!(!verify_password("anything", "not-a-valid-argon2-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn each_hash_is_unique_due_to_random_salt() {
        let h1 = hash_fast("samepassword");
        let h2 = hash_fast("samepassword");
        assert_ne!(h1, h2, "two hashes of the same password should differ");
        assert!(verify_password("samepassword", &h1));
        assert!(verify_password("samepassword", &h2));
    }
}
