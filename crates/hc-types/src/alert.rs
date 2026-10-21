//! Alerts: what the house's own state says needs attention (H4).
//!
//! homeCore raises and clears them — whether or not any screen is open, the
//! same for every client — and announces each on the event stream
//! (`alert_raised`, `alert_cleared`). Screens only show them.
//!
//! Three sources:
//! - **built in:** something wet, smoke, a device offline, a lock jammed, a
//!   battery low;
//! - **time rules** an admin writes: "the garage door open after 22:00",
//!   "the front door unlocked for 30 minutes", "the freezer above -10";
//! - **plugins**, about their own devices: "firmware 17.0.0 is out", "the
//!   bridge needs its button pressed". A plugin declares the kinds it can
//!   raise ([`PluginAlertKind`]) and publishes, per device, the ones that are
//!   true now ([`PluginAlert`]); homeCore decides whether each is shown and
//!   where it is sent ([`AlertTopicSetting`]).
//!
//! A person can **acknowledge** an alert (it stays, marked as seen, until its
//! condition ends) or **ignore** a kind of alert, a device's alerts, or both
//! — for good or for a while. An ignored alert is not raised at all.

use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::AlertSeverity;

/// Why an alert was raised.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// A plugin's, about one of its devices or itself; `plugin_id` and
    /// `topic` say which.
    Plugin,
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
            AlertKind::Plugin => "plugin",
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
    /// A plugin's alert: the plugin that raised it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// A plugin's alert: which of its declared kinds (`firmware_update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// A plugin's alert: what this one is about within its kind — the version
    /// on offer, say — so a person can skip this one without muting the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// A plugin's alert: the one thing to do about it ("Update…").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<AlertAction>,
    /// The plugin the alert's device belongs to, whatever raised it — so an
    /// automation can mean "any YoLink leak sensor" ([`AlertFilter::plugin_id`])
    /// without listing them. Unlike `plugin_id`, set for homeCore's own kinds
    /// and time rules' too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_plugin_id: Option<String>,
}

/// The button on a plugin's alert: one of the device's own actions, or one of
/// the plugin's, run with these params when someone (or an automation)
/// presses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertAction {
    /// What the button says: "Update…".
    pub label: String,
    /// The action to run: a device action id (`install_firmware`) when the
    /// alert is about a device, else one of the plugin's manifest actions.
    pub id: String,
    /// `device` (the default) runs it on the alert's device; `plugin` runs it
    /// as a plugin action.
    #[serde(default, skip_serializing_if = "AlertActionTarget::is_device")]
    pub target: AlertActionTarget,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    /// Ask before running it: it interrupts something.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// Where an [`AlertAction`] runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertActionTarget {
    #[default]
    Device,
    Plugin,
}

impl AlertActionTarget {
    fn is_device(&self) -> bool {
        *self == AlertActionTarget::Device
    }
}

/// A kind of alert a plugin can raise, declared on its capability manifest —
/// which is how the settings screen can list it before it has ever happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginAlertKind {
    /// `snake_case`, stable: what [`PluginAlert::topic`] names.
    pub topic: String,
    /// For a person: "Firmware update available".
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How serious one is. Never `critical`: that is homeCore's own (smoke,
    /// water); a plugin asking for it gets `error`.
    #[serde(default = "warning")]
    pub severity: AlertSeverity,
    /// Shown unless someone turns it off (true), or only once someone turns
    /// it on (false) — right for news that isn't a problem.
    #[serde(default = "yes")]
    pub default_on: bool,
}

/// One alert a plugin says is true now, for one device (or for itself).
///
/// **Current state, not an event**: the plugin publishes the whole list for a
/// device each time, and homeCore replaces what it held — an alert it stops
/// sending has cleared. The same contract as plugin notices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginAlert {
    /// One of the plugin's declared kinds.
    pub topic: String,
    /// What this one is about within its kind (`17.0.0`); a different key is
    /// a different alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Without the device's name, for its card: "Firmware 17.0.0 is out".
    pub reason: String,
    /// With it, when the plugin wants other words than "{name}: {reason}".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Overrides the kind's declared severity for this one; capped at `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<AlertSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<AlertAction>,
    /// When the condition began, if the plugin knows; else when homeCore
    /// first heard of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

