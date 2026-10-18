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
//!   "the front door unlocked for 30 minutes", "the freezer above -10".
//!
//! A person can **acknowledge** an alert (it stays, marked as seen, until its
//! condition ends) or **ignore** a kind of alert, a device's alerts, or both
//! — for good or for a while. An ignored alert is not raised at all.

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
    /// Someone has seen it. Lasts while the alert does: if the condition ends
    /// and comes back, it is a new alert, unacknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<Acknowledged>,
}

/// Who acknowledged an alert, and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Acknowledged {
    /// Their name, as they signed in.
    pub by: String,
    pub at: DateTime<Utc>,
}

/// How a time rule compares the device's value with its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertOp {
    /// The value is the rule's (the default, and all there was before).
    #[default]
    Eq,
    /// The value is anything but the rule's.
    Ne,
    /// A number above the rule's.
    Above,
    /// A number below the rule's.
    Below,
}

impl AlertOp {
    fn is_eq(&self) -> bool {
        *self == AlertOp::Eq
    }
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
    /// How the device's value is compared with `value`; equal unless said.
    #[serde(default, skip_serializing_if = "AlertOp::is_eq")]
    pub op: AlertOp,
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
    /// A notification channel to tell when one of its alerts is raised
    /// (`all` for every channel). None: shown, not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<String>,
}

/// Alerts a person has chosen not to see: a kind (low battery), a device's,
/// one time rule's — or a kind on one device. For good, or `until` a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertIgnore {
    /// Assigned by homeCore on create.
    #[serde(default)]
    pub id: String,
    /// Only this kind; every kind when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AlertKind>,
    /// Only this device; every device when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Only this time rule's alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Ignored until then, and seen again after; for good when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    /// Why, in the person's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Who asked, set by homeCore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Set by homeCore.
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl AlertIgnore {
    /// What a person must fix before it can be saved: ignoring every alert
    /// in the house is turning alerts off, which this is not.
    pub fn validate(&self) -> Result<(), String> {
        if self.kind.is_none() && self.device_id.is_none() && self.rule_id.is_none() {
            return Err("say which alerts: a kind, a device or a rule".into());
        }
        if self
            .device_id
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            return Err("`device_id` is empty".into());
        }
        Ok(())
    }

    /// Still in force at `now`.
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.until.is_none_or(|u| now < u)
    }

    /// Whether it covers this alert at `now`.
    pub fn covers(&self, alert: &Alert, now: DateTime<Utc>) -> bool {
        self.covers_parts(alert.kind, &alert.device_id, alert.rule_id.as_deref(), now)
    }

    /// The same, for an alert not made yet — a low battery the battery
    /// watcher is about to tell someone of, say.
    pub fn covers_parts(
        &self,
        kind: AlertKind,
        device_id: &str,
        rule_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> bool {
        self.active(now)
            && self.kind.is_none_or(|k| k == kind)
            && self.device_id.as_deref().is_none_or(|d| d == device_id)
            && self.rule_id.as_deref().is_none_or(|r| rule_id == Some(r))
    }
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
        if matches!(self.op, AlertOp::Above | AlertOp::Below) && self.value.as_f64().is_none() {
            return Err("above and below compare numbers: `value` must be a number".into());
        }
        if self.notify.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err("`notify` names a channel, or is left out".into());
        }
        Ok(())
    }

    /// Whether a device's value is the state that counts.
    pub fn matches(&self, actual: &serde_json::Value) -> bool {
        let num = |v: &serde_json::Value| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        };
        match self.op {
            AlertOp::Eq => same(actual, &self.value),
            AlertOp::Ne => !same(actual, &self.value),
            AlertOp::Above => num(actual)
                .zip(self.value.as_f64())
                .is_some_and(|(a, b)| a > b),
            AlertOp::Below => num(actual)
                .zip(self.value.as_f64())
                .is_some_and(|(a, b)| a < b),
        }
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
        let mut s = self.state_label.clone().unwrap_or_else(|| {
            let v = plain(&self.value);
            match self.op {
                AlertOp::Eq => format!("{} is {v}", self.attribute),
                AlertOp::Ne => format!("{} is not {v}", self.attribute),
                AlertOp::Above => format!("{} above {v}", self.attribute),
                AlertOp::Below => format!("{} below {v}", self.attribute),
            }
        });
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

