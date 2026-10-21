//! Which automations an alert starts — worked out from their triggers, so an
//! automation written by hand is found as surely as one made from an alert's
//! "Do something…". Nothing is stored beside the rules.
//!
//! An alert's sources are what can raise one: an admin's time rule
//! (`rule:{id}`), one of homeCore's own kinds (`kind:wet`), or a kind a plugin
//! declared (`plugin:{plugin_id}/{topic}`). A trigger's [`AlertFilter`] is
//! linked to every source whose alerts it could match.
//!
//! A source that can't raise anything now — a time rule turned off, a plugin
//! kind set not to show, alerts someone ignored — leaves the automations it
//! starts **waiting**: they don't run, because the alert never comes. That is
//! enforced by the alerts themselves (an ignored or turned-off alert is never
//! raised); this module only says so, for the screens.
//!
//! Deleting a time rule must not leave automations pointing at nothing:
//! [`detach`] takes its triggers out of an automation that has others, and
//! for one it alone started either it goes too or it is kept turned off,
//! its trigger marked [`DELETED_RULE`] — which [`deleted_trigger`] refuses
//! to run until it is given another.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hc_types::alert::{
    plugin_severity, severity_rank, AlertFilter, AlertIgnore, AlertKind, AlertRule,
    AlertTopicSetting, PluginAlertKind, ALERT_DELETED, DELETED_RULE,
};
use hc_types::device::DeviceState;
use hc_types::event::AlertSeverity;
use hc_types::rule::{Rule, Trigger};
use serde::Serialize;
use uuid::Uuid;

/// homeCore's own kinds, and how serious each is when raised.
pub const OWN_KINDS: [(AlertKind, AlertSeverity); 5] = [
    (AlertKind::Wet, AlertSeverity::Critical),
    (AlertKind::Smoke, AlertSeverity::Critical),
    (AlertKind::Offline, AlertSeverity::Warning),
    (AlertKind::Jammed, AlertSeverity::Critical),
    (AlertKind::LowBattery, AlertSeverity::Warning),
];

/// What the house knows about where alerts come from, now.
pub struct Sources<'a> {
    pub rules: &'a [AlertRule],
    /// By plugin id, as declared.
    pub kinds: &'a BTreeMap<String, Vec<PluginAlertKind>>,
    pub topics: &'a [AlertTopicSetting],
    pub ignores: &'a [AlertIgnore],
    pub devices: &'a [DeviceState],
    pub now: DateTime<Utc>,
}

/// One automation, as far as alerts are concerned.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutomationAlerts {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    /// Its triggers that are alerts.
    pub links: Vec<AlertLink>,
    /// How many of its triggers are something else.
    pub other_triggers: usize,
    /// On, but every trigger it has is an alert that can't be raised now.
    pub waiting: bool,
    /// Kept, turned off, when the time rule it started from was deleted.
    pub alert_deleted: bool,
}

/// One alert trigger, and what can start it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlertLink {
    /// Which trigger: 0 is the rule's `trigger`, then its `triggers` in order.
    pub index: usize,
    /// `raised`, `cleared`, `acknowledged` or `active_for`.
    pub when: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
    pub filter: AlertFilter,
    /// Every source whose alerts it could match: `rule:{id}`, `kind:{kind}`,
    /// `plugin:{plugin_id}/{topic}`.
    pub sources: Vec<String>,
    /// Why none of them can start it now; absent while one can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
}

/// Why an alert can't be raised now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Waiting {
    /// Its time rule is turned off, or its plugin kind set not to show.
    Off,
    /// Someone ignored it.
    Ignored,
    /// Its time rule is gone.
    Deleted,
}

/// A trigger's alert filter, with what kind of moment it is.
pub fn alert_trigger(t: &Trigger) -> Option<(&'static str, Option<u32>, &AlertFilter)> {
    match t {
        Trigger::AlertRaised { filter } => Some(("raised", None, filter)),
        Trigger::AlertCleared { filter } => Some(("cleared", None, filter)),
        Trigger::AlertAcknowledged { filter } => Some(("acknowledged", None, filter)),
        Trigger::AlertActiveFor { filter, minutes } => Some(("active_for", Some(*minutes), filter)),
        _ => None,
    }
}

