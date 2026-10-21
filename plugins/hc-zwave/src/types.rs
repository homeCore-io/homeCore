//! zwave-js-server WebSocket protocol types.
//!
//! The server sends JSON frames with a top-level `"type"` discriminator:
//!   - `"version"`  — initial handshake, sent on connect
//!   - `"result"`   — response to a command we sent, matched by `messageId`
//!   - `"event"`    — ongoing node/controller events
//!
//! **Parsing is tolerant, value by value.** A node used to be one serde
//! struct: a single value zwave-js shaped differently from what this file
//! expected dropped the *whole node*, and the reconcile that followed deleted
//! its device from homeCore. Now a node's header is read on its own and each
//! value separately; a value that does not parse is skipped and counted, and
//! the node stays.
//!
//! **A lock's codes never get this far.** User Code (CC 99) `userCode` values
//! are dropped as they are parsed, so nothing downstream — state, logs, the
//! status snapshot — can hold one by accident.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Version (first message on connect)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct VersionMsg {
    #[serde(rename = "driverVersion", default)]
    pub driver_version: String,
    #[serde(rename = "serverVersion", default)]
    pub server_version: String,
    #[serde(rename = "minSchemaVersion", default)]
    pub min_schema_version: u32,
    #[serde(rename = "maxSchemaVersion", default)]
    pub max_schema_version: u32,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One event frame. zwave-js-server puts an event's fields at the top level of
/// `event` — `{source: "controller", event: "node added", node: {…}, result}`,
/// `{source: "node", event: "value updated", nodeId, args}` — so the whole
/// object is kept in `body` and read by name where the shape differs.
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub source: String,
    /// The event name, e.g. "value updated", "node status changed".
    pub event: String,
    pub node_id: Option<u32>,
    /// Present on "value updated", "value added", "notification", …
    pub args: Option<Value>,
    /// Present on "ready".
    pub node_state: Option<Value>,
    /// The whole event object.
    pub body: Value,
}

impl RawEvent {
    /// The event inside an `{"type": "event", "event": {…}}` frame.
    pub fn from_value(event: Value) -> Option<Self> {
        let name = event.get("event")?.as_str()?.to_string();
        Some(Self {
            source: event
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            event: name,
            node_id: event
                .get("nodeId")
                .and_then(Value::as_u64)
                .map(|n| n as u32),
            args: event.get("args").cloned(),
            node_state: event.get("nodeState").cloned(),
            body: event,
        })
    }

    /// A top-level field of the event.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.body.get(key)
    }
}

// ---------------------------------------------------------------------------
// Value updated args
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ValueUpdatedArgs {
    #[serde(rename = "commandClass")]
    pub command_class: u32,
    #[serde(default)]
    pub endpoint: u32,
    /// A string for most command classes and a number for Configuration (CC
    /// 112) — the same as a node's values, read the same way.
    #[serde(deserialize_with = "de_string_or_num")]
    pub property: String,
    /// Can be null, a number, or a string depending on the CC.
    #[serde(rename = "propertyKey", default)]
    pub property_key: Option<Value>,
    /// `value updated` and `value added` say `newValue`; `value notification`
    /// says `value`.
    #[serde(rename = "newValue", alias = "value", default)]
    pub new_value: Value,
}

// ---------------------------------------------------------------------------
// Node state (from start_listening result and the `ready` event)
// ---------------------------------------------------------------------------

