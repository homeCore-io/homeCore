//! User model and role definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Access role assigned to a user.
///
/// The scope model is coarse by design — a handful of curated preset roles
/// rather than user-assignable scope grants. Each role's scope set is
/// returned by [`Role::scopes`] and is hardcoded; callers cannot mint new
/// roles at runtime.
///
/// | Role               | Intended use                                         |
/// |--------------------|------------------------------------------------------|
/// | `Admin`            | Full system access, including user mgmt              |
/// | `User`             | Normal operator: command devices, edit automations   |
/// | `ReadOnly`         | Minimal read-only view (dashboards, devices, rules)  |
/// | `Observer`         | Broader read: adds plugins + audit log               |
/// | `DeviceOperator`   | Observer + command devices; no automation authoring  |
/// | `RuleEditor`       | Observer + automation/scene/area authoring           |
/// | `ServiceOperator`  | Typical service account: User + audit:read           |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Full system access including user management.
    Admin,
    /// Read/write access to devices, automations, scenes.
    User,
    /// Minimal read-only view over devices, automations, dashboards,
    /// scenes, areas. Cannot see plugins or the audit log.
    ReadOnly,
    /// Broader read view: `ReadOnly` plus `plugins:read` and `audit:read`.
    /// Good default for dashboards and observability services.
    Observer,
    /// Observer permissions plus `devices:write` — can command devices
    /// but cannot author automations, scenes, or areas. Typical shape for
    /// a physical-operator account (wall panel, kiosk) that should drive
    /// devices without touching automation logic.
    DeviceOperator,
    /// Observer permissions plus authoring automations, scenes, and areas.
    /// No device writes (can't command); no user/plugin management.
    RuleEditor,
    /// Normal `User` plus `audit:read` — typical scope envelope for a
    /// service account that needs to command devices and wants audit
    /// visibility over its own actions.
    ServiceOperator,
}

impl Role {
    /// Every role, in rough order of decreasing privilege. The client lists
    /// these so a person can see the whole ladder, not just the three the UI
    /// once hardcoded.
    pub const fn all() -> [Role; 7] {
        [
            Role::Admin,
            Role::User,
            Role::ReadOnly,
            Role::Observer,
            Role::DeviceOperator,
            Role::RuleEditor,
            Role::ServiceOperator,
        ]
    }

    /// The `snake_case` wire form (`device_operator`), matching the serde
    /// representation used everywhere else a role crosses the boundary.
    pub fn wire(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
            Role::ReadOnly => "read_only",
            Role::Observer => "observer",
            Role::DeviceOperator => "device_operator",
            Role::RuleEditor => "rule_editor",
            Role::ServiceOperator => "service_operator",
        }
    }

    /// Returns the set of JWT scopes granted to this role.
    pub fn scopes(&self) -> Vec<String> {
        // Scope primitives — grouped here so adding a new one lands in one
        // place and propagates to every role mapping below.
        let read_all: &[&str] = &[
            "devices:read",
            "automations:read",
            "dashboards:read",
            "scenes:read",
            "areas:read",
        ];
        let write_authoring: &[&str] = &[
            "automations:write",
            "dashboards:write",
            "scenes:write",
            "areas:write",
        ];
        let to_strings = |xs: &[&str]| xs.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        match self {
            Role::Admin => {
                let mut v = to_strings(read_all);
                v.push("devices:write".into());
                v.extend(to_strings(write_authoring));
                v.extend([
                    "users:read".into(),
                    "users:write".into(),
                    "plugins:read".into(),
                    "plugins:write".into(),
                    "audit:read".into(),
                    // Management of API keys owned by OTHER users. Self-scoped
                    // key operations don't need any scope (self-authentication
                    // is sufficient).
                    "api_keys:admin".into(),
                ]);
                v
            }
            Role::User => {
                let mut v = to_strings(read_all);
                v.push("devices:write".into());
                v.extend(to_strings(write_authoring));
                v
            }
            Role::ReadOnly => to_strings(read_all),
            Role::Observer => {
                let mut v = to_strings(read_all);
                v.extend(["plugins:read".into(), "audit:read".into()]);
                v
            }
            Role::DeviceOperator => {
                let mut v = to_strings(read_all);
                v.push("devices:write".into());
                v.extend(["plugins:read".into(), "audit:read".into()]);
                v
            }
            Role::RuleEditor => {
                let mut v = to_strings(read_all);
                v.extend(to_strings(write_authoring));
                v.extend(["plugins:read".into(), "audit:read".into()]);
                v
            }
            Role::ServiceOperator => {
                let mut v = to_strings(read_all);
                v.push("devices:write".into());
                v.extend(to_strings(write_authoring));
                v.push("audit:read".into());
                v
            }
        }
    }
}

