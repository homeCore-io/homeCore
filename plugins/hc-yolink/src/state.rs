//! One device's memory: what it last said, when it last spoke, what is wrong
//! with it — and, from that, what to publish when it says something new.
//!
//! Pure (no I/O, the time passed in), so each rule here is tested:
//!
//! - **Only changes are published.** A poll that reads what the last report
//!   said publishes nothing; the hub's own echo of a poll is ignored before it
//!   gets here.
//! - **Availability is earned.** Nothing is marked online at startup; a device
//!   is online once it reports or a poll says so, and a battery device that
//!   has been silent too long is offline whatever its last reading was — a
//!   leak sensor with flat batteries is never shown as dry.
//! - **A lock's history.** A lock or unlock becomes `last_lock_action`,
//!   `last_lock_method` and `last_lock_event` (with when), as hc-zwave's
//!   locks say it; a failure (`LockFailed`, `UnLockFailed`) becomes a plugin
//!   alert that clears on the next lock or unlock that works. `last_alert`
//!   keeps YoLink's own word, now with `last_alert_at` and
//!   `last_alert_source`. A `getState` repeats the last alert the lock ever
//!   had, so only a report makes one new.
//! - **Power only from plugs that meter**, in watts (YoLink's tenths ÷ 10);
//!   others publish no `power_w` at all rather than an invented 0.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use plugin_sdk_rs::types::PluginAlert;
use serde_json::{json, Map, Value};

use crate::devices::{
    alert_action, alert_is_door_open, alert_words, method_of, watts, Caps, DeviceKind, LockAlert,
    Reading,
};
use crate::yolink::types::DeviceInfo;

/// A lock that changes within this long of a command from homeCore was moved
/// by homeCore.
const OURS_WITHIN_SECS: i64 = 30;
/// A lock failure is news for this long, if nothing clears it sooner.
const FAILURE_LASTS_HOURS: i64 = 12;

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// A `getState` the plugin asked for.
    Poll,
    /// A report the device sent, as the hub stamped it.
    Event { name: &'a str, at: DateTime<Utc> },
    /// The hub's answer to a command homeCore sent.
    Command { at: DateTime<Utc> },
}

