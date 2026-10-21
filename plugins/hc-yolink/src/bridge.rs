//! Between the YoLink hub and homeCore.
//!
//! # Tasks
//!
//! Four, so that nothing slow stands in front of something urgent:
//!
//! - **the feed** ([`feed_loop`]): device reports as the hub pushes them — a
//!   leak, a door, a lock. Handled at once; it never waits on the hub's HTTP
//!   API. (A poll of 28 devices takes 28 s or more, and used to run inline
//!   here, holding up every leak alert and lock command behind it.)
//! - **commands** ([`command_worker`]): one at a time, each answered — a
//!   failure is said on the device (`last_command_error`) and as a
//!   `plugin_command_result` event. A full queue is said too, never dropped.
//! - **polling** ([`poll_loop`]): the hourly true-up, the device inventory,
//!   "Check every device now", and a sweep after the feed reconnects
//!   (rate-limited) to catch what it missed while down.
//! - **the silence watcher** ([`watch_loop`]): a battery sensor that has not
//!   reported for `silent_after_mins` goes offline.
//!
//! The device registry is behind a std mutex held only for in-memory work —
//! never across an await.
//!
//! **Devices are removed only when the hub's list leaves them out twice
//! running** (a single short list is shown as offline), and leftovers from an
//! earlier run are cleared at the first inventory, not an hour later.

use anyhow::Result;
use chrono::{DateTime, Duration as Span, Utc};
use plugin_sdk_rs::types::{DeviceChange, PluginAlert, PluginNotice};
use plugin_sdk_rs::{DeviceHardware, DevicePublisher, PluginNotices};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, trace, warn};

use crate::config::Mode;
use crate::devices::{read, DeviceKind};
use crate::state::{iso, Dev, Effects, Source};
use crate::yolink::api::YolinkApi;
use crate::yolink::mqtt::FeedEvent;
use crate::yolink::types::{is_device_unreachable, ApiError, DeviceInfo, YolinkReport};

/// After the feed comes back, wait this long, then sweep — at most once per
/// [`SWEEP_EVERY`].
const SWEEP_SETTLE: Duration = Duration::from_secs(20);
const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// How often the silence watcher looks.
const WATCH_EVERY: Duration = Duration::from_secs(60);

pub struct Options {
    pub poll_interval: Duration,
    pub inventory_interval: Duration,
    pub poll_device_delay: Duration,
    pub initial_fetch_delay: Duration,
    pub silent_after: Span,
}

/// How the feed is, for the page.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub connected: bool,
    pub since: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub reconnects: u32,
    pub last_event_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct Registry {
    /// By YoLink device id.
    pub devices: BTreeMap<String, Dev>,
    /// Plugs known to meter, kept across restarts.
    pub metering: BTreeSet<String>,
    /// The hub, when the list includes it.
    pub hub: Option<DeviceInfo>,
    pub inventoried: bool,
}