fn alert_filter_mut(t: &mut Trigger) -> Option<&mut AlertFilter> {
    match t {
        Trigger::AlertRaised { filter }
        | Trigger::AlertCleared { filter }
        | Trigger::AlertAcknowledged { filter }
        | Trigger::AlertActiveFor { filter, .. } => Some(filter),
        _ => None,
    }
}

/// A source, while working out links.
enum Source<'a> {
    Rule(&'a AlertRule),
    Own(AlertKind, AlertSeverity),
    Plugin(&'a str, &'a PluginAlertKind),
}

impl Source<'_> {
    fn key(&self) -> String {
        match self {
            Source::Rule(r) => format!("rule:{}", r.id),
            Source::Own(k, _) => format!("kind:{}", k.as_str()),
            Source::Plugin(p, k) => format!("plugin:{p}/{}", k.topic),
        }
    }
}

impl<'a> Sources<'a> {
    fn all(&self) -> Vec<Source<'a>> {
        let mut out: Vec<Source<'a>> = self.rules.iter().map(Source::Rule).collect();
        out.extend(OWN_KINDS.iter().map(|(k, s)| Source::Own(*k, *s)));
        for (p, list) in self.kinds {
            out.extend(list.iter().map(|k| Source::Plugin(p.as_str(), k)));
        }
        out
    }

    fn plugin_of(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.device_id == device_id)
            .map(|d| d.plugin_id.as_str())
    }

    /// Whether an alert from `source` could match `f`.
    fn could_match(&self, f: &AlertFilter, source: &Source) -> bool {
        let severe_enough = |s: AlertSeverity| {
            f.min_severity
                .is_none_or(|m| severity_rank(s) >= severity_rank(m))
        };
        let of_plugin = |ids: &[String], plugin: &str| {
            ids.iter()
                .any(|d| d == plugin || self.plugin_of(d) == Some(plugin))
        };
        match source {
            Source::Rule(r) => {
                let devices: Vec<String> = if f.device_ids.is_empty() {
                    r.device_ids.clone()
                } else {
                    r.device_ids
                        .iter()
                        .filter(|d| f.device_ids.contains(d))
                        .cloned()
                        .collect()
                };
                f.kind.is_none_or(|k| k == AlertKind::Rule)
                    && f.topic.is_none()
                    && f.key.is_none()
                    && f.rule_id.as_deref().is_none_or(|id| id == r.id)
                    && !devices.is_empty()
                    && f.plugin_id
                        .as_deref()
                        .is_none_or(|p| of_plugin(&devices, p))
                    && severe_enough(r.severity)
            }
            Source::Own(kind, severity) => {
                f.kind.is_none_or(|k| k == *kind)
                    && f.topic.is_none()
                    && f.key.is_none()
                    && f.rule_id.is_none()
                    && (f.device_ids.is_empty()
                        || f.plugin_id
                            .as_deref()
                            .is_none_or(|p| of_plugin(&f.device_ids, p)))
                    && severe_enough(*severity)
            }
            Source::Plugin(plugin, k) => {
                f.kind.is_none_or(|x| x == AlertKind::Plugin)
                    && f.plugin_id.as_deref().is_none_or(|p| p == *plugin)
                    && f.topic.as_deref().is_none_or(|t| t == k.topic)
                    && f.rule_id.is_none()
                    && (f.device_ids.is_empty() || of_plugin(&f.device_ids, plugin))
                    && severe_enough(plugin_severity(k.severity))
            }
        }
    }

    /// Why `source` can't raise an alert like `f` now, if it can't.
    fn waiting_on(&self, f: &AlertFilter, source: &Source) -> Option<Waiting> {
        match source {
            Source::Rule(r) => {
                if !r.enabled {
                    return Some(Waiting::Off);
                }
                let devices = if f.device_ids.is_empty() {
                    r.device_ids.clone()
                } else {
                    f.device_ids.clone()
                };
                self.ignored(&Pinned {
                    kind: AlertKind::Rule,
                    rule_id: Some(&r.id),
                    plugin_id: None,
                    topic: None,
                    key: None,
                    devices: &devices,
                })
                .then_some(Waiting::Ignored)
            }
            Source::Own(kind, _) => {
                // "Any YoLink leak sensor" is ignored when each of them is.
                let mine: Vec<String>;
                let devices = match (&f.plugin_id, f.device_ids.is_empty()) {
                    (Some(p), true) => {
                        mine = self
                            .devices
                            .iter()
                            .filter(|d| &d.plugin_id == p)
                            .map(|d| d.device_id.clone())
                            .collect();
                        &mine
                    }
                    _ => &f.device_ids,
                };
                self.ignored(&Pinned {
                    kind: *kind,
                    rule_id: None,
                    plugin_id: None,
                    topic: None,
                    key: None,
                    devices,
                })
                .then_some(Waiting::Ignored)
            }
            Source::Plugin(plugin, k) => {
                let on = self
                    .topics
                    .iter()
                    .find(|t| t.plugin_id == *plugin && t.topic == k.topic)
                    .and_then(|t| t.enabled)
                    .unwrap_or(k.default_on);
                if !on {
                    return Some(Waiting::Off);
                }
                self.ignored(&Pinned {
                    kind: AlertKind::Plugin,
                    rule_id: None,
                    plugin_id: Some(plugin),
                    topic: Some(&k.topic),
                    key: f.key.as_deref(),
                    devices: &f.device_ids,
                })
                .then_some(Waiting::Ignored)
            }
        }
    }

    /// Whether what a person ignored covers every alert `p` describes.
    fn ignored(&self, p: &Pinned) -> bool {
        let fits = |i: &AlertIgnore| {
            i.active(self.now)
                && i.kind.is_none_or(|k| k == p.kind)
                && i.rule_id.as_deref().is_none_or(|r| p.rule_id == Some(r))
                && i.plugin_id
                    .as_deref()
                    .is_none_or(|x| p.plugin_id == Some(x))
                && i.topic.as_deref().is_none_or(|x| p.topic == Some(x))
                && i.key.as_deref().is_none_or(|x| p.key == Some(x))
        };
        if p.devices.is_empty() {
            self.ignores
                .iter()
                .any(|i| fits(i) && i.device_id.is_none())
        } else {
            p.devices.iter().all(|d| {
                self.ignores
                    .iter()
                    .any(|i| fits(i) && i.device_id.as_deref().is_none_or(|x| x == d))
            })
        }
    }

    /// An alert trigger's sources, and whether it waits.
    pub fn link(&self, index: usize, t: &Trigger) -> Option<AlertLink> {
        let (when, minutes, filter) = alert_trigger(t)?;
        let matched: Vec<Source> = self
            .all()
            .into_iter()
            .filter(|s| self.could_match(filter, s))
            .collect();
        let waiting = if matched.is_empty() {
            filter.rule_id.is_some().then_some(Waiting::Deleted)
        } else {
            let each: Vec<Option<Waiting>> =
                matched.iter().map(|s| self.waiting_on(filter, s)).collect();
            if each.iter().all(Option::is_some) {
                each[0]
            } else {
                None
            }
        };
        Some(AlertLink {
            index,
            when,
            minutes,
            filter: filter.clone(),
            sources: matched.iter().map(Source::key).collect(),
            waiting,
        })
    }

    /// An automation's alert triggers; None when it has none.
    pub fn automation(&self, rule: &Rule) -> Option<AutomationAlerts> {
        let links: Vec<AlertLink> = rule
            .all_triggers()
            .enumerate()
            .filter_map(|(i, t)| self.link(i, t))
            .collect();
        if links.is_empty() {
            return None;
        }
        let other_triggers = rule.all_triggers().count() - links.len();
        let alert_deleted = links.iter().any(|l| l.filter.rule_deleted());
        Some(AutomationAlerts {
            id: rule.id,
            name: rule.name.clone(),
            enabled: rule.enabled,
            waiting: rule.enabled
                && rule.error.is_none()
                && other_triggers == 0
                && links.iter().all(|l| l.waiting.is_some()),
            other_triggers,
            alert_deleted,
            links,
        })
    }

    /// Every automation an alert starts, or can't any longer.
    pub fn automations(&self, rules: &[Rule]) -> Vec<AutomationAlerts> {
        rules.iter().filter_map(|r| self.automation(r)).collect()
    }
}