/// A node, as zwave-js dumps it. Every field but the id is optional: a node
/// still interviewing has almost none of them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NodeState {
    #[serde(rename = "nodeId")]
    pub node_id: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// 0=Unknown, 1=Asleep, 2=Awake, 3=Dead, 4=Alive.
    #[serde(default, deserialize_with = "de_opt_u8")]
    pub status: Option<u8>,
    /// Interview finished.
    #[serde(default)]
    pub ready: Option<bool>,
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
    /// (Schema 47+; older schemas carry it in `deviceConfig`.)
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// zwave-js calls the model `label`.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "firmwareVersion", default)]
    pub firmware_version: Option<String>,
    /// The device file zwave-js matched: manufacturer, label, description.
    #[serde(rename = "deviceConfig", default)]
    pub device_config: Option<Value>,
    /// What Z-Wave says the node *is*, from its interview.
    ///
    /// `generic` and `specific` are the device class pair every node carries —
    /// "Binary Switch"/"Binary Power Switch", "Entry Control"/"Secure Keypad
    /// Door Lock". Absent until the interview completes, like the fields above.
    #[serde(rename = "deviceClass", default)]
    pub device_class: Option<DeviceClass>,
    /// `"Complete"`, `"ProtocolInfo"`, … (or the enum's number on some
    /// schemas).
    #[serde(rename = "interviewStage", default)]
    pub interview_stage: Option<Value>,
    /// SecurityClass: 2 = S2 Access Control, 1 = S2 Authenticated,
    /// 0 = S2 Unauthenticated, 7 = S0, -1 = none.
    #[serde(rename = "highestSecurityClass", default)]
    pub highest_security_class: Option<i64>,
    #[serde(rename = "isSecure", default)]
    pub is_secure: Option<Value>,
    #[serde(rename = "isControllerNode", default)]
    pub is_controller_node: Option<bool>,
    /// Radio statistics (schema 7+): commands sent and received, dropped,
    /// timeouts, round trip, RSSI and the last working route.
    #[serde(default)]
    pub statistics: Option<Value>,
    /// When zwave-js last heard from it (schema 30+).
    #[serde(rename = "lastSeen", default)]
    pub last_seen: Option<Value>,
    /// Values may be absent on freshly-included nodes or those still
    /// interviewing. Filled in value by value by [`NodeState::from_value`].
    #[serde(skip)]
    pub values: Vec<NodeValue>,
}

/// The Z-Wave device class pair, as zwave-js reports it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceClass {
    #[serde(default)]
    pub generic: Option<DeviceClassEntry>,
    #[serde(default)]
    pub specific: Option<DeviceClassEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceClassEntry {
    #[serde(default)]
    pub label: Option<String>,
}

impl DeviceClass {
    pub fn generic_label(&self) -> Option<&str> {
        self.generic.as_ref()?.label.as_deref()
    }

    pub fn specific_label(&self) -> Option<&str> {
        self.specific.as_ref()?.label.as_deref()
    }
}

/// One value entry from a node's value list.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeValue {
    #[serde(rename = "commandClass")]
    pub command_class: u32,
    #[serde(default)]
    pub endpoint: u32,
    /// zwave-js can send `property` as either a string or an integer (e.g. 37).
    #[serde(deserialize_with = "de_string_or_num")]
    pub property: String,
    #[serde(rename = "propertyKey", default)]
    pub property_key: Option<Value>,
    #[serde(default)]
    pub value: Option<Value>,
    /// The value's metadata: its `unit` (a sensor's scale, a setpoint's °F
    /// or °C), for a mode the `states` the device supports, and for a
    /// configuration parameter its label, range and default.
    #[serde(default)]
    pub metadata: Option<Value>,
}

impl NodeValue {
    /// Its identity within the node: `cc-endpoint-property[-key]`.
    pub fn key(&self) -> String {
        value_key(
            self.command_class,
            self.endpoint,
            &self.property,
            self.property_key.as_ref(),
        )
    }
}

/// A value's identity within its node.
pub fn value_key(cc: u32, endpoint: u32, property: &str, property_key: Option<&Value>) -> String {
    match property_key.and_then(crate::translator::property_key_str) {
        Some(pk) => format!("{cc}-{endpoint}-{property}-{pk}"),
        None => format!("{cc}-{endpoint}-{property}"),
    }
}

