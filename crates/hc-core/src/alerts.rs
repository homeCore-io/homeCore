//! Alerts (H4): homeCore decides what needs attention and announces it, so
//! every screen agrees and nothing waits for a screen to be open (M6).
//!
//! The engine works out the whole set of alerts the house's state calls for
//! — from the stored devices and the time rules — whenever something
//! relevant happens and every 30 seconds (time rules depend on the clock),
//! then announces the difference: `AlertRaised` for a new alert or one whose
//! words changed, `AlertCleared` for one whose condition ended. Working it
//! out whole, rather than tracking edges, means a missed event or a restart
//! can't leave a stale alert behind.
//!
//! "Held for" uses the device's persisted `attributes_changed_at`, so a rule
//! like "unlocked for 30 minutes" survives a restart. Being offline has no
//! such record, so it counts from when this engine first saw it.
//!
//! What a person ignored (stored) is left out before the difference is
//! announced, so an ignored alert clears. An acknowledgement is kept here,
//! for as long as its alert lasts — a restart forgets it, and the alert is
//! shown as new. A new alert is sent to its rule's channel, or for the
//! built-in kinds to `[alerts] notify_channel` (low batteries have
//! `[battery]`'s own); not on the first pass after a start, which would
//! re-send everything already true.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, NaiveTime, Utc};
use hc_notify::NotificationService;
use hc_state::StateStore;
use hc_types::alert::{Acknowledged, Alert, AlertIgnore, AlertKind, AlertOp, AlertRule};
use hc_types::device::DeviceState;
use hc_types::event::{AlertSeverity, Event};
use serde_json::Value;
use tokio::sync::{watch, Notify};
use tracing::{info, warn};

use crate::battery_watcher::BatteryConfig;
use crate::EventBus;

/// How long a device must be offline before it is an alert — long enough
/// that a bridge restarting doesn't raise one for every device behind it.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(5 * 60);
/// How often the whole set is worked out again, for rules that depend on
/// the time of day or on how long something has lasted.
const TICK: Duration = Duration::from_secs(30);
/// After an event, wait this long for its neighbours before working the set
/// out, so a burst of state changes costs one pass.
const SETTLE: Duration = Duration::from_millis(300);

/// The active alerts, and a way to wake the engine when rules change. Shared
/// with the API, which lists the alerts and saves the rules.
#[derive(Clone, Default)]
pub struct AlertsHandle {
    active: Arc<RwLock<BTreeMap<String, Alert>>>,
    /// Acknowledgements, by alert id, while their alerts last.
    acks: Arc<RwLock<HashMap<String, Acknowledged>>>,
    wake: Arc<Notify>,
}

impl AlertsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Active alerts, most serious first, then newest.
    pub fn list(&self) -> Vec<Alert> {
        let mut out: Vec<Alert> = self
            .active
            .read()
            .map(|a| a.values().cloned().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| {
            rank(a.severity)
                .cmp(&rank(b.severity))
                .then(b.raised_at.cmp(&a.raised_at))
        });
        out
    }

    /// The time rules changed: work the alerts out again now.
    pub fn rules_changed(&self) {
        self.wake.notify_one();
    }

    /// Mark an active alert seen. The alert as it now is, or None when no
    /// such alert is active. Acknowledging it again keeps the first.
    pub fn acknowledge(&self, id: &str, by: &str) -> Option<Alert> {
        let mut active = self.active.write().ok()?;
        let alert = active.get_mut(id)?;
        if alert.acknowledged.is_none() {
            let ack = Acknowledged {
                by: by.to_string(),
                at: Utc::now(),
            };
            if let Ok(mut acks) = self.acks.write() {
                acks.insert(id.to_string(), ack.clone());
            }
            alert.acknowledged = Some(ack);
        }
        Some(alert.clone())
    }
}