pub struct Shared {
    pub publisher: DevicePublisher,
    pub api: Arc<YolinkApi>,
    pub notices: PluginNotices,
    pub opts: Options,
    pub mode: Mode,
    /// The hub's address (local) or YoLink's (cloud), for the page.
    pub host: String,
    reg: Mutex<Registry>,
    pub feed: Mutex<Feed>,
    pub last_poll: Mutex<Option<DateTime<Utc>>>,
    pub polling: Mutex<bool>,
    last_errors: Mutex<HashMap<String, Option<String>>>,
    pub poll_now: Notify,
    pub rescan: Notify,
    sweep: Notify,
    store: PathBuf,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(
        publisher: DevicePublisher,
        api: Arc<YolinkApi>,
        notices: PluginNotices,
        opts: Options,
        mode: Mode,
        host: String,
        store: PathBuf,
    ) -> Arc<Self> {
        let metering = std::fs::read_to_string(&store)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| serde_json::from_value::<BTreeSet<String>>(v["metering"].clone()).ok())
            .unwrap_or_default();
        Arc::new(Self {
            publisher,
            api,
            notices,
            opts,
            mode,
            host,
            reg: Mutex::new(Registry {
                metering,
                ..Default::default()
            }),
            feed: Mutex::new(Feed::default()),
            last_poll: Mutex::new(None),
            polling: Mutex::new(false),
            last_errors: Mutex::new(HashMap::new()),
            poll_now: Notify::new(),
            rescan: Notify::new(),
            sweep: Notify::new(),
            store,
        })
    }

    pub fn registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.reg)
    }

    fn save_store(&self) {
        let metering = self.registry().metering.clone();
        let body = json!({ "metering": metering });
        if let Err(e) = std::fs::write(&self.store, body.to_string()) {
            warn!(error = %e, "Could not keep what was learnt about the plugs");
        }
    }

    /// Say what a reading changed.
    async fn emit(&self, hc_id: &str, fx: Effects, cmd: Option<&Value>) {
        if fx.is_empty() {
            return;
        }
        if !fx.patch.is_empty() {
            let patch = Value::Object(fx.patch);
            trace!(hc_id, patch = %patch, "Publishing");
            let r = match cmd {
                Some(c) => {
                    self.publisher
                        .publish_state_partial_for_command(hc_id, &patch, c, "plugin.yolink")
                        .await
                }
                None => {
                    self.publisher
                        .publish_state_partial_with_change(hc_id, &patch, Some(&provenance(&patch)))
                        .await
                }
            };
            if let Err(e) = r {
                warn!(hc_id, error = %e, "Failed to publish state");
            }
        }
        if let Some(online) = fx.availability {
            if let Err(e) = self.publisher.publish_availability(hc_id, online).await {
                warn!(hc_id, error = %e, "Failed to publish availability");
            }
        }
        if let Some(alerts) = fx.alerts {
            if let Err(e) = self.publisher.set_alerts(hc_id, &alerts).await {
                warn!(hc_id, error = %e, "Failed to publish alerts");
            }
        }
    }

    /// Take a reading for a device, publish what changed, and keep a plug's
    /// newly learnt meter (with its schema).
    async fn take(&self, yl_id: &str, src: Source<'_>, data: &Value, cmd: Option<&Value>) {
        let now = Utc::now();
        let (hc_id, fx, learnt) = {
            let mut reg = self.registry();
            let Some(dev) = reg.devices.get_mut(yl_id) else {
                debug!(device_id = %yl_id, "Report for a device not in the hub's list, ignoring");
                return;
            };
            let Some(r) = read(&dev.kind, &dev.caps, data) else {
                debug!(hc_id = %dev.hc_id, "Nothing this kind understands in that report");
                return;
            };
            let fx = dev.apply(src, r, now, self.opts.silent_after);
            let learnt = fx.learnt_metering;
            let hc_id = dev.hc_id.clone();
            if learnt {
                reg.metering.insert(yl_id.to_string());
            }
            (hc_id, fx, learnt)
        };
        if learnt {
            info!(hc_id, "This plug reports power; publishing it");
            self.save_store();
            self.publish_schema(yl_id).await;
        }
        self.emit(&hc_id, fx, cmd).await;
        self.publish_hardware(yl_id).await;
    }

    /// The schema, when it differs from the one last published.
    async fn publish_schema(&self, yl_id: &str) {
        let (hc_id, value) = {
            let mut reg = self.registry();
            let Some(dev) = reg.devices.get_mut(yl_id) else {
                return;
            };
            let Some(schema) = crate::schema::schema_for(&dev.kind, &dev.caps) else {
                return;
            };
            let value = serde_json::to_value(&schema).unwrap_or(Value::Null);
            let text = value.to_string();
            if dev.schema_sent.as_deref() == Some(text.as_str()) {
                return;
            }
            dev.schema_sent = Some(text);
            (dev.hc_id.clone(), value)
        };
        if let Err(e) = self
            .publisher
            .register_device_schema_json(&hc_id, &value)
            .await
        {
            warn!(hc_id, error = %e, "Failed to publish device schema");
            if let Some(d) = self.registry().devices.get_mut(yl_id) {
                d.schema_sent = None;
            }
        }
    }

    /// Register (again) when it is new or what it is changed: name, type,
    /// model, firmware.
    async fn publish_hardware(&self, yl_id: &str) {
        let job = {
            let mut reg = self.registry();
            let Some(dev) = reg.devices.get_mut(yl_id) else {
                return;
            };
            let now = (dev.info.model_name.clone(), dev.version.clone());
            if dev.hardware_sent.as_ref() == Some(&now) {
                return;
            }
            dev.hardware_sent = Some(now.clone());
            let mut hw = DeviceHardware::new().manufacturer("YoLink");
            hw.model = now.0;
            hw.sw_version = now.1;
            (
                dev.hc_id.clone(),
                dev.info.name.clone(),
                dev.kind.homecore_device_type().to_string(),
                hw,
            )
        };
        let (hc_id, name, ty, hw) = job;
        if let Err(e) = self
            .publisher
            .register_device_detailed(&hc_id, &name, Some(&ty), None, None, Some(&hw))
            .await
        {
            warn!(hc_id, error = %e, "Failed to register device");
            if let Some(d) = self.registry().devices.get_mut(yl_id) {
                d.hardware_sent = None;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Inventory
    // -----------------------------------------------------------------------

    /// Read the hub's device list: register what is new or changed, mark
    /// offline what is missing, remove what is missing twice running.
    pub async fn sync_inventory(&self, republish: bool) -> Result<usize> {
        let list = self.api.get_device_list().await?;
        if list.is_empty() {
            anyhow::bail!("the hub listed no devices");
        }
        let first = !self.registry().inventoried;
        let mut seen = HashSet::new();
        let mut fresh = Vec::new();
        let mut renamed = Vec::new();
        {
            let mut reg = self.registry();
            for info in list {
                let kind = DeviceKind::from_yolink_type(&info.device_type);
                if kind == DeviceKind::Hub {
                    reg.hub = Some(info);
                    continue;
                }
                if !kind.is_supported() {
                    debug!(device_type = %info.device_type, "Skipping unsupported device type");
                    continue;
                }
                let id = info.device_id.clone();
                seen.insert(id.clone());
                let metering = reg.metering.contains(&id);
                match reg.devices.get_mut(&id) {
                    Some(dev) => {
                        dev.missing = 0;
                        if dev.info.name != info.name
                            || dev.kind != kind
                            || dev.info.model_name != info.model_name
                            || republish
                        {
                            dev.hardware_sent = None;
                            if republish {
                                dev.schema_sent = None;
                            }
                            renamed.push(id.clone());
                        }
                        dev.info = info;
                        dev.kind = kind;
                    }
                    None => {
                        info!(name = %info.name, device_type = %info.device_type, "YoLink device found; registering");
                        reg.devices
                            .insert(id.clone(), Dev::new(info, kind, metering));
                        fresh.push(id);
                    }
                }
            }
        }
        for id in fresh.iter().chain(renamed.iter()) {
            self.publish_hardware(id).await;
            self.publish_schema(id).await;
        }
        for id in &fresh {
            let (hc_id, kind) = {
                let reg = self.registry();
                let d = &reg.devices[id];
                (d.hc_id.clone(), d.kind.clone())
            };
            // Names this kind no longer publishes, removed from homeCore.
            if let Some(gone) = crate::schema::retired_state(&kind) {
                let _ = self.publisher.publish_state_partial(&hc_id, &gone).await;
            }
            if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
                warn!(hc_id, error = %e, "Failed to subscribe commands");
            }
        }

        // Missing: offline the first time, removed the second.
        let missing: Vec<String> = self
            .registry()
            .devices
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        for id in missing {
            let (hc_id, gone, fx) = {
                let mut reg = self.registry();
                let Some(d) = reg.devices.get_mut(&id) else {
                    continue;
                };
                d.missing += 1;
                let gone = d.missing >= 2;
                (
                    d.hc_id.clone(),
                    gone,
                    if gone {
                        Effects::default()
                    } else {
                        d.unreachable()
                    },
                )
            };
            if gone {
                info!(hc_id, "No longer in the hub's list twice running; removing");
                if self
                    .publisher
                    .unregister_device(self.publisher.plugin_id(), &hc_id)
                    .await
                    .is_ok()
                {
                    self.registry().devices.remove(&id);
                    self.registry().metering.remove(&id);
                }
            } else {
                warn!(
                    hc_id,
                    "Missing from the hub's list; offline until it is back"
                );
                self.emit(&hc_id, fx, None).await;
            }
        }

        // Leftovers from an earlier run — devices this plugin published that
        // the hub no longer has — are cleared at the first inventory.
        if first {
            let live: HashSet<String> = self
                .registry()
                .devices
                .values()
                .map(|d| d.hc_id.clone())
                .collect();
            match self.publisher.reconcile_devices(live).await {
                Ok(r) if !r.stale_unregistered.is_empty() => {
                    info!(removed = ?r.stale_unregistered, "Removed devices the hub no longer has")
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "Could not clear leftover devices"),
            }
            self.registry().inventoried = true;
        }
        let n = self.registry().devices.len();
        info!(devices = n, "Inventory read");
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------

    /// `getState` every device, paced. Exclusive: a second request while one
    /// runs is dropped.
    pub async fn poll_all(&self, why: &str) {
        {
            let mut p = lock(&self.polling);
            if *p {
                return;
            }
            *p = true;
        }
        let ids: Vec<(String, DeviceInfo)> = self
            .registry()
            .devices
            .iter()
            .map(|(id, d)| (id.clone(), d.info.clone()))
            .collect();
        info!(count = ids.len(), why, "Checking every device");
        for (i, (id, info)) in ids.iter().enumerate() {
            if i > 0 && !self.opts.poll_device_delay.is_zero() {
                tokio::time::sleep(self.opts.poll_device_delay).await;
            }
            self.poll_one(id, info).await;
        }
        *lock(&self.last_poll) = Some(Utc::now());
        *lock(&self.polling) = false;
        // The silence check right away, not a minute later.
        self.check_silence().await;
    }

    async fn poll_one(&self, id: &str, info: &DeviceInfo) {
        match self.api.get_device_state(info).await {
            Ok(data) => {
                trace!(device_id = %id, data = %data, "getState");
                self.take(id, Source::Poll, &data, None).await;
            }
            Err(e) if is_device_unreachable(&e) => {
                // Still unreachable after the retries: what flat batteries
                // look like from here.
                warn!(device = %info.name, error = %e, "Device unreachable after retries; offline");
                let fx = {
                    let mut reg = self.registry();
                    reg.devices
                        .get_mut(id)
                        .map(|d| (d.hc_id.clone(), d.unreachable()))
                };
                if let Some((hc_id, fx)) = fx {
                    self.emit(&hc_id, fx, None).await;
                }
            }
            Err(e) => warn!(device = %info.name, error = %e, "getState failed"),
        }
    }

    pub async fn check_silence(&self) {
        let now = Utc::now();
        let out: Vec<(String, Effects)> = self
            .registry()
            .devices
            .values_mut()
            .map(|d| {
                (
                    d.hc_id.clone(),
                    d.check_silence(now, self.opts.silent_after),
                )
            })
            .filter(|(_, fx)| !fx.is_empty())
            .collect();
        for (hc_id, fx) in out {
            warn!(hc_id, "No report for too long; showing it as not reporting");
            self.emit(&hc_id, fx, None).await;
        }
    }

    // -----------------------------------------------------------------------
    // Reports
    // -----------------------------------------------------------------------

    pub async fn on_report(&self, report: YolinkReport) {
        let at = report
            .time
            .and_then(|ms| chrono::TimeZone::timestamp_millis_opt(&Utc, ms).single())
            .unwrap_or_else(Utc::now);
        lock(&self.feed).last_event_at = Some(Utc::now());
        // The hub repeats every API call it answers as a report; the poll
        // or command that made it has already said what it said.
        if is_echo(&report.event) {
            trace!(event = %report.event, "The hub's echo of an API call; ignored");
            return;
        }
        if DeviceKind::is_power_report(&report.event) {
            // Undocumented units; only proof the plug meters.
            let learnt = {
                let mut reg = self.registry();
                let learnt = match reg.devices.get_mut(&report.device_id) {
                    Some(d) if d.kind.is_relay() && !d.caps.metering => {
                        d.caps.metering = true;
                        true
                    }
                    _ => false,
                };
                if learnt {
                    reg.metering.insert(report.device_id.clone());
                }
                learnt
            };
            if learnt {
                self.save_store();
                self.publish_schema(&report.device_id).await;
            }
            return;
        }
        debug!(device_id = %report.device_id, event = %report.event, "YoLink report");
        self.take(
            &report.device_id,
            Source::Event {
                name: &report.event,
                at,
            },
            &report.data,
            None,
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    pub async fn run_command(&self, hc_id: &str, cmd: &Value) {
        let result = self.command(hc_id, cmd).await;
        report(self, hc_id, &operation_of(cmd), result).await;
    }

    async fn command(&self, hc_id: &str, cmd: &Value) -> Result<(), String> {
        let yl_id = hc_id.strip_prefix("yolink_").unwrap_or(hc_id).to_string();
        let (info, kind) = {
            let reg = self.registry();
            let d = reg
                .devices
                .get(&yl_id)
                .ok_or_else(|| "not one of the hub's devices".to_string())?;
            (d.info.clone(), d.kind.clone())
        };
        let calls = kind.translate_command(cmd).map_err(|e| e.to_string())?;
        for (suffix, params) in calls {
            debug!(hc_id, method = suffix, params = %params, "Sending to YoLink");
            let at = Utc::now();
            if let Some(d) = self.registry().devices.get_mut(&yl_id) {
                d.last_command_at = Some(at);
            }
            let data = self
                .api
                .device_call(&info, suffix, params.clone())
                .await
                .map_err(|e| said(&e))?;
            // What the hub answered is the state now. setAttributes answers
            // with what was set, in YoLink's names.
            let reading = if suffix == "setAttributes" {
                json!({ "state": { "lock": lock_word(self, &yl_id) }, "attributes": params })
            } else {
                data
            };
            self.take(&yl_id, Source::Command { at }, &reading, Some(cmd))
                .await;
        }
        Ok(())
    }
}

/// The lock's current word, so a settings change re-reads only the settings.
fn lock_word(s: &Shared, yl_id: &str) -> &'static str {
    match s
        .registry()
        .devices
        .get(yl_id)
        .and_then(|d| d.published.get("locked"))
    {
        Some(Value::Bool(false)) => "unlocked",
        _ => "locked",
    }
}

/// Where a change the device reported came from: a hand at a lock
/// (fingerprint, button, key, code, card) is `physical`; anything else the
/// hub tells us is `external` — "another app or hub", which is what an
/// automation's "changed outside homeCore" means. Without it every report was
/// `unknown`, and a rule waiting for an outside change never started.
pub fn provenance(patch: &Value) -> DeviceChange {
    match patch.get("last_lock_method").and_then(Value::as_str) {
        Some(m @ ("fingerprint" | "button" | "key" | "code" | "card")) => {
            DeviceChange::physical(Some(format!("YoLink {m}")))
        }
        _ => DeviceChange::external("YoLink"),
    }
}

/// The hub's echo of an API call (`Outlet.getState`, `LockV2.setState`).
pub fn is_echo(event: &str) -> bool {
    [
        ".getState",
        ".setState",
        ".setAttributes",
        ".fetchState",
        ".setSettings",
    ]
    .iter()
    .any(|s| event.ends_with(s))
}

/// An error in words for `last_command_error`.
fn said(e: &anyhow::Error) -> String {
    match e.downcast_ref::<ApiError>() {
        Some(a) if a.is_device_unreachable() => {
            "The hub couldn't reach it (000201) — out of range, or its batteries are flat".into()
        }
        Some(a) if a.is_rate_limited() => format!(
            "The hub is busy ({}) — too many requests to it in a minute; try again shortly",
            a.code
        ),
        Some(a) => format!("YoLink said no ({}: {})", a.code, a.desc),
        None => format!("The hub didn't answer: {e}"),
    }
}

/// The attributes a command sets, without homeCore's own metadata
/// (`_hc`, `correlation_id`).
fn operation_of(cmd: &Value) -> String {
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

/// A failure's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("000201") || l.contains("couldn't reach") {
        "unreachable"
    } else if l.contains("busy") || l.contains("020104") || l.contains("010301") {
        "rate_limited"
    } else if l.contains("queue") || l.contains("waiting") {
        "busy"
    } else if l.contains("needs") || l.contains("takes") || l.contains("only reports") {
        "invalid_params"
    } else if l.contains("didn't answer") {
        "no_answer"
    } else {
        "rejected"
    }
}

/// Say how a command went: on the device (`last_command_error`, null once
/// one succeeds) and as a `plugin_command_result` event.
pub async fn report(s: &Shared, hc_id: &str, operation: &str, result: Result<(), String>) {
    if let Err(e) = &result {
        warn!(hc_id, operation, error = %e, "Command failed");
    }
    let now = result.as_ref().err().cloned();
    let changed = {
        let mut held = lock(&s.last_errors);
        let before = held.insert(hc_id.to_string(), now.clone());
        before.flatten() != now
    };
    if changed {
        let _ = s
            .publisher
            .publish_state_partial(hc_id, &json!({ "last_command_error": now }))
            .await;
    }
    let mut payload = json!({
        "plugin_id": s.publisher.plugin_id(),
        "device_id": hc_id,
        "operation": operation,
        "success": result.is_ok(),
    });
    if let Err(e) = &result {
        payload["error"] = json!(e);
        payload["error_code"] = json!(error_code(e));
    }
    if let Err(e) = s
        .publisher
        .publish_event("plugin_command_result", &payload)
        .await
    {
        debug!(hc_id, error = %e, "Could not publish the command result");
    }
}

// ---------------------------------------------------------------------------
// The tasks
// ---------------------------------------------------------------------------

pub async fn feed_loop(s: Arc<Shared>, mut rx: mpsc::Receiver<FeedEvent>) {
    let mut was_down = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            FeedEvent::Up => {
                {
                    let mut f = lock(&s.feed);
                    f.connected = true;
                    f.since = Some(Utc::now());
                    f.last_error = None;
                    if was_down {
                        f.reconnects += 1;
                    }
                }
                if was_down {
                    // What happened while it was down is only in the devices.
                    s.sweep.notify_one();
                }
                was_down = false;
            }
            FeedEvent::Down(e) => {
                let mut f = lock(&s.feed);
                f.connected = false;
                f.since = Some(Utc::now());
                f.last_error = Some(e);
                was_down = true;
            }
            FeedEvent::Report(r) => s.on_report(r).await,
        }
    }
}