/// A value that must never be held, logged or published: a lock's codes.
pub fn is_code_value(cc: u32, property: &str) -> bool {
    cc == 99 && matches!(property, "userCode" | "adminCode" | "masterCode")
}

/// What became of a node's values while parsing it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParseReport {
    /// Values that did not parse and were skipped.
    pub skipped: usize,
}

impl NodeState {
    /// Parse from the raw JSON returned by start_listening or nodeState.
    ///
    /// `None` only when the node has no `nodeId` (or its header is not an
    /// object at all); a value that does not parse is skipped on its own.
    pub fn from_value(v: &Value) -> Option<Self> {
        Self::parse(v).map(|(n, _)| n)
    }

    /// [`Self::from_value`], saying how many values were skipped.
    pub fn parse(v: &Value) -> Option<(Self, ParseReport)> {
        let obj = v.as_object()?;
        let mut header = serde_json::Map::with_capacity(obj.len());
        for (k, val) in obj {
            if k != "values" && k != "endpoints" {
                header.insert(k.clone(), val.clone());
            }
        }
        let mut node: NodeState = match serde_json::from_value(Value::Object(header)) {
            Ok(n) => n,
            Err(e) => {
                let node_id = v.get("nodeId").and_then(Value::as_u64).unwrap_or(0);
                tracing::warn!(node_id, error = %e, "A node's header did not parse — node skipped");
                return None;
            }
        };
        let mut report = ParseReport::default();
        if let Some(values) = obj.get("values").and_then(Value::as_array) {
            for raw in values {
                match serde_json::from_value::<NodeValue>(raw.clone()) {
                    Ok(value) if is_code_value(value.command_class, &value.property) => {}
                    Ok(value) => node.values.push(value),
                    Err(_) => report.skipped += 1,
                }
            }
        }
        if report.skipped > 0 {
            tracing::warn!(
                node_id = node.node_id,
                skipped = report.skipped,
                "Some of a node's values did not parse and were skipped; the node is kept"
            );
        }
        Some((node, report))
    }

    /// Whether the node is reachable.
    /// NodeStatus: 0=Unknown, 1=Asleep, 2=Awake, 3=Dead, 4=Alive.
    /// Asleep is normal for battery devices — they wake periodically and are
    /// still functional.  Only Dead (3) means the node cannot be reached.
    pub fn is_available(&self) -> bool {
        !matches!(self.status, Some(3))
    }

    /// Mains-powered, always listening.
    pub fn is_mains(&self) -> bool {
        self.is_listening.unwrap_or(false)
    }

    /// Battery-powered but wakes several times a second for commands (a lock).
    pub fn is_flirs(&self) -> bool {
        matches!(
            self.is_frequent_listening.as_ref(),
            Some(v) if !v.is_null() && !matches!(v, Value::Bool(false))
        )
    }

    /// Who made it: schema 47 says `manufacturer`, older ones only in the
    /// device file.
    pub fn manufacturer_name(&self) -> Option<String> {
        self.manufacturer
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.device_config
                    .as_ref()?
                    .get("manufacturer")?
                    .as_str()
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
    }

    /// The model: zwave-js's `label`, else the device file's.
    pub fn model_name(&self) -> Option<String> {
        self.label
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.device_config
                    .as_ref()?
                    .get("label")?
                    .as_str()
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
    }

    /// Insert or replace a value, keyed by its identity. Codes are refused.
    pub fn upsert_value(&mut self, value: NodeValue) {
        if is_code_value(value.command_class, &value.property) {
            return;
        }
        let key = value.key();
        match self.values.iter_mut().find(|v| v.key() == key) {
            Some(existing) => {
                existing.value = value.value;
                if value.metadata.is_some() {
                    existing.metadata = value.metadata;
                }
            }
            None => self.values.push(value),
        }
    }