/// Where new alerts are sent, besides a rule's own channel.
#[derive(Clone, Default)]
pub struct AlertNotify {
    pub service: Option<Arc<NotificationService>>,
    /// For the built-in kinds other than a low battery.
    pub channel: Option<String>,
}

fn rank(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Critical => 0,
        AlertSeverity::Error => 1,
        AlertSeverity::Warning => 2,
        AlertSeverity::Info => 3,
    }
}

/// What the engine remembers between passes that the store doesn't.
#[derive(Default)]
pub struct Memory {
    /// When each device was first seen offline.
    offline_since: HashMap<String, DateTime<Utc>>,
    /// When a rule's state was first seen on a device whose attribute has no
    /// change time recorded.
    held_since: HashMap<String, DateTime<Utc>>,
}

/// Start the engine. It reads the store, so it needs nothing from before it
/// started; it listens to the bus only to know when to look again.
pub fn spawn(
    pub_bus: EventBus,
    state: StateStore,
    handle: AlertsHandle,
    battery: Option<watch::Receiver<BatteryConfig>>,
    notify: AlertNotify,
) {
    tokio::spawn(run(pub_bus, state, handle, battery, notify));
}

async fn run(
    pub_bus: EventBus,
    state: StateStore,
    handle: AlertsHandle,
    battery: Option<watch::Receiver<BatteryConfig>>,
    notify: AlertNotify,
) {
    info!("Alert engine starting");
    let mut rx = pub_bus.subscribe();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut memory = Memory::default();
    let mut primed = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = handle.wake.notified() => {}
            event = rx.recv() => match event {
                Ok(e) if relevant(&e) => {
                    tokio::time::sleep(SETTLE).await;
                    // Whatever else arrived meanwhile is covered by this pass.
                    while rx.try_recv().is_ok() {}
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("Alert engine: event bus closed, stopping");
                    return;
                }
            },
        }

        let devices = match state.list_devices().await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "Alert engine could not read devices");
                continue;
            }
        };
        let rules = state.list_alert_rules().await.unwrap_or_else(|e| {
            warn!(error = %e, "Alert engine could not read its rules");
            Vec::new()
        });
        let cfg = battery
            .as_ref()
            .map(|b| b.borrow().clone())
            .unwrap_or_default();
        let ignores = state.list_alert_ignores().await.unwrap_or_else(|e| {
            warn!(error = %e, "Alert engine could not read what is ignored");
            Vec::new()
        });
        let before = handle.active.read().map(|a| a.clone()).unwrap_or_default();
        let now = Utc::now();
        let mut after = desired(
            &devices,
            &rules,
            &before,
            now,
            hc_time::now_local().time(),
            &cfg,
            &mut memory,
        );
        unignored(&mut after, &ignores, now);
        if let Ok(mut acks) = handle.acks.write() {
            acks.retain(|id, _| after.contains_key(id));
            for (id, alert) in after.iter_mut() {
                alert.acknowledged = acks.get(id).cloned();
            }
        }

        if primed {
            for alert in after.values().filter(|a| !before.contains_key(&a.id)) {
                send(&notify, &rules, alert).await;
            }
        }
        primed = true;

        for (id, alert) in &after {
            if before.get(id) != Some(alert) {
                let _ = pub_bus.publish(Event::AlertRaised {
                    timestamp: now,
                    alert: alert.clone(),
                });
            }
        }
        for (id, alert) in &before {
            if !after.contains_key(id) {
                let _ = pub_bus.publish(Event::AlertCleared {
                    timestamp: now,
                    alert_id: id.clone(),
                    device_id: alert.device_id.clone(),
                });
            }
        }
        if let Ok(mut active) = handle.active.write() {
            *active = after;
        }
    }
}

/// Leave out what a person chose not to see.
pub fn unignored(
    alerts: &mut BTreeMap<String, Alert>,
    ignores: &[AlertIgnore],
    now: DateTime<Utc>,
) {
    alerts.retain(|_, a| !ignores.iter().any(|i| i.covers(a, now)));
}

