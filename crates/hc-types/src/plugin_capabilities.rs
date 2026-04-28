//! Plugin capability manifest — typed shape of
//! `homecore/plugins/{id}/capabilities` (retained JSON).
//!
//! Plugins publish this once per session right after CONNACK; core caches it
//! in `PluginRecord` and exposes it at `GET /api/v1/plugins/:id/capabilities`.
//! The UI + hc-mcp read it to render/expose plugin-specific actions without
//! plugin-specific code.
//!
//! Spec: see `pluginCapabilitiesPlan.md` §2. `spec = "1"` is frozen; any
//! breaking change bumps the string.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Top-level manifest published on `homecore/plugins/{id}/capabilities`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Manifest schema version. v1 is `"1"`.
    pub spec: String,
    /// Must match the MQTT client id.
    pub plugin_id: String,
    /// May be empty.
    #[serde(default)]
    pub actions: Vec<Action>,
}

/// One plugin-declared action, non-streaming or streaming.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-schema-style param map. Subset keywords only — see spec §2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Advisory shape of the `complete.data` payload (streaming) or the
    /// sync reply (non-streaming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub cancelable: bool,
    #[serde(default)]
    pub concurrency: Concurrency,
    /// Names the dedup key inside `item.data` when the action emits `item`
    /// events. Required when `item_operations` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_operations: Option<Vec<ItemOp>>,
    #[serde(default)]
    pub requires_role: RequiresRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// Action may run concurrently with itself.
    #[default]
    Multi,
    /// Second invocation rejected with `busy` + active `request_id`.
    Single,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemOp {
    Add,
    Update,
    Remove,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiresRole {
    Admin,
    #[default]
    User,
    ReadOnly,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_manifest_roundtrip() {
        let m = Capabilities {
            spec: "1".into(),
            plugin_id: "hc-foo".into(),
            actions: vec![Action {
                id: "rescan".into(),
                label: "Rescan".into(),
                description: None,
                params: None,
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::default(),
                timeout_ms: None,
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Capabilities = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
        // Defaults must omit noise from the wire form.
        assert!(!s.contains("\"description\""));
        assert!(!s.contains("\"params\""));
        assert!(!s.contains("\"result\""));
        assert!(!s.contains("\"item_key\""));
        assert!(!s.contains("\"item_operations\""));
        assert!(!s.contains("\"timeout_ms\""));
    }

    #[test]
    fn streaming_manifest_roundtrip() {
        let raw = json!({
            "spec": "1",
            "plugin_id": "hc-zwave",
            "actions": [{
                "id": "include_node",
                "label": "Include Z-Wave device",
                "params": { "secure": { "type": "boolean", "default": true } },
                "result": { "nodes_added": { "type": "array" } },
                "stream": true,
                "cancelable": true,
                "concurrency": "single",
                "item_key": "node_id",
                "item_operations": ["add", "update"],
                "requires_role": "admin",
                "timeout_ms": 60000
            }]
        });
        let m: Capabilities = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(m.actions.len(), 1);
        let a = &m.actions[0];
        assert_eq!(a.concurrency, Concurrency::Single);
        assert_eq!(a.requires_role, RequiresRole::Admin);
        assert_eq!(
            a.item_operations.as_deref(),
            Some(&[ItemOp::Add, ItemOp::Update][..])
        );
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn defaults_apply_on_missing_fields() {
        let m: Capabilities = serde_json::from_str(
            r#"{"spec":"1","plugin_id":"x","actions":[{"id":"a","label":"A"}]}"#,
        )
        .unwrap();
        let a = &m.actions[0];
        assert!(!a.stream);
        assert!(!a.cancelable);
        assert_eq!(a.concurrency, Concurrency::Multi);
        assert_eq!(a.requires_role, RequiresRole::User);
    }
}