/// An alert described as far as a source and a filter pin it.
struct Pinned<'a> {
    kind: AlertKind,
    rule_id: Option<&'a str>,
    plugin_id: Option<&'a str>,
    topic: Option<&'a str>,
    key: Option<&'a str>,
    /// Empty: any device.
    devices: &'a [String],
}

/// Whether the automation has a trigger naming this time rule.
pub fn starts_from_rule(rule: &Rule, alert_rule_id: &str) -> bool {
    rule.all_triggers().any(|t| names_rule(t, alert_rule_id))
}

fn names_rule(t: &Trigger, alert_rule_id: &str) -> bool {
    alert_trigger(t).is_some_and(|(_, _, f)| f.rule_id.as_deref() == Some(alert_rule_id))
}

/// Whether the automation keeps a trigger once this time rule's are gone.
pub fn keeps_running_without(rule: &Rule, alert_rule_id: &str) -> bool {
    rule.all_triggers().any(|t| !names_rule(t, alert_rule_id))
}

/// What deleting a time rule did to one automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// It never named the rule.
    Untouched,
    /// It had other triggers: only the rule's went, and it runs on.
    LostTriggers,
    /// The rule was all that started it: turned off and marked, to be given
    /// another trigger or deleted.
    TurnedOff,
    /// The rule was all that started it, and it is to be deleted.
    Delete,
}

