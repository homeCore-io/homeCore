//! User model and role definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Access role assigned to a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Full system access including user management.
    Admin,
    /// Read/write access to devices, automations, scenes.
    User,
    /// Read-only access to device state and events.
    ReadOnly,
}

impl Role {
    /// Returns the set of JWT scopes granted to this role.
    pub fn scopes(&self) -> Vec<String> {
        match self {
            Role::Admin => vec![
                "devices:read".into(),
                "devices:write".into(),
                "automations:read".into(),
                "automations:write".into(),
                "dashboards:read".into(),
                "dashboards:write".into(),
                "scenes:read".into(),
                "scenes:write".into(),
                "areas:read".into(),
                "areas:write".into(),
                "users:read".into(),
                "users:write".into(),
                "plugins:read".into(),
                "plugins:write".into(),
            ],
            Role::User => vec![
                "devices:read".into(),
                "devices:write".into(),
                "automations:read".into(),
                "automations:write".into(),
                "dashboards:read".into(),
                "dashboards:write".into(),
                "scenes:read".into(),
                "scenes:write".into(),
                "areas:read".into(),
            ],
            Role::ReadOnly => vec![
                "devices:read".into(),
                "automations:read".into(),
                "dashboards:read".into(),
                "scenes:read".into(),
                "areas:read".into(),
            ],
        }
    }
}

/// A HomeCore user account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    /// Argon2id hash of the plaintext password.
    pub password_hash: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

/// Public-facing user record (no password hash).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: Uuid,
    pub username: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

impl From<&User> for UserInfo {
    fn from(u: &User) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            role: u.role,
            created_at: u.created_at,
        }
    }
}
