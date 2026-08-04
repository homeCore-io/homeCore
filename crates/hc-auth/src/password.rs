//! Argon2id password hashing and verification.
//!
//! Uses Argon2id (PHC winner) with recommended parameters:
//! - memory: 64 MiB
//! - iterations: 3
//! - parallelism: 4

use anyhow::{anyhow, Result};
use argon2::password_hash::Salt;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};

/// A fresh 16-byte salt (`Salt::RECOMMENDED_LENGTH`) straight from the OS.
///
/// `SaltString::generate(&mut OsRng)` does exactly this, but takes its RNG
/// through password-hash 0.5's rand_core 0.6 — and argon2 0.5.3 is still the
/// newest release, so that version is frozen until it moves. Encoding the
/// bytes ourselves keeps the salt identical and the dependency current.
pub(crate) fn random_salt() -> Result<SaltString> {
    let mut bytes = [0u8; Salt::RECOMMENDED_LENGTH];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    SaltString::encode_b64(&bytes).map_err(|e| anyhow!("salt encoding failed: {e}"))
}

/// Hash a plaintext password using Argon2id with a random salt.
/// This is CPU-intensive and should be called from `spawn_blocking`.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = random_salt()?;
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

    /// Hashes that already exist in somebody's database.
    ///
    /// Every credential this server stores — user passwords, API keys, refresh
    /// tokens — is an Argon2id PHC string, and each carries its own salt and
    /// its own cost parameters. Verification must read both out of the string
    /// rather than assume today's values, or a dependency bump or a params
    /// change silently locks every existing account out.
    ///
    /// The second fixture deliberately uses different parameters from the
    /// first (m=8192,t=1,p=1 against m=65536,t=3,p=4) — if verification were
    /// using the current constants instead of the encoded ones, exactly one of
    /// these two would pass.
    const PASSWORD: &str = "correct horse battery staple";
    const STORED_PRODUCTION_PARAMS: &str = "$argon2id$v=19$m=65536,t=3,p=4$\
        cGVwcGVyc2FsdHNhbHQ$uWgxVkMfVxJCWK1QqmHXito9jpB9GVMcQ869UuHMd0E";
    const STORED_OTHER_PARAMS: &str = "$argon2id$v=19$m=8192,t=1,p=1$\
        b3RoZXJzYWx0dmFsdWU$Zcj1J1FAN+9sZ01nCkL+J8y8zx4wu6jqjwDe7TRQkTo";

    #[test]
    fn a_hash_stored_before_this_build_still_verifies() {
        assert!(verify_password(PASSWORD, STORED_PRODUCTION_PARAMS));
        assert!(verify_password(PASSWORD, STORED_OTHER_PARAMS));
    }

    #[test]
    fn a_stored_hash_rejects_the_wrong_password() {
        assert!(!verify_password(
            "wrong horse battery staple",
            STORED_PRODUCTION_PARAMS
        ));
        assert!(!verify_password("", STORED_OTHER_PARAMS));
    }

    /// The salt comes from the OS, so two hashes of the same password differ.
    /// Cheap, but it is the property that a broken RNG would break silently —
    /// and the salt source changed from rand_core's OsRng to getrandom.
    #[test]
    fn every_hash_gets_its_own_salt() {
        let a = hash_password(PASSWORD).unwrap();
        let b = hash_password(PASSWORD).unwrap();
        assert_ne!(a, b, "identical hashes mean the salt is not random");
        assert!(verify_password(PASSWORD, &a));
        assert!(verify_password(PASSWORD, &b));
    }

    /// Minimal Argon2id params so tests run fast.
    fn hash_fast(password: &str) -> String {
        let salt = random_salt().unwrap();
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