pub async fn command_worker(s: Arc<Shared>, mut rx: mpsc::Receiver<(String, Value)>) {
    while let Some((hc_id, cmd)) = rx.recv().await {
        s.run_command(&hc_id, &cmd).await;
    }
}

pub async fn poll_loop(s: Arc<Shared>) {
    if !s.opts.initial_fetch_delay.is_zero() {
        tokio::time::sleep(s.opts.initial_fetch_delay).await;
        s.poll_all("startup").await;
    }
    let mut poll = tokio::time::interval(s.opts.poll_interval);
    poll.tick().await;
    let mut inv = tokio::time::interval(s.opts.inventory_interval);
    inv.tick().await;
    let mut last_sweep: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = poll.tick() => s.poll_all("hourly").await,
            _ = inv.tick() => {
                if let Err(e) = s.sync_inventory(false).await {
                    warn!(error = %e, "Reading the hub's device list failed");
                }
            }
            _ = s.poll_now.notified() => s.poll_all("asked").await,
            _ = s.rescan.notified() => {
                if let Err(e) = s.sync_inventory(true).await {
                    warn!(error = %e, "Rescan failed");
                }
            }
            _ = s.sweep.notified() => {
                if last_sweep.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
                    debug!("The feed came back again; the last sweep was recent enough");
                    continue;
                }
                tokio::time::sleep(SWEEP_SETTLE).await;
                last_sweep = Some(Instant::now());
                s.poll_all("the event feed reconnected").await;
            }
        }
    }
}

