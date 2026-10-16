//! Alerts: what the house's own state says needs attention (H4).
//!
//! homeCore raises and clears them — whether or not any screen is open, the
//! same for every client — and announces each on the event stream
//! (`alert_raised`, `alert_cleared`). Screens only show them.
//!
//! Two sources:
//! - **built in:** something wet, smoke, a device offline, a lock jammed, a
//!   battery low;
//! - **time rules** an admin writes: "the garage door open after 22:00",
//!   "the front door unlocked for 30 minutes".

use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::AlertSeverity;

/// Why an alert was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Wet,
    Smoke,
    Offline,
    Jammed,
    LowBattery,
    /// An admin's time rule; `rule_id` says which.
    Rule,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::Wet => "wet",
            AlertKind::Smoke => "smoke",
            AlertKind::Offline => "offline",
            AlertKind::Jammed => "jammed",
            AlertKind::LowBattery => "low_battery",
            AlertKind::Rule => "rule",
        }
    }
}

/// One alert, active while its condition lasts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable while the condition lasts: `wet:{device}`, `rule:{rule}:{device}`.
    pub id: String,
    pub kind: AlertKind,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// In words, for a person: "Garage Door open after 22:00".
    pub message: String,
    /// The same without the device's name, for its card: "Open after 22:00".
    pub reason: String,
    pub severity: AlertSeverity,
    /// When the condition began, as near as homeCore knows.
    pub raised_at: DateTime<Utc>,
}

/// A time rule (C9): a device in a state, optionally only between two times
/// of day, optionally only once it has lasted a while.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    /// Assigned by homeCore on create.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub device_ids: Vec<String>,
    /// The state that counts: `open` equal to `true`, say.
    pub attribute: String,
    pub value: serde_json::Value,
    /// The state in the device's own words ("Open"), for the alert's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_label: Option<String>,
    /// From this time of day, `HH:MM` local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Until this time of day, `HH:MM` local. Earlier than `after` spans midnight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Only once the state has lasted this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_minutes: Option<u32>,
    #[serde(default = "warning")]
    pub severity: AlertSeverity,
}

fn yes() -> bool {
    true
}
fn warning() -> AlertSeverity {
    AlertSeverity::Warning
}

/// `HH:MM` (or `HH:MM:SS`, as some time pickers send) as a time of day.
pub fn parse_time_of_day(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

impl AlertRule {
    /// What a person must fix before the rule can be saved.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("give the rule a name".into());
        }
        if self.device_ids.is_empty() {
            return Err("choose at least one device".into());
        }
        if self.attribute.trim().is_empty() {
            return Err("choose the state that counts".into());
        }
        for (label, t) in [("after", &self.after), ("until", &self.until)] {
            if let Some(t) = t {
                if parse_time_of_day(t).is_none() {
                    return Err(format!("`{label}` must be a time of day like 22:00"));
                }
            }
        }
        if self.for_minutes == Some(0) {
            return Err("`for_minutes` must be at least 1, or left out".into());
        }
        if matches!(self.severity, AlertSeverity::Info | AlertSeverity::Error) {
            return Err("severity is `warning` or `critical`".into());
        }
        Ok(())
    }

    /// Whether `now` (local time of day) is inside the rule's window. No
    /// times means always; only `after` means until midnight; only `until`
    /// means from midnight.
    pub fn in_window(&self, now: NaiveTime) -> bool {
        let after = self.after.as_deref().and_then(parse_time_of_day);
        let until = self.until.as_deref().and_then(parse_time_of_day);
        match (after, until) {
            (None, None) => true,
            (Some(a), None) => now >= a,
            (None, Some(u)) => now < u,
            (Some(a), Some(u)) if a <= u => now >= a && now < u,
            (Some(a), Some(u)) => now >= a || now < u,
        }
    }

    /// What the rule says, after the device's name: "open after 22:00 for 10 min".
    pub fn describe(&self) -> String {
        let mut s = self
            .state_label
            .clone()
            .unwrap_or_else(|| format!("{} is {}", self.attribute, plain(&self.value)));
        match (&self.after, &self.until) {
            (Some(a), Some(u)) => s.push_str(&format!(" between {a} and {u}")),
            (Some(a), None) => s.push_str(&format!(" after {a}")),
            (None, Some(u)) => s.push_str(&format!(" before {u}")),
            (None, None) => {}
        }
        if let Some(m) = self.for_minutes {
            s.push_str(&format!(" for {m} min"));
        }
        s
    }
}

fn plain(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(after: Option<&str>, until: Option<&str>) -> AlertRule {
        AlertRule {
            id: "r".into(),
            name: "Garage at night".into(),
            enabled: true,
            device_ids: vec!["oh1".into()],
            attribute: "open".into(),
            value: serde_json::json!(true),
            state_label: Some("Open".into()),
            after: after.map(Into::into),
            until: until.map(Into::into),
            for_minutes: None,
            severity: AlertSeverity::Warning,
        }
    }
    fn t(s: &str) -> NaiveTime {
        parse_time_of_day(s).unwrap()
    }

    #[test]
    fn a_window_across_midnight() {
        let r = rule(Some("22:00"), Some("06:00"));
        assert!(r.in_window(t("23:30")));
        assert!(r.in_window(t("02:00")));
        assert!(!r.in_window(t("06:00")));
        assert!(!r.in_window(t("12:00")));
    }

    #[test]
    fn a_window_within_a_day_and_open_ended_ones() {
        assert!(rule(Some("09:00"), Some("17:00")).in_window(t("12:00")));
        assert!(!rule(Some("09:00"), Some("17:00")).in_window(t("18:00")));
        assert!(rule(Some("22:00"), None).in_window(t("22:00")));
        assert!(!rule(Some("22:00"), None).in_window(t("01:00")));
        assert!(rule(None, Some("06:00")).in_window(t("01:00")));
        assert!(rule(None, None).in_window(t("12:00")));
    }

    #[test]
    fn describes_itself() {
        let mut r = rule(Some("22:00"), None);
        assert_eq!(r.describe(), "Open after 22:00");
        r.for_minutes = Some(10);
        r.until = Some("06:00".into());
        assert_eq!(r.describe(), "Open between 22:00 and 06:00 for 10 min");
        r.state_label = None;
        assert!(r.describe().starts_with("open is true"));
    }

    #[test]
    fn validation_says_what_is_wrong() {
        assert!(rule(Some("22:00"), None).validate().is_ok());
        assert!(rule(Some("10pm"), None).validate().is_err());
        let mut r = rule(None, None);
        r.device_ids.clear();
        assert!(r.validate().is_err());
        let mut r = rule(None, None);
        r.severity = AlertSeverity::Info;
        assert!(r.validate().is_err());
    }

    #[test]
    fn reads_what_a_client_sends() {
        let r: AlertRule = serde_json::from_value(serde_json::json!({
            "name": "x", "device_ids": ["d"], "attribute": "open", "value": true,
            "after": "22:00", "until": null, "for_minutes": null
        }))
        .unwrap();
        assert!(r.enabled);
        assert_eq!(r.severity, AlertSeverity::Warning);
        assert_eq!(r.until, None);
    }
}
