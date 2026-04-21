//! API key generation, storage format, and verification.
//!
//! Tokens are 32 random bytes, base64url-encoded, prefixed with `hc_sk_`.
//! The server stores only an Argon2id hash of the full token. The first
//! 12 characters of the body serve as a prefix used to look up the stored
//! record efficiently without scanning (collision probability ~72 bits).
//!
//! Hashing uses lighter Argon2id parameters than user passwords because
//! every API-key-authenticated request re-verifies the hash, and tokens
//! are 256-bit random — brute-force resistance is not the threat model.
//!
//! **Token namespace.** The `hc_<2-char-type>_<body>` convention is
//! reserved for future token types. Current and planned types:
//!
//! | Prefix   | Type                     |
//! |----------|--------------------------|
//! | `hc_sk_` | service key (API key)    |
//! | `hc_rt_` | refresh token (Phase B)  |
//! | `hc_at_` | short-lived access token |

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand_core::{OsRng, RngCore};

/// Plaintext token prefix for API keys.
pub const API_KEY_PREFIX: &str = "hc_sk_";

/// Length of the random portion (bytes) before base64 encoding.
const KEY_BYTES: usize = 32;

/// Number of characters of the base64 body used as an indexed lookup
/// prefix. Collision probability ≈ 2^-72 per pair.
pub const LOOKUP_PREFIX_LEN: usize = 12;

/// A freshly minted API key.
pub struct NewApiKey {
    /// Full token with `hc_sk_` prefix. Shown to the caller once; do not log.
    pub full_token: String,
    /// Indexed lookup prefix (characters of the base64 body).
    pub lookup_prefix: String,
    /// Argon2id hash of the full token. This is what the server persists.
    pub hash: String,
}

/// Generate a fresh API key. Caller should persist `(lookup_prefix, hash)`
/// and return `full_token` to the user once.
pub fn generate() -> Result<NewApiKey> {
    let mut raw = [0u8; KEY_BYTES];
    OsRng.fill_bytes(&mut raw);
    let body = URL_SAFE_NO_PAD.encode(raw);
    let full_token = format!("{API_KEY_PREFIX}{body}");
    let lookup_prefix = body[..LOOKUP_PREFIX_LEN].to_string();
    let hash = hash_token(&full_token)?;
    Ok(NewApiKey {
        full_token,
        lookup_prefix,
        hash,
    })
}

/// Extract the lookup prefix from a bare token body (the portion *after*
/// the `hc_sk_` prefix, as the middleware receives it).
pub fn lookup_prefix_from_body(body: &str) -> Option<&str> {
    if body.len() < LOOKUP_PREFIX_LEN {
        return None;
    }
    Some(&body[..LOOKUP_PREFIX_LEN])
}

/// Hash a full token (`hc_sk_...`) with Argon2id using light params tuned
/// for per-request verification.
pub fn hash_token(full_token: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    // ~19 MiB, 1 iteration, parallelism 1 — roughly 5 ms verify on
    // commodity hardware. Tokens are 256-bit random, so brute force isn't
    // the threat model.
    let params = Params::new(19_456, 1, 1, None)
        .map_err(|e| anyhow!("Argon2 params error: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2
        .hash_password(full_token.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("Argon2 hash error: {e}"))
}

/// Verify a full token against a stored hash. Returns `true` if it matches.
pub fn verify_token(full_token: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(full_token.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_start_with_prefix() {
        let k = generate().unwrap();
        assert!(k.full_token.starts_with(API_KEY_PREFIX));
        assert_eq!(k.lookup_prefix.len(), LOOKUP_PREFIX_LEN);
    }

    #[test]
    fn generated_tokens_are_unique() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a.full_token, b.full_token);
        assert_ne!(a.lookup_prefix, b.lookup_prefix);
    }

    #[test]
    fn generated_token_verifies() {
        let k = generate().unwrap();
        assert!(verify_token(&k.full_token, &k.hash));
    }

    #[test]
    fn tampered_token_rejected() {
        let k = generate().unwrap();
        let tampered = format!("{}x", k.full_token);
        assert!(!verify_token(&tampered, &k.hash));
    }

    #[test]
    fn lookup_prefix_extracted_from_body() {
        let k = generate().unwrap();
        let body = k.full_token.strip_prefix(API_KEY_PREFIX).unwrap();
        assert_eq!(lookup_prefix_from_body(body), Some(k.lookup_prefix.as_str()));
    }

    #[test]
    fn short_body_has_no_prefix() {
        assert!(lookup_prefix_from_body("short").is_none());
    }

    #[test]
    fn garbage_hash_rejected() {
        let k = generate().unwrap();
        assert!(!verify_token(&k.full_token, "not-a-valid-argon2-hash"));
    }
}