pub async fn watch_loop(s: Arc<Shared>) {
    let mut t = tokio::time::interval(WATCH_EVERY);
    loop {
        t.tick().await;
        s.check_silence().await;
    }
}

/// The SDK's command callback: hand the command to the worker, and when the
/// queue is full say so on the device rather than drop it.
pub fn enqueue(s: &Arc<Shared>, tx: &mpsc::Sender<(String, Value)>, hc_id: String, cmd: Value) {
    if let Err(e) = tx.try_send((hc_id, cmd)) {
        let (hc_id, cmd) = match e {
            mpsc::error::TrySendError::Full(x) | mpsc::error::TrySendError::Closed(x) => x,
        };
        let s = s.clone();
        tokio::spawn(async move {
            report(
                &s,
                &hc_id,
                &operation_of(&cmd),
                Err("Too many commands waiting for the hub; this one was not sent".into()),
            )
            .await;
        });
    }
}

// ---------------------------------------------------------------------------
// Status, for the page
// ---------------------------------------------------------------------------

/// Signal strength in words, from LoRa dBm.
pub fn signal_words(dbm: Option<i64>) -> &'static str {
    match dbm {
        None => "unknown",
        Some(d) if d >= -90 => "good",
        Some(d) if d >= -105 => "fair",
        Some(_) => "poor",
    }
}

