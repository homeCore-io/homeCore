//! A lock's codes and history.
//!
//! **Codes are write-only.** A code goes from the screen to the lock through
//! the `set_user_code` action and nowhere else: it is not published, not
//! logged, not kept, not returned, and an error about it never quotes it.
//! zwave-js reports every slot's code in the clear; those values are dropped
//! as frames are parsed ([`crate::types::NodeState::parse`]), so this plugin
//! never even holds them.
//!
//! **Who a code is for is kept here**, by slot, in a small file beside the
//! plugin's config (`.zwave-code-names.json`) — names, never codes. The lock
//! knows only numbers; the Codes tab, the history and the automation step
//! say "code 2 · Cleaner".
//!
//! **History** comes from what the lock reports when it is used. Notification
//! CC v2+ locks send Access Control *events* — "Keypad unlock operation" with
//! the user's slot — and older ones a v1 alarm (`alarmType` + `alarmLevel`),
//! decoded here for the common Kwikset/Yale/Schlage numbering. Each becomes
//! `last_lock_action`, `last_lock_method`, `last_lock_slot` and
//! `last_lock_event` (all four, with when) on the lock, so an automation can
//! start on "unlocked with code 2" and the History tab reads the lock's own
//! device history.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What happened at the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockEvent {
    /// `locked`, `unlocked`, `jammed`, `wrong_code`, `code_added`,
    /// `code_removed`, `codes_cleared`, `code_rejected`, `keypad_disabled`.
    pub action: &'static str,
    /// `keypad`, `manual`, `rf` (homeCore, or another controller), `auto`.
    pub method: Option<&'static str>,
    /// The user code slot, where the lock said which.
    pub slot: Option<u32>,
}

impl LockEvent {
    const fn new(action: &'static str, method: Option<&'static str>) -> Self {
        Self {
            action,
            method,
            slot: None,
        }
    }

    fn with_slot(mut self, slot: Option<u32>) -> Self {
        self.slot = slot.filter(|s| *s > 0);
        self
    }

    /// The partial state that says it, stamped `at`.
    pub fn patch(&self, at: &str) -> Value {
        json!({
            "last_lock_action": self.action,
            "last_lock_method": self.method,
            "last_lock_slot": self.slot,
            "last_lock_event": {
                "action": self.action,
                "method": self.method,
                "slot": self.slot,
                "at": at,
            },
        })
    }
}

/// Every `last_lock_action`, for the schema.
pub const ACTIONS: &[(&str, &str)] = &[
    ("locked", "Locked"),
    ("unlocked", "Unlocked"),
    ("jammed", "Jammed"),
    ("wrong_code", "Wrong code entered"),
    ("code_added", "Code added"),
    ("code_removed", "Code removed"),
    ("codes_cleared", "Every code removed"),
    ("code_rejected", "Code refused (a duplicate)"),
    ("keypad_disabled", "Keypad switched off for a while"),
];

/// Every `last_lock_method`, for the schema.
pub const METHODS: &[(&str, &str)] = &[
    ("keypad", "At the keypad"),
    ("manual", "By hand"),
    ("rf", "From homeCore or another controller"),
    ("auto", "By itself"),
];

fn slot_param(args: &Value) -> Option<u32> {
    let p = args.get("parameters")?;
    p.get("userId")
        .or_else(|| p.get("userID"))
        .and_then(Value::as_u64)
        .or_else(|| p.as_u64())
        .map(|n| n as u32)
}

/// A Notification CC event (the zwave-js-server `notification` event's
/// `args`, CC 113) as a lock event, where it is one.
///
/// Access Control (type 6) events, by number: 1/2 manual lock/unlock, 3/4 RF,
/// 5/6 keypad (with the slot), 7/8/10 "not fully locked", 9 auto-lock, 11
/// jammed, 12 all codes deleted, 13 one deleted, 14 one added, 15 a duplicate
/// refused, 16 keypad disabled, 19 too many wrong codes, 20 RF with an invalid
/// code. Home Security (type 7) event 4 is "Tampering, invalid code".
pub fn from_notification(args: &Value) -> Option<LockEvent> {
    let kind = args.get("type").and_then(Value::as_u64)?;
    let event = args.get("event").and_then(Value::as_u64)?;
    let slot = slot_param(args);
    let e = match (kind, event) {
        (6, 1) => LockEvent::new("locked", Some("manual")),
        (6, 2) => LockEvent::new("unlocked", Some("manual")),
        (6, 3) => LockEvent::new("locked", Some("rf")),
        (6, 4) => LockEvent::new("unlocked", Some("rf")),
        (6, 5) => LockEvent::new("locked", Some("keypad")).with_slot(slot),
        (6, 6) => LockEvent::new("unlocked", Some("keypad")).with_slot(slot),
        (6, 7) => LockEvent::new("jammed", Some("manual")),
        (6, 8) => LockEvent::new("jammed", Some("rf")),
        (6, 9) => LockEvent::new("locked", Some("auto")),
        (6, 10) => LockEvent::new("jammed", Some("auto")),
        (6, 11) => LockEvent::new("jammed", None),
        (6, 12) => LockEvent::new("codes_cleared", None),
        (6, 13) => LockEvent::new("code_removed", None).with_slot(slot),
        (6, 14) => LockEvent::new("code_added", None).with_slot(slot),
        (6, 15) => LockEvent::new("code_rejected", None).with_slot(slot),
        (6, 16) => LockEvent::new("keypad_disabled", Some("keypad")),
        (6, 19) => LockEvent::new("wrong_code", Some("keypad")),
        (6, 20) => LockEvent::new("wrong_code", Some("rf")),
        (7, 4) => LockEvent::new("wrong_code", Some("keypad")),
        _ => return None,
    };
    Some(e)
}

