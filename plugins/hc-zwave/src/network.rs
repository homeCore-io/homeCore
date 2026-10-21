//! What the plugin knows about the Z-Wave network right now, for the `status`
//! action — answered from memory, never by asking the radio.
//!
//! The connection, the server and driver versions, the controller, and every
//! node's health: whether it answers, whether it sleeps, its battery, when it
//! was last heard, its radio statistics and a one-word signal.
//!
//! ## Signal
//!
//! From zwave-js's statistics for the node (`statistics updated` events and
//! the node dump), the worst of:
//!
//! | | good | fair | poor |
//! |---|---|---|---|
//! | RSSI of its last working route | above −80 dBm | −80 to −90 dBm | −90 dBm or less |
//! | Messages that never arrived (dropped + no answer), once 10+ were sent | under 3 % | 3 – 10 % | 10 % or more |
//! | Round trip | under 300 ms | 300 ms – 1 s | 1 s or more |
//! | Hops | direct or one repeater | two or more repeaters | |
//!
//! `unknown` until there is something to judge. A dead node is `poor`
//! whatever its last numbers said. RSSI 125–127 are zwave-js's "no reading"
//! markers and count as none.

use crate::types::NodeState;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The connection to zwave-js-server.
#[derive(Debug, Clone)]
pub struct Connection {
    /// `connecting`, `connected`, `disconnected`.
    pub state: &'static str,
    pub since: DateTime<Utc>,
    pub last_error: Option<String>,
    pub server_version: Option<String>,
    pub driver_version: Option<String>,
    pub schema: Option<u32>,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            state: "connecting",
            since: Utc::now(),
            last_error: None,
            server_version: None,
            driver_version: None,
            schema: None,
        }
    }
}

/// The network as last heard.
#[derive(Debug, Default)]
pub struct Network {
    pub connection: Connection,
    /// The controller, as zwave-js dumps it, kept current from its events.
    pub controller: Value,
    pub nodes: BTreeMap<u32, NodeState>,
    /// Route rebuilding: whether it is running, and each node's progress.
    pub rebuilding: bool,
    pub rebuild_progress: BTreeMap<String, String>,
    pub rebuild_finished: Option<DateTime<Utc>>,
    /// What the controller is doing: `idle`, `including`, `excluding`, …
    pub inclusion: Option<String>,
}

/// RSSI values zwave-js uses to mean "no reading".
fn rssi_of(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_i64()?;
    (!(125..=127).contains(&n)).then_some(n)
}

/// One node's radio, from its statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct Radio {
    pub rssi: Option<i64>,
    pub rtt_ms: Option<f64>,
    pub sent: u64,
    pub received: u64,
    pub failed: u64,
    pub repeaters: Option<usize>,
}

impl Radio {
    pub fn from_statistics(stats: Option<&Value>) -> Self {
        let s = stats.cloned().unwrap_or(Value::Null);
        let n = |k: &str| s.get(k).and_then(Value::as_u64).unwrap_or(0);
        let lwr = s.get("lwr");
        Self {
            rssi: rssi_of(lwr.and_then(|l| l.get("rssi"))).or_else(|| rssi_of(s.get("rssi"))),
            rtt_ms: s.get("rtt").and_then(Value::as_f64),
            sent: n("commandsTX"),
            received: n("commandsRX"),
            failed: n("commandsDroppedTX") + n("timeoutResponse"),
            repeaters: lwr
                .and_then(|l| l.get("repeaters"))
                .and_then(Value::as_array)
                .map(Vec::len),
        }
    }

    /// `good`, `fair`, `poor` or `unknown` (see the module's table).
    pub fn signal(&self, dead: bool) -> &'static str {
        if dead {
            return "poor";
        }
        let ratio = (self.sent >= 10).then(|| self.failed as f64 / self.sent as f64);
        if self.rssi.is_none() && ratio.is_none() && self.rtt_ms.is_none() {
            return "unknown";
        }
        let poor = self.rssi.is_some_and(|r| r <= -90)
            || ratio.is_some_and(|r| r >= 0.10)
            || self.rtt_ms.is_some_and(|t| t >= 1000.0);
        if poor {
            return "poor";
        }
        let fair = self.rssi.is_some_and(|r| r <= -80)
            || ratio.is_some_and(|r| r >= 0.03)
            || self.rtt_ms.is_some_and(|t| t >= 300.0)
            || self.repeaters.is_some_and(|n| n >= 2);
        if fair {
            "fair"
        } else {
            "good"
        }
    }
}

/// A node's status in a word.
pub fn status_word(status: Option<u8>) -> &'static str {
    match status {
        Some(1) => "asleep",
        Some(2) => "awake",
        Some(3) => "dead",
        Some(4) => "alive",
        _ => "unknown",
    }
}

/// A security class in words.
pub fn security_word(class: Option<i64>, is_secure: Option<&Value>) -> &'static str {
    match class {
        Some(2) => "S2 Access Control",
        Some(1) => "S2 Authenticated",
        Some(0) => "S2 Unauthenticated",
        Some(7) => "S0",
        Some(_) => "none",
        None => match is_secure.and_then(Value::as_bool) {
            Some(true) => "secure",
            Some(false) => "none",
            None => "unknown",
        },
    }
}