    /// Forget a value (a `value removed` event).
    pub fn remove_value(&mut self, key: &str) -> Option<NodeValue> {
        let at = self.values.iter().position(|v| v.key() == key)?;
        Some(self.values.remove(at))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn de_opt_u8<'de, D>(d: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|v| v.as_u64())
        .and_then(|n| u8::try_from(n).ok()))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// **One bad value used to drop the whole node** — and the reconcile that
    /// followed deleted its device. Now only that value goes.
    #[test]
    fn a_value_that_does_not_parse_drops_only_itself() {
        let (n, report) = NodeState::parse(&json!({
            "nodeId": 5,
            "name": "Lock",
            "values": [
                { "commandClass": 98, "endpoint": 0, "property": "currentMode", "value": 255 },
                { "commandClass": "not a number", "property": "x" },
                { "endpoint": 0 },
                { "commandClass": 128, "endpoint": 0, "property": "level", "value": 29 }
            ]
        }))
        .unwrap();
        assert_eq!(n.node_id, 5);
        assert_eq!(n.values.len(), 2);
        assert_eq!(report.skipped, 2);
    }

    /// A node's header being odd (a status as a string) is not fatal either.
    #[test]
    fn an_odd_header_field_is_not_fatal() {
        let n = NodeState::from_value(&json!({ "nodeId": 3, "status": "alive", "ready": true }))
            .unwrap();
        assert_eq!(n.status, None);
        assert_eq!(n.ready, Some(true));
        assert!(NodeState::from_value(&json!({ "name": "no id" })).is_none());
    }

    /// Codes never get into the node at all.
    #[test]
    fn a_lock_s_codes_are_dropped_as_they_are_parsed() {
        let n = NodeState::from_value(&json!({
            "nodeId": 5,
            "values": [
                { "commandClass": 99, "endpoint": 0, "property": "userCode", "propertyKey": 1, "value": "27182818" },
                { "commandClass": 99, "endpoint": 0, "property": "userIdStatus", "propertyKey": 1, "value": 1 }
            ]
        }))
        .unwrap();
        assert_eq!(n.values.len(), 1);
        assert_eq!(n.values[0].property, "userIdStatus");
        let mut n = n;
        n.upsert_value(NodeValue {
            command_class: 99,
            endpoint: 0,
            property: "userCode".into(),
            property_key: Some(json!(2)),
            value: Some(json!("16180339")),
            metadata: None,
        });
        assert!(!format!("{n:?}").contains("16180339"));
    }

    /// M5: a Configuration parameter's property is a number.
    #[test]
    fn a_numeric_property_in_a_value_update_parses() {
        let a: ValueUpdatedArgs = serde_json::from_value(json!({
            "commandClass": 112, "endpoint": 0, "property": 3, "newValue": 7
        }))
        .unwrap();
        assert_eq!(a.property, "3");
        assert_eq!(a.new_value, json!(7));
        let a: ValueUpdatedArgs = serde_json::from_value(json!({
            "commandClass": 91, "endpoint": 0, "property": "scene", "propertyKey": "001", "value": 0
        }))
        .unwrap();
        assert_eq!(a.new_value, json!(0));
    }

    #[test]
    fn controller_events_keep_their_top_level_fields() {
        let e = RawEvent::from_value(json!({
            "source": "controller", "event": "node removed",
            "node": { "nodeId": 9 }, "reason": 0
        }))
        .unwrap();
        assert_eq!(e.node_id, None);
        assert_eq!(e.get("node").unwrap()["nodeId"], 9);
        assert_eq!(e.get("reason").unwrap(), 0);
    }

    #[test]
    fn manufacturer_and_model_fall_back_to_the_device_file() {
        let n = NodeState::from_value(&json!({
            "nodeId": 4,
            "deviceConfig": { "manufacturer": "Zooz", "label": "ZEN15" }
        }))
        .unwrap();
        assert_eq!(n.manufacturer_name().as_deref(), Some("Zooz"));
        assert_eq!(n.model_name().as_deref(), Some("ZEN15"));
    }
}
