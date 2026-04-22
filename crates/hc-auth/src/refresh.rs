//! Refresh-token generation, hashing, and verification.
//!
//! Refresh tokens are opaque 32-byte secrets prefixed with `hc_rt_`
//! (reserved in the API-key namespace), argon2id-hashed server-side with
//! the same light params as API keys. They are long-lived (configurable,
//! default 30 days), single-use, and rotating — each successful refresh
//! issues a new one, marks the old one as used, and links them via a
//! parent_id chain so token-theft reuse can be detected.

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand_core::{OsRng, RngCore};

/// Plaintext token prefix for refresh tokens.
pub const REFRESH_TOKEN_PREFIX: &str = "hc_rt_";

/// Length of the random portion (bytes) before base64 encoding.
const REFRESH_BYTES: usize = 32;

/// Number of characters of the base64 body used as an indexed lookup
/// prefix. Same collision budget as API keys (~2^-72).
pub const LOOKUP_PREFIX_LEN: usize = 12;

/// A freshly minted refresh token.
pub struct NewRefreshToken {
    /// Full `hc_rt_<body>` string. Shown to the caller once.
    pub full_token: String,
    /// Lookup prefix (first 12 chars of the base64 body).
    pub lookup_prefix: String,
    /// Argon2id hash of the full token. This is what the server persists.
    pub hash: String,
}

pub fn generate() -> Result<NewRefreshToken> {
    let mut raw = [0u8; REFRESH_BYTES];
    OsRng.fill_bytes(&mut raw);
    let body = URL_SAFE_NO_PAD.encode(raw);
    let full_token = format!("{REFRESH_TOKEN_PREFIX}{body}");
    let lookup_prefix = body[..LOOKUP_PREFIX_LEN].to_string();
    let hash = hash_token(&full_token)?;
    Ok(NewRefreshToken {
        full_token,
        lookup_prefix,
        hash,
    })
}

pub fn lookup_prefix_from_body(body: &str) -> Option<&str> {
    if body.len() < LOOKUP_PREFIX_LEN {
        return None;
    }
    Some(&body[..LOOKUP_PREFIX_LEN])
}

pub fn hash_token(full_token: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(19_456, 1, 1, None)
        .map_err(|e| anyhow!("Argon2 params error: {e}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2
        .hash_password(full_token.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("Argon2 hash error: {e}"))
}

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
        assert!(k.full_token.starts_with(REFRESH_TOKEN_PREFIX));
        assert_eq!(k.lookup_prefix.len(), LOOKUP_PREFIX_LEN);
    }

    #[test]
    fn verify_roundtrip() {
        let k = generate().unwrap();
        assert!(verify_token(&k.full_token, &k.hash));
        assert!(!verify_token(&format!("{}x", k.full_token), &k.hash));
    }

    #[test]
    fn prefix_extracted_from_body() {
        let k = generate().unwrap();
        let body = k.full_token.strip_prefix(REFRESH_TOKEN_PREFIX).unwrap();
        assert_eq!(lookup_prefix_from_body(body), Some(k.lookup_prefix.as_str()));
    }
}