/// A Notification v1 alarm (`alarmType`, `alarmLevel`) as a lock event.
///
/// v1 alarms are each manufacturer's own numbering; this is the set shared by
/// Kwikset, Yale and Schlage locks (and what Home Assistant decodes for them):
/// 9 jammed; 16/19 keypad unlock (level = slot); 18 keypad lock (level = slot,
/// 0 for the lock button); 21/22 manual lock/unlock; 24/25 RF lock/unlock; 26
/// auto-lock failed; 27 auto-locked; 32 all codes deleted; 33 one deleted
/// (level = slot); 112 one added (level = slot); 113 a duplicate; 161 too
/// many wrong codes. Anything else is left alone — not guessed at.
pub fn from_v1_alarm(alarm_type: u64, level: u64) -> Option<LockEvent> {
    let slot = u32::try_from(level).ok();
    let e = match alarm_type {
        9 => LockEvent::new("jammed", None),
        16 | 19 => LockEvent::new("unlocked", Some("keypad")).with_slot(slot),
        18 => LockEvent::new("locked", Some("keypad")).with_slot(slot),
        21 => LockEvent::new("locked", Some("manual")),
        22 => LockEvent::new("unlocked", Some("manual")),
        24 => LockEvent::new("locked", Some("rf")),
        25 => LockEvent::new("unlocked", Some("rf")),
        26 => LockEvent::new("jammed", Some("auto")),
        27 => LockEvent::new("locked", Some("auto")),
        32 => LockEvent::new("codes_cleared", None),
        33 => LockEvent::new("code_removed", None).with_slot(slot),
        112 => LockEvent::new("code_added", None).with_slot(slot),
        113 => LockEvent::new("code_rejected", None).with_slot(slot),
        161 => LockEvent::new("wrong_code", Some("keypad")),
        _ => return None,
    };
    Some(e)
}

/// A slot's state, from User Code `userIdStatus`: 0 available, 1 enabled,
/// 2 disabled, 3 messaging, 4 passage mode.
pub fn slot_status(user_id_status: Option<u64>) -> &'static str {
    match user_id_status {
        Some(0) => "available",
        Some(1 | 3 | 4) => "in_use",
        Some(2) => "disabled",
        _ => "unknown",
    }
}