/// The interview stage, as a word, whether zwave-js sent the name or the
/// number.
pub fn interview_word(stage: Option<&Value>) -> String {
    match stage {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(0) => "None",
            Some(1) => "ProtocolInfo",
            Some(2) => "NodeInfo",
            Some(3) => "CommandClasses",
            Some(4) => "OverwriteConfig",
            Some(5) => "Complete",
            _ => "Unknown",
        }
        .into(),
        _ => "Unknown".into(),
    }
}

fn value_of(node: &NodeState, cc: u32, property: &str) -> Option<Value> {
    node.values
        .iter()
        .find(|v| v.command_class == cc && v.property == property)
        .and_then(|v| v.value.clone())
}

/// One node, for the status action and the Health tab.
pub fn node_summary(node: &NodeState) -> Value {
    let radio = Radio::from_statistics(node.statistics.as_ref());
    let dead = node.status == Some(3);
    let power = if node.is_controller_node == Some(true) || node.is_mains() {
        "mains"
    } else if node.is_flirs() {
        "flirs"
    } else {
        "battery"
    };
    let wake_up = value_of(node, 132, "wakeUpInterval").and_then(|v| v.as_u64());
    let route = node
        .statistics
        .as_ref()
        .and_then(|s| s.get("lwr"))
        .map(|l| {
            json!({
                "repeaters": l.get("repeaters").cloned().unwrap_or(json!([])),
                "data_rate": l.get("protocolDataRate").cloned().unwrap_or(Value::Null),
            })
        });
    let name = node.name.clone().filter(|n| !n.is_empty());
    json!({
        "node_id": node.node_id,
        "device_id": crate::bridge::node_device_id(node.node_id),
        "name": name,
        "location": node.location.clone().filter(|l| !l.is_empty()),
        "manufacturer": node.manufacturer_name(),
        "model": node.model_name(),
        "description": node.device_config.as_ref().and_then(|d| d.get("description")).cloned(),
        "firmware": node.firmware_version,
        "is_controller": node.is_controller_node.unwrap_or(false),
        "status": status_word(node.status),
        "ready": node.ready.unwrap_or(false),
        "interview_stage": interview_word(node.interview_stage.as_ref()),
        "security": security_word(node.highest_security_class, node.is_secure.as_ref()),
        "power": power,
        "sleeps": power == "battery",
        "wake_up_secs": wake_up,
        "battery_pct": value_of(node, 128, "level"),
        "last_seen": node.last_seen.clone().filter(|v| !v.is_null()),
        "signal": radio.signal(dead),
        "statistics": {
            "rssi": radio.rssi,
            "rtt_ms": radio.rtt_ms,
            "sent": radio.sent,
            "received": radio.received,
            "failed": radio.failed,
        },
        "route": route,
    })
}

impl Network {
    /// The whole snapshot, as the `status` action answers.
    pub fn status(&self, url: &str) -> Value {
        let c = &self.controller;
        let home_id = c
            .get("homeId")
            .and_then(Value::as_u64)
            .map(|h| format!("0x{h:08x}"));
        let own = c.get("ownNodeId").and_then(Value::as_u64);
        let controller_node = own.and_then(|n| self.nodes.get(&(n as u32)));
        let nodes: Vec<Value> = self.nodes.values().map(node_summary).collect();
        let devices = nodes
            .iter()
            .filter(|n| n["is_controller"] != json!(true))
            .count();
        let answering = nodes
            .iter()
            .filter(|n| n["is_controller"] != json!(true) && n["status"] != json!("dead"))
            .count();
        json!({
            "status": "ok",
            "connection": {
                "state": self.connection.state,
                "since": self.connection.since.to_rfc3339(),
                "last_error": self.connection.last_error,
                "url": redact_url(url),
                "server_version": self.connection.server_version,
                "driver_version": self.connection.driver_version,
                "schema": self.connection.schema,
            },
            "controller": {
                "home_id": home_id,
                "node_id": own,
                "model": controller_node.and_then(NodeState::model_name),
                "manufacturer": controller_node.and_then(NodeState::manufacturer_name),
                "location": controller_node.and_then(|n| n.location.clone()).filter(|l| !l.is_empty()),
                "sdk_version": c.get("sdkVersion"),
                "firmware_version": c.get("firmwareVersion"),
                "rf_region": c.get("rfRegion").map(rf_region),
                "inclusion": self.inclusion.clone().or_else(|| c.get("inclusionState").map(inclusion_word)),
                "rebuilding_routes": self.rebuilding,
                "rebuild_progress": self.rebuild_progress,
                "rebuild_finished": self.rebuild_finished.map(|t| t.to_rfc3339()),
                "statistics": c.get("statistics"),
            },
            "devices": devices,
            "answering": answering,
            "nodes": nodes,
        })
    }
}