/// The channel a new alert goes to, if any.
pub fn channel_for<'a>(
    alert: &Alert,
    rules: &'a [AlertRule],
    builtin: Option<&'a str>,
) -> Option<&'a str> {
    match alert.kind {
        AlertKind::Rule => rules
            .iter()
            .find(|r| Some(r.id.as_str()) == alert.rule_id.as_deref())
            .and_then(|r| r.notify.as_deref()),
        // `[battery]` has its own notification.
        AlertKind::LowBattery => None,
        _ => builtin,
    }
    .filter(|c| !c.trim().is_empty())
}

async fn send(notify: &AlertNotify, rules: &[AlertRule], alert: &Alert) {
    let Some(svc) = notify.service.as_ref() else {
        return;
    };
    let Some(channel) = channel_for(alert, rules, notify.channel.as_deref()) else {
        return;
    };
    let title = match alert.severity {
        AlertSeverity::Critical => "Alert",
        _ => "Needs attention",
    };
    if let Err(e) = svc.notify(channel, title, &alert.message).await {
        warn!(channel, alert = %alert.id, error = %e, "Alert notification failed");
    }
}

/// Events that can change what is alarming. The engine's own are not.
fn relevant(e: &Event) -> bool {
    matches!(
        e,
        Event::DeviceStateChanged { .. }
            | Event::DeviceAvailabilityChanged { .. }
            | Event::DeviceAdded { .. }
            | Event::DeviceRemoved { .. }
            | Event::DeviceMetadataChanged { .. }
            | Event::DeviceNameChanged { .. }
    )
}

