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

    /// Whether this caller may do `scope`.
    ///
    /// **A user's scopes are derived from their role here, not read from the
    /// token.** `issue` writes `role.scopes()` into the token, so the two agree
    /// the moment a token is minted — and disagree from the moment a release
    /// adds a scope. A session issued before `skins:write` existed carried a
    /// list without it and got a 403 from a role that plainly grants it, with
    /// no way out but re-logging in: a 403 is not a 401, so it never reached
    /// the client's silent refresh. Deriving here means a new scope takes
    /// effect on deploy, for everyone, without a forced sign-out — which
    /// matters most for the wall panels nobody is standing in front of.
    ///
    /// **An API key is the exception, and it is the whole reason this is a
    /// match rather than one line.** A key carries a deliberately *narrowed*
    /// set, and its `role` is a decorative `Admin` placeholder (see
    /// `auth_middleware`); deriving from that would hand every key full admin.
    /// Keys keep being judged on exactly what they were granted.
    pub fn has_scope(&self, scope: &str) -> bool {
        match self.actor {
            Some(Actor::ApiKey { .. }) => self.scopes.iter().any(|s| s == scope),
            _ => self.role.scopes().iter().any(|s| s == scope),
        }
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

    /// A token this code did not produce.
    ///
    /// Built by hand from the HS256 standard — base64url(header).base64url(
    /// claims).HMAC-SHA256 — with a fixed secret, so it is what *any*
    /// conforming issuer emits, including every version of jsonwebtoken we
    /// have ever run. Sessions on a live box outlive a deploy, and an upgrade
    /// that silently stopped accepting them would log every user out with the
    /// server reporting nothing wrong.
    ///
    /// Regenerate only if the claim set changes, never to make a failure go
    /// away: a failure here means already-issued tokens have been invalidated.
    const EXTERNALLY_SIGNED_SECRET: &[u8] = b"a-fixed-test-secret-for-jwt-compat";
    const EXTERNALLY_SIGNED_TOKEN: &str = concat!(
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
        "eyJzdWIiOiJhbGljZSIsInVpZCI6IjExMTExMTExLTIyMjItMzMzMy00NDQ0LTU1NTU1",
        "NTU1NTU1NSIsImV4cCI6NDEwMjQ0NDgwMCwicm9sZSI6ImFkbWluIiwic2NvcGVzIjpb",
        "ImRldmljZXM6cmVhZCIsImRldmljZXM6d3JpdGUiXSwidHYiOjd9.",
        "EoYM5NsqoKQE511sQ9gX1IHQVaWaSthdtVwckpI3G-w"
    );

    #[test]
    fn a_token_issued_before_this_build_still_validates() {
        let svc = JwtService::new_hs256(EXTERNALLY_SIGNED_SECRET, 24);
        let claims = svc
            .validate(EXTERNALLY_SIGNED_TOKEN)
            .expect("an HS256 token from any conforming issuer must validate");

        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.uid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(claims.role, Role::Admin);
        assert_eq!(claims.tv, 7);
        assert!(claims.scopes.iter().any(|s| s == "devices:write"));
    }

    /// The bug this file's `has_scope` was changed for.
    #[test]
    fn a_user_token_minted_before_a_scope_existed_still_gets_it() {
        // Exactly the shape of a session issued by an older build: the role is
        // right, the frozen list is missing something the role now grants.
        let claims = Claims {
            sub: "admin".into(),
            uid: Uuid::nil().to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: vec!["devices:read".into()],
            actor: Some(Actor::User {
                uid: Uuid::nil(),
                username: "admin".into(),
            }),
            tv: 0,
        };
        assert!(
            claims.has_scope("skins:write"),
            "a scope added by a later release must not need a re-login"
        );
    }

    /// The reason `has_scope` branches instead of always deriving.
    #[test]
    fn an_api_key_is_never_widened_to_its_decorative_role() {
        // Middleware builds API-key claims with `role: Role::Admin` because the
        // role is unused for keys. Deriving from it would turn every key into a
        // full administrator.
        let claims = Claims {
            sub: "api_key:readonly dashboard".into(),
            uid: Uuid::nil().to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: vec!["devices:read".into()],
            actor: Some(Actor::ApiKey {
                id: Uuid::nil(),
                owner_uid: Uuid::nil(),
                label: "readonly dashboard".into(),
            }),
            tv: 0,
        };
        assert!(claims.has_scope("devices:read"), "its own scope");
        assert!(!claims.has_scope("devices:write"), "NOT its role's scopes");
        assert!(!claims.has_scope("users:write"), "NOT its role's scopes");
    }

    /// A token from before the Actor field existed reads as a user, and a user
    /// is judged on their role — which is the safe direction only because the
    /// role travels inside the signed token.
    #[test]
    fn a_legacy_token_without_an_actor_is_treated_as_a_user() {
        let claims = Claims {
            sub: "someone".into(),
            uid: Uuid::nil().to_string(),
            exp: u64::MAX,
            role: Role::ReadOnly,
            scopes: vec![],
            actor: None,
            tv: 0,
        };
        assert!(claims.has_scope("devices:read"));
        assert!(
            !claims.has_scope("devices:write"),
            "read_only stays read only"
        );
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        // The same token with its last signature character changed — proves
        // the test above passes because the signature verifies, not because
        // verification was skipped.
        let mut bad = EXTERNALLY_SIGNED_TOKEN.to_string();
        bad.pop();
        bad.push(if EXTERNALLY_SIGNED_TOKEN.ends_with('w') {
            'x'
        } else {
            'w'
        });
        let svc = JwtService::new_hs256(EXTERNALLY_SIGNED_SECRET, 24);
        assert!(svc.validate(&bad).is_err());
    }

    #[test]
    fn the_wrong_secret_is_rejected() {
        let svc = JwtService::new_hs256(b"not-the-secret-that-signed-it!!!", 24);
        assert!(svc.validate(EXTERNALLY_SIGNED_TOKEN).is_err());
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