impl PluginAlert {
    pub fn new(topic: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            key: None,
            reason: reason.into(),
            message: None,
            severity: None,
            action: None,
            since: None,
        }
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn severity(mut self, severity: AlertSeverity) -> Self {
        self.severity = Some(severity);
        self
    }

    pub fn action(mut self, action: AlertAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// What the house decided about one kind of plugin alert: shown or not, and
/// where it is sent. Absent fields mean the plugin's own default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertTopicSetting {
    pub plugin_id: String,
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// A notification channel (`all` for every one); None: shown, not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<String>,
}

/// Which alerts an automation means — for a trigger, a check, or a step that
/// acts on alerts. Every field narrows; left empty, it means any alert.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertFilter {
    /// Only this kind (`plugin` for any plugin's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AlertKind>,
    /// Only this plugin's: the alerts it raised, and homeCore's own about its
    /// devices — `wet` with `plugin.yolink` is any YoLink leak sensor, those
    /// added later included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Only this kind of plugin alert (`firmware_update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Only these devices' alerts; any device when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_ids: Vec<String>,
    /// Only this serious or more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<AlertSeverity>,
    /// Only the one about this (`17.0.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Only one of a time rule's. `DELETED:{id}` once that rule is deleted and
    /// the automation was kept: it matches nothing, and the automation is
    /// held off until it is given another trigger ([`DELETED_RULE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
}

/// What an [`AlertFilter::rule_id`] becomes when its rule is deleted and an
/// automation it started is kept, turned off: `DELETED:{id}`.
pub const DELETED_RULE: &str = "DELETED:";

/// Why an automation kept after its alert rule was deleted can't run.
pub const ALERT_DELETED: &str = "its alert was deleted — choose another trigger or delete it";

/// How serious, as a number to compare: info 0 … critical 3.
pub fn severity_rank(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Info => 0,
        AlertSeverity::Warning => 1,
        AlertSeverity::Error => 2,
        AlertSeverity::Critical => 3,
    }
}

impl AlertFilter {
    /// Names a time rule that has since been deleted.
    pub fn rule_deleted(&self) -> bool {
        self.rule_id
            .as_deref()
            .is_some_and(|r| r.starts_with(DELETED_RULE))
    }

    pub fn matches(&self, a: &Alert) -> bool {
        let same = |want: &Option<String>, have: &Option<String>| {
            want.as_deref().is_none_or(|w| have.as_deref() == Some(w))
        };
        let plugin = a.plugin_id.as_deref().or(a.device_plugin_id.as_deref());
        self.kind.is_none_or(|k| k == a.kind)
            && self.plugin_id.as_deref().is_none_or(|w| plugin == Some(w))
            && same(&self.topic, &a.topic)
            && same(&self.key, &a.key)
            && same(&self.rule_id, &a.rule_id)
            && (self.device_ids.is_empty() || self.device_ids.contains(&a.device_id))
            && self
                .min_severity
                .is_none_or(|m| severity_rank(a.severity) >= severity_rank(m))
    }
}