/// Every alert the house's state calls for now. `active` is the set from
/// the last pass: an alert still called for keeps when it was raised, and a
/// low battery stays low until it has recovered past the band.
pub fn desired(
    devices: &[DeviceState],
    rules: &[AlertRule],
    active: &BTreeMap<String, Alert>,
    now: DateTime<Utc>,
    local: NaiveTime,
    battery: &BatteryConfig,
    memory: &mut Memory,
) -> BTreeMap<String, Alert> {
    let mut out = BTreeMap::new();
    let mut add = |mut alert: Alert| {
        if let Some(was) = active.get(&alert.id) {
            alert.raised_at = was.raised_at;
        }
        out.insert(alert.id.clone(), alert);
    };
    let since = |d: &DeviceState, attr: &str| d.attributes_changed_at.get(attr).copied();

    for d in devices {
        let id = &d.device_id;
        let name = d.effective_name();
        let built_in =
            |kind: AlertKind, severity, reason: String, at: Option<DateTime<Utc>>| Alert {
                id: format!("{}:{id}", kind.as_str()),
                kind,
                device_id: id.clone(),
                rule_id: None,
                message: format!("{name}: {}", lower_first(&reason)),
                reason,
                severity,
                raised_at: at.unwrap_or(now),
                acknowledged: None,
            };

        // ---- offline ----
        if d.available {
            memory.offline_since.remove(id);
        } else {
            let from = *memory.offline_since.entry(id.clone()).or_insert(now);
            if now.signed_duration_since(from).to_std().unwrap_or_default() >= OFFLINE_AFTER {
                add(Alert {
                    message: format!("{name} is offline"),
                    ..built_in(
                        AlertKind::Offline,
                        AlertSeverity::Warning,
                        "Offline".into(),
                        Some(from),
                    )
                });
            }
        }

        // ---- wet, smoke, jammed ----
        if let Some(attr) = first_true(d, &["water_detected", "leak", "wet"]) {
            add(built_in(
                AlertKind::Wet,
                AlertSeverity::Critical,
                "Wet".into(),
                since(d, attr),
            ));
        }
        if let Some(attr) = first_true(d, &["smoke", "smoke_detected"]) {
            add(built_in(
                AlertKind::Smoke,
                AlertSeverity::Critical,
                "Smoke".into(),
                since(d, attr),
            ));
        }
        let jammed = d.attributes.get("jammed") == Some(&Value::Bool(true))
            || d.attributes
                .get("last_alert")
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_ascii_lowercase().contains("jam"));
        if jammed {
            add(Alert {
                message: format!("{name} is jammed"),
                ..built_in(
                    AlertKind::Jammed,
                    AlertSeverity::Critical,
                    "Jammed".into(),
                    since(d, "last_alert").or(since(d, "jammed")),
                )
            });
        }

        // ---- low battery, with the battery watcher's thresholds ----
        let pct = ["battery_pct", "battery", "battery_level"]
            .iter()
            .find_map(|k| d.attributes.get(*k).and_then(Value::as_f64));
        let flagged = d.attributes.get("battery_low") == Some(&Value::Bool(true));
        let was_low = active.contains_key(&format!("low_battery:{id}"));
        let low = flagged
            || pct.is_some_and(|p| {
                p <= battery.threshold_pct
                    || (was_low && p < battery.threshold_pct + battery.recover_band_pct)
            });
        if low {
            let reason = match pct {
                Some(p) => format!("Battery low ({}%)", p.round()),
                None => "Battery low".into(),
            };
            add(built_in(
                AlertKind::LowBattery,
                AlertSeverity::Warning,
                reason,
                since(d, "battery_low").or(since(d, "battery")),
            ));
        }
    }

    // ---- time rules ----
    let by_id: HashMap<&str, &DeviceState> =
        devices.iter().map(|d| (d.device_id.as_str(), d)).collect();
    for rule in rules.iter().filter(|r| r.enabled) {
        for device_id in &rule.device_ids {
            let Some(d) = by_id.get(device_id.as_str()) else {
                continue;
            };
            let id = format!("rule:{}:{device_id}", rule.id);
            let matches = d
                .attributes
                .get(&rule.attribute)
                .is_some_and(|v| rule.matches(v));
            if !matches {
                memory.held_since.remove(&id);
                continue;
            }
            // An attribute's change time says when it became the value, which
            // is when "is" began — but a temperature above a line changes
            // while staying above it, so the rest count from when this
            // engine first saw it so.
            let changed = match rule.op {
                AlertOp::Eq => since(d, &rule.attribute),
                _ => None,
            };
            let from =
                changed.unwrap_or_else(|| *memory.held_since.entry(id.clone()).or_insert(now));
            let held = rule
                .for_minutes
                .is_none_or(|m| now.signed_duration_since(from).num_seconds() >= i64::from(m) * 60);
            if !held || !rule.in_window(local) {
                continue;
            }
            let reason = rule.describe();
            add(Alert {
                id,
                kind: AlertKind::Rule,
                device_id: device_id.clone(),
                rule_id: Some(rule.id.clone()),
                message: format!("{} {}", d.effective_name(), lower_first(&reason)),
                reason,
                severity: rule.severity,
                raised_at: from,
                acknowledged: None,
            });
        }
    }
    out
}