#[cfg(test)]
mod role_tests {
    use super::*;

    #[test]
    fn admin_has_all_management_scopes() {
        let s = Role::Admin.scopes();
        assert!(s.contains(&"users:write".into()));
        assert!(s.contains(&"plugins:write".into()));
        assert!(s.contains(&"api_keys:admin".into()));
        assert!(s.contains(&"audit:read".into()));
    }

    #[test]
    fn user_can_command_but_not_manage() {
        let s = Role::User.scopes();
        assert!(s.contains(&"devices:write".into()));
        assert!(!s.contains(&"users:write".into()));
        assert!(!s.contains(&"plugins:write".into()));
        assert!(!s.contains(&"api_keys:admin".into()));
        assert!(!s.contains(&"audit:read".into()));
    }

    #[test]
    fn readonly_has_no_write_scopes() {
        let s = Role::ReadOnly.scopes();
        assert!(s.iter().all(|x| !x.ends_with(":write")));
        assert!(!s.contains(&"audit:read".into()));
    }

    #[test]
    fn observer_gets_audit_and_plugins_read_but_no_writes() {
        let s = Role::Observer.scopes();
        assert!(s.contains(&"audit:read".into()));
        assert!(s.contains(&"plugins:read".into()));
        assert!(s.iter().all(|x| !x.ends_with(":write")));
    }

    #[test]
    fn device_operator_can_command_but_not_author() {
        let s = Role::DeviceOperator.scopes();
        assert!(s.contains(&"devices:write".into()));
        assert!(s.contains(&"audit:read".into()));
        assert!(s.contains(&"plugins:read".into()));
        assert!(!s.contains(&"automations:write".into()));
        assert!(!s.contains(&"scenes:write".into()));
        assert!(!s.contains(&"areas:write".into()));
        assert!(!s.contains(&"users:write".into()));
        assert!(!s.contains(&"api_keys:admin".into()));
    }

    #[test]
    fn rule_editor_can_author_but_not_command_devices() {
        let s = Role::RuleEditor.scopes();
        assert!(s.contains(&"automations:write".into()));
        assert!(s.contains(&"scenes:write".into()));
        assert!(s.contains(&"areas:write".into()));
        assert!(!s.contains(&"devices:write".into()));
        assert!(!s.contains(&"users:write".into()));
    }

    #[test]
    fn service_operator_is_user_plus_audit_read() {
        let s = Role::ServiceOperator.scopes();
        assert!(s.contains(&"devices:write".into()));
        assert!(s.contains(&"audit:read".into()));
        assert!(!s.contains(&"users:write".into()));
        assert!(!s.contains(&"api_keys:admin".into()));
    }

    #[test]
    fn role_serde_roundtrip_uses_snake_case() {
        let roles = [
            Role::Admin,
            Role::User,
            Role::ReadOnly,
            Role::Observer,
            Role::DeviceOperator,
            Role::RuleEditor,
            Role::ServiceOperator,
        ];
        for r in roles {
            let s = serde_json::to_string(&r).unwrap();
            let back: Role = serde_json::from_str(&s).unwrap();
            assert_eq!(r, back);
        }
        // Specifically assert the on-wire shape for the new roles — clients
        // and older persisted JSON rely on these strings.
        assert_eq!(
            serde_json::to_string(&Role::Observer).unwrap(),
            "\"observer\""
        );
        assert_eq!(
            serde_json::to_string(&Role::DeviceOperator).unwrap(),
            "\"device_operator\""
        );
        assert_eq!(
            serde_json::to_string(&Role::RuleEditor).unwrap(),
            "\"rule_editor\""
        );
        assert_eq!(
            serde_json::to_string(&Role::ServiceOperator).unwrap(),
            "\"service_operator\""
        );
    }

    #[test]
    fn all_covers_every_variant_and_wire_matches_serde() {
        // `all()` feeds the /auth/roles endpoint; if a variant is added to the
        // enum but not to `all()`, the client silently stops offering it.
        assert_eq!(Role::all().len(), 7);
        // `wire()` must agree with the serde representation exactly, since the
        // client sends role strings back on user create / set-role.
        for r in Role::all() {
            let serde = serde_json::to_string(&r).unwrap();
            assert_eq!(format!("\"{}\"", r.wire()), serde);
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