/// The highest severity a plugin may raise.
pub fn plugin_severity(s: AlertSeverity) -> AlertSeverity {
    match s {
        AlertSeverity::Critical => AlertSeverity::Error,
        other => other,
    }
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
    /// Only this plugin's alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Only this kind of plugin alert (with `plugin_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Only the plugin alert about this (`17.0.0`): "skip this version".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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
        if self.kind.is_none()
            && self.device_id.is_none()
            && self.rule_id.is_none()
            && self.plugin_id.is_none()
        {
            return Err("say which alerts: a kind, a device, a rule or a plugin".into());
        }
        if (self.topic.is_some() || self.key.is_some()) && self.plugin_id.is_none() {
            return Err("a `topic` or `key` belongs to a plugin: say which `plugin_id`".into());
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
        let same = |mine: &Option<String>, theirs: &Option<String>| {
            mine.as_deref().is_none_or(|m| theirs.as_deref() == Some(m))
        };
        self.covers_parts(alert.kind, &alert.device_id, alert.rule_id.as_deref(), now)
            && same(&self.plugin_id, &alert.plugin_id)
            && same(&self.topic, &alert.topic)
            && same(&self.key, &alert.key)
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
            plugin_id: None,
            topic: None,
            key: None,
            action: None,
            device_plugin_id: None,
        }
    }

    fn ignore(kind: Option<AlertKind>, device: Option<&str>) -> AlertIgnore {
        AlertIgnore {
            id: "i".into(),
            kind,
            device_id: device.map(Into::into),
            rule_id: None,
            plugin_id: None,
            topic: None,
            key: None,
            until: None,
            note: None,
            by: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn skipping_one_plugin_alert_leaves_the_next_of_its_kind() {
        let now = Utc::now();
        let update = |key: &str| Alert {
            plugin_id: Some("plugin.wled".into()),
            topic: Some("firmware_update".into()),
            key: Some(key.into()),
            ..alert(AlertKind::Plugin, "wled_1", None)
        };
        let skip = AlertIgnore {
            plugin_id: Some("plugin.wled".into()),
            topic: Some("firmware_update".into()),
            key: Some("17.0.0".into()),
            device_id: Some("wled_1".into()),
            ..ignore(None, None)
        };
        assert!(skip.validate().is_ok());
        assert!(skip.covers(&update("17.0.0"), now));
        assert!(!skip.covers(&update("17.1.0"), now));
        // The whole kind, on every device.
        let kind = AlertIgnore {
            plugin_id: Some("plugin.wled".into()),
            topic: Some("firmware_update".into()),
            ..ignore(None, None)
        };
        assert!(kind.covers(&update("17.1.0"), now));
        assert!(!kind.covers(&alert(AlertKind::Offline, "wled_1", None), now));
        // A topic says nothing without its plugin.
        let loose = AlertIgnore {
            topic: Some("firmware_update".into()),
            ..ignore(Some(AlertKind::Plugin), None)
        };
        assert!(loose.validate().is_err());
    }

    #[test]
    fn a_plugin_cannot_raise_a_critical_alert() {
        assert_eq!(
            plugin_severity(AlertSeverity::Critical),
            AlertSeverity::Error
        );
        assert_eq!(plugin_severity(AlertSeverity::Info), AlertSeverity::Info);
        let k: PluginAlertKind =
            serde_json::from_str(r#"{"topic":"firmware_update","label":"Firmware update"}"#)
                .unwrap();
        assert_eq!((k.severity, k.default_on), (AlertSeverity::Warning, true));
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
    fn a_plugin_filter_means_its_devices_for_homecores_own_kinds_too() {
        let leak = |plugin: &str| Alert {
            device_plugin_id: Some(plugin.into()),
            ..alert(AlertKind::Wet, "dishwasher", None)
        };
        let any_yolink_leak = AlertFilter {
            kind: Some(AlertKind::Wet),
            plugin_id: Some("plugin.yolink".into()),
            ..Default::default()
        };
        assert!(any_yolink_leak.matches(&leak("plugin.yolink")));
        assert!(!any_yolink_leak.matches(&leak("plugin.zwave")));
        // An alert from before the field: not this plugin's.
        assert!(!any_yolink_leak.matches(&alert(AlertKind::Wet, "dishwasher", None)));
        // A plugin's own alert still matches by the plugin that raised it.
        let fw = Alert {
            plugin_id: Some("plugin.wled".into()),
            topic: Some("firmware_update".into()),
            ..alert(AlertKind::Plugin, "wled_1", None)
        };
        let wled = AlertFilter {
            plugin_id: Some("plugin.wled".into()),
            ..Default::default()
        };
        assert!(wled.matches(&fw));
        // A deleted rule's placeholder matches nothing.
        let gone = AlertFilter {
            rule_id: Some(format!("{DELETED_RULE}r1")),
            ..Default::default()
        };
        assert!(gone.rule_deleted());
        assert!(!gone.matches(&alert(AlertKind::Rule, "x", Some("r1"))));
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