fn first_true<'a>(d: &DeviceState, attrs: &[&'a str]) -> Option<&'a str> {
    attrs
        .iter()
        .copied()
        .find(|a| d.attributes.get(*a) == Some(&Value::Bool(true)))
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as Span;
    use hc_types::alert::same;
    use serde_json::json;

    fn device(id: &str, attrs: Value) -> DeviceState {
        let mut d = DeviceState::new(id, id.replace('_', " "), "test");
        d.attributes = serde_json::from_value(attrs).unwrap();
        d.available = true;
        d
    }
    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }
    fn garage_rule() -> AlertRule {
        AlertRule {
            id: "r1".into(),
            name: "Garage at night".into(),
            enabled: true,
            device_ids: vec!["garage_door".into()],
            attribute: "open".into(),
            value: json!(true),
            state_label: Some("Open".into()),
            after: Some("22:00".into()),
            until: Some("06:00".into()),
            for_minutes: None,
            severity: AlertSeverity::Warning,
            op: AlertOp::Eq,
            notify: None,
        }
    }
    fn pass(
        devices: &[DeviceState],
        rules: &[AlertRule],
        active: &BTreeMap<String, Alert>,
        now: DateTime<Utc>,
        local: NaiveTime,
        memory: &mut Memory,
    ) -> BTreeMap<String, Alert> {
        desired(
            devices,
            rules,
            active,
            now,
            local,
            &BatteryConfig::default(),
            memory,
        )
    }

    #[test]
    fn nothing_alarming_is_no_alerts() {
        let d = [device("door", json!({"open": false, "battery": 90}))];
        let out = pass(
            &d,
            &[],
            &BTreeMap::new(),
            Utc::now(),
            at(12, 0),
            &mut Memory::default(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn wet_and_smoke_are_urgent_and_say_where() {
        let d = [
            device("kitchen_leak", json!({"water_detected": true})),
            device("hall_smoke", json!({"smoke": true})),
        ];
        let out = pass(
            &d,
            &[],
            &BTreeMap::new(),
            Utc::now(),
            at(12, 0),
            &mut Memory::default(),
        );
        let wet = &out["wet:kitchen_leak"];
        assert_eq!(wet.severity, AlertSeverity::Critical);
        assert_eq!(wet.message, "kitchen leak: wet");
        assert_eq!(wet.reason, "Wet");
        assert!(out.contains_key("smoke:hall_smoke"));
    }

    #[test]
    fn a_jammed_lock_by_flag_or_by_its_last_alert() {
        let d = [
            device("front", json!({"locked": false, "last_alert": "Jammed"})),
            device("back", json!({"jammed": true})),
            device("side", json!({"last_alert": "UnLockFailed"})),
        ];
        let out = pass(
            &d,
            &[],
            &BTreeMap::new(),
            Utc::now(),
            at(12, 0),
            &mut Memory::default(),
        );
        assert!(out.contains_key("jammed:front"));
        assert!(out.contains_key("jammed:back"));
        assert!(!out.contains_key("jammed:side"));
    }

    #[test]
    fn low_battery_has_hysteresis() {
        let mut memory = Memory::default();
        let now = Utc::now();
        let low = [device("sensor", json!({"battery": 15}))];
        let active = pass(&low, &[], &BTreeMap::new(), now, at(12, 0), &mut memory);
        assert_eq!(active["low_battery:sensor"].reason, "Battery low (15%)");
        // 22% is above the threshold but inside the band: still low.
        let mid = [device("sensor", json!({"battery": 22}))];
        let still = pass(&mid, &[], &active, now, at(12, 0), &mut memory);
        assert!(still.contains_key("low_battery:sensor"));
        // Not low before, 22% isn't low now.
        assert!(pass(&mid, &[], &BTreeMap::new(), now, at(12, 0), &mut memory).is_empty());
        // 26% has recovered.
        let up = [device("sensor", json!({"battery": 26}))];
        assert!(pass(&up, &[], &still, now, at(12, 0), &mut memory).is_empty());
        // A device that only says it is low.
        let flag = [device("th", json!({"battery_low": true}))];
        let out = pass(&flag, &[], &BTreeMap::new(), now, at(12, 0), &mut memory);
        assert_eq!(out["low_battery:th"].reason, "Battery low");
    }

    #[test]
    fn offline_only_once_it_has_lasted() {
        let mut memory = Memory::default();
        let mut d = device("bridge", json!({}));
        d.available = false;
        let t0 = Utc::now();
        assert!(pass(
            &[d.clone()],
            &[],
            &BTreeMap::new(),
            t0,
            at(12, 0),
            &mut memory
        )
        .is_empty());
        let later = t0 + Span::minutes(6);
        let out = pass(
            &[d.clone()],
            &[],
            &BTreeMap::new(),
            later,
            at(12, 0),
            &mut memory,
        );
        assert_eq!(out["offline:bridge"].message, "bridge is offline");
        assert_eq!(out["offline:bridge"].raised_at, t0);
        // Back online, and the clock starts again.
        d.available = true;
        assert!(pass(&[d.clone()], &[], &out, later, at(12, 0), &mut memory).is_empty());
        d.available = false;
        assert!(pass(&[d], &[], &BTreeMap::new(), later, at(12, 0), &mut memory).is_empty());
    }

    #[test]
    fn a_time_rule_only_inside_its_window() {
        let d = [device("garage_door", json!({"open": true}))];
        let rules = [garage_rule()];
        let mut memory = Memory::default();
        let now = Utc::now();
        assert!(pass(&d, &rules, &BTreeMap::new(), now, at(21, 59), &mut memory).is_empty());
        let out = pass(&d, &rules, &BTreeMap::new(), now, at(22, 30), &mut memory);
        let a = &out["rule:r1:garage_door"];
        assert_eq!(a.message, "garage door open between 22:00 and 06:00");
        assert_eq!(a.reason, "Open between 22:00 and 06:00");
        assert_eq!(a.rule_id.as_deref(), Some("r1"));
        // Closed: cleared.
        let shut = [device("garage_door", json!({"open": false}))];
        assert!(pass(&shut, &rules, &out, now, at(22, 31), &mut memory).is_empty());
    }

    #[test]
    fn held_for_uses_when_the_state_changed() {
        let mut d = device("front", json!({"locked": false}));
        let now = Utc::now();
        d.attributes_changed_at
            .insert("locked".into(), now - Span::minutes(20));
        let rule = AlertRule {
            id: "r2".into(),
            device_ids: vec!["front".into()],
            attribute: "locked".into(),
            value: json!(false),
            state_label: Some("Unlocked".into()),
            after: None,
            until: None,
            for_minutes: Some(30),
            ..garage_rule()
        };
        let mut memory = Memory::default();
        let rules = [rule];
        assert!(pass(
            &[d.clone()],
            &rules,
            &BTreeMap::new(),
            now,
            at(12, 0),
            &mut memory
        )
        .is_empty());
        let later = now + Span::minutes(11);
        let out = pass(
            &[d],
            &rules,
            &BTreeMap::new(),
            later,
            at(12, 11),
            &mut memory,
        );
        assert_eq!(out["rule:r2:front"].message, "front unlocked for 30 min");
        assert_eq!(out["rule:r2:front"].raised_at, now - Span::minutes(20));
    }

    #[test]
    fn a_disabled_rule_or_a_missing_device_raises_nothing() {
        let d = [device("garage_door", json!({"open": true}))];
        let mut off = garage_rule();
        off.enabled = false;
        let mut elsewhere = garage_rule();
        elsewhere.device_ids = vec!["gone".into()];
        let out = pass(
            &d,
            &[off, elsewhere],
            &BTreeMap::new(),
            Utc::now(),
            at(23, 0),
            &mut Memory::default(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn an_alert_still_called_for_keeps_when_it_was_raised() {
        let d = [device("leak", json!({"water_detected": true}))];
        let mut memory = Memory::default();
        let t0 = Utc::now();
        let first = pass(&d, &[], &BTreeMap::new(), t0, at(12, 0), &mut memory);
        let again = pass(
            &d,
            &[],
            &first,
            t0 + Span::minutes(5),
            at(12, 5),
            &mut memory,
        );
        assert_eq!(first, again);
    }

    #[test]
    fn a_reading_above_a_line_counts_from_when_it_was_first_seen_there() {
        // The freezer warms: its temperature changes every reading, so its
        // change time can't say how long it has been too warm.
        let now = Utc::now();
        let mut d = device("freezer", json!({"temperature": -4.0}));
        d.attributes_changed_at
            .insert("temperature".into(), now - Span::seconds(30));
        let rule = AlertRule {
            id: "r3".into(),
            device_ids: vec!["freezer".into()],
            attribute: "temperature".into(),
            value: json!(-10),
            op: AlertOp::Above,
            state_label: None,
            after: None,
            until: None,
            for_minutes: Some(15),
            ..garage_rule()
        };
        let rules = [rule];
        let mut memory = Memory::default();
        assert!(pass(
            &[d.clone()],
            &rules,
            &BTreeMap::new(),
            now,
            at(3, 0),
            &mut memory
        )
        .is_empty());
        let later = now + Span::minutes(16);
        d.attributes_changed_at.insert("temperature".into(), later);
        let out = pass(
            &[d],
            &rules,
            &BTreeMap::new(),
            later,
            at(3, 16),
            &mut memory,
        );
        assert_eq!(
            out["rule:r3:freezer"].reason,
            "temperature above -10 for 15 min"
        );
        assert_eq!(out["rule:r3:freezer"].raised_at, now);
    }

    #[test]
    fn an_ignored_alert_is_left_out() {
        let d = [
            device("lock", json!({"battery_pct": 5})),
            device("remote", json!({"battery_pct": 4})),
            device("leak", json!({"water_detected": true})),
        ];
        let now = Utc::now();
        let mut out = pass(
            &d,
            &[],
            &BTreeMap::new(),
            now,
            at(12, 0),
            &mut Memory::default(),
        );
        assert_eq!(out.len(), 3);
        let ignore = AlertIgnore {
            id: "i".into(),
            kind: Some(AlertKind::LowBattery),
            device_id: Some("lock".into()),
            rule_id: None,
            until: None,
            note: None,
            by: None,
            created_at: now,
        };
        unignored(&mut out, &[ignore], now);
        assert!(!out.contains_key("low_battery:lock"));
        assert!(out.contains_key("low_battery:remote"));
        assert!(out.contains_key("wet:leak"));
    }

    #[test]
    fn a_new_alert_goes_to_its_rules_channel_or_the_built_in_one() {
        let d = [
            device("garage_door", json!({"open": true})),
            device("lock", json!({"battery_pct": 5})),
            device("leak", json!({"water_detected": true})),
        ];
        let mut rule = garage_rule();
        rule.notify = Some("telegram".into());
        let rules = [rule];
        let out = pass(
            &d,
            &rules,
            &BTreeMap::new(),
            Utc::now(),
            at(23, 0),
            &mut Memory::default(),
        );
        let ch = |id: &str, builtin| channel_for(&out[id], &rules, builtin);
        assert_eq!(ch("rule:r1:garage_door", Some("all")), Some("telegram"));
        assert_eq!(ch("wet:leak", Some("all")), Some("all"));
        assert_eq!(ch("wet:leak", None), None);
        // A low battery has [battery]'s own notification.
        assert_eq!(ch("low_battery:lock", Some("all")), None);
    }

    #[test]
    fn an_acknowledgement_marks_the_active_alert_and_keeps_the_first() {
        let handle = AlertsHandle::new();
        let d = [device("leak", json!({"water_detected": true}))];
        let out = pass(
            &d,
            &[],
            &BTreeMap::new(),
            Utc::now(),
            at(12, 0),
            &mut Memory::default(),
        );
        *handle.active.write().unwrap() = out;
        assert!(handle.acknowledge("wet:nothing", "sam").is_none());
        let a = handle.acknowledge("wet:leak", "sam").unwrap();
        assert_eq!(a.acknowledged.as_ref().unwrap().by, "sam");
        let again = handle.acknowledge("wet:leak", "alex").unwrap();
        assert_eq!(again.acknowledged.unwrap().by, "sam");
        assert_eq!(handle.list()[0].acknowledged.as_ref().unwrap().by, "sam");
    }

    #[test]
    fn numbers_compare_as_numbers() {
        assert!(same(&json!(1), &json!(1.0)));
        assert!(same(&json!("on"), &json!("on")));
        assert!(!same(&json!(true), &json!(1)));
    }
}
