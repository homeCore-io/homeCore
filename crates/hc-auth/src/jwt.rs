//! JWT issuance and validation (HS256).

use crate::actor::Actor;
use crate::user::Role;
use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Claims embedded in a HomeCore JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Username (subject).
    pub sub: String,
    /// User UUID.
    pub uid: String,
    /// Expiry (Unix timestamp seconds).
    pub exp: u64,
    /// User role.
    pub role: Role,
    /// Scopes granted by the role.
    pub scopes: Vec<String>,
    /// Who is making this request. `None` on wire = old token pre-dating
    /// the Actor refactor; `Claims::ensure_actor` synthesises a `User`
    /// variant from `uid`/`sub` on first access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    /// The `User::token_version` that was current when this token was minted.
    /// The auth middleware compares it against the stored value and rejects a
    /// mismatch, which is what makes a password change invalidate live
    /// sessions.
    ///
    /// `serde(default)` so tokens issued before this field existed decode as
    /// version 0 and keep working against a user record still at version 0.
    /// The first password change bumps the record past them and they die.
    ///
    /// Meaningless for synthetic claims (API key, UDS, whitelist) — those are
    /// not backed by a password, so the middleware skips the check for them.
    #[serde(default)]
    pub tv: u64,
}

impl Claims {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Return the Actor for this token, synthesising a `User` from `uid`/`sub`
    /// if the token predates the Actor refactor. Tokens issued after Phase A
    /// always include the field natively.
    pub fn actor(&self) -> Actor {
        if let Some(a) = &self.actor {
            return a.clone();
        }
        let uid = Uuid::parse_str(&self.uid).unwrap_or(Uuid::nil());
        Actor::User {
            uid,
            username: self.sub.clone(),
        }
    }
}

/// JWT service using HS256 (symmetric HMAC-SHA256).
pub struct JwtService {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    pub(crate) expiry_hours: u64,
}

impl JwtService {
    pub fn new_hs256(secret: impl AsRef<[u8]>, expiry_hours: u64) -> Self {
        let s = secret.as_ref();
        Self {
            encoding_key: EncodingKey::from_secret(s),
            decoding_key: DecodingKey::from_secret(s),
            expiry_hours,
        }
    }

    /// Return the configured token expiry duration in hours.
    pub fn expiry_hours(&self) -> u64 {
        self.expiry_hours
    }

    /// Issue a JWT for the given user ID, username, and role.
    ///
    /// `token_version` must be the issuing user's current
    /// [`User::token_version`](crate::user::User::token_version). It is a
    /// required parameter rather than an optional one so that a new call site
    /// cannot quietly mint a token that outlives a password change.
    pub fn issue(
        &self,
        uid: &str,
        username: &str,
        role: Role,
        token_version: u64,
    ) -> Result<String> {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + self.expiry_hours * 3600;
        let actor_uid = Uuid::parse_str(uid).unwrap_or(Uuid::nil());
        let claims = Claims {
            sub: username.to_string(),
            uid: uid.to_string(),
            exp,
            scopes: role.scopes(),
            role,
            actor: Some(Actor::User {
                uid: actor_uid,
                username: username.to_string(),
            }),
            tv: token_version,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding_key)
            .context("JWT encoding failed")
    }

    /// Validate a JWT and return its claims.
    pub fn validate(&self, token: &str) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        let data = decode::<Claims>(token, &self.decoding_key, &validation)
            .map_err(|e| anyhow!("JWT validation failed: {e}"))?;
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn svc() -> JwtService {
        JwtService::new_hs256(b"test-secret-key-32-bytes-minimum!", 24)
    }

    #[test]
    fn issue_and_validate_admin_token() {
        let uid = Uuid::new_v4().to_string();
        let token = svc().issue(&uid, "alice", Role::Admin, 0).unwrap();
        let claims = svc().validate(&token).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.uid, uid);
        assert_eq!(claims.role, Role::Admin);
        assert!(claims.is_admin());
        assert!(claims.has_scope("users:write"));
        assert!(claims.has_scope("devices:write"));
    }

    #[test]
    fn user_role_lacks_admin_scopes() {
        let token = svc().issue("uid", "bob", Role::User, 0).unwrap();
        let claims = svc().validate(&token).unwrap();
        assert!(!claims.is_admin());
        assert!(!claims.has_scope("users:write"));
        assert!(claims.has_scope("devices:write"));
    }

    #[test]
    fn readonly_role_has_only_read_scopes() {
        let token = svc().issue("uid", "carol", Role::ReadOnly, 0).unwrap();
        let claims = svc().validate(&token).unwrap();
        assert!(!claims.is_admin());
        assert!(!claims.has_scope("devices:write"));
        assert!(claims.has_scope("devices:read"));
    }

    #[test]
    fn wrong_secret_fails_validation() {
        let token = svc().issue("uid", "alice", Role::Admin, 0).unwrap();
        let other = JwtService::new_hs256(b"completely-different-secret-here!", 24);
        assert!(other.validate(&token).is_err());
    }

    #[test]
    fn tampered_token_rejected() {
        let token = svc().issue("uid", "alice", Role::Admin, 0).unwrap();
        let tampered = format!("{token}x");
        assert!(svc().validate(&tampered).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        // Build a token whose `exp` is already in the past (Unix epoch = 1970).
        let svc = svc();
        let claims = Claims {
            sub: "alice".into(),
            uid: "uid".into(),
            exp: 1, // way in the past
            role: Role::Admin,
            scopes: Role::Admin.scopes(),
            actor: None,
            tv: 0,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret-key-32-bytes-minimum!"),
        )
        .unwrap();
        assert!(svc.validate(&token).is_err());
    }
}
