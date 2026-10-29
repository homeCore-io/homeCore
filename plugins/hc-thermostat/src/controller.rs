//! Every thermostat's decisions, as one state machine.
//!
//! **Nothing here touches MQTT.** Each input — a sensor's state, a switch's
//! state or availability, a command, the 10 s tick — changes the state and
//! queues [`Effect`]s, which `bridge` carries out in order. It runs on one
//! task (the bridge's actor), so a decision and what it publishes can never be
//! interleaved with another's; it used to be one `tokio::spawn` per MQTT
//! message, racing each other between deciding and publishing. And it can be
//! driven with a fake clock, which is how the tests below walk through a cold
//! start, a quiet sensor, a hand flip and a plug that never answers.
//!
//! # What it promises
//!
//! - **Before [`Controller::start`] it only listens.** Readings, switch states,
//!   its own retained state and learned state are taken in; nothing is sent.
//!   `start` then makes one settling pass. A thermostat that is Off is never
//!   switched at boot.
//! - **A reading counts while it is current**: from a sensor that is online,
//!   within −40…60 °C, received within `sensor_timeout_secs`. With none left
//!   it turns the switch off (`on_stale = "off"`, the default) and says so
//!   (`sensors_stale`, an alert).
//! - **Every on and off is checked.** The switch has [`CONFIRM_SECS`] to report
//!   the state asked for; until then it is not "cooling", after that it is
//!   `actuator_not_responding` (an alert) and it is asked again, backing off.
//! - **Someone flipping the switch is a manual override**, left alone for
//!   `override_secs` or until the target or mode changes.
//! - **Off means off, then hands off**: one OFF, then the switch is the
//!   household's.
//! - **A heater is never left running**: off after `max_run_secs`, off when a
//!   thermostat is removed or the plugin stops.

use crate::config::{setpoint_problem, Kind, Loaded, SkippedEntry, ThermostatEntry};
use crate::control::{aggregate, compute_call_for, lockout_remaining};
use chrono::{DateTime, Duration, TimeZone, Utc};
use plugin_sdk_rs::types::{AlertAction, AlertActionTarget, PluginAlert, PluginNotice};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::{info, warn};

/// How long a switch has to report the state it was asked for.
pub const CONFIRM_SECS: i64 = 30;
/// The longest wait between two tries of an unconfirmed command.
pub const MAX_RETRY_SECS: i64 = 600;
/// A reading outside this is a broken sensor, not a room.
pub const READING_RANGE: std::ops::RangeInclusive<f64> = -40.0..=60.0;
/// A sensor's published `at` moves on this often when its value doesn't —
/// so the thermostat doesn't change state every time a sensor repeats itself.
pub const SHOWN_AT_EVERY_SECS: i64 = 300;
/// How long a newly chosen switch has to show it is a switch.
pub const CANDIDATE_SECS: i64 = 10;
/// The longest hold.
pub const MAX_HOLD_SECS: u64 = 7 * 24 * 3600;
/// How long a new thermostat (or new sensors) has for the first reading
/// before having none counts as quiet.
pub const FIRST_READING_SECS: i64 = 15;
/// The same at start. homeCore's broker forgets retained state when homeCore
/// restarts (it restarts every plugin with it), so a sensor's reading can
/// take as long as its plugin needs to come back and say it again.
pub const STARTUP_GRACE_SECS: i64 = 120;

/// What the bridge is to do, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Register the device, its schema, its command topic, and say it is online.
    Register(Box<ThermostatEntry>),
    /// Take a removed thermostat's device away.
    Unregister(String),
    /// Unregister anything registered in an earlier run that is not these.
    Reconcile(Vec<String>),
    /// The device's whole state (retained).
    PublishState {
        device_id: String,
        state: Value,
    },
    /// A command to a switch, at least once.
    Command {
        actuator: String,
        payload: Value,
    },
    /// The thermostat's alerts, the whole list.
    Alerts {
        device_id: String,
        alerts: Vec<PluginAlert>,
    },
    /// The plugin's own alerts, the whole list.
    PluginAlerts(Vec<PluginAlert>),
    /// The plugin's notices, the whole list.
    Notices(Vec<PluginNotice>),
    /// Hear another device's state and availability.
    Subscribe(String),
    Unsubscribe(String),
    /// Stop hearing our own retained state (after start).
    UnsubscribeOwn(String),
    /// How a command went, as a `plugin_command_result` event.
    CommandResult {
        device_id: String,
        operation: String,
        error: Option<String>,
    },
    /// Runtime changes, to core's learned state.
    PersistLearned(Value),
}

// ── Small pure helpers ───────────────────────────────────────────────────────

/// Interpret a switch's state payload as on/off: `on`, then `state`, then
/// `power`. `None` when it says neither.
pub fn interpret_actuator_on(payload: &Value) -> Option<bool> {
    if let Some(b) = payload.get("on").and_then(Value::as_bool) {
        return Some(b);
    }
    let word = |v: &Value| match v.as_str()?.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    };
    if let Some(b) = payload.get("state").and_then(word) {
        return Some(b);
    }
    if let Some(b) = payload.get("power").and_then(Value::as_bool) {
        return Some(b);
    }
    payload.get("power").and_then(word)
}

/// Which command a payload asks for: `{"action": …}`. The older `command`
/// spelling is still read (undocumented) so a payload written by hand keeps
/// working.
pub fn command_of(payload: &Value) -> &str {
    ["action", "command"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str))
        .unwrap_or("")
}

/// The attribute writes a command carries: `{"setpoint": 21}` and
/// `{"hvac_mode": "cool"}` (`mode` is the old spelling, still read).
pub fn attribute_writes(payload: &Value) -> (Option<f64>, Option<&str>) {
    let setpoint = payload.get("setpoint").and_then(Value::as_f64);
    let mode = ["hvac_mode", "mode"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(Value::as_str));
    (setpoint, mode)
}