pub fn status(s: &Shared) -> Value {
    let feed = lock(&s.feed).clone();
    let last_poll = *lock(&s.last_poll);
    let polling = *lock(&s.polling);
    let reg = s.registry();
    let devices: Vec<Value> = reg
        .devices
        .values()
        .map(|d| {
            json!({
                "device_id": d.hc_id,
                "name": d.info.name,
                "yolink_type": d.info.device_type,
                "model": d.info.model_name,
                "hub_id": d.info.parent_device_id,
                "kind": d.kind.words(),
                "battery_powered": d.kind.is_battery(),
                "last_report_at": d.last_report.map(iso),
                "battery_level": d.battery_level,
                "signal_dbm": d.signal,
                "signal": signal_words(d.signal),
                "online": d.online,
                "silent": d.silent,
                "metering": d.caps.metering,
                "temperature_only": d.caps.temp_only,
                "firmware": d.version,
                "alerts": d.alerts.iter().map(|a: &PluginAlert| json!({ "topic": a.topic, "reason": a.reason })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "status": "ok",
        "configured": true,
        "mode": match s.mode { Mode::Local => "local", Mode::Cloud => "cloud" },
        "hub": {
            "address": s.host,
            "model": reg.hub.as_ref().and_then(|h| h.model_name.clone()),
            "name": reg.hub.as_ref().map(|h| h.name.clone()),
        },
        "feed": {
            "connected": feed.connected,
            "since": feed.since.map(iso),
            "last_error": feed.last_error,
            "reconnects": feed.reconnects,
        },
        "last_event_at": feed.last_event_at.map(iso),
        "last_poll_at": last_poll.map(iso),
        "polling": polling,
        "silent_after_mins": s.opts.silent_after.num_minutes(),
        "devices": devices,
    })
}

/// Raise the notice that says YoLink could not be reached at startup.
pub fn startup_failed(notices: &PluginNotices, e: &anyhow::Error, attempt: u32, of: u32) {
    notices.raise(
        PluginNotice::error(
            "hub_unreachable",
            format!("Couldn't start talking to YoLink (try {attempt} of {of}): {e}"),
        )
        .with_remedy(
            "Check the hub's address and the Client ID and Secret under Configuration. \
             The plugin tries again in a minute, then gives up and homeCore restarts it.",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_are_recognised() {
        assert!(is_echo("Outlet.getState"));
        assert!(is_echo("LockV2.setState"));
        assert!(is_echo("LockV2.setAttributes"));
        assert!(!is_echo("LeakSensor.Alert"));
        assert!(!is_echo("THSensor.Report"));
        assert!(!is_echo("Outlet.StatusChange"));
    }

    #[test]
    fn failures_have_kinds() {
        assert_eq!(
            error_code("The hub couldn't reach it (000201) — out of range"),
            "unreachable"
        );
        assert_eq!(
            error_code("The hub is busy (020104) — too many"),
            "rate_limited"
        );
        assert_eq!(
            error_code("Too many commands waiting for the hub; this one was not sent"),
            "busy"
        );
        assert_eq!(
            error_code("'sound_level' needs 0–3 (mute to high)"),
            "invalid_params"
        );
        assert_eq!(
            error_code("YoLink said no (010200: params is not valid)"),
            "rejected"
        );
    }

    #[test]
    fn the_operation_is_what_was_set() {
        let cmd = json!({ "_hc": {}, "correlation_id": "x", "sound_level": 1 });
        assert_eq!(operation_of(&cmd), "sound_level");
    }

    #[test]
    fn a_report_is_an_outside_change_and_a_hand_is_physical() {
        let ext = provenance(&json!({ "water_detected": true }));
        assert_eq!(serde_json::to_value(&ext.kind).unwrap(), json!("external"));
        let hand = provenance(&json!({ "locked": false, "last_lock_method": "fingerprint" }));
        assert_eq!(serde_json::to_value(&hand.kind).unwrap(), json!("physical"));
        let app = provenance(&json!({ "locked": false, "last_lock_method": "app" }));
        assert_eq!(serde_json::to_value(&app.kind).unwrap(), json!("external"));
    }

    #[test]
    fn signal_in_words() {
        assert_eq!(signal_words(Some(-71)), "good");
        assert_eq!(signal_words(Some(-100)), "fair");
        assert_eq!(signal_words(Some(-115)), "poor");
        assert_eq!(signal_words(None), "unknown");
    }

    #[test]
    fn said_names_the_codes() {
        let e: anyhow::Error = ApiError {
            code: "020104".into(),
            desc: "Device is busy".into(),
        }
        .into();
        assert!(said(&e).contains("busy"));
        let e: anyhow::Error = ApiError {
            code: "000201".into(),
            desc: String::new(),
        }
        .into();
        assert_eq!(error_code(&said(&e)), "unreachable");
    }
}
