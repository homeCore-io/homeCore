//! The **Actor** model — who is making a request.
//!
//! Historically the codebase treated the authenticated principal as a user
//! (via `Claims.uid` / `Claims.sub`). With API keys and same-host admin
//! connections added in Phase A of the auth plan, we need a richer model
//! so the audit log, scope rules, and handlers can distinguish:
//!
//! - `User` — interactive human, authenticated via username/password JWT.
//! - `ApiKey` — long-lived service credential owned by a user.
//! - `LocalAdmin` — implicit admin from a trusted transport (UDS listener),
//!   trusted by filesystem permission rather than bearer token.
//!
//! The enum is serialised with a `type` tag for forward compatibility when
//! more variants land (e.g. refresh tokens in Phase B could populate
//! `Actor::User` with a short-lived access-token session id).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Actor {
    /// Authenticated human, via password login.
    User { uid: Uuid, username: String },
    /// Long-lived API key. `owner_uid` is the user who owns the key —
    /// audit logs key on this.
    ApiKey {
        id: Uuid,
        owner_uid: Uuid,
        label: String,
    },
    /// Request arrived via a trusted local transport (the admin UDS
    /// listener). `peer_uid` is the Unix effective UID of the caller,
    /// when the platform exposes it via `SO_PEERCRED`.
    LocalAdmin { peer_uid: Option<u32> },
}

impl Actor {
    /// Return the user identity that should be used for audit purposes.
    /// For `ApiKey`, returns the owner. For `LocalAdmin`, returns `None`.
    pub fn effective_uid(&self) -> Option<Uuid> {
        match self {
            Actor::User { uid, .. } => Some(*uid),
            Actor::ApiKey { owner_uid, .. } => Some(*owner_uid),
            Actor::LocalAdmin { .. } => None,
        }
    }

    /// Human-readable label for logs.
    pub fn audit_label(&self) -> String {
        match self {
            Actor::User { username, .. } => format!("user:{username}"),
            Actor::ApiKey { label, .. } => format!("api_key:{label}"),
            Actor::LocalAdmin {
                peer_uid: Some(uid),
            } => format!("local_admin:{uid}"),
            Actor::LocalAdmin { peer_uid: None } => "local_admin".into(),
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Actor::User { .. })
    }
    pub fn is_api_key(&self) -> bool {
        matches!(self, Actor::ApiKey { .. })
    }
    pub fn is_local_admin(&self) -> bool {
        matches!(self, Actor::LocalAdmin { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_effective_uid_is_self() {
        let uid = Uuid::new_v4();
        let a = Actor::User {
            uid,
            username: "alice".into(),
        };
        assert_eq!(a.effective_uid(), Some(uid));
    }

    #[test]
    fn api_key_effective_uid_is_owner() {
        let owner = Uuid::new_v4();
        let a = Actor::ApiKey {
            id: Uuid::new_v4(),
            owner_uid: owner,
            label: "mcp-service".into(),
        };
        assert_eq!(a.effective_uid(), Some(owner));
    }

    #[test]
    fn local_admin_effective_uid_is_none() {
        let a = Actor::LocalAdmin {
            peer_uid: Some(1000),
        };
        assert!(a.effective_uid().is_none());
    }

    #[test]
    fn audit_labels_are_distinct() {
        let u = Actor::User {
            uid: Uuid::new_v4(),
            username: "alice".into(),
        };
        let k = Actor::ApiKey {
            id: Uuid::new_v4(),
            owner_uid: Uuid::new_v4(),
            label: "bot".into(),
        };
        let a = Actor::LocalAdmin {
            peer_uid: Some(1000),
        };
        assert_eq!(u.audit_label(), "user:alice");
        assert_eq!(k.audit_label(), "api_key:bot");
        assert_eq!(a.audit_label(), "local_admin:1000");
    }

    #[test]
    fn serde_tag_round_trip() {
        let a = Actor::ApiKey {
            id: Uuid::new_v4(),
            owner_uid: Uuid::new_v4(),
            label: "x".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"type\":\"api_key\""));
        let back: Actor = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