/// The sensor ids a `set_sensors` command names: a list, or one string of
/// ids separated by commas.
pub fn sensor_ids_of(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(ids) => Some(
            ids.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        Value::String(s) => Some(
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

/// What a command asked, for `plugin_command_result`.
pub fn operation_of(cmd: &Value) -> String {
    let c = command_of(cmd);
    if !c.is_empty() {
        return c.to_string();
    }
    cmd.as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !k.starts_with('_') && k.as_str() != "correlation_id")
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// The runtime changes to keep for one thermostat, or `None` when it is as the
/// config file has it. `based_on` is the file's entry the change was made
/// over, so an edit to the file afterwards wins (see [`learned_entry`]).
pub fn runtime_change(file: &ThermostatEntry, now: &ThermostatEntry) -> Option<Value> {
    let (file, now) = (
        serde_json::to_value(file).ok()?,
        serde_json::to_value(now).ok()?,
    );
    (file != now).then(|| json!({ "based_on": file, "entry": now }))
}

/// A thermostat as learned state left it, if the config file still says what
/// it said when the change was made — and the change is still one it can run.
pub fn learned_entry(file: &ThermostatEntry, kept: &Value) -> Option<ThermostatEntry> {
    if kept.get("based_on")? != &serde_json::to_value(file).ok()? {
        return None;
    }
    let entry: ThermostatEntry = serde_json::from_value(kept.get("entry")?.clone()).ok()?;
    (entry.id == file.id && entry.problem().is_none()).then_some(entry)
}

/// The part of a thermostat's state worth keeping over a homeCore restart:
/// when its switch last changed, a hold or override, and today's figures.
/// Readings are left out — they change all day, and every sensor says its
/// reading again when its plugin comes back.
pub fn runtime_of(state: &Value) -> Value {
    const KEEP: [&str; 10] = [
        "actuator_last_change",
        "on_since",
        "reason",
        "hold_setpoint",
        "hold_until",
        "override_until",
        "today_from",
        "run_secs_today",
        "cycles_today",
        "longest_run_secs_today",
    ];
    KEEP.iter()
        .filter_map(|k| Some((k.to_string(), state.get(*k)?.clone())))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// `hvac_action`'s word for running in a mode.
fn running_word(mode: &str) -> &'static str {
    if mode == "heat" {
        "heating"
    } else {
        "cooling"
    }
}

/// The wait before the `attempts`th try: 30 s, 60, 120 … up to 10 min.
fn backoff_secs(attempts: u32) -> i64 {
    (CONFIRM_SECS << attempts.saturating_sub(1).min(5)).min(MAX_RETRY_SECS)
}

fn local_midnight(t: DateTime<Utc>) -> DateTime<Utc> {
    let tz = hc_time::configured_tz();
    let d = t.with_timezone(&tz).date_naive();
    tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
        .earliest()
        .map(|x| x.with_timezone(&Utc))
        .unwrap_or(t)
}

fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&hc_time::configured_tz())
        .format("%H:%M")
        .to_string()
}

fn iso(t: Option<DateTime<Utc>>) -> Value {
    t.map(|t| json!(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .unwrap_or(Value::Null)
}

fn parse_time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v?.as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

// ── State ────────────────────────────────────────────────────────────────────

/// The last reading of one sensor.
#[derive(Debug, Clone, Default)]
struct Reading {
    value: Option<f64>,
    /// When it was received.
    at: Option<DateTime<Utc>>,
    /// The `at` published: moved on when the value changes, or every
    /// [`SHOWN_AT_EVERY_SECS`].
    shown_at: Option<DateTime<Utc>>,
}

/// What another device has said about itself.
#[derive(Debug, Clone, Default)]
struct Seen {
    on: Option<bool>,
    online: Option<bool>,
    /// It has a boolean `on`.
    has_on: bool,
    /// It has a `locked` — a lock is never a thermostat's switch.
    lockish: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    setpoint: f64,
    until: DateTime<Utc>,
}

/// A switch someone chose with `set_actuator`, waiting to show it is one.
#[derive(Debug, Clone)]
struct Candidate {
    thermostat: String,
    deadline: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct Thermo {
    cfg: ThermostatEntry,
    temperature: Option<f64>,
    /// The hysteresis call: wants the switch on.
    calling: bool,
    stale: bool,

    /// What it last asked the switch for, and whether the switch said so.
    commanded: Option<bool>,
    confirmed: bool,
    /// When the current, unconfirmed command was first sent.
    waiting_since: Option<DateTime<Utc>>,
    attempts: u32,
    retry_at: Option<DateTime<Utc>>,
    not_responding: bool,
    /// When the switch last changed, for the short-cycle guards.
    last_change: Option<DateTime<Utc>>,
    /// Since when it has been on.
    on_since: Option<DateTime<Utc>>,
    pending: Option<&'static str>,
    lockout_until: Option<DateTime<Utc>>,
    /// Send the desired state even if it seems so already (start, `resync`).
    force: bool,
    /// Skip the short-cycle guard once (`start_now`).
    bypass_guard: bool,
    /// Just set to Off: one OFF is owed.
    entering_off: bool,
    override_until: Option<DateTime<Utc>>,
    override_at: Option<DateTime<Utc>>,
    hold: Option<Hold>,
    max_run_tripped: bool,
    tripped_at: Option<DateTime<Utc>>,
    /// Just added, or given new sensors: their first readings are on their
    /// way, so having none yet is not "quiet".
    grace_until: Option<DateTime<Utc>>,

    /// When the day today's figures count began (local midnight).
    day_start: Option<DateTime<Utc>>,
    run_secs_today: u64,
    cycles_today: u32,
    longest_run_secs_today: u64,

    last_command_error: Option<String>,
    published: Option<Value>,
    alerts_sent: Option<Value>,
}

impl Thermo {
    fn new(cfg: ThermostatEntry) -> Self {
        Self {
            cfg,
            temperature: None,
            calling: false,
            stale: false,
            commanded: None,
            confirmed: false,
            waiting_since: None,
            attempts: 0,
            retry_at: None,
            not_responding: false,
            last_change: None,
            on_since: None,
            pending: None,
            lockout_until: None,
            force: false,
            bypass_guard: false,
            entering_off: false,
            override_until: None,
            override_at: None,
            hold: None,
            max_run_tripped: false,
            tripped_at: None,
            grace_until: None,
            day_start: None,
            run_secs_today: 0,
            cycles_today: 0,
            longest_run_secs_today: 0,
            last_command_error: None,
            published: None,
            alerts_sent: None,
        }
    }

    fn kind(&self) -> Kind {
        self.cfg.kind()
    }
    fn active(&self) -> bool {
        self.cfg.mode != "off"
    }
    fn target(&self) -> f64 {
        self.hold.map(|h| h.setpoint).unwrap_or(self.cfg.setpoint)
    }

    /// A new day: today's figures start again (a run going on counts from
    /// midnight).
    ///
    /// The house's timezone arrives a moment after the plugin connects, so
    /// "local midnight" can move by up to a zone's offset once at startup.
    /// That moves the day's start, never ends the day: only a midnight 20 h
    /// or more after the one counted from is a new day.
    fn roll_day(&mut self, now: DateTime<Utc>, on: bool) {
        let midnight = local_midnight(now);
        let Some(start) = self.day_start else {
            self.day_start = Some(midnight);
            return;
        };
        if midnight - start < Duration::hours(20) {
            self.day_start = Some(midnight);
            return;
        }
        self.day_start = Some(midnight);
        self.run_secs_today = 0;
        self.longest_run_secs_today = 0;
        self.cycles_today = u32::from(on);
    }

    /// The switch went on or off (as it reported it).
    fn note_switch(&mut self, on: bool, now: DateTime<Utc>) {
        self.roll_day(now, !on);
        if on {
            // From when it said so, not from when it was asked.
            self.on_since = Some(now);
            self.cycles_today += 1;
        } else if let Some(since) = self.on_since.take() {
            // Today's part of it: a run going on over midnight counts from there.
            let from = since.max(local_midnight(now));
            let secs = (now - from).num_seconds().max(0) as u64;
            self.run_secs_today += secs;
            self.longest_run_secs_today = self.longest_run_secs_today.max(secs);
        }
    }

    /// Any change of target or mode ends a hand-flip override and a tripped
    /// longest run.
    fn person_changed_it(&mut self) {
        self.override_until = None;
        self.override_at = None;
        self.max_run_tripped = false;
        self.tripped_at = None;
    }

    fn reset_switch(&mut self) {
        self.commanded = None;
        self.confirmed = false;
        self.waiting_since = None;
        self.attempts = 0;
        self.retry_at = None;
        self.not_responding = false;
        self.pending = None;
        self.lockout_until = None;
        self.on_since = None;
        self.force = true;
    }
}

/// The plugin's thermostats and everything they read.
pub struct Controller {
    started: bool,
    therms: BTreeMap<String, Thermo>,
    /// Each thermostat as the config file had it — what a runtime change is
    /// kept relative to.
    file_entries: HashMap<String, ThermostatEntry>,
    skipped: Vec<SkippedEntry>,
    capped: Vec<(String, Vec<String>)>,
    readings: HashMap<String, Reading>,
    seen: HashMap<String, Seen>,
    /// Readings as our own retained state had them, for their age at a restart.
    restored: HashMap<String, (f64, DateTime<Utc>)>,
    /// Thermostats whose own retained state was heard before start — fresher
    /// than what learned state kept, so that is not applied to them.
    own_restored: HashSet<String>,
    /// The running state last kept in learned state (see [`runtime_of`]).
    runtime_kept: Option<Value>,
    candidates: HashMap<String, Candidate>,
    sync_pending: Option<HashSet<String>>,
    fx: Vec<Effect>,
}

impl Controller {
    pub fn new(loaded: Loaded) -> Self {
        let therms = loaded
            .config
            .thermostats
            .iter()
            .cloned()
            .map(|t| (t.id.clone(), Thermo::new(t)))
            .collect();
        let mut c = Self {
            started: false,
            therms,
            file_entries: loaded
                .config
                .thermostats
                .iter()
                .map(|t| (t.id.clone(), t.clone()))
                .collect(),
            skipped: loaded.skipped,
            capped: loaded.capped,
            readings: HashMap::new(),
            seen: HashMap::new(),
            restored: HashMap::new(),
            own_restored: HashSet::new(),
            runtime_kept: None,
            candidates: HashMap::new(),
            sync_pending: None,
            fx: Vec::new(),
        };
        c.refresh_notices();
        c
    }

    /// What to do now, in order.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        self.keep_runtime();
        std::mem::take(&mut self.fx)
    }

    /// Keep every thermostat's running state in core's learned state when it
    /// has changed: our own retained state is lost when homeCore restarts
    /// (its broker keeps none), learned state is not.
    fn keep_runtime(&mut self) {
        if !self.started {
            return;
        }
        let now: serde_json::Map<String, Value> = self
            .therms
            .iter()
            .filter_map(|(id, t)| Some((id.clone(), runtime_of(t.published.as_ref()?))))
            .collect();
        let now = Value::Object(now);
        if self.runtime_kept.as_ref() != Some(&now) {
            self.runtime_kept = Some(now.clone());
            // Learned state merges at the top level, so the whole set is sent.
            self.fx
                .push(Effect::PersistLearned(json!({ "runtime": now })));
        }
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Every sensor and switch any thermostat uses.
    pub fn external_ids(&self) -> Vec<String> {
        let mut s: Vec<String> = self.used_ids().into_iter().collect();
        s.sort();
        s
    }

    fn used_ids(&self) -> HashSet<String> {
        let mut s = HashSet::new();
        for t in self.therms.values() {
            s.extend(t.cfg.sensor_device_ids.iter().cloned());
            if !t.cfg.actuator_device_id.is_empty() {
                s.insert(t.cfg.actuator_device_id.clone());
            }
        }
        s.extend(self.candidates.keys().cloned());
        s
    }

    pub fn own_ids(&self) -> Vec<String> {
        self.therms.values().map(|t| t.cfg.device_id()).collect()
    }

    /// One of our devices: matched by id, never by the `thermostat_` prefix
    /// another plugin's device might have.
    fn own_id<'a>(&self, device_id: &'a str) -> Option<&'a str> {
        let id = device_id.strip_prefix("thermostat_")?;
        self.therms.contains_key(id).then_some(id)
    }

    pub fn taken(&self, id: &str) -> bool {
        self.therms.contains_key(id)
    }

    pub fn entries(&self) -> Vec<ThermostatEntry> {
        self.therms.values().map(|t| t.cfg.clone()).collect()
    }

    // ── Startup ──────────────────────────────────────────────────────────

    pub fn begin_sync(&mut self, ids: Vec<String>) {
        self.sync_pending = Some(ids.into_iter().collect());
    }

    /// How many subscribed devices have not said anything yet.
    pub fn sync_remaining(&self) -> Vec<String> {
        self.sync_pending
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn heard(&mut self, id: &str) {
        if let Some(s) = &mut self.sync_pending {
            s.remove(id);
        }
    }

    /// Start: register every device, then one settling pass — with what
    /// learned state, our own retained state, the sensors and the switches
    /// said in the meantime.
    pub fn start(&mut self, now: DateTime<Utc>) {
        self.started = true;
        self.sync_pending = None;
        let ids: Vec<String> = self.therms.keys().cloned().collect();
        for id in &ids {
            let t = &self.therms[id];
            self.fx.push(Effect::Register(Box::new(t.cfg.clone())));
            self.fx.push(Effect::UnsubscribeOwn(t.cfg.device_id()));
        }
        self.fx.push(Effect::Reconcile(self.own_ids()));
        for id in &ids {
            let observed = self.observed(id);
            let t = self.therms.get_mut(id).expect("listed");
            // What the switch said beats what we remembered.
            if observed == Some(false) {
                t.on_since = None;
            }
            if observed == Some(true) && t.on_since.is_none() {
                t.on_since = Some(now);
            }
            if observed == Some(true) && t.cycles_today == 0 {
                // Running now is at least one run today.
                t.cycles_today = 1;
            }
            t.day_start.get_or_insert(local_midnight(now));
            // Off: never switched at boot. Otherwise settle — send what it
            // should be even if it seems so already.
            t.force = t.active();
            t.grace_until = Some(now + Duration::seconds(STARTUP_GRACE_SECS));
            info!(thermostat = %id, mode = %t.cfg.mode, "Settling");
        }
        for id in &ids {
            self.evaluate(id, now);
        }
        self.refresh_notices();
    }

    /// Our own retained state from the last run: when the switch last changed
    /// (for the short-cycle guards), a hold or override still running, today's
    /// figures, and how old each reading was. Only before start — afterwards
    /// it is our own echo.
    fn restore_own(&mut self, id: &str, p: &Value, now: DateTime<Utc>) {
        let Some(t) = self.therms.get_mut(id) else {
            return;
        };
        if let Some(ts) = parse_time(p.get("actuator_last_change")) {
            t.last_change = Some(ts);
        }
        t.override_until = parse_time(p.get("override_until")).filter(|u| *u > now);
        if t.override_until.is_some() {
            t.override_at = t.last_change;
        }
        if let (Some(until), Some(sp)) = (
            parse_time(p.get("hold_until")),
            p.get("hold_setpoint").and_then(Value::as_f64),
        ) {
            if until > now && setpoint_problem(sp).is_none() {
                t.hold = Some(Hold {
                    setpoint: sp,
                    until,
                });
            }
        }
        t.on_since = parse_time(p.get("on_since"));
        t.calling = matches!(
            p.get("reason").and_then(Value::as_str),
            Some("heating" | "cooling" | "waiting_min_off")
        );
        if let Some(from) = parse_time(p.get("today_from")) {
            // The figures of a day that began less than a day ago — told by
            // the instant, which needs no timezone (it may not have arrived).
            if now >= from && now - from < Duration::hours(25) {
                t.day_start = Some(from);
                let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
                t.run_secs_today = n("run_secs_today");
                t.cycles_today = n("cycles_today") as u32;
                t.longest_run_secs_today = n("longest_run_secs_today");
            }
        }
        if let Some(readings) = p.get("sensor_readings").and_then(Value::as_object) {
            for (sid, r) in readings {
                if let (Some(v), Some(at)) = (
                    r.get("temperature").and_then(Value::as_f64),
                    parse_time(r.get("at")),
                ) {
                    self.restored.insert(sid.clone(), (v, at));
                    // A reading already heard with the same value is that old.
                    if let Some(cur) = self.readings.get_mut(sid) {
                        if cur.value.is_some_and(|c| (c - v).abs() < 1e-6) {
                            cur.at = Some(at);
                            cur.shown_at = Some(at);
                        }
                    }
                }
            }
        }
    }

    /// The runtime changes learned state kept, where the config file has not
    /// been edited since. Only before start: afterwards we are its writer.
    pub fn apply_learned(&mut self, doc: &Value, now: DateTime<Utc>) {
        if self.started {
            return;
        }
        // The running state kept there, where our own retained state (newer)
        // is gone — after homeCore restarted.
        if let Some(runtime) = doc.get("runtime").and_then(Value::as_object) {
            for (id, p) in runtime {
                if !self.own_restored.contains(id) && self.therms.contains_key(id) {
                    info!(thermostat = %id, "Took up its running state from learned state");
                    self.restore_own(id, p, now);
                }
            }
        }
        let Some(kept) = doc.get("thermostats").and_then(Value::as_object) else {
            return;
        };
        let before = self.used_ids();
        for (id, k) in kept {
            let Some(file) = self.file_entries.get(id) else {
                continue;
            };
            if let Some(entry) = learned_entry(file, k) {
                if let Some(t) = self.therms.get_mut(id) {
                    info!(thermostat = %id, "Applied the settings changed at runtime");
                    t.cfg = entry;
                }
            }
        }
        self.resubscribe(&before);
    }

    /// Subscribe what is newly used, unsubscribe what no longer is.
    fn resubscribe(&mut self, before: &HashSet<String>) {
        let after = self.used_ids();
        let mut added: Vec<_> = after.difference(before).cloned().collect();
        let mut gone: Vec<_> = before.difference(&after).cloned().collect();
        added.sort();
        gone.sort();
        for id in added {
            self.fx.push(Effect::Subscribe(id));
        }
        for id in gone {
            self.readings.remove(&id);
            self.seen.remove(&id);
            self.fx.push(Effect::Unsubscribe(id));
        }
    }

    // ── What others say ──────────────────────────────────────────────────

    /// A state message on a subscribed topic.
    pub fn on_state(&mut self, device_id: &str, payload: &Value, now: DateTime<Utc>) {
        if let Some(id) = self.own_id(device_id) {
            if !self.started {
                let id = id.to_string();
                self.heard(device_id);
                self.own_restored.insert(id.clone());
                self.restore_own(&id, payload, now);
            }
            return;
        }
        self.heard(device_id);
        {
            let s = self.seen.entry(device_id.to_string()).or_default();
            if payload.get("on").is_some_and(Value::is_boolean) {
                s.has_on = true;
            }
            if payload.get("locked").is_some() {
                s.lockish = true;
            }
        }
        if self.candidates.contains_key(device_id) {
            self.settle_candidate(device_id, now);
        }
        self.on_sensor_state(device_id, payload, now);
        if let Some(on) = interpret_actuator_on(payload) {
            self.on_switch_state(device_id, on, now);
        }
    }

    fn on_sensor_state(&mut self, device_id: &str, payload: &Value, now: DateTime<Utc>) {
        let attrs: HashSet<String> = self
            .therms
            .values()
            .filter(|t| t.cfg.sensor_device_ids.iter().any(|s| s == device_id))
            .map(|t| t.cfg.sensor_attribute.clone())
            .collect();
        let Some(raw) = attrs.iter().find_map(|a| payload.get(a)) else {
            return; // a partial without the reading
        };
        let r = self.readings.entry(device_id.to_string()).or_default();
        match raw.as_f64() {
            // "No reading" is something heard: the sensor is there and has none.
            None if raw.is_null() => {
                r.value = None;
                r.at = Some(now);
                r.shown_at = Some(now);
            }
            None => return,
            Some(v) if !READING_RANGE.contains(&v) => {
                warn!(
                    sensor = device_id,
                    value = v,
                    "Reading outside −40…60 °C ignored"
                );
                return;
            }
            Some(v) => {
                let mut at = now;
                if !self.started {
                    if let Some((rv, rat)) = self.restored.get(device_id) {
                        if (rv - v).abs() < 1e-6 {
                            at = *rat;
                        }
                    }
                }
                let changed = r.value != Some(v);
                r.value = Some(v);
                r.at = Some(at);
                if changed
                    || r.shown_at
                        .is_none_or(|s| (at - s).num_seconds() >= SHOWN_AT_EVERY_SECS)
                {
                    r.shown_at = Some(at);
                }
            }
        }
        self.evaluate_using(device_id, now);
    }

    fn on_switch_state(&mut self, device_id: &str, on: bool, now: DateTime<Utc>) {
        let prev = {
            let s = self.seen.entry(device_id.to_string()).or_default();
            let prev = s.on;
            s.on = Some(on);
            prev
        };
        let started = self.started;
        let ids: Vec<String> = self
            .therms
            .values()
            .filter(|t| t.cfg.actuator_device_id == device_id)
            .map(|t| t.cfg.id.clone())
            .collect();
        for id in ids {
            let t = self.therms.get_mut(&id).expect("listed");
            if prev.is_some() && prev != Some(on) {
                t.note_switch(on, now);
            } else if prev.is_none() && on && t.on_since.is_none() && started {
                t.on_since = Some(now);
            }
            if t.commanded == Some(on) && !t.confirmed {
                // The echo of what we asked for.
                t.confirmed = true;
                t.attempts = 0;
                t.retry_at = None;
                t.waiting_since = None;
                t.not_responding = false;
                if !on {
                    t.entering_off = false;
                }
            } else if t.commanded == Some(!on) && !t.confirmed {
                // Still on its way — not a person.
            } else if started && prev.is_some() && prev != Some(on) {
                // Nobody here asked for that: a person, a rule, a voice
                // assistant.
                t.last_change = Some(now);
                t.commanded = Some(on);
                t.confirmed = true;
                if t.active() && !t.cfg.actuator_device_id.is_empty() {
                    let secs = t.cfg.override_secs();
                    if secs > 0 {
                        info!(thermostat = %id, on, "Switched by hand — leaving it alone");
                        t.override_until = Some(now + Duration::seconds(secs as i64));
                        t.override_at = Some(now);
                    }
                }
            }
            self.evaluate(&id, now);
        }
    }

    /// An availability message: a sensor offline no longer counts; a switch
    /// offline can't be told anything, and back online it is asked again.
    pub fn on_availability(&mut self, device_id: &str, online: bool, now: DateTime<Utc>) {
        if self.own_id(device_id).is_some() {
            return;
        }
        let s = self.seen.entry(device_id.to_string()).or_default();
        let was = s.online;
        s.online = Some(online);
        if online && was == Some(false) {
            for t in self.therms.values_mut() {
                if t.cfg.actuator_device_id == device_id && t.commanded.is_some() && !t.confirmed {
                    t.retry_at = Some(now);
                }
            }
        }
        self.evaluate_using(device_id, now);
    }

    fn evaluate_using(&mut self, device_id: &str, now: DateTime<Utc>) {
        let ids: Vec<String> = self
            .therms
            .values()
            .filter(|t| {
                t.cfg.actuator_device_id == device_id
                    || t.cfg.sensor_device_ids.iter().any(|s| s == device_id)
            })
            .map(|t| t.cfg.id.clone())
            .collect();
        for id in ids {
            self.evaluate(&id, now);
        }
    }

    fn observed(&self, id: &str) -> Option<bool> {
        let t = self.therms.get(id)?;
        self.seen.get(&t.cfg.actuator_device_id)?.on
    }

    // ── Deciding ─────────────────────────────────────────────────────────

    /// Decide one thermostat now, send what is owed, publish what changed.
    fn evaluate(&mut self, id: &str, now: DateTime<Utc>) {
        if !self.started {
            return;
        }
        let Controller {
            therms,
            readings,
            seen,
            fx,
            ..
        } = self;
        let Some(t) = therms.get_mut(id) else {
            return;
        };
        let act = t.cfg.actuator_device_id.clone();
        let sw = seen.get(&act).cloned().unwrap_or_default();
        let observed = sw.on;
        let now_on = observed.or(t.commanded);
        t.roll_day(now, now_on == Some(true));

        if t.hold.is_some_and(|h| now >= h.until) {
            info!(thermostat = %id, "Hold over — back to the usual target");
            t.hold = None;
        }
        if t.override_until.is_some_and(|u| now >= u) {
            info!(thermostat = %id, "Override over — taking over again");
            t.override_until = None;
            t.override_at = None;
            t.force = true;
        }

        // What it reads: current readings only.
        let timeout = t.cfg.sensor_timeout_secs() as i64;
        let fresh: Vec<f64> = t
            .cfg
            .sensor_device_ids
            .iter()
            .filter(|s| seen.get(*s).and_then(|x| x.online) != Some(false))
            .filter_map(|s| readings.get(s))
            .filter(|r| r.at.is_some_and(|at| (now - at).num_seconds() <= timeout))
            .filter_map(|r| r.value)
            .collect();
        t.temperature = aggregate(&fresh, &t.cfg.aggregation);
        let active = t.active();
        let was_stale = t.stale;
        if t.grace_until.is_some_and(|g| now >= g) {
            t.grace_until = None;
        }
        // Nothing at all heard from any of its sensors yet — not even an old
        // reading or "offline" — while its grace runs: not quiet, unknown.
        let heard_any = t.cfg.sensor_device_ids.iter().any(|s| {
            readings.get(s).is_some_and(|r| r.at.is_some())
                || seen.get(s).and_then(|x| x.online) == Some(false)
        });
        let waiting_for_first = t.grace_until.is_some() && fresh.is_empty() && !heard_any;
        t.stale = active && fresh.is_empty() && !waiting_for_first;
        if t.stale && !was_stale {
            warn!(thermostat = %id, "No current reading");
        }

        // The longest run.
        let max_run = t.cfg.max_run_secs() as i64;
        if active && max_run > 0 && now_on == Some(true) && !t.max_run_tripped {
            if let Some(since) = t.on_since {
                if (now - since).num_seconds() >= max_run {
                    warn!(thermostat = %id, "Longest run reached — turning it off");
                    t.max_run_tripped = true;
                    t.tripped_at = Some(now);
                }
            }
        }

        // What the switch should be: None is "leave it alone".
        let mut safety = false;
        let desired: Option<bool> = if act.is_empty() {
            t.entering_off = false;
            None
        } else if t.entering_off {
            safety = true;
            Some(false)
        } else if !active || t.override_until.is_some() || waiting_for_first {
            None
        } else if t.stale {
            t.calling = false;
            if t.cfg.off_when_stale() {
                safety = true;
                Some(false)
            } else {
                None
            }
        } else {
            let prev = if t.calling {
                t.cfg.mode.as_str()
            } else {
                "idle"
            };
            let current = t.temperature.unwrap_or(t.target());
            t.calling = compute_call_for(current, t.target(), t.cfg.hysteresis, &t.cfg.mode, prev)
                != "idle";
            if t.max_run_tripped && !t.calling {
                t.max_run_tripped = false;
                t.tripped_at = None;
            }
            if t.max_run_tripped {
                safety = true;
            }
            Some(t.calling && !t.max_run_tripped)
        };

        t.pending = None;
        t.lockout_until = None;
        let mut send: Option<bool> = None;
        match desired {
            Some(w) if t.commanded == Some(w) && !t.confirmed && !t.force => {
                // Waiting for the switch to say so; ask again when due.
                if sw.online != Some(false) && t.retry_at.is_some_and(|r| now >= r) {
                    send = Some(w);
                }
            }
            Some(w) if now_on == Some(w) && !t.force => {
                if observed == Some(w) {
                    t.commanded = Some(w);
                    t.confirmed = true;
                }
            }
            Some(w) => {
                // Asking again for what it was already asked is no new switch:
                // the guards count from the first ask.
                let transition = now_on != Some(w) && t.commanded != Some(w);
                let remaining = if safety || t.bypass_guard || !transition {
                    0
                } else {
                    lockout_remaining(
                        now_on == Some(true),
                        t.cfg.min_on_secs,
                        t.cfg.min_off_secs,
                        t.last_change,
                        now,
                    )
                };
                if remaining > 0 {
                    t.pending = Some(if now_on == Some(true) {
                        "waiting_min_on"
                    } else {
                        "waiting_min_off"
                    });
                    t.lockout_until = Some(now + Duration::seconds(remaining as i64));
                } else {
                    if transition {
                        t.last_change = Some(now);
                    }
                    send = Some(w);
                }
            }
            None => {
                t.retry_at = None;
                t.force = false;
            }
        }

        if let Some(w) = send {
            let retry = t.commanded == Some(w) && !t.confirmed;
            if retry {
                t.attempts += 1;
                info!(thermostat = %id, actuator = %act, on = w, attempt = t.attempts, "Asking the switch again");
            } else {
                t.attempts = 1;
                t.waiting_since = Some(now);
                info!(thermostat = %id, actuator = %act, on = w, "Switching");
            }
            t.commanded = Some(w);
            // Sent again while it already says so: it may not report a
            // change that isn't one, so that is its confirmation.
            t.confirmed = observed == Some(w);
            t.retry_at = Some(now + Duration::seconds(backoff_secs(t.attempts)));
            t.force = false;
            t.bypass_guard = false;
            if t.entering_off && t.attempts >= 3 {
                t.entering_off = false;
            }
            if w && t.on_since.is_none() {
                t.on_since = Some(now);
            }
            if !w && observed.is_none() {
                t.on_since = None;
            }
            let payload = if w {
                t.cfg.actuator_on_cmd.clone().unwrap_or(json!({"on": true}))
            } else {
                t.cfg
                    .actuator_off_cmd
                    .clone()
                    .unwrap_or(json!({"on": false}))
            };
            fx.push(Effect::Command {
                actuator: act.clone(),
                payload,
            });
        }

        t.not_responding = desired.is_some()
            && (sw.online == Some(false)
                || (t.commanded.is_some()
                    && !t.confirmed
                    && t.waiting_since
                        .is_some_and(|s| (now - s).num_seconds() >= CONFIRM_SECS)));

        let state = state_payload(t, readings, seen, now);
        if t.published.as_ref() != Some(&state) {
            t.published = Some(state.clone());
            fx.push(Effect::PublishState {
                device_id: t.cfg.device_id(),
                state,
            });
        }
        let alerts = alerts_of(t, &sw);
        let key = serde_json::to_value(&alerts).unwrap_or_default();
        if t.alerts_sent.as_ref() != Some(&key) {
            t.alerts_sent = Some(key);
            fx.push(Effect::Alerts {
                device_id: t.cfg.device_id(),
                alerts,
            });
        }
    }

    /// Every 10 s: holds and overrides ending, readings going quiet, guards
    /// ending, switches to ask again, a run getting too long, a chosen switch
    /// that never showed itself.
    pub fn tick(&mut self, now: DateTime<Utc>) {
        let expired: Vec<String> = self
            .candidates
            .iter()
            .filter(|(_, c)| now >= c.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(c) = self.candidates.remove(&id) {
                let before = self.used_ids_with(&id);
                self.report(
                    &c.thermostat,
                    "set_actuator",
                    Err(format!(
                        "{id} didn't report a state in {CANDIDATE_SECS} s — is it a plug or switch?"
                    )),
                );
                self.resubscribe(&before);
            }
        }
        let ids: Vec<String> = self.therms.keys().cloned().collect();
        for id in ids {
            self.evaluate(&id, now);
        }
    }

    fn used_ids_with(&self, extra: &str) -> HashSet<String> {
        let mut s = self.used_ids();
        s.insert(extra.to_string());
        s
    }

    /// Stopping: every heater this plugin may have left running is turned off.
    pub fn shutdown(&mut self) {
        for t in self.therms.values() {
            if t.kind() != Kind::Heater || !t.active() || t.cfg.actuator_device_id.is_empty() {
                continue;
            }
            let observed = self.seen.get(&t.cfg.actuator_device_id).and_then(|s| s.on);
            if observed == Some(true) || t.commanded == Some(true) {
                info!(thermostat = %t.cfg.id, "Stopping — turning the heater off");
                self.fx.push(Effect::Command {
                    actuator: t.cfg.actuator_device_id.clone(),
                    payload: t
                        .cfg
                        .actuator_off_cmd
                        .clone()
                        .unwrap_or(json!({"on": false})),
                });
            }
        }
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// Say how a command went: on the device and as an event.
    fn report(&mut self, id: &str, operation: &str, result: Result<(), String>) {
        let Some(t) = self.therms.get_mut(id) else {
            return;
        };
        if let Err(e) = &result {
            warn!(thermostat = %id, operation, error = %e, "Command refused");
        }
        t.last_command_error = result.as_ref().err().cloned();
        self.fx.push(Effect::CommandResult {
            device_id: t.cfg.device_id(),
            operation: operation.to_string(),
            error: result.err(),
        });
    }

    /// A command on one of our devices.
    pub fn on_command(&mut self, device_id: &str, payload: &Value, now: DateTime<Utc>) {
        let Some(id) = self.own_id(device_id).map(str::to_string) else {
            warn!(
                device_id,
                "Command for a thermostat this plugin doesn't have"
            );
            return;
        };
        let op = operation_of(payload);
        let cmd = command_of(payload).to_string();
        let before = self.used_ids();
        let result = self.apply_command(&id, &cmd, payload, now);
        match result {
            // Reported when the chosen switch shows itself (or doesn't).
            Ok(Outcome::Later) => {}
            Ok(Outcome::Done { changed }) => {
                self.report(&id, &op, Ok(()));
                if changed {
                    self.keep_runtime_changes();
                }
            }
            Err(e) => self.report(&id, &op, Err(e)),
        }
        self.resubscribe(&before);
        self.evaluate(&id, now);
        // `report` changed `last_command_error`; evaluate published it.
    }

    fn apply_command(
        &mut self,
        id: &str,
        cmd: &str,
        p: &Value,
        now: DateTime<Utc>,
    ) -> Result<Outcome, String> {
        let num = |k: &str| p.get(k).and_then(Value::as_f64);
        match cmd {
            "set_setpoint" => {
                let v = num("value").ok_or("set_setpoint needs a number `value` (°C)")?;
                self.set_setpoint(id, v)
            }
            "set_mode" => {
                let m = p
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or("set_mode needs `value`")?
                    .to_string();
                self.set_mode(id, &m)
            }
            "" => {
                let (sp, mode) = attribute_writes(p);
                let mode = mode.map(str::to_string);
                if sp.is_none() && mode.is_none() {
                    return Err("nothing to do: send setpoint, hvac_mode or an action".to_string());
                }
                let mut changed = false;
                if let Some(m) = mode {
                    changed |= self.set_mode(id, &m)?.changed();
                }
                if let Some(v) = sp {
                    changed |= self.set_setpoint(id, v)?.changed();
                }
                Ok(Outcome::Done { changed })
            }
            "hold" => {
                let sp = num("setpoint").ok_or("hold needs `setpoint` (°C)")?;
                if let Some(problem) = setpoint_problem(sp) {
                    return Err(problem.message);
                }
                let secs = match (
                    p.get("for_secs").and_then(Value::as_u64),
                    parse_time(p.get("until")),
                ) {
                    (Some(s), _) => s,
                    (None, Some(u)) => (u - now).num_seconds().max(0) as u64,
                    _ => return Err("hold needs `for_secs` (or `until`)".into()),
                };
                if !(60..=MAX_HOLD_SECS).contains(&secs) {
                    return Err(format!(
                        "a hold lasts from a minute to {} days",
                        MAX_HOLD_SECS / 86400
                    ));
                }
                let t = self.therm(id)?;
                t.person_changed_it();
                t.hold = Some(Hold {
                    setpoint: sp,
                    until: now + Duration::seconds(secs as i64),
                });
                Ok(Outcome::Done { changed: false })
            }
            "resume" => {
                let t = self.therm(id)?;
                t.hold = None;
                t.person_changed_it();
                t.force = t.active();
                Ok(Outcome::Done { changed: false })
            }
            "resync" | "recalculate" => {
                let t = self.therm(id)?;
                t.force = t.active() && !t.cfg.actuator_device_id.is_empty();
                Ok(Outcome::Done { changed: false })
            }
            "start_now" => {
                let t = self.therm(id)?;
                t.bypass_guard = true;
                Ok(Outcome::Done { changed: false })
            }
            "set_hysteresis" => {
                let v = num("value").ok_or("set_hysteresis needs a number `value` (°C)")?;
                if !(0.0..=crate::config::MAX_HYSTERESIS).contains(&v) {
                    return Err(format!(
                        "the swing is 0 to {} °C",
                        crate::config::MAX_HYSTERESIS
                    ));
                }
                self.therm(id)?.cfg.hysteresis = v;
                Ok(Outcome::Done { changed: true })
            }
            "set_aggregation" => {
                let v = p.get("value").and_then(Value::as_str).unwrap_or("");
                if !matches!(v, "average" | "min" | "max") {
                    return Err("aggregation is average, min or max".into());
                }
                self.therm(id)?.cfg.aggregation = v.to_string();
                Ok(Outcome::Done { changed: true })
            }
            "set_short_cycle" => {
                let on = p.get("min_on_secs").and_then(Value::as_u64);
                let off = p.get("min_off_secs").and_then(Value::as_u64);
                if on.is_none() && off.is_none() {
                    return Err("set_short_cycle needs min_on_secs or min_off_secs".into());
                }
                let max = crate::config::MAX_GUARD_SECS;
                if on.is_some_and(|v| v > max) || off.is_some_and(|v| v > max) {
                    return Err(format!("a guard is at most {max} s"));
                }
                let t = self.therm(id)?;
                if let Some(v) = on {
                    t.cfg.min_on_secs = v;
                }
                if let Some(v) = off {
                    t.cfg.min_off_secs = v;
                }
                Ok(Outcome::Done { changed: true })
            }
            "set_sensors" => {
                let own: Vec<String> = self.own_ids();
                let ids = p.get("sensor_ids").and_then(sensor_ids_of);
                let attr = p
                    .get("attribute")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let t = self.therm(id)?;
                if let Some(ids) = &ids {
                    if ids.iter().any(|s| own.contains(s)) {
                        return Err("a thermostat isn't a sensor".into());
                    }
                    if ids.is_empty() && t.active() {
                        return Err(format!(
                            "it is set to {} and needs at least one sensor",
                            t.cfg.mode
                        ));
                    }
                    if ids.contains(&t.cfg.actuator_device_id) {
                        return Err("its switch can't be one of its sensors".into());
                    }
                }
                if ids.is_none() && attr.is_none() {
                    return Err("set_sensors needs sensor_ids".into());
                }
                if let Some(ids) = ids {
                    if ids != t.cfg.sensor_device_ids {
                        t.grace_until = Some(now + Duration::seconds(FIRST_READING_SECS));
                    }
                    t.cfg.sensor_device_ids = ids;
                }
                if let Some(a) = attr {
                    t.cfg.sensor_attribute = a;
                }
                Ok(Outcome::Done { changed: true })
            }
            "set_actuator" => self.set_actuator(id, p, now),
            other => Err(format!("unknown command \"{other}\"")),
        }
    }

    fn therm(&mut self, id: &str) -> Result<&mut Thermo, String> {
        self.therms
            .get_mut(id)
            .ok_or_else(|| format!("no thermostat {id}"))
    }

    fn set_setpoint(&mut self, id: &str, v: f64) -> Result<Outcome, String> {
        if let Some(problem) = setpoint_problem(v) {
            return Err(problem.message);
        }
        let t = self.therm(id)?;
        let changed = t.cfg.setpoint != v;
        t.cfg.setpoint = v;
        t.hold = None;
        t.person_changed_it();
        Ok(Outcome::Done { changed })
    }

    fn set_mode(&mut self, id: &str, m: &str) -> Result<Outcome, String> {
        let t = self.therm(id)?;
        let kind = t.kind();
        if !kind.modes().contains(&m) {
            return Err(format!(
                "{} — {m} isn't one of its modes ({})",
                kind.only(),
                kind.modes().join(" or ")
            ));
        }
        let changed = t.cfg.mode != m;
        if changed {
            t.cfg.mode = m.to_string();
            t.calling = false;
            if m == "off" {
                // One OFF, then hands off.
                t.entering_off = !t.cfg.actuator_device_id.is_empty();
                t.force = true;
                t.hold = None;
            } else {
                t.entering_off = false;
            }
        }
        t.person_changed_it();
        Ok(Outcome::Done { changed })
    }

    /// Choose the switch. Only a plug or switch — a device that reports a
    /// boolean `on` and isn't a lock, a thermostat, or one of its sensors —
    /// and only `{"on": …}`: a custom payload comes from the config file, or
    /// anyone allowed to command a thermostat could send anything to any
    /// device.
    fn set_actuator(&mut self, id: &str, p: &Value, now: DateTime<Utc>) -> Result<Outcome, String> {
        if p.get("on_cmd").is_some() || p.get("off_cmd").is_some() {
            return Err("custom on/off payloads come from the config file only".into());
        }
        let new = p
            .get("device_id")
            .and_then(Value::as_str)
            .ok_or("set_actuator needs `device_id`")?
            .to_string();
        let t = self.therms.get(id).ok_or("no such thermostat")?;
        if new.is_empty() {
            self.switch_to(id, String::new(), now);
            return Ok(Outcome::Done { changed: true });
        }
        if new.starts_with("thermostat_") {
            return Err("a thermostat can't switch a thermostat".into());
        }
        if t.cfg.sensor_device_ids.contains(&new) {
            return Err(format!("{new} is one of its sensors"));
        }
        if new == t.cfg.actuator_device_id {
            return Ok(Outcome::Done { changed: false });
        }
        match self.seen.get(&new) {
            Some(s) if s.has_on && !s.lockish => {
                self.switch_to(id, new, now);
                Ok(Outcome::Done { changed: true })
            }
            Some(s) if s.lockish => Err(format!("{new} is a lock, not a plug or switch")),
            _ => {
                self.candidates.insert(
                    new,
                    Candidate {
                        thermostat: id.to_string(),
                        deadline: now + Duration::seconds(CANDIDATE_SECS),
                    },
                );
                Ok(Outcome::Later)
            }
        }
    }

    fn settle_candidate(&mut self, device_id: &str, now: DateTime<Utc>) {
        let s = self.seen.get(device_id).cloned().unwrap_or_default();
        if !(s.has_on || s.lockish) {
            return; // a partial that says nothing yet
        }
        let Some(c) = self.candidates.remove(device_id) else {
            return;
        };
        let before = self.used_ids_with(device_id);
        if s.has_on && !s.lockish {
            self.switch_to(&c.thermostat, device_id.to_string(), now);
            self.report(&c.thermostat, "set_actuator", Ok(()));
            self.keep_runtime_changes();
        } else {
            self.report(
                &c.thermostat,
                "set_actuator",
                Err(format!("{device_id} is a lock, not a plug or switch")),
            );
        }
        self.resubscribe(&before);
        self.evaluate(&c.thermostat, now);
    }

    /// Move to another switch, turning the old one off first if it is on and
    /// this thermostat runs it.
    fn switch_to(&mut self, id: &str, new: String, now: DateTime<Utc>) {
        let observed = self.observed(id);
        let Some(t) = self.therms.get_mut(id) else {
            return;
        };
        let old = std::mem::replace(&mut t.cfg.actuator_device_id, new);
        if !old.is_empty() && t.active() && (observed == Some(true) || t.commanded == Some(true)) {
            info!(thermostat = %id, actuator = %old, "Turning the old switch off");
            self.fx.push(Effect::Command {
                actuator: old,
                payload: t
                    .cfg
                    .actuator_off_cmd
                    .clone()
                    .unwrap_or(json!({"on": false})),
            });
        }
        t.reset_switch();
        t.last_change = Some(now);
        t.force = t.active();
    }

    /// Keep every thermostat's runtime changes in core's learned state.
    fn keep_runtime_changes(&mut self) {
        let kept: serde_json::Map<String, Value> = self
            .therms
            .iter()
            .filter_map(|(id, t)| {
                let file = self.file_entries.get(id)?;
                Some((id.clone(), runtime_change(file, &t.cfg)?))
            })
            .collect();
        // Learned state merges at the top level, so the whole set is sent.
        self.fx
            .push(Effect::PersistLearned(json!({ "thermostats": kept })));
        self.refresh_notices();
    }

    // ── Adding and removing ──────────────────────────────────────────────

    /// A thermostat added from the page (already written to the file).
    pub fn add(&mut self, entry: ThermostatEntry, now: DateTime<Utc>) {
        let before = self.used_ids();
        let id = entry.id.clone();
        self.file_entries.insert(id.clone(), entry.clone());
        let mut t = Thermo::new(entry.clone());
        t.force = t.active();
        t.grace_until = Some(now + Duration::seconds(FIRST_READING_SECS));
        t.day_start = Some(local_midnight(now));
        self.therms.insert(id.clone(), t);
        self.resubscribe(&before);
        if self.started {
            self.fx.push(Effect::Register(Box::new(entry)));
        }
        self.evaluate(&id, now);
        self.refresh_notices();
    }

    /// Remove a thermostat: its switch off first if it runs it and it is on,
    /// then its device. Returns whether it was one that ran.
    pub fn remove(&mut self, id: &str) -> bool {
        let observed = self.observed(id);
        let Some(t) = self.therms.get(id) else {
            let before = self.skipped.len();
            self.skipped.retain(|s| s.id.as_deref() != Some(id));
            if self.skipped.len() != before {
                self.refresh_notices();
            }
            return false;
        };
        let before = self.used_ids();
        if !t.cfg.actuator_device_id.is_empty()
            && t.active()
            && (observed == Some(true) || t.commanded == Some(true))
        {
            info!(thermostat = %id, "Removed — turning its switch off");
            self.fx.push(Effect::Command {
                actuator: t.cfg.actuator_device_id.clone(),
                payload: t
                    .cfg
                    .actuator_off_cmd
                    .clone()
                    .unwrap_or(json!({"on": false})),
            });
        }
        let device_id = t.cfg.device_id();
        self.therms.remove(id);
        self.file_entries.remove(id);
        self.fx.push(Effect::Alerts {
            device_id: device_id.clone(),
            alerts: vec![],
        });
        self.fx.push(Effect::Unregister(device_id));
        self.resubscribe(&before);
        self.refresh_notices();
        true
    }

    /// `recalculate_all`: every thermostat again; with `force`, every switch
    /// is sent what it should be.
    pub fn recalculate_all(&mut self, force: bool, now: DateTime<Utc>) {
        if force {
            for t in self.therms.values_mut() {
                t.force = t.active() && !t.cfg.actuator_device_id.is_empty();
            }
        }
        self.tick(now);
    }

    // ── Saying ───────────────────────────────────────────────────────────

    /// The plugin's notices and its own alerts, the whole set.
    fn refresh_notices(&mut self) {
        let mut n = Vec::new();
        if self.therms.is_empty() && self.skipped.is_empty() {
            n.push(
                PluginNotice::warning(
                    "no_thermostats_configured",
                    "No thermostats yet, so this plugin publishes no devices.",
                )
                .with_remedy(
                    "Add one from this page: a temperature sensor in the room and the plug or \
                     switch that runs the AC, heater or fan.",
                ),
            );
        }
        let mut alerts = Vec::new();
        for s in &self.skipped {
            let notice = PluginNotice::warning(
                s.code(),
                format!("Not running: “{}” — {}.", s.called(), s.problem),
            );
            n.push(match &s.fix {
                Some(f) => notice.with_remedy(format!(
                    "Make it {} °F ({} °C) from this page, or change it in Settings.",
                    f.fahrenheit, f.celsius
                )),
                None => notice.with_remedy("Change it in Settings; the others run as normal."),
            });
            alerts.push(
                PluginAlert::new(
                    "config_entry_skipped",
                    format!("“{}” isn't running: {}", s.called(), s.problem),
                )
                .key(s.id.clone().unwrap_or_else(|| s.index.to_string())),
            );
        }
        for (id, said) in &self.capped {
            n.push(PluginNotice::info(
                format!("config_value_capped:{id}"),
                format!("{id}: {} — the largest is used.", said.join("; ")),
            ));
        }
        for t in self.therms.values() {
            if t.kind() == Kind::Ac && t.cfg.min_on_secs == 0 && t.cfg.min_off_secs == 0 {
                n.push(
                    PluginNotice::warning(
                        format!("short_cycle_unguarded:{}", t.cfg.id),
                        format!(
                            "“{}” runs an AC with no short-cycle guard: it can be switched \
                             back on seconds after it went off, which wears out a compressor.",
                            t.cfg.name
                        ),
                    )
                    .with_remedy("Set Once on 3 min and Once off 5 min under How it switches."),
                );
            }
        }
        self.fx.push(Effect::Notices(n));
        self.fx.push(Effect::PluginAlerts(alerts));
    }

    /// What the page reads: the skipped entries, and each thermostat's settings.
    pub fn status(&self) -> Value {
        json!({
            "skipped": self.skipped,
            "thermostats": self.therms.values().map(|t| json!({
                "id": t.cfg.id,
                "device_id": t.cfg.device_id(),
                "name": t.cfg.name,
                "kind": t.kind().as_str(),
                "custom_payloads": t.cfg.actuator_on_cmd.is_some() || t.cfg.actuator_off_cmd.is_some(),
            })).collect::<Vec<_>>(),
            "started": self.started,
        })
    }

    #[cfg(test)]
    fn t(&self, id: &str) -> &Thermo {
        &self.therms[id]
    }
}

/// What `apply_command` did.
enum Outcome {
    Done {
        changed: bool,
    },
    /// Answered once the chosen switch shows itself.
    Later,
}

impl Outcome {
    fn changed(&self) -> bool {
        matches!(self, Outcome::Done { changed: true })
    }
}

/// What a thermostat publishes. `schema.rs` declares exactly these.
fn state_payload(
    t: &Thermo,
    readings: &HashMap<String, Reading>,
    seen: &HashMap<String, Seen>,
    now: DateTime<Utc>,
) -> Value {
    let sw = seen.get(&t.cfg.actuator_device_id);
    let observed = sw.and_then(|s| s.on);
    let active = t.active();
    let timeout = t.cfg.sensor_timeout_secs() as i64;
    let sensor_readings: serde_json::Map<String, Value> = t
        .cfg
        .sensor_device_ids
        .iter()
        .map(|s| {
            let r = readings.get(s).cloned().unwrap_or_default();
            let online = seen.get(s).and_then(|x| x.online) != Some(false);
            let current = online
                && r.value.is_some()
                && r.at.is_some_and(|at| (now - at).num_seconds() <= timeout);
            (
                s.clone(),
                json!({
                    "temperature": r.value,
                    "at": iso(r.shown_at),
                    "stale": !current,
                    "online": online,
                }),
            )
        })
        .collect();
    let hvac_action = if !active {
        "off"
    } else if observed == Some(true) {
        running_word(&t.cfg.mode)
    } else {
        "idle"
    };
    let reason = if !active {
        "off"
    } else if t.cfg.actuator_device_id.is_empty() {
        "no_switch"
    } else if t.stale {
        "sensors_stale"
    } else if t.override_until.is_some() {
        "override"
    } else if t.max_run_tripped {
        "max_run_exceeded"
    } else if t.not_responding {
        "actuator_not_responding"
    } else if let Some(p) = t.pending {
        p
    } else if t.calling {
        running_word(&t.cfg.mode)
    } else {
        "idle_in_band"
    };
    json!({
        "kind": t.kind().as_str(),
        "temperature": t.temperature.map(|v| (v * 10.0).round() / 10.0),
        "setpoint": t.cfg.setpoint,
        "hvac_mode": t.cfg.mode,
        "hvac_action": hvac_action,
        "reason": reason,
        "hold_setpoint": t.hold.map(|h| h.setpoint),
        "hold_until": iso(t.hold.map(|h| h.until)),
        "override_until": iso(t.override_until),
        "sensors_stale": t.stale,
        "sensor_readings": sensor_readings,
        "actuator_state": observed.or(t.commanded).unwrap_or(false),
        "actuator_confirmed": t.commanded.is_none() || t.confirmed,
        "actuator_online": sw.and_then(|s| s.online).unwrap_or(true),
        "actuator_last_change": iso(t.last_change),
        "on_since": iso(t.on_since),
        "pending_call": t.pending.map(|_| running_word(&t.cfg.mode)),
        "lockout_until": iso(t.lockout_until),
        "last_command_error": t.last_command_error,
        "today_from": iso(t.day_start),
        "run_secs_today": t.run_secs_today,
        "cycles_today": t.cycles_today,
        "longest_run_secs_today": t.longest_run_secs_today,
        "settled": true,

        "hysteresis": t.cfg.hysteresis,
        "aggregation": t.cfg.aggregation,
        "sensor_ids": t.cfg.sensor_device_ids,
        "sensor_attribute": t.cfg.sensor_attribute,
        "actuator_device_id": t.cfg.actuator_device_id,
        "min_on_secs": t.cfg.min_on_secs,
        "min_off_secs": t.cfg.min_off_secs,
        "max_run_secs": t.cfg.max_run_secs(),
        "sensor_timeout_secs": t.cfg.sensor_timeout_secs(),
        "on_stale": if t.cfg.off_when_stale() { "off" } else { "hold" },
        "override_secs": t.cfg.override_secs(),
    })
}

/// A thermostat's alerts now. The words leave the device names to the app:
/// the plugin only knows ids.
fn alerts_of(t: &Thermo, sw: &Seen) -> Vec<PluginAlert> {
    let mut a = Vec::new();
    if t.stale {
        a.push(PluginAlert::new(
            "sensors_stale",
            if t.cfg.off_when_stale() {
                "No current temperature reading, so its switch was turned off"
            } else {
                "No current temperature reading; its switch is left as it is"
            },
        ));
    }
    if t.not_responding {
        let reason = if sw.online == Some(false) {
            "Its switch is offline and can't be told anything".to_string()
        } else {
            format!(
                "Its switch hasn't confirmed turning {} (asked {} time{})",
                if t.commanded == Some(true) {
                    "on"
                } else {
                    "off"
                },
                t.attempts,
                if t.attempts == 1 { "" } else { "s" }
            )
        };
        a.push(
            PluginAlert::new("actuator_not_responding", reason).action(AlertAction {
                label: "Send it again".into(),
                id: "resync".into(),
                target: AlertActionTarget::Device,
                params: Value::Null,
                confirm: false,
            }),
        );
    }
    if t.max_run_tripped {
        let secs = t.cfg.max_run_secs();
        a.push(
            PluginAlert::new(
                "max_run_exceeded",
                format!(
                    "Ran {} without a break, so it was turned off{}",
                    words_secs(secs),
                    t.tripped_at
                        .map(|x| format!(" at {}", clock(x)))
                        .unwrap_or_default()
                ),
            )
            .action(AlertAction {
                label: "Start it again".into(),
                id: "resume".into(),
                target: AlertActionTarget::Device,
                params: Value::Null,
                confirm: true,
            }),
        );
    }
    if let (Some(until), true) = (t.override_until, t.active()) {
        a.push(
            PluginAlert::new(
                "manual_override",
                format!(
                    "Switched by hand{} — left alone until {}",
                    t.override_at
                        .map(|x| format!(" at {}", clock(x)))
                        .unwrap_or_default(),
                    clock(until)
                ),
            )
            .action(AlertAction {
                label: "Take over now".into(),
                id: "resume".into(),
                target: AlertActionTarget::Device,
                params: Value::Null,
                confirm: false,
            }),
        );
    }
    a
}

/// "4 h", "90 min".
pub fn words_secs(s: u64) -> String {
    if s >= 3600 && s.is_multiple_of(3600) {
        format!("{} h", s / 3600)
    } else if s >= 60 {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
pub fn sample_state_payload() -> Value {
    let cfg: ThermostatEntry = serde_json::from_value(json!({
        "id": "t1", "name": "Test", "sensor_device_ids": ["sensor_a"], "setpoint": 21.0,
    }))
    .expect("minimal thermostat entry deserialises");
    state_payload(
        &Thermo::new(cfg),
        &HashMap::new(),
        &HashMap::new(),
        Utc::now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const T0: &str = "2026-10-21T14:00:00Z";

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(T0)
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(secs)
    }

    /// The house's two ACs, as a stand-in: the MBR AC with no guards (as the
    /// house has it), the office with 5 min each way, and a garage heater.
    fn house() -> Controller {
        Controller::new(
            Config::parse(
                r#"
[[thermostat]]
id = "mbr"
name = "Master Bedroom AC Thermostat"
sensor_device_ids = ["mbr_temp"]
setpoint = 20.0
hysteresis = 1.0
mode = "cool"
actuator_device_id = "mbr_ac"

[[thermostat]]
id = "office"
name = "Office AC Thermostat"
sensor_device_ids = ["office_temp"]
setpoint = 20.0
hysteresis = 1.0
mode = "cool"
actuator_device_id = "office_ac"
min_on_secs = 300
min_off_secs = 300

[[thermostat]]
id = "garage"
name = "Garage Heater Thermostat"
kind = "heater"
sensor_device_ids = ["garage_temp"]
setpoint = 7.0
mode = "heat"
actuator_device_id = "heater"

[[thermostat]]
id = "den"
name = "Den"
sensor_device_ids = ["den_temp"]
setpoint = 20.0
mode = "off"
actuator_device_id = "den_ac"
"#,
            )
            .unwrap(),
        )
    }

    fn cmds(fx: &[Effect]) -> Vec<(String, bool)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Command { actuator, payload } => {
                    Some((actuator.clone(), payload["on"].as_bool().unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    /// What was sent to one switch.
    fn sent(fx: &[Effect], to: &str) -> Vec<bool> {
        cmds(fx)
            .into_iter()
            .filter(|(a, _)| a == to)
            .map(|(_, on)| on)
            .collect()
    }

    fn last_state(fx: &[Effect], device: &str) -> Option<Value> {
        fx.iter().rev().find_map(|e| match e {
            Effect::PublishState { device_id, state } if device_id == device => Some(state.clone()),
            _ => None,
        })
    }

    fn alert_topics(fx: &[Effect], device: &str) -> Option<Vec<String>> {
        fx.iter().rev().find_map(|e| match e {
            Effect::Alerts { device_id, alerts } if device_id == device => {
                Some(alerts.iter().map(|a| a.topic.clone()).collect())
            }
            _ => None,
        })
    }

    fn results(fx: &[Effect]) -> Vec<(String, Option<String>)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::CommandResult {
                    operation, error, ..
                } => Some((operation.clone(), error.clone())),
                _ => None,
            })
            .collect()
    }

    fn temp(c: &mut Controller, id: &str, v: f64, s: i64) {
        c.on_state(id, &json!({ "temperature": v }), at(s));
    }
    fn switch(c: &mut Controller, id: &str, on: bool, s: i64) {
        c.on_state(id, &json!({ "on": on }), at(s));
    }

    /// Started with every switch off and every room at `room`.
    fn started(room: f64) -> Controller {
        let mut c = house();
        for (s, a) in [
            ("mbr_temp", "mbr_ac"),
            ("office_temp", "office_ac"),
            ("garage_temp", "heater"),
            ("den_temp", "den_ac"),
        ] {
            temp(&mut c, s, if s == "garage_temp" { 3.0 } else { room }, 0);
            switch(&mut c, a, false, 0);
        }
        c.start(at(0));
        c
    }

    /// H2: before start nothing is sent, whatever arrives; at start one pass,
    /// and an Off thermostat's switch is never touched — even if it is on.
    #[test]
    fn nothing_is_sent_before_start_and_off_is_never_switched_at_boot() {
        let mut c = house();
        temp(&mut c, "mbr_temp", 25.0, 0);
        switch(&mut c, "mbr_ac", false, 0);
        switch(&mut c, "den_ac", true, 0);
        temp(&mut c, "den_temp", 30.0, 0);
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_setpoint", "value": 21.0}),
            at(0),
        );
        let fx = c.take_effects();
        assert!(cmds(&fx).is_empty(), "{fx:?}");
        assert!(last_state(&fx, "thermostat_mbr").is_none());

        c.start(at(1));
        let fx = c.take_effects();
        let sent = cmds(&fx);
        assert!(sent.contains(&("mbr_ac".into(), true)), "{sent:?}");
        assert!(!sent.iter().any(|(a, _)| a == "den_ac"), "{sent:?}");
        // Registered before anything is published for it.
        let reg = fx
            .iter()
            .position(|e| matches!(e, Effect::Register(t) if t.id == "mbr"))
            .unwrap();
        let pubd = fx
            .iter()
            .position(|e| matches!(e, Effect::PublishState { device_id, .. } if device_id == "thermostat_mbr"))
            .unwrap();
        assert!(reg < pubd);
    }

    /// H2: settling sends the state even when it seems so already — the
    /// cached switch state may be wrong after a restart.
    #[test]
    fn the_settling_pass_resends_what_it_should_be() {
        let c = started(19.0);
        let mut c = c;
        let fx = c.take_effects();
        let sent = cmds(&fx);
        assert!(sent.contains(&("mbr_ac".into(), false)), "{sent:?}");
        assert!(sent.contains(&("office_ac".into(), false)), "{sent:?}");
    }

    /// H1: the reading ages out; with none current the switch is turned off,
    /// `sensors_stale` is true and the alert is up — and a fresh reading
    /// brings it back.
    #[test]
    fn a_quiet_sensor_turns_the_switch_off_and_says_so() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 1);
        c.take_effects();
        c.tick(at(7000));
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(7210));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [false]);
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["sensors_stale"], json!(true));
        assert_eq!(s["reason"], json!("sensors_stale"));
        assert_eq!(s["sensor_readings"]["mbr_temp"]["stale"], json!(true));
        assert_eq!(s["temperature"], Value::Null);
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            ["sensors_stale"]
        );
        switch(&mut c, "mbr_ac", false, 7215);
        temp(&mut c, "mbr_temp", 25.0, 7300);
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [true]);
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            Vec::<String>::new()
        );
    }

    /// H1: offline, null and impossible readings don't count.
    #[test]
    fn offline_null_and_impossible_readings_do_not_count() {
        let mut c = started(25.0);
        c.take_effects();
        c.on_availability("mbr_temp", false, at(10));
        let s = last_state(&c.take_effects(), "thermostat_mbr").unwrap();
        assert_eq!(s["sensors_stale"], json!(true));
        assert_eq!(s["sensor_readings"]["mbr_temp"]["online"], json!(false));
        c.on_availability("mbr_temp", true, at(20));
        assert_eq!(
            last_state(&c.take_effects(), "thermostat_mbr").unwrap()["sensors_stale"],
            json!(false)
        );
        c.on_state("mbr_temp", &json!({"temperature": null}), at(30));
        assert_eq!(
            last_state(&c.take_effects(), "thermostat_mbr").unwrap()["sensors_stale"],
            json!(true)
        );
        temp(&mut c, "mbr_temp", 85.0, 40);
        assert!(last_state(&c.take_effects(), "thermostat_mbr").is_none());
        assert!(c.t("mbr").stale);
    }

    /// H1: `on_stale = "hold"` keeps the switch as it is, still said.
    #[test]
    fn on_stale_hold_leaves_the_switch() {
        let mut c = Controller::new(
            Config::parse(
                "[[thermostat]]\nid=\"a\"\nname=\"A\"\nsensor_device_ids=[\"s\"]\nsetpoint=20.0\nmode=\"cool\"\nactuator_device_id=\"p\"\non_stale=\"hold\"\n",
            )
            .unwrap(),
        );
        temp(&mut c, "s", 25.0, 0);
        switch(&mut c, "p", true, 0);
        c.start(at(0));
        c.take_effects();
        c.tick(at(8000));
        let fx = c.take_effects();
        assert!(cmds(&fx).is_empty());
        assert_eq!(
            last_state(&fx, "thermostat_a").unwrap()["reason"],
            json!("sensors_stale")
        );
    }

    /// A reading's age survives a restart: our own retained state says when
    /// the value it still has was heard.
    #[test]
    fn a_reading_keeps_its_age_across_a_restart() {
        let mut c = house();
        c.on_state(
            "thermostat_mbr",
            &json!({"sensor_readings": {"mbr_temp": {"temperature": 25.0, "at": "2026-10-21T10:00:00Z"}}}),
            at(0),
        );
        temp(&mut c, "mbr_temp", 25.0, 0);
        switch(&mut c, "mbr_ac", true, 0);
        c.start(at(0));
        let fx = c.take_effects();
        assert_eq!(
            last_state(&fx, "thermostat_mbr").unwrap()["sensors_stale"],
            json!(true)
        );
        assert!(cmds(&fx).contains(&("mbr_ac".to_string(), false)));
    }

    /// H3: a switch that never says it went on is "not responding" after
    /// 30 s, asked again with backoff, and confirmed by its echo.
    #[test]
    fn an_unconfirmed_command_is_retried_and_alerted() {
        let mut c = started(19.0);
        switch(&mut c, "mbr_ac", false, 1);
        c.take_effects();
        temp(&mut c, "mbr_temp", 25.0, 10);
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [true]);
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["actuator_confirmed"], json!(false));
        assert_eq!(
            s["hvac_action"],
            json!("idle"),
            "not cooling until it says so"
        );
        c.tick(at(30));
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(41));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [true]);
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            ["actuator_not_responding"]
        );
        assert_eq!(
            last_state(&fx, "thermostat_mbr").unwrap()["reason"],
            json!("actuator_not_responding")
        );
        // Next try 60 s later, not 30.
        c.tick(at(80));
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(102));
        assert_eq!(sent(&c.take_effects(), "mbr_ac").len(), 1);
        switch(&mut c, "mbr_ac", true, 105);
        let fx = c.take_effects();
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["actuator_confirmed"], json!(true));
        assert_eq!(s["hvac_action"], json!("cooling"));
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            Vec::<String>::new()
        );
        assert!(
            c.t("mbr").override_until.is_none(),
            "an echo is not a hand flip"
        );
    }

    /// H3: a switch that goes offline is said at once, and asked again the
    /// moment it is back.
    #[test]
    fn an_offline_switch_is_said_and_asked_again_when_back() {
        let mut c = started(19.0);
        switch(&mut c, "mbr_ac", false, 1);
        c.on_availability("mbr_ac", false, at(5));
        let fx = c.take_effects();
        assert_eq!(
            last_state(&fx, "thermostat_mbr").unwrap()["actuator_online"],
            json!(false)
        );
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            ["actuator_not_responding"]
        );
        temp(&mut c, "mbr_temp", 25.0, 10);
        assert_eq!(sent(&c.take_effects(), "mbr_ac").len(), 1);
        c.tick(at(100));
        assert!(
            sent(&c.take_effects(), "mbr_ac").is_empty(),
            "not sent into the void"
        );
        c.on_availability("mbr_ac", true, at(110));
        assert_eq!(sent(&c.take_effects(), "mbr_ac"), [true]);
    }

    /// M1: a hand flip is an override — left alone, said, and over when the
    /// time is up or a person changes the target.
    #[test]
    fn a_hand_flip_is_left_alone_until_the_override_ends() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 1);
        c.take_effects();
        switch(&mut c, "mbr_ac", false, 100);
        let fx = c.take_effects();
        assert!(sent(&fx, "mbr_ac").is_empty());
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["reason"], json!("override"));
        assert_eq!(s["override_until"], json!("2026-10-21T15:01:40Z"));
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            ["manual_override"]
        );
        c.tick(at(3000));
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(3700));
        assert_eq!(sent(&c.take_effects(), "mbr_ac"), [true]);

        // …or a person changing the target ends it now.
        switch(&mut c, "mbr_ac", true, 3705);
        switch(&mut c, "mbr_ac", false, 3800);
        c.take_effects();
        c.on_command("thermostat_mbr", &json!({"setpoint": 19.5}), at(3810));
        assert_eq!(sent(&c.take_effects(), "mbr_ac"), [true]);
    }

    /// M1: Off sends one OFF and then leaves the switch to the household.
    #[test]
    fn off_sends_one_off_then_hands_off() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 1);
        c.take_effects();
        c.on_command("thermostat_mbr", &json!({"hvac_mode": "off"}), at(10));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [false]);
        assert_eq!(results(&fx), vec![("hvac_mode".to_string(), None)]);
        switch(&mut c, "mbr_ac", false, 11);
        switch(&mut c, "mbr_ac", true, 200);
        c.tick(at(400));
        let fx = c.take_effects();
        assert!(sent(&fx, "mbr_ac").is_empty());
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["hvac_action"], json!("off"));
        assert_eq!(
            s["override_until"],
            Value::Null,
            "Off: a hand flip is no override"
        );
    }

    /// M2 + guards: a lockout delays; a forced re-sync is not lost behind it.
    #[test]
    fn the_short_cycle_guard_waits_and_a_resync_survives_it() {
        let mut c = started(25.0);
        switch(&mut c, "office_ac", true, 1);
        c.take_effects();
        temp(&mut c, "office_temp", 18.0, 60);
        let fx = c.take_effects();
        assert!(sent(&fx, "office_ac").is_empty());
        let s = last_state(&fx, "thermostat_office").unwrap();
        assert_eq!(s["reason"], json!("waiting_min_on"));
        assert_eq!(s["lockout_until"], json!("2026-10-21T14:05:00Z"));
        c.on_command("thermostat_office", &json!({"action": "resync"}), at(70));
        assert!(sent(&c.take_effects(), "office_ac").is_empty());
        c.tick(at(301));
        assert_eq!(sent(&c.take_effects(), "office_ac"), [false]);
        // start_now skips the guard once.
        switch(&mut c, "office_ac", false, 302);
        temp(&mut c, "office_temp", 25.0, 310);
        assert_eq!(
            last_state(&c.take_effects(), "thermostat_office").unwrap()["reason"],
            json!("waiting_min_off")
        );
        c.on_command(
            "thermostat_office",
            &json!({"action": "start_now"}),
            at(320),
        );
        assert_eq!(sent(&c.take_effects(), "office_ac"), [true]);
    }

    /// H5: removing a thermostat turns its running switch off first; one that
    /// is Off is left alone.
    #[test]
    fn removing_turns_its_switch_off_first() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 1);
        switch(&mut c, "den_ac", true, 1);
        c.take_effects();
        assert!(c.remove("mbr"));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [false]);
        assert!(fx.contains(&Effect::Unregister("thermostat_mbr".into())));
        assert!(fx.contains(&Effect::Unsubscribe("mbr_ac".into())));
        assert!(c.remove("den"));
        assert!(sent(&c.take_effects(), "den_ac").is_empty());
    }

    /// H5 + M7: a new switch must show it is one; the old one is turned off.
    #[test]
    fn choosing_another_switch_checks_it_and_turns_the_old_one_off() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 1);
        c.take_effects();
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_actuator", "device_id": "front_door"}),
            at(10),
        );
        let fx = c.take_effects();
        assert!(fx.contains(&Effect::Subscribe("front_door".into())));
        assert!(results(&fx).is_empty(), "answered once it shows itself");
        c.on_state(
            "front_door",
            &json!({"locked": true, "battery_pct": 80}),
            at(11),
        );
        let fx = c.take_effects();
        assert!(results(&fx)[0].1.as_ref().unwrap().contains("lock"));
        assert!(fx.contains(&Effect::Unsubscribe("front_door".into())));
        assert_eq!(c.t("mbr").cfg.actuator_device_id, "mbr_ac");

        for bad in [
            json!({"action": "set_actuator", "device_id": "thermostat_office"}),
            json!({"action": "set_actuator", "device_id": "mbr_temp"}),
            json!({"action": "set_actuator", "device_id": "x", "on_cmd": {"open": true}}),
        ] {
            c.on_command("thermostat_mbr", &bad, at(12));
            assert!(results(&c.take_effects())[0].1.is_some(), "{bad}");
        }

        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_actuator", "device_id": "plug_2"}),
            at(20),
        );
        c.take_effects();
        c.on_state("plug_2", &json!({"on": false}), at(21));
        let fx = c.take_effects();
        assert_eq!(results(&fx), vec![("set_actuator".to_string(), None)]);
        let sent = cmds(&fx);
        assert_eq!(sent[0], ("mbr_ac".to_string(), false), "{sent:?}");
        assert!(sent.contains(&("plug_2".to_string(), true)), "{sent:?}");
        assert!(fx.iter().any(|e| matches!(e, Effect::PersistLearned(_))));

        // One that never answers is refused after 10 s.
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_actuator", "device_id": "ghost"}),
            at(30),
        );
        c.take_effects();
        c.tick(at(41));
        let fx = c.take_effects();
        assert!(results(&fx)[0]
            .1
            .as_ref()
            .unwrap()
            .contains("didn't report"));
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert!(s["last_command_error"].as_str().unwrap().contains("ghost"));
    }

    /// H8: an AC offers Cool and Off; Heat is refused with a command error.
    #[test]
    fn a_mode_its_kind_hasnt_got_is_refused() {
        let mut c = started(25.0);
        c.take_effects();
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_mode", "value": "heat"}),
            at(5),
        );
        let fx = c.take_effects();
        let e = results(&fx)[0].1.clone().unwrap();
        assert!(e.contains("An AC only cools"), "{e}");
        assert_eq!(c.t("mbr").cfg.mode, "cool");
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["last_command_error"], json!(e));
        c.on_command("thermostat_garage", &json!({"hvac_mode": "cool"}), at(6));
        assert!(results(&c.take_effects())[0]
            .1
            .as_ref()
            .unwrap()
            .contains("heats"));
        // A good command clears the error.
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_setpoint", "value": 21.0}),
            at(7),
        );
        let fx = c.take_effects();
        assert_eq!(
            last_state(&fx, "thermostat_mbr").unwrap()["last_command_error"],
            Value::Null
        );
        // °F is refused, not obeyed.
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "set_setpoint", "value": 68}),
            at(8),
        );
        assert!(results(&c.take_effects())[0]
            .1
            .as_ref()
            .unwrap()
            .contains("°F"));
    }

    /// H9: the longest run turns a heater off and says so; stopping turns
    /// running heaters off and leaves ACs alone.
    #[test]
    fn a_heater_is_never_left_running() {
        let mut c = started(25.0);
        switch(&mut c, "heater", true, 2);
        switch(&mut c, "mbr_ac", true, 2);
        c.take_effects();
        temp(&mut c, "garage_temp", 3.0, 4 * 3600);
        c.tick(at(4 * 3600 + 5));
        let fx = c.take_effects();
        assert!(cmds(&fx).contains(&("heater".to_string(), false)));
        assert_eq!(
            last_state(&fx, "thermostat_garage").unwrap()["reason"],
            json!("max_run_exceeded")
        );
        assert_eq!(
            alert_topics(&fx, "thermostat_garage").unwrap(),
            ["max_run_exceeded"]
        );
        switch(&mut c, "heater", false, 4 * 3600 + 6);
        c.take_effects();
        c.on_command(
            "thermostat_garage",
            &json!({"action": "resume"}),
            at(4 * 3600 + 60),
        );
        assert!(cmds(&c.take_effects()).contains(&("heater".to_string(), true)));
        switch(&mut c, "heater", true, 4 * 3600 + 61);
        c.take_effects();
        c.shutdown();
        // The heater only: an AC left running is not a fire risk.
        assert_eq!(cmds(&c.take_effects()), vec![("heater".to_string(), false)]);
    }

    /// Hold: a different target for a while, then back by itself.
    #[test]
    fn a_hold_ends_by_itself() {
        let mut c = started(19.8);
        c.take_effects();
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "hold", "setpoint": 19.0, "for_secs": 3600}),
            at(10),
        );
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [true]);
        let s = last_state(&fx, "thermostat_mbr").unwrap();
        assert_eq!(s["hold_setpoint"], json!(19.0));
        assert_eq!(s["hold_until"], json!("2026-10-21T15:00:10Z"));
        switch(&mut c, "mbr_ac", true, 11);
        // Cooled to 19.4: still above the hold's band, below the usual one.
        temp(&mut c, "mbr_temp", 19.4, 3000);
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(3611));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [false]);
        assert_eq!(
            last_state(&fx, "thermostat_mbr").unwrap()["hold_until"],
            Value::Null
        );
        // Refused: too short, or °F.
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "hold", "setpoint": 19.0, "for_secs": 5}),
            at(4000),
        );
        assert!(results(&c.take_effects())[0].1.is_some());
    }

    /// L4: a sensor repeating itself doesn't change the thermostat.
    #[test]
    fn nothing_changes_by_itself() {
        let mut c = started(19.0);
        c.take_effects();
        temp(&mut c, "mbr_temp", 19.0, 60);
        c.tick(at(70));
        assert!(last_state(&c.take_effects(), "thermostat_mbr").is_none());
        temp(&mut c, "mbr_temp", 19.0, 400);
        assert!(last_state(&c.take_effects(), "thermostat_mbr").is_some());
    }

    /// Today's figures, from what the switch reported.
    #[test]
    fn today_counts_runs() {
        let mut c = started(25.0);
        switch(&mut c, "mbr_ac", true, 10);
        switch(&mut c, "mbr_ac", false, 610);
        c.take_effects();
        c.on_command("thermostat_mbr", &json!({"action": "resume"}), at(700));
        switch(&mut c, "mbr_ac", true, 701);
        switch(&mut c, "mbr_ac", false, 1001);
        let s = last_state(&c.take_effects(), "thermostat_mbr").unwrap();
        assert_eq!(s["cycles_today"], json!(2));
        assert_eq!(s["run_secs_today"], json!(900));
        assert_eq!(s["longest_run_secs_today"], json!(600));
    }

    /// L1: what our own retained state remembered is used at start — the
    /// guard's clock, a hold — and our own state after start is an echo.
    #[test]
    fn own_retained_state_is_used_at_start_and_ignored_after() {
        let mut c = house();
        c.on_state(
            "thermostat_office",
            &json!({"actuator_last_change": "2026-10-21T13:58:00Z",
                    "hold_until": "2026-10-21T16:00:00Z", "hold_setpoint": 19.0}),
            at(0),
        );
        temp(&mut c, "office_temp", 25.0, 0);
        switch(&mut c, "office_ac", false, 0);
        c.start(at(0));
        let fx = c.take_effects();
        assert!(!cmds(&fx).iter().any(|(a, _)| a == "office_ac"), "{fx:?}");
        let s = last_state(&fx, "thermostat_office").unwrap();
        assert_eq!(s["reason"], json!("waiting_min_off"));
        assert_eq!(s["hold_setpoint"], json!(19.0));
        c.on_state("thermostat_office", &json!({"hold_until": null}), at(5));
        assert!(c.t("office").hold.is_some());
        // An id that only looks like ours is someone else's device.
        assert!(c.own_id("thermostat_nobody").is_none());
    }

    /// What a homeCore restart would lose (our own retained state with it) is
    /// kept in learned state when it changes, and only then.
    #[test]
    fn the_running_state_is_kept_in_learned_state() {
        let mut c = started(19.8);
        c.take_effects();
        c.on_command(
            "thermostat_mbr",
            &json!({"action": "hold", "setpoint": 19.0, "for_secs": 3600}),
            at(10),
        );
        let fx = c.take_effects();
        let kept: Vec<_> = fx
            .iter()
            .filter_map(|e| match e {
                Effect::PersistLearned(d) => d.get("runtime").cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(kept.len(), 1, "{fx:?}");
        assert_eq!(kept[0]["mbr"]["hold_setpoint"], json!(19.0));
        assert_eq!(kept[0]["mbr"]["hold_until"], json!("2026-10-21T15:00:10Z"));
        assert!(kept[0]["mbr"].get("sensor_readings").is_none());
        // A new reading alone changes nothing kept.
        temp(&mut c, "mbr_temp", 19.7, 20);
        assert!(!c
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::PersistLearned(d) if d.get("runtime").is_some())));
    }

    /// After homeCore restarted, our own retained state is gone: learned
    /// state brings back the hold and the guard's clock. Our own retained
    /// state, when there is any, is newer and wins whichever comes first.
    #[test]
    fn learned_state_stands_in_for_lost_retained_state() {
        let runtime = json!({"runtime": {"office": {
            "actuator_last_change": "2026-10-21T13:58:00Z",
            "hold_until": "2026-10-21T16:00:00Z", "hold_setpoint": 19.0}}});
        let mut c = house();
        c.apply_learned(&runtime, at(0));
        temp(&mut c, "office_temp", 25.0, 0);
        switch(&mut c, "office_ac", false, 0);
        c.start(at(0));
        let fx = c.take_effects();
        assert!(!cmds(&fx).iter().any(|(a, _)| a == "office_ac"), "{fx:?}");
        let s = last_state(&fx, "thermostat_office").unwrap();
        assert_eq!(s["reason"], json!("waiting_min_off"));
        assert_eq!(s["hold_setpoint"], json!(19.0));

        let mut c = house();
        c.on_state("thermostat_office", &json!({"hold_until": null}), at(0));
        c.apply_learned(&runtime, at(0));
        assert!(c.t("office").hold.is_none());
    }

    /// Today's figures survive a restart — told by the instant the day began,
    /// not a date that needs the timezone — and start again at midnight.
    #[test]
    fn todays_figures_survive_a_restart_and_end_at_midnight() {
        let mut c = house();
        c.on_state(
            "thermostat_mbr",
            &json!({"today_from": "2026-10-21T00:00:00Z", "run_secs_today": 600,
                    "cycles_today": 2, "longest_run_secs_today": 400}),
            at(0),
        );
        temp(&mut c, "mbr_temp", 19.0, 0);
        switch(&mut c, "mbr_ac", false, 0);
        c.start(at(0));
        let s = last_state(&c.take_effects(), "thermostat_mbr").unwrap();
        assert_eq!(
            (s["cycles_today"].clone(), s["run_secs_today"].clone()),
            (json!(2), json!(600))
        );
        temp(&mut c, "mbr_temp", 19.0, 11 * 3600 - 60);
        c.tick(at(11 * 3600));
        let s = last_state(&c.take_effects(), "thermostat_mbr").unwrap();
        assert_eq!(s["cycles_today"], json!(0));
        assert_eq!(s["today_from"], json!("2026-10-22T00:00:00Z"));
        // A day that ended long ago isn't today's.
        let mut c = house();
        c.on_state(
            "thermostat_mbr",
            &json!({"today_from": "2026-10-19T00:00:00Z", "cycles_today": 7}),
            at(0),
        );
        c.start(at(0));
        assert_eq!(
            last_state(&c.take_effects(), "thermostat_mbr").unwrap()["cycles_today"],
            json!(0)
        );
    }

    /// Learned changes apply before start, when the file hasn't moved on.
    #[test]
    fn learned_changes_apply_before_start() {
        let mut c = house();
        let mut changed = c.t("mbr").cfg.clone();
        changed.setpoint = 21.5;
        let doc =
            json!({"thermostats": {"mbr": runtime_change(&c.t("mbr").cfg, &changed).unwrap()}});
        c.apply_learned(&doc, Utc::now());
        assert_eq!(c.t("mbr").cfg.setpoint, 21.5);
    }

    /// Adding registers it (once started) and subscribes what it reads.
    #[test]
    fn adding_registers_and_subscribes() {
        let mut c = started(20.0);
        c.take_effects();
        let e: ThermostatEntry = serde_json::from_value(json!({
            "id": "new", "name": "New", "kind": "fan", "sensor_device_ids": ["n_temp"],
            "setpoint": 26.0, "mode": "cool", "actuator_device_id": "n_fan"
        }))
        .unwrap();
        c.add(e, at(5));
        let fx = c.take_effects();
        assert!(fx.contains(&Effect::Subscribe("n_temp".into())));
        assert!(fx.contains(&Effect::Subscribe("n_fan".into())));
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::Register(t) if t.id == "new")));
    }

    /// A thermostat just added waits for its sensor's first reading before
    /// calling it quiet — no OFF and alert flicker on the way in.
    #[test]
    fn a_new_thermostat_waits_for_its_first_reading() {
        let mut c = started(20.0);
        c.take_effects();
        let e: ThermostatEntry = serde_json::from_value(json!({
            "id": "new", "name": "New", "kind": "ac", "sensor_device_ids": ["n_temp"],
            "setpoint": 20.0, "mode": "cool", "actuator_device_id": "n_ac"
        }))
        .unwrap();
        c.add(e, at(5));
        let fx = c.take_effects();
        assert!(sent(&fx, "n_ac").is_empty());
        assert_eq!(
            alert_topics(&fx, "thermostat_new").unwrap(),
            Vec::<String>::new()
        );
        temp(&mut c, "n_temp", 25.0, 6);
        assert_eq!(sent(&c.take_effects(), "n_ac"), [true]);
        // One whose sensor never says anything is quiet once the grace is over.
        let e: ThermostatEntry = serde_json::from_value(json!({
            "id": "mute", "name": "Mute", "kind": "ac", "sensor_device_ids": ["m_temp"],
            "setpoint": 20.0, "mode": "cool", "actuator_device_id": "m_ac"
        }))
        .unwrap();
        c.add(e, at(10));
        c.tick(at(30));
        let fx = c.take_effects();
        assert_eq!(
            alert_topics(&fx, "thermostat_mute").unwrap(),
            ["sensors_stale"]
        );
    }

    /// At start a sensor not heard from yet is unknown, not quiet: its plugin
    /// may still be coming back (homeCore's broker forgets retained state when
    /// homeCore restarts). Nothing is switched for it until it reports, or the
    /// grace is over.
    #[test]
    fn at_start_an_unheard_sensor_gets_a_grace() {
        let mut c = house();
        switch(&mut c, "mbr_ac", true, 0);
        c.start(at(0));
        let fx = c.take_effects();
        assert!(sent(&fx, "mbr_ac").is_empty(), "{fx:?}");
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            Vec::<String>::new()
        );
        c.tick(at(60));
        assert!(sent(&c.take_effects(), "mbr_ac").is_empty());
        c.tick(at(121));
        let fx = c.take_effects();
        assert_eq!(sent(&fx, "mbr_ac"), [false]);
        assert_eq!(
            alert_topics(&fx, "thermostat_mbr").unwrap(),
            ["sensors_stale"]
        );
    }

    /// M3: an AC with no guards is warned about.
    #[test]
    fn an_unguarded_ac_is_a_notice() {
        let mut c = house();
        let fx = c.take_effects();
        let n = fx
            .iter()
            .find_map(|e| match e {
                Effect::Notices(n) => Some(n.clone()),
                _ => None,
            })
            .unwrap();
        assert!(n.iter().any(|n| n.code == "short_cycle_unguarded:mbr"));
        assert!(!n.iter().any(|n| n.code == "short_cycle_unguarded:office"));
    }

    #[test]
    fn interprets_switch_states() {
        assert_eq!(interpret_actuator_on(&json!({"on": true})), Some(true));
        assert_eq!(interpret_actuator_on(&json!({"state": "OFF"})), Some(false));
        assert_eq!(interpret_actuator_on(&json!({"power": "on"})), Some(true));
        assert_eq!(
            interpret_actuator_on(&json!({"on": false, "state": "on"})),
            Some(false)
        );
        assert_eq!(interpret_actuator_on(&json!({"brightness": 1})), None);
    }

    #[test]
    fn commands_are_read_as_the_app_sends_them() {
        assert_eq!(command_of(&json!({"action": "hold"})), "hold");
        assert_eq!(command_of(&json!({"command": "set_mode"})), "set_mode");
        assert_eq!(
            attribute_writes(&json!({"mode": "cool", "hvac_mode": "off"})),
            (None, Some("off"))
        );
        assert_eq!(
            sensor_ids_of(&json!("a, b,")),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            operation_of(&json!({"setpoint": 20, "_hc": {}})),
            "setpoint"
        );
    }

    #[test]
    fn a_kept_fahrenheit_setpoint_is_dropped() {
        let f: ThermostatEntry = serde_json::from_value(
            json!({"id": "o", "name": "O", "sensor_device_ids": ["t"], "setpoint": 21.0}),
        )
        .unwrap();
        let mut g = f.clone();
        g.setpoint = 68.0;
        assert!(learned_entry(&f, &runtime_change(&f, &g).unwrap()).is_none());
        g.setpoint = 20.5;
        assert_eq!(
            learned_entry(&f, &runtime_change(&f, &g).unwrap())
                .unwrap()
                .setpoint,
            20.5
        );
        let mut h = f.clone();
        h.setpoint = 22.0;
        assert!(learned_entry(&h, &runtime_change(&f, &g).unwrap()).is_none());
    }
}