/// What to say about a device after a reading.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    /// Changed attributes; `null` removes one.
    pub patch: Map<String, Value>,
    pub availability: Option<bool>,
    /// The device's plugin alerts, when they changed.
    pub alerts: Option<Vec<PluginAlert>>,
    /// The plug turned out to meter: its schema gains `power_w`.
    pub learnt_metering: bool,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        self.patch.is_empty() && self.availability.is_none() && self.alerts.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct Dev {
    pub info: DeviceInfo,
    pub kind: DeviceKind,
    pub hc_id: String,
    pub caps: Caps,
    /// What was last published, attribute by attribute.
    pub published: Map<String, Value>,
    pub online: Option<bool>,
    /// When the device itself last reported.
    pub last_report: Option<DateTime<Utc>>,
    pub silent: bool,
    pub signal: Option<i64>,
    pub battery_level: Option<u8>,
    pub version: Option<String>,
    pub alerts: Vec<PluginAlert>,
    /// The last alert the lock reported, to tell a new one from `getState`
    /// repeating it.
    pub last_alert: Option<LockAlert>,
    /// A lock that failed: (action, when, words).
    pub lock_failure: Option<(String, DateTime<Utc>, String)>,
    /// A door left open, since.
    pub left_open: Option<DateTime<Utc>>,
    pub faults: Vec<&'static str>,
    pub last_command_at: Option<DateTime<Utc>>,
    /// Missing from the hub's device list this many times running.
    pub missing: u8,
    /// The schema last published, to publish it only when it changes.
    pub schema_sent: Option<String>,
    /// The hardware last registered (model, version).
    pub hardware_sent: Option<(Option<String>, Option<String>)>,
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Who moved the lock: a hand at the lock (button, fingerprint, key, code,
/// card) whatever homeCore just sent; else homeCore when it sent something
/// moments ago — the hub reports its own commands as "App" or remote.
fn credit(ours: bool, said: Option<&'static str>) -> Option<&'static str> {
    match said {
        Some(m @ ("button" | "fingerprint" | "key" | "code" | "card" | "auto")) => Some(m),
        _ if ours => Some("homecore"),
        other => other,
    }
}

impl Dev {
    pub fn new(info: DeviceInfo, kind: DeviceKind, metering_seen: bool) -> Self {
        let mut caps = Caps::from_model(info.model_name.as_deref());
        caps.metering |= metering_seen;
        Self {
            hc_id: format!("yolink_{}", info.device_id),
            info,
            kind,
            caps,
            published: Map::new(),
            online: None,
            last_report: None,
            silent: false,
            signal: None,
            battery_level: None,
            version: None,
            alerts: Vec::new(),
            last_alert: None,
            lock_failure: None,
            left_open: None,
            faults: Vec::new(),
            last_command_at: None,
            missing: 0,
            schema_sent: None,
            hardware_sent: None,
        }
    }

    fn silent_now(&self, now: DateTime<Utc>, after: Duration) -> bool {
        self.kind.is_battery() && self.last_report.is_some_and(|t| now - t > after)
    }

    /// Take in one reading.
    pub fn apply(
        &mut self,
        src: Source<'_>,
        r: Reading,
        now: DateTime<Utc>,
        silent_after: Duration,
    ) -> Effects {
        let mut fx = Effects::default();
        let mut a = r.attrs;

        // ---- when it last spoke, and whether it is there ----
        let spoke = match src {
            // A device that answered a command is there, too.
            Source::Event { at, .. } | Source::Command { at } => Some(at),
            Source::Poll => r.report_at,
        };
        if let Some(t) = spoke {
            if self.last_report.is_none_or(|old| t > old) {
                self.last_report = Some(t);
            }
        }
        if self.kind.is_battery() {
            if let Some(t) = self.last_report {
                a.insert("last_seen".into(), json!(iso(t)));
            }
        }
        self.signal = r.signal.or(self.signal);
        self.battery_level = r.battery_level.or(self.battery_level);
        if r.version.is_some() {
            self.version = r.version;
        }
        self.silent = match src {
            Source::Event { .. } | Source::Command { .. } => false,
            Source::Poll => self.silent_now(now, silent_after),
        };
        let online = !self.silent && r.online.unwrap_or(true);
        if self.online != Some(online) {
            self.online = Some(online);
            fx.availability = Some(online);
        }

        // ---- power ----
        if self.kind.is_relay() {
            if let Some(raw) = r.power_raw {
                if raw > 0.0 && !self.caps.metering {
                    self.caps.metering = true;
                    fx.learnt_metering = true;
                }
                a.insert(
                    "power_w".into(),
                    if self.caps.metering {
                        json!(watts(raw))
                    } else {
                        Value::Null
                    },
                );
            } else if !self.caps.metering {
                a.insert("power_w".into(), Value::Null);
            }
        }

        // ---- a lock's history ----
        if self.kind.is_lock() {
            self.lock_history(src, r.alert, &mut a, now);
        }

        // ---- a door left open, a leak sensor's faults ----
        if let (DeviceKind::DoorSensor, true, Source::Event { at, .. }) =
            (&self.kind, r.open_remind, src)
        {
            self.left_open.get_or_insert(at);
        }
        if matches!(a.get("open"), Some(Value::Bool(false))) {
            self.left_open = None;
        }
        if self.kind == DeviceKind::LeakSensor && a.contains_key("probe_fault") {
            self.faults = r.faults;
        }
        if let Some((_, at, _)) = &self.lock_failure {
            if now - *at > Duration::hours(FAILURE_LASTS_HOURS) {
                self.lock_failure = None;
            }
        }

        // ---- only what changed ----
        for (k, v) in a {
            let before = self.published.get(&k).cloned().unwrap_or(Value::Null);
            if before != v {
                if v.is_null() {
                    self.published.remove(&k);
                } else {
                    self.published.insert(k.clone(), v.clone());
                }
                fx.patch.insert(k, v);
            }
        }
        fx.alerts = self.refresh_alerts();
        fx
    }

    fn lock_history(
        &mut self,
        src: Source<'_>,
        alert: Option<LockAlert>,
        a: &mut Map<String, Value>,
        now: DateTime<Utc>,
    ) {
        let at = match src {
            Source::Event { at, .. } | Source::Command { at } => at,
            Source::Poll => now,
        };
        // A new alert: a report carrying one it had not reported before, or
        // any `.Alert` event. `getState` repeats the last one forever.
        let fresh = match (&src, &alert) {
            (Source::Event { name, .. }, Some(al)) => {
                // Not on first sight: after a restart the first report
                // repeats what the lock last had, like `getState` does.
                name.ends_with(".Alert")
                    || self.last_alert.as_ref().is_some_and(|known| known != al)
            }
            _ => false,
        };
        // A plain lock or unlock is history, not a problem: `last_alert`
        // keeps the last thing that went wrong.
        let problem = alert
            .as_ref()
            .is_some_and(|al| !matches!(alert_action(&al.kind), Some("locked" | "unlocked")));
        if let Some(al) = alert.as_ref().filter(|_| problem) {
            if fresh {
                let source = al.source.as_deref();
                a.insert("last_alert".into(), json!(al.kind));
                a.insert("last_alert_at".into(), json!(iso(at)));
                a.insert(
                    "last_alert_source".into(),
                    json!(source.map(|s| method_of(s)
                        .map(str::to_string)
                        .unwrap_or_else(|| s.to_ascii_lowercase()))),
                );
                if alert_is_door_open(&al.kind) {
                    self.left_open.get_or_insert(at);
                }
            } else if !self.published.contains_key("last_alert") {
                // Known from a poll: what it was, not when.
                a.insert("last_alert".into(), json!(al.kind));
            }
        }
        if let Some(al) = &alert {
            self.last_alert = Some(al.clone());
        }

        let before = self.published.get("locked").and_then(Value::as_bool);
        let now_locked = a.get("locked").and_then(Value::as_bool);
        let fresh_action = alert
            .as_ref()
            .filter(|_| fresh)
            .and_then(|al| alert_action(&al.kind).map(|act| (act, al.source.clone())));

        let ours = matches!(src, Source::Command { .. })
            || self
                .last_command_at
                .is_some_and(|t| (at - t).num_seconds().abs() <= OURS_WITHIN_SECS);
        let method_from = |source: &Option<String>| source.as_deref().and_then(method_of);

        let mut event: Option<(&'static str, Option<&'static str>)> = None;
        if let Some((act @ ("lock_failed" | "unlock_failed" | "jammed"), source)) = &fresh_action {
            let how = credit(ours, method_from(source));
            self.lock_failure = Some((act.to_string(), at, alert_words(act)));
            event = Some((act, how));
        } else if let (Some(b), Some(n)) = (before, now_locked) {
            if b != n {
                let act = if n { "locked" } else { "unlocked" };
                // homeCore's own command first: the hub reports those with a
                // generic source of its own.
                let how = credit(
                    ours,
                    match &fresh_action {
                        Some((x, source)) if *x == act => method_from(source),
                        _ => None,
                    },
                );
                event = Some((act, how));
                self.lock_failure = None;
            }
        }
        if let Some((act, how)) = event {
            a.insert("last_lock_action".into(), json!(act));
            a.insert("last_lock_method".into(), json!(how));
            a.insert(
                "last_lock_event".into(),
                json!({ "action": act, "method": how, "at": iso(at) }),
            );
        }
    }

    /// The device went quiet past its limit, or the hub could not reach it.
    pub fn check_silence(&mut self, now: DateTime<Utc>, after: Duration) -> Effects {
        let mut fx = Effects::default();
        if !self.silent && self.silent_now(now, after) {
            self.silent = true;
            if self.online != Some(false) {
                self.online = Some(false);
                fx.availability = Some(false);
            }
            fx.alerts = self.refresh_alerts();
        }
        fx
    }

    /// The hub could not reach it after retrying: offline.
    pub fn unreachable(&mut self) -> Effects {
        let mut fx = Effects::default();
        if self.online != Some(false) {
            self.online = Some(false);
            fx.availability = Some(false);
        }
        fx
    }

    /// The alerts true of it now, if they differ from the last said.
    fn refresh_alerts(&mut self) -> Option<Vec<PluginAlert>> {
        let mut now = Vec::new();
        if let Some((act, at, words)) = &self.lock_failure {
            let mut x = PluginAlert::new("lock_failed", words.clone()).key(act.clone());
            x.since = Some(*at);
            now.push(x);
        }
        if let Some(at) = self.left_open {
            let mut x = PluginAlert::new("door_left_open", "Door left open");
            x.since = Some(at);
            now.push(x);
        }
        if !self.faults.is_empty() {
            now.push(PluginAlert::new(
                "leak_sensor_fault",
                format!("Sensor fault: {}", self.faults.join(" and ")),
            ));
        }
        if self.silent {
            let mut x = PluginAlert::new("sensor_silent", "Not reporting");
            x.since = self.last_report;
            now.push(x);
        }
        if now == self.alerts {
            return None;
        }
        self.alerts = now.clone();
        Some(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::read;

    fn dev(kind: DeviceKind, model: Option<&str>) -> Dev {
        let info: DeviceInfo = serde_json::from_value(json!({
            "deviceId": "aa00000000000001", "token": "t", "name": "Thing",
            "type": "x", "modelName": model
        }))
        .unwrap();
        Dev::new(info, kind, false)
    }
    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    const NINE_H: i64 = 9;
    fn after() -> Duration {
        Duration::hours(NINE_H)
    }
    fn reading(d: &Dev, data: Value) -> Reading {
        read(&d.kind, &d.caps, &data).unwrap()
    }

    /// A leak that a poll reads stays wet (fix 1, through the state too), and
    /// the repeat publishes nothing.
    #[test]
    fn a_poll_during_a_leak_stays_wet_and_says_nothing_new() {
        let mut d = dev(DeviceKind::LeakSensor, Some("YS7903-UC"));
        let now = t("2026-10-21T10:05:00Z");
        let ev = Source::Event {
            name: "LeakSensor.Alert",
            at: t("2026-10-21T10:00:00Z"),
        };
        let fx = d.apply(
            ev,
            reading(&d, json!({ "state": "alert", "battery": 4 })),
            now,
            after(),
        );
        assert_eq!(fx.patch["water_detected"], json!(true));
        assert_eq!(fx.availability, Some(true));
        let poll = json!({ "online": true, "state": { "state": "alert", "battery": "4" }, "reportAt": "2026-10-21T10:00:00.000Z" });
        let fx = d.apply(Source::Poll, reading(&d, poll), now, after());
        assert!(fx.patch.is_empty(), "nothing new: {:?}", fx.patch);
        assert_eq!(d.published["water_detected"], json!(true));
    }

    /// Fix 11: no availability until something is known; a sensor silent past
    /// its limit goes offline, and comes back when it reports.
    #[test]
    fn a_silent_sensor_goes_offline_and_comes_back() {
        let mut d = dev(DeviceKind::LeakSensor, None);
        assert_eq!(d.online, None, "nothing is assumed at startup");
        let poll = json!({ "online": true, "state": { "state": "normal", "battery": "4" }, "reportAt": "2026-10-21T00:00:00Z" });
        // Polled 12 h after its last report: offline, whatever it last said.
        let fx = d.apply(
            Source::Poll,
            reading(&d, poll.clone()),
            t("2026-10-21T12:00:00Z"),
            after(),
        );
        assert_eq!(fx.availability, Some(false));
        assert_eq!(fx.patch["last_seen"], json!("2026-10-21T00:00:00Z"));
        assert_eq!(fx.alerts.as_ref().unwrap()[0].topic, "sensor_silent");
        // It reports: back.
        let ev = Source::Event {
            name: "LeakSensor.Report",
            at: t("2026-10-21T12:30:00Z"),
        };
        let fx = d.apply(
            ev,
            reading(&d, json!({ "state": "normal", "battery": 4 })),
            t("2026-10-21T12:30:00Z"),
            after(),
        );
        assert_eq!(fx.availability, Some(true));
        assert_eq!(fx.alerts, Some(vec![]));
        // And the watcher notices silence without a poll.
        let fx = d.check_silence(t("2026-10-21T22:00:00Z"), after());
        assert_eq!(fx.availability, Some(false));
        // A plug never goes "silent": it is mains-powered and answers polls.
        let mut p = dev(DeviceKind::Outlet, None);
        p.last_report = Some(t("2026-10-01T00:00:00Z"));
        assert!(p
            .check_silence(t("2026-10-21T00:00:00Z"), after())
            .is_empty());
    }

    /// Fix 4: 37 → 3.7 W on a metering plug; nothing on one that doesn't.
    #[test]
    fn power_only_from_plugs_that_meter() {
        let now = t("2026-10-21T10:00:00Z");
        let mut meter = dev(DeviceKind::Outlet, Some("YS6602-UC"));
        let fx = meter.apply(
            Source::Poll,
            reading(&meter, json!({ "state": "open", "power": 37 })),
            now,
            after(),
        );
        assert_eq!(fx.patch["power_w"], json!(3.7));
        let mut plain = dev(DeviceKind::Outlet, Some("YS6604-UC"));
        plain.published.insert("power_w".into(), json!(0));
        let fx = plain.apply(
            Source::Poll,
            reading(&plain, json!({ "state": "closed", "power": 0 })),
            now,
            after(),
        );
        assert_eq!(
            fx.patch["power_w"],
            Value::Null,
            "an old invented 0 is removed"
        );
        assert!(!plain.published.contains_key("power_w"));
        // An unknown model that reports power is a meter from then on.
        let mut unknown = dev(DeviceKind::Outlet, None);
        let fx = unknown.apply(
            Source::Poll,
            reading(&unknown, json!({ "state": "open", "power": 52 })),
            now,
            after(),
        );
        assert!(fx.learnt_metering);
        assert_eq!(fx.patch["power_w"], json!(5.2));
        let fx = unknown.apply(
            Source::Poll,
            reading(&unknown, json!({ "state": "open", "power": 0 })),
            now,
            after(),
        );
        assert_eq!(
            fx.patch["power_w"],
            json!(0.0),
            "a meter reading 0 is a real 0"
        );
    }

    /// Fix 15: a lock's history, and a failure that clears.
    #[test]
    fn lock_history_and_failures() {
        let mut d = dev(DeviceKind::LockV2, Some("YS7607-UC"));
        let now = t("2026-10-21T08:00:00Z");
        // The poll: the sticky alert is known, but not as news.
        let poll = json!({ "alert": { "source": "Fingerprint", "type": "UnLockFailed" }, "attributes": { "autoLock": 10, "soundLevel": 2 }, "battery": 4, "state": { "door": "closed", "lock": "locked" } });
        let fx = d.apply(Source::Poll, reading(&d, poll.clone()), now, after());
        assert_eq!(fx.patch["last_alert"], json!("UnLockFailed"));
        assert!(fx.patch.get("last_alert_at").is_none());
        assert!(fx.patch.get("last_lock_event").is_none());
        assert_eq!(fx.alerts, None, "no alert from a repeated one");
        // Unlocked by fingerprint.
        let at = t("2026-10-21T08:05:00Z");
        let ev = json!({ "alert": { "source": "Fingerprint", "type": "UnLock" }, "battery": 4, "state": { "door": "closed", "lock": "unlocked" } });
        let fx = d.apply(
            Source::Event {
                name: "LockV2.Alert",
                at,
            },
            reading(&d, ev),
            at,
            after(),
        );
        assert_eq!(fx.patch["locked"], json!(false));
        assert_eq!(fx.patch["last_lock_action"], json!("unlocked"));
        assert_eq!(fx.patch["last_lock_method"], json!("fingerprint"));
        assert_eq!(
            fx.patch["last_lock_event"]["at"],
            json!("2026-10-21T08:05:00Z")
        );
        assert!(
            fx.patch.get("last_alert").is_none(),
            "an unlock is not a problem"
        );
        // Couldn't lock, from the app: an alert.
        let at = t("2026-10-21T08:10:00Z");
        let ev = json!({ "alert": { "source": "App", "type": "LockFailed" }, "state": { "door": "closed", "lock": "unlocked" } });
        let fx = d.apply(
            Source::Event {
                name: "LockV2.Alert",
                at,
            },
            reading(&d, ev),
            at,
            after(),
        );
        assert_eq!(fx.patch["last_lock_action"], json!("lock_failed"));
        assert_eq!(fx.patch["last_alert_at"], json!("2026-10-21T08:10:00Z"));
        let alerts = fx.alerts.unwrap();
        assert_eq!(alerts[0].topic, "lock_failed");
        assert_eq!(alerts[0].reason, "Couldn't lock");
        // homeCore locks it: the failure clears, and the change is ours.
        let at = t("2026-10-21T08:11:00Z");
        d.last_command_at = Some(at);
        let ok = json!({ "state": { "lock": "locked" } });
        let fx = d.apply(Source::Command { at }, reading(&d, ok), at, after());
        assert_eq!(fx.patch["last_lock_method"], json!("homecore"));
        assert_eq!(fx.alerts, Some(vec![]));
        // A hand at the lock moments later is the hand, not homeCore.
        let at = t("2026-10-21T08:11:20Z");
        let ev = json!({ "alert": { "source": "Button", "type": "UnLock" }, "state": { "lock": "unlocked" } });
        let fx = d.apply(
            Source::Event {
                name: "LockV2.Alert",
                at,
            },
            reading(&d, ev),
            at,
            after(),
        );
        assert_eq!(fx.patch["last_lock_method"], json!("button"));
    }

    #[test]
    fn a_door_left_open_alerts_until_it_closes() {
        let mut d = dev(DeviceKind::DoorSensor, None);
        let at = t("2026-10-21T08:00:00Z");
        let fx = d.apply(
            Source::Event {
                name: "DoorSensor.Alert",
                at,
            },
            reading(&d, json!({ "state": "open", "alertType": "openRemind" })),
            at,
            after(),
        );
        assert_eq!(fx.alerts.unwrap()[0].topic, "door_left_open");
        let fx = d.apply(
            Source::Event {
                name: "DoorSensor.Alert",
                at,
            },
            reading(&d, json!({ "state": "closed", "alertType": "normal" })),
            at,
            after(),
        );
        assert_eq!(fx.alerts, Some(vec![]));
    }

    #[test]
    fn a_leak_sensor_fault_alerts_until_it_clears() {
        let mut d = dev(DeviceKind::LeakSensor, None);
        let at = t("2026-10-21T08:00:00Z");
        let fx = d.apply(Source::Event { name: "LeakSensor.Report", at }, reading(&d, json!({ "state": "normal", "alarmState": { "detectorError": true, "freezeError": false } })), at, after());
        assert_eq!(fx.alerts.unwrap()[0].reason, "Sensor fault: probe fault");
        let fx = d.apply(Source::Event { name: "LeakSensor.Report", at }, reading(&d, json!({ "state": "normal", "alarmState": { "detectorError": false, "freezeError": false } })), at, after());
        assert_eq!(fx.alerts, Some(vec![]));
    }
}