/// JSON equality, with numbers compared as numbers (`1` and `1.0` agree).
pub fn same(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
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
            op: AlertOp::Eq,
            notify: None,
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
    fn compares_numbers_above_and_below() {
        let mut r = rule(None, None);
        r.attribute = "temperature".into();
        r.value = serde_json::json!(-10);
        r.state_label = None;
        r.op = AlertOp::Above;
        assert!(r.matches(&serde_json::json!(-4.5)));
        assert!(r.matches(&serde_json::json!("-2")));
        assert!(!r.matches(&serde_json::json!(-10)));
        assert!(!r.matches(&serde_json::json!(true)));
        assert_eq!(r.describe(), "temperature above -10");
        r.op = AlertOp::Below;
        assert!(r.matches(&serde_json::json!(-12)));
        r.op = AlertOp::Ne;
        assert!(r.matches(&serde_json::json!(3)));
        assert!(!r.matches(&serde_json::json!(-10.0)));
        r.value = serde_json::json!("cold");
        r.op = AlertOp::Above;
        assert!(r.validate().is_err(), "above needs a number");
    }

    fn alert(kind: AlertKind, device: &str, rule: Option<&str>) -> Alert {
        Alert {
            id: format!("{}:{device}", kind.as_str()),
            kind,
            device_id: device.into(),
            rule_id: rule.map(Into::into),
            message: String::new(),
            reason: String::new(),
            severity: AlertSeverity::Warning,
            raised_at: Utc::now(),
            acknowledged: None,
        }
    }

    fn ignore(kind: Option<AlertKind>, device: Option<&str>) -> AlertIgnore {
        AlertIgnore {
            id: "i".into(),
            kind,
            device_id: device.map(Into::into),
            rule_id: None,
            until: None,
            note: None,
            by: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn an_ignore_covers_what_it_names_and_only_that() {
        let now = Utc::now();
        let low = alert(AlertKind::LowBattery, "lock", None);
        let off = alert(AlertKind::Offline, "lock", None);
        let other = alert(AlertKind::LowBattery, "remote", None);
        let every_low = ignore(Some(AlertKind::LowBattery), None);
        assert!(every_low.covers(&low, now) && every_low.covers(&other, now));
        assert!(!every_low.covers(&off, now));
        let lock_low = ignore(Some(AlertKind::LowBattery), Some("lock"));
        assert!(lock_low.covers(&low, now));
        assert!(!lock_low.covers(&other, now) && !lock_low.covers(&off, now));
        let lock_all = ignore(None, Some("lock"));
        assert!(lock_all.covers(&low, now) && lock_all.covers(&off, now));
        let mut by_rule = ignore(None, None);
        by_rule.rule_id = Some("r1".into());
        assert!(by_rule.covers(&alert(AlertKind::Rule, "x", Some("r1")), now));
        assert!(!by_rule.covers(&alert(AlertKind::Rule, "x", Some("r2")), now));
    }

    #[test]
    fn an_ignore_for_a_while_ends() {
        let now = Utc::now();
        let mut i = ignore(Some(AlertKind::Offline), None);
        i.until = Some(now + chrono::Duration::hours(1));
        assert!(i.covers(&alert(AlertKind::Offline, "d", None), now));
        assert!(!i.covers(
            &alert(AlertKind::Offline, "d", None),
            now + chrono::Duration::hours(2)
        ));
    }

    #[test]
    fn ignoring_everything_is_not_an_ignore() {
        assert!(ignore(None, None).validate().is_err());
        assert!(ignore(Some(AlertKind::LowBattery), None).validate().is_ok());
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