/// A URL without any user name or password in it.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority = rest.find('/').unwrap_or(rest.len());
    match rest[..authority].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// RFRegion as zwave-js numbers it.
pub fn rf_region(v: &Value) -> Value {
    let name = match v.as_u64() {
        Some(0) => "Europe",
        Some(1) => "USA",
        Some(2) => "Australia/New Zealand",
        Some(3) => "Hong Kong",
        Some(5) => "India",
        Some(6) => "Israel",
        Some(7) => "Russia",
        Some(8) => "China",
        Some(9) => "USA (Long Range)",
        Some(10) => "USA (Long Range end device)",
        Some(11) => "Europe (Long Range)",
        Some(32) => "Japan",
        Some(33) => "Korea",
        Some(254) => "Unknown",
        Some(255) => "Default (Europe)",
        _ => return v.clone(),
    };
    json!(name)
}

/// InclusionState as zwave-js numbers it.
pub fn inclusion_word(v: &Value) -> String {
    match v.as_u64() {
        Some(0) => "idle",
        Some(1) => "including",
        Some(2) => "excluding",
        Some(3) => "busy",
        Some(4) => "smart_start",
        _ => "unknown",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(stats: Value) -> Radio {
        Radio::from_statistics(Some(&stats))
    }

    #[test]
    fn signal_thresholds() {
        let good = radio(
            json!({ "commandsTX": 100, "commandsDroppedTX": 1, "rtt": 24, "lwr": { "rssi": -62, "repeaters": [] } }),
        );
        assert_eq!(good.signal(false), "good");
        assert_eq!(good.rssi, Some(-62));
        assert_eq!(good.failed, 1);
        let weak = radio(json!({ "lwr": { "rssi": -84, "repeaters": [] } }));
        assert_eq!(weak.signal(false), "fair");
        let far = radio(json!({ "lwr": { "rssi": -92 } }));
        assert_eq!(far.signal(false), "poor");
        let lossy =
            radio(json!({ "commandsTX": 100, "commandsDroppedTX": 4, "timeoutResponse": 1 }));
        assert_eq!(lossy.signal(false), "fair");
        let lost = radio(json!({ "commandsTX": 50, "timeoutResponse": 6 }));
        assert_eq!(lost.signal(false), "poor");
        let slow = radio(json!({ "rtt": 450 }));
        assert_eq!(slow.signal(false), "fair");
        let hops = radio(json!({ "rtt": 40, "lwr": { "rssi": -60, "repeaters": [4, 7] } }));
        assert_eq!(hops.signal(false), "fair");
        // Too few messages to judge a ratio, nothing else known.
        assert_eq!(
            radio(json!({ "commandsTX": 3, "commandsDroppedTX": 2 })).signal(false),
            "unknown"
        );
        // zwave-js's "no reading" markers are no reading.
        assert_eq!(radio(json!({ "rssi": 127 })).rssi, None);
        assert_eq!(good.signal(true), "poor");
        assert_eq!(Radio::from_statistics(None).signal(false), "unknown");
    }

    #[test]
    fn words() {
        assert_eq!(status_word(Some(3)), "dead");
        assert_eq!(status_word(Some(1)), "asleep");
        assert_eq!(security_word(Some(2), None), "S2 Access Control");
        assert_eq!(security_word(Some(7), None), "S0");
        assert_eq!(security_word(Some(-1), None), "none");
        assert_eq!(interview_word(Some(&json!("Complete"))), "Complete");
        assert_eq!(interview_word(Some(&json!(3))), "CommandClasses");
        assert_eq!(rf_region(&json!(1)), json!("USA"));
        assert_eq!(inclusion_word(&json!(1)), "including");
    }

    #[test]
    fn urls_lose_their_password() {
        assert_eq!(redact_url("ws://u:p@10.0.0.2:3000"), "ws://10.0.0.2:3000");
        assert_eq!(redact_url("ws://10.0.0.2:3000"), "ws://10.0.0.2:3000");
        assert_eq!(redact_url("ws://h:3000/a@b"), "ws://h:3000/a@b");
    }

    #[test]
    fn a_node_s_health() {
        let n = NodeState::from_value(&json!({
            "nodeId": 5, "name": "Lock - Living Room", "status": 1,
            "isListening": false, "isFrequentListening": "1000ms",
            "highestSecurityClass": 2, "interviewStage": "Complete", "ready": true,
            "statistics": { "commandsTX": 20, "commandsRX": 19, "rtt": 80, "lwr": { "rssi": -71, "repeaters": [] } },
            "values": [ { "commandClass": 128, "endpoint": 0, "property": "level", "value": 29 } ]
        }))
        .unwrap();
        let s = node_summary(&n);
        assert_eq!(s["device_id"], "zwave_5");
        assert_eq!(s["power"], "flirs");
        assert_eq!(s["sleeps"], false);
        assert_eq!(s["battery_pct"], 29);
        assert_eq!(s["security"], "S2 Access Control");
        assert_eq!(s["status"], "asleep");
        assert_eq!(s["signal"], "good");
    }
}