/// Whether a code is one a lock takes: 4 to 10 digits.
pub fn valid_code(code: &str) -> bool {
    (4..=10).contains(&code.len()) && code.chars().all(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Who each code is for
// ---------------------------------------------------------------------------

/// Names by node and slot, kept on disk. Never codes.
pub struct CodeNames {
    path: Option<PathBuf>,
    names: Mutex<BTreeMap<u32, BTreeMap<u32, String>>>,
}

impl CodeNames {
    /// Load from `path` (missing is empty).
    pub fn load(path: PathBuf) -> Self {
        let names = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| {
                serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&s).ok()
            })
            .map(|m| {
                m.into_iter()
                    .filter_map(|(node, slots)| {
                        let node: u32 = node.parse().ok()?;
                        let slots = slots
                            .into_iter()
                            .filter_map(|(slot, name)| Some((slot.parse().ok()?, name)))
                            .collect();
                        Some((node, slots))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: Some(path),
            names: Mutex::new(names),
        }
    }

    /// Where the file for a config at `config_path` lives.
    pub fn path_for(config_path: &str) -> PathBuf {
        Path::new(config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".zwave-code-names.json")
    }

    pub fn of(&self, node: u32) -> BTreeMap<u32, String> {
        self.names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set(&self, node: u32, slot: u32, name: Option<&str>) {
        {
            let mut all = self.names.lock().unwrap_or_else(|e| e.into_inner());
            let slots = all.entry(node).or_default();
            match name.map(str::trim).filter(|n| !n.is_empty()) {
                Some(n) => {
                    slots.insert(slot, n.chars().take(60).collect());
                }
                None => {
                    slots.remove(&slot);
                }
            }
            if slots.is_empty() {
                all.remove(&node);
            }
        }
        self.save();
    }

    pub fn forget_node(&self, node: u32) {
        let removed = self
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&node)
            .is_some();
        if removed {
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let doc: BTreeMap<String, BTreeMap<String, String>> = self
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(n, slots)| {
                (
                    n.to_string(),
                    slots
                        .iter()
                        .map(|(s, name)| (s.to_string(), name.clone()))
                        .collect(),
                )
            })
            .collect();
        if let Ok(text) = serde_json::to_string_pretty(&doc) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypad_unlock_says_which_code() {
        let e = from_notification(&json!({
            "type": 6, "event": 6, "label": "Access Control",
            "eventLabel": "Keypad unlock operation", "parameters": { "userId": 2 }
        }))
        .unwrap();
        assert_eq!(
            e,
            LockEvent {
                action: "unlocked",
                method: Some("keypad"),
                slot: Some(2)
            }
        );
        let p = e.patch("2026-10-20T14:14:00Z");
        assert_eq!(p["last_lock_action"], "unlocked");
        assert_eq!(p["last_lock_method"], "keypad");
        assert_eq!(p["last_lock_slot"], 2);
        assert_eq!(p["last_lock_event"]["at"], "2026-10-20T14:14:00Z");
    }

    #[test]
    fn access_control_events() {
        let ev = |t, e| {
            from_notification(&json!({ "type": t, "event": e })).map(|x| (x.action, x.method))
        };
        assert_eq!(ev(6, 1), Some(("locked", Some("manual"))));
        assert_eq!(ev(6, 2), Some(("unlocked", Some("manual"))));
        assert_eq!(ev(6, 3), Some(("locked", Some("rf"))));
        assert_eq!(ev(6, 9), Some(("locked", Some("auto"))));
        assert_eq!(ev(6, 11), Some(("jammed", None)));
        assert_eq!(ev(7, 4), Some(("wrong_code", Some("keypad"))));
        // A door opening is a state, not a lock event.
        assert_eq!(ev(6, 22), None);
        assert_eq!(ev(7, 8), None);
        assert!(from_notification(&json!({ "event": 6 })).is_none());
    }

    #[test]
    fn v1_alarms_for_older_locks() {
        let a = |t, l| from_v1_alarm(t, l);
        assert_eq!(
            a(19, 3),
            Some(LockEvent {
                action: "unlocked",
                method: Some("keypad"),
                slot: Some(3)
            })
        );
        assert_eq!(a(21, 1).unwrap().action, "locked");
        assert_eq!(a(21, 1).unwrap().method, Some("manual"));
        assert_eq!(a(22, 1).unwrap().action, "unlocked");
        assert_eq!(a(24, 0).unwrap().method, Some("rf"));
        assert_eq!(a(27, 1).unwrap().method, Some("auto"));
        assert_eq!(a(9, 0).unwrap().action, "jammed");
        assert_eq!(a(161, 0).unwrap().action, "wrong_code");
        // The lock button: no slot.
        assert_eq!(a(18, 0).unwrap().slot, None);
        // Battery warnings and unknown numbers are not lock events.
        assert_eq!(a(167, 0), None);
        assert_eq!(a(200, 1), None);
    }

    #[test]
    fn slot_states() {
        assert_eq!(slot_status(Some(0)), "available");
        assert_eq!(slot_status(Some(1)), "in_use");
        assert_eq!(slot_status(Some(2)), "disabled");
        assert_eq!(slot_status(None), "unknown");
    }

    #[test]
    fn codes_are_digits_four_to_ten() {
        assert!(valid_code("1234"));
        assert!(valid_code("1234567890"));
        assert!(!valid_code("123"));
        assert!(!valid_code("12345678901"));
        assert!(!valid_code("12a4"));
        assert!(!valid_code(""));
    }

    #[test]
    fn names_are_kept_by_slot_and_never_a_code() {
        let dir = std::env::temp_dir().join(format!("hc-zwave-names-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".zwave-code-names.json");
        let n = CodeNames::load(path.clone());
        n.set(5, 2, Some("Cleaner"));
        n.set(5, 1, Some("Family"));
        n.set(5, 3, Some("  "));
        assert_eq!(n.of(5).len(), 2);
        let again = CodeNames::load(path.clone());
        assert_eq!(again.of(5).get(&2).map(String::as_str), Some("Cleaner"));
        again.set(5, 2, None);
        assert_eq!(CodeNames::load(path.clone()).of(5).len(), 1);
        again.forget_node(5);
        assert!(CodeNames::load(path).of(5).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
