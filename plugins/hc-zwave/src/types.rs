//! zwave-js-server WebSocket protocol types.
//!
//! The server sends JSON frames with a top-level `"type"` discriminator:
//!   - `"version"`  — initial handshake, sent on connect
//!   - `"result"`   — response to commands we send
//!   - `"event"`    — ongoing node/controller events

use serde::{Deserialize, Deserializer};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Top-level incoming message
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Version(VersionMsg),
    Result(ResultMsg),
    Event(EventWrapper),
}

// ---------------------------------------------------------------------------
// Version (first message on connect)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct VersionMsg {
    #[serde(rename = "driverVersion")]
    pub driver_version: String,
    #[serde(rename = "serverVersion")]
    pub server_version: String,
    #[serde(rename = "homeId")]
    #[allow(dead_code)]
    pub home_id: Option<u64>,
    #[serde(rename = "minSchemaVersion")]
    pub min_schema_version: u32,
    #[serde(rename = "maxSchemaVersion")]
    pub max_schema_version: u32,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ResultMsg {
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub success: bool,
    pub result: Option<Value>,
    #[serde(rename = "errorCode")]
    pub error_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EventWrapper {
    pub event: RawEvent,
}

/// Raw event — we keep extra fields as a flattened Value for flexibility
/// since zwave-js-server event shapes vary by event name.
#[derive(Debug, Deserialize)]
pub struct RawEvent {
    #[allow(dead_code)]
    pub source: String,
    /// The event name, e.g. "value updated", "node status changed".
    pub event: String,
    #[serde(rename = "nodeId")]
    pub node_id: Option<u32>,
    /// Present on "value updated", "value added".
    pub args: Option<Value>,
    /// Present on "node ready".
    #[serde(rename = "nodeState")]
    pub node_state: Option<Value>,
    /// Present on "node name updated".
    pub name: Option<String>,
    /// Present on "node location updated".
    pub location: Option<String>,
}

// ---------------------------------------------------------------------------
// Value updated args
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ValueUpdatedArgs {
    #[serde(rename = "commandClass")]
    pub command_class: u32,
    pub endpoint: u32,
    pub property: String,
    /// Can be null, a number, or a string depending on the CC.
    #[serde(rename = "propertyKey")]
    pub property_key: Option<Value>,
    #[serde(rename = "newValue")]
    pub new_value: Value,
}

// ---------------------------------------------------------------------------
// Node state (from start_listening result and node ready event)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NodeState {
    #[serde(rename = "nodeId")]
    pub node_id: u32,
    pub name: Option<String>,
    pub location: Option<String>,
    /// 1=Asleep, 2=Awake, 3=Dead, 4=Alive, 5=Unknown
    pub status: Option<u8>,
    /// Mains-powered (always-listening) flag from zwave-js. False for any
    /// battery device (sleeping or FLiRS).
    #[serde(rename = "isListening", default)]
    pub is_listening: Option<bool>,
    /// FLiRS (Frequently Listening Routing Slave) — battery-powered but
    /// wakes ~250ms/1000ms to accept commands. Door locks are typical.
    /// zwave-js encodes this as `false | "1000ms" | "250ms"`, so accept
    /// the raw JSON and treat any non-false/non-null value as FLiRS.
    #[serde(rename = "isFrequentListening", default)]
    pub is_frequent_listening: Option<Value>,
    /// What the node is, as zwave-js reports it after the interview.
    ///
    /// Absent on a freshly-included node and present once it has been
    /// interviewed, which is exactly the case homeCore's registration handles:
    /// a missing field leaves what core already stored alone, so registering
    /// twice — once bare, once informed — fills these in without a special path.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// zwave-js calls the model `label`.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "firmwareVersion", default)]
    pub firmware_version: Option<String>,
    /// Values may be absent on freshly-included nodes or those still interviewing.
    #[serde(default)]
    pub values: Vec<NodeValue>,
}

/// One value entry from a node's value list.
#[derive(Debug, Deserialize)]
pub struct NodeValue {
    #[serde(rename = "commandClass")]
    pub command_class: u32,
    pub endpoint: u32,
    /// zwave-js can send `property` as either a string or an integer (e.g. 37).
    #[serde(deserialize_with = "de_string_or_num")]
    pub property: String,
    #[serde(rename = "propertyKey")]
    pub property_key: Option<Value>,
    pub value: Option<Value>,
}

impl NodeState {
    /// Parse from the raw JSON returned by start_listening or nodeState.
    /// Logs a warning and returns None if the node itself can't be parsed,
    /// but individual bad values are already filtered by NodeValue's deserializer.
    pub fn from_value(v: &Value) -> Option<Self> {
        match serde_json::from_value(v.clone()) {
            Ok(n) => Some(n),
            Err(e) => {
                let node_id = v.get("nodeId").and_then(|n| n.as_u64()).unwrap_or(0);
                tracing::warn!(node_id, error = %e, "Failed to parse NodeState — node skipped");
                None
            }
        }
    }

    /// Whether the node is reachable.
    /// NodeStatus: 0=Unknown, 1=Asleep, 2=Awake, 3=Dead, 4=Alive.
    /// Asleep is normal for battery devices — they wake periodically and are
    /// still functional.  Only Dead (3) means the node cannot be reached.
    pub fn is_available(&self) -> bool {
        !matches!(self.status, Some(3))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deserialize a field that zwave-js may send as either a JSON string or a
/// JSON number (e.g. the `property` field on some command classes).
fn de_string_or_num<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error, Visitor};
    struct StrOrNum;
    impl<'de> Visitor<'de> for StrOrNum {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or integer")
        }
        fn visit_str<E: Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_owned())
        }
        fn visit_string<E: Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_u64<E: Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_f64<E: Error>(self, v: f64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_bool<E: Error>(self, v: bool) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_none<E: Error>(self) -> Result<String, E> {
            Ok(String::new())
        }
        fn visit_unit<E: Error>(self) -> Result<String, E> {
            Ok(String::new())
        }
    }
    d.deserialize_any(StrOrNum)
}