/// Take a deleted time rule out of an automation. With `keep`, one that
/// only the rule started is turned off and marked; without, it is left as it
/// was for the caller to delete ([`Detached::Delete`]).
pub fn detach(rule: &mut Rule, alert_rule_id: &str, keep: bool) -> Detached {
    if !starts_from_rule(rule, alert_rule_id) {
        return Detached::Untouched;
    }
    if keeps_running_without(rule, alert_rule_id) {
        let mut rest: Vec<Trigger> = rule
            .all_triggers()
            .filter(|t| !names_rule(t, alert_rule_id))
            .cloned()
            .collect();
        rule.trigger = rest.remove(0);
        rule.triggers = rest;
        return Detached::LostTriggers;
    }
    if !keep {
        return Detached::Delete;
    }
    for t in rule.all_triggers_mut() {
        if let Some(f) = alert_filter_mut(t) {
            if f.rule_id.as_deref() == Some(alert_rule_id) {
                f.rule_id = Some(format!("{DELETED_RULE}{alert_rule_id}"));
            }
        }
    }
    rule.enabled = false;
    rule.error = Some(ALERT_DELETED.to_string());
    Detached::TurnedOff
}

/// Refuse to run an automation whose trigger names a deleted time rule.
pub fn deleted_trigger(rule: &Rule) -> Option<&'static str> {
    rule.all_triggers()
        .filter_map(alert_trigger)
        .any(|(_, _, f)| f.rule_deleted())
        .then_some(ALERT_DELETED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(id: &str, enabled: bool) -> AlertRule {
        serde_json::from_value(json!({
            "id": id, "name": format!("Rule {id}"), "enabled": enabled,
            "device_ids": ["freezer"], "attribute": "temperature", "value": -10,
            "op": "above", "for_minutes": 15
        }))
        .unwrap()
    }
    fn auto(name: &str, triggers: serde_json::Value) -> Rule {
        let list = triggers.as_array().unwrap().clone();
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "name": name, "enabled": true, "priority": 0,
            "trigger": list[0], "triggers": list[1..],
            "actions": [{ "enabled": true, "action": { "LogMessage": { "message": "{alert.message}" } } }]
        }))
        .unwrap()
    }
    fn device(id: &str, plugin: &str) -> DeviceState {
        DeviceState::new(id, id, plugin)
    }
    fn kinds() -> BTreeMap<String, Vec<PluginAlertKind>> {
        let mut k = BTreeMap::new();
        k.insert(
            "plugin.wled".to_string(),
            vec![PluginAlertKind {
                topic: "firmware_update".into(),
                label: "Firmware update available".into(),
                description: None,
                severity: AlertSeverity::Info,
                default_on: true,
            }],
        );
        k
    }
    fn ignore(v: serde_json::Value) -> AlertIgnore {
        serde_json::from_value(v).unwrap()
    }

    struct World {
        rules: Vec<AlertRule>,
        kinds: BTreeMap<String, Vec<PluginAlertKind>>,
        topics: Vec<AlertTopicSetting>,
        ignores: Vec<AlertIgnore>,
        devices: Vec<DeviceState>,
    }
    impl World {
        fn new() -> Self {
            Self {
                rules: vec![rule("r1", true), rule("r2", false)],
                kinds: kinds(),
                topics: vec![],
                ignores: vec![],
                devices: vec![
                    device("freezer", "plugin.yolink"),
                    device("dishwasher", "plugin.yolink"),
                    device("sink", "plugin.yolink"),
                    device("porch", "plugin.hue"),
                    device("wled_1", "plugin.wled"),
                ],
            }
        }
        fn sources(&self) -> Sources<'_> {
            Sources {
                rules: &self.rules,
                kinds: &self.kinds,
                topics: &self.topics,
                ignores: &self.ignores,
                devices: &self.devices,
                now: Utc::now(),
            }
        }
    }

    #[test]
    fn a_trigger_is_linked_to_what_can_raise_its_alert() {
        let w = World::new();
        let s = w.sources();
        let a = auto(
            "Freezer",
            json!([{ "AlertRaised": { "filter": { "kind": "rule", "rule_id": "r1" } } }]),
        );
        let got = s.automation(&a).unwrap();
        assert_eq!(got.links[0].sources, vec!["rule:r1"]);
        assert_eq!(got.links[0].when, "raised");
        assert!(!got.waiting);

        // Any YoLink leak sensor: homeCore's wet kind, nothing else.
        let wet = auto(
            "Leaks",
            json!([{ "AlertActiveFor": { "filter": { "kind": "wet", "plugin_id": "plugin.yolink" }, "minutes": 10 } }]),
        );
        let got = s.automation(&wet).unwrap();
        assert_eq!(got.links[0].sources, vec!["kind:wet"]);
        assert_eq!(got.links[0].minutes, Some(10));

        // A plugin's kind.
        let fw = auto(
            "Update",
            json!([{ "AlertRaised": { "filter": { "plugin_id": "plugin.wled", "topic": "firmware_update" } } }]),
        );
        assert_eq!(
            s.automation(&fw).unwrap().links[0].sources,
            vec!["plugin:plugin.wled/firmware_update"]
        );

        // Anything this serious or worse: every source that is.
        let serious = auto(
            "Serious",
            json!([{ "AlertRaised": { "filter": { "min_severity": "critical" } } }]),
        );
        assert_eq!(
            s.automation(&serious).unwrap().links[0].sources,
            vec!["kind:wet", "kind:smoke", "kind:jammed"]
        );

        // One that watches a device, not an alert, isn't an alert's.
        let state = auto(
            "Valve",
            json!([{ "DeviceStateChanged": { "device_id": "dishwasher", "attribute": "water_detected" } }]),
        );
        assert!(s.automation(&state).is_none());
    }

    #[test]
    fn an_alert_that_cant_be_raised_leaves_its_automation_waiting() {
        let mut w = World::new();
        // A time rule turned off.
        let off = auto(
            "Close OH1",
            json!([{ "AlertRaised": { "filter": { "rule_id": "r2" } } }]),
        );
        let got = w.sources().automation(&off).unwrap();
        assert_eq!(got.links[0].waiting, Some(Waiting::Off));
        assert!(got.waiting);

        // With another trigger that can still happen, it isn't waiting.
        let two = auto(
            "Two",
            json!([
                { "AlertRaised": { "filter": { "rule_id": "r2" } } },
                { "AlertRaised": { "filter": { "rule_id": "r1" } } }
            ]),
        );
        assert!(!w.sources().automation(&two).unwrap().waiting);

        // A plugin's kind set not to show.
        let fw = auto(
            "Update",
            json!([{ "AlertRaised": { "filter": { "plugin_id": "plugin.wled", "topic": "firmware_update" } } }]),
        );
        assert!(!w.sources().automation(&fw).unwrap().waiting);
        w.topics.push(AlertTopicSetting {
            plugin_id: "plugin.wled".into(),
            topic: "firmware_update".into(),
            enabled: Some(false),
            notify: None,
        });
        assert_eq!(
            w.sources().automation(&fw).unwrap().links[0].waiting,
            Some(Waiting::Off)
        );

        // Ignored: one sensor's wet alerts, then the time rule's.
        let leak = auto(
            "Dishwasher",
            json!([{ "AlertRaised": { "filter": { "kind": "wet", "device_ids": ["dishwasher"] } } }]),
        );
        assert!(!w.sources().automation(&leak).unwrap().waiting);
        w.ignores
            .push(ignore(json!({ "kind": "wet", "device_id": "dishwasher" })));
        assert_eq!(
            w.sources().automation(&leak).unwrap().links[0].waiting,
            Some(Waiting::Ignored)
        );
        // Another sensor's alerts aren't covered by that.
        let sink = auto(
            "Sink",
            json!([{ "AlertRaised": { "filter": { "kind": "wet", "device_ids": ["sink"] } } }]),
        );
        assert!(!w.sources().automation(&sink).unwrap().waiting);
        let freezer = auto(
            "Freezer",
            json!([{ "AlertRaised": { "filter": { "rule_id": "r1" } } }]),
        );
        assert!(!w.sources().automation(&freezer).unwrap().waiting);
        w.ignores.push(ignore(json!({ "rule_id": "r1" })));
        assert!(w.sources().automation(&freezer).unwrap().waiting);
        // An ignore that has run out hides nothing.
        w.ignores = vec![ignore(
            json!({ "rule_id": "r1", "until": "2000-01-01T00:00:00Z" }),
        )];
        assert!(!w.sources().automation(&freezer).unwrap().waiting);

        // A turned-off automation isn't "waiting": it is off.
        let mut quiet = off.clone();
        quiet.enabled = false;
        assert!(!w.sources().automation(&quiet).unwrap().waiting);
    }

    #[test]
    fn deleting_a_rule_takes_only_its_triggers_or_holds_the_automation_off() {
        let only = || {
            auto(
                "Flash",
                json!([{ "AlertRaised": { "filter": { "kind": "rule", "rule_id": "r1" } } }]),
            )
        };
        let two = || {
            auto(
                "Say it",
                json!([
                    { "AlertRaised": { "filter": { "rule_id": "r1" } } },
                    { "AlertRaised": { "filter": { "rule_id": "power" } } }
                ]),
            )
        };
        let other = auto(
            "Other",
            json!([{ "AlertRaised": { "filter": { "kind": "rule" } } }]),
        );

        assert!(starts_from_rule(&only(), "r1"));
        assert!(!keeps_running_without(&only(), "r1"));
        assert!(keeps_running_without(&two(), "r1"));
        // "Any of your rules" isn't this rule's.
        assert!(!starts_from_rule(&other, "r1"));

        let mut a = two();
        assert_eq!(detach(&mut a, "r1", true), Detached::LostTriggers);
        assert!(a.enabled && a.triggers.is_empty());
        assert!(
            matches!(&a.trigger, Trigger::AlertRaised { filter } if filter.rule_id.as_deref() == Some("power"))
        );

        let mut b = only();
        assert_eq!(detach(&mut b, "r1", false), Detached::Delete);
        // Left as it was, for the caller to delete.
        assert!(b.enabled && b.error.is_none() && starts_from_rule(&b, "r1"));

        let mut c = only();
        assert_eq!(detach(&mut c, "r1", true), Detached::TurnedOff);
        assert!(!c.enabled);
        assert_eq!(c.error.as_deref(), Some(ALERT_DELETED));
        assert_eq!(deleted_trigger(&c), Some(ALERT_DELETED));
        assert_eq!(deleted_trigger(&only()), None);

        // Shown as deleted, and not waiting: it's off.
        let w = World::new();
        let got = w.sources().automation(&c).unwrap();
        assert!(got.alert_deleted && !got.waiting);
        assert_eq!(got.links[0].waiting, Some(Waiting::Deleted));
        assert!(got.links[0].sources.is_empty());

        let mut d = other.clone();
        assert_eq!(detach(&mut d, "r1", true), Detached::Untouched);
        assert_eq!(d, other);
    }
}
