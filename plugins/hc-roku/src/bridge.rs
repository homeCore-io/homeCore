//! Device lifecycle, polling, and command routing.
//!
//! ECP has no push channel, so everything homeCore knows about a Roku comes
//! from asking it on a timer: `query/device-info` every cycle (the only way
//! to see the power state), the app and player queries while it is on, and
//! the installed-channel catalogue on its own slow cadence — **whether it is
//! on or not**, since a Roku in standby still answers, and kept across
//! restarts so a picker is never empty.
//!
//! # One worker per Roku
//!
//! Each Roku has one task that does everything for it, in order: its
//! commands, one after another as they arrived, and its polls. Presses never
//! race each other (Down, Down, OK arrives as Down, Down, OK), and a burst of
//! them is followed by **one** re-poll once it has settled, not one per press.
//! Every command ends in a `plugin_command_result` event, and a failure is
//! said on the device (`last_command_error`, null again once one succeeds).
//!
//! # Identity
//!
//! A Roku is its serial number. The serial → device id map, with each one's
//! address, MACs, capabilities, catalogue and launch counts, is kept in the
//! plugin's learned state, so after a restart every known Roku is registered
//! at once (a TV that is fully off included, and Wake-on-LAN can reach it),
//! and one that moved address is found again by its serial and keeps its id,
//! name, room and automations. Each answer's serial is checked: an address
//! that now answers as another Roku re-homes that one, and a sweep of the
//! known addresses looks for the one that moved. One serial is never two
//! devices.
//!
//! A Roku that stops answering is shown offline with what it was doing
//! cleared — never removed; "Forget it" (`forget_device`) is the operator's.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use plugin_sdk_rs::types::{PluginAlert, PluginNotice};
use plugin_sdk_rs::{DevicePublisher, PluginNotices, PluginStateWriter};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Notify, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use std::sync::atomic::AtomicBool;

use crate::commands::{self, CommandContext};
use crate::config::{DeviceConfig, RokuConfig};
use crate::discovery::{self, SsdpHit};
use crate::ecp::{App, DeviceInfo, EcpClient, EcpError};
use crate::icons::Icons;
use crate::state::{self, Caps, RokuSnapshot};

/// How long after a burst of commands to re-poll. Roku applies a keypress
/// asynchronously — polling immediately reads the state *before* the press
/// landed. Another command inside this window pushes the re-poll back, so a
/// burst gets one.
const POST_COMMAND_SETTLE: Duration = Duration::from_millis(900);
/// Between one command and the next already queued: as `press_repeated`
/// spaces a key's repeats.
const KEY_GAP: Duration = Duration::from_millis(60);
/// A command that waited longer than this in a queue is not sent: whoever
/// asked has moved on, and a TV switching itself on half a minute later is
/// worse than an error.
const STALE_AFTER: Duration = Duration::from_secs(30);
/// Commands waiting per Roku before more are refused (and said so).
const QUEUE_DEPTH: usize = 64;
/// How long to wait at startup for the learned state core keeps for us.
const LEARNED_WAIT: Duration = Duration::from_secs(3);
/// The least time between two sweeps for a Roku that moved.
const SWEEP_GAP: Duration = Duration::from_secs(20);
/// Learned state is written this long after the last change, not per change.
const PERSIST_DEBOUNCE: Duration = Duration::from_secs(2);
/// A position within this of where it should be is not a jump.
const POSITION_SLACK_SECS: f64 = 3.0;
/// After PowerOn, how long a Roku has to say it is on — and after a
/// Wake-on-LAN packet, to answer at all (a TV from fully off boots).
const WAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WOL_TIMEOUT: Duration = Duration::from_secs(30);
/// After it says it is on, before a launch: its home screen is still drawing.
const POWER_SETTLE: Duration = Duration::from_millis(1000);

/// What is said on a Roku whose "Control by mobile apps" is disabled.
pub const ECP_DISABLED_REASON: &str = "Control by mobile apps is disabled, so homeCore can see it \
     but not control it. On the Roku: Settings › System › Advanced system settings › Control \
     by mobile apps › Network access: Default (or Permissive).";

/// Work for a Roku's worker, done in the order it arrives.
pub enum Job {
    Command {
        cmd: Value,
        at: Instant,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    },
    /// Poll now (it moved, a sweep found it, an icon arrived).
    PollNow,
    /// Read the channel list again now, and say how it went.
    Refresh { reply: oneshot::Sender<Value> },
}

/// What is remembered about a Roku across restarts, by serial.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Known {
    pub hc_id: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<Caps>,
    /// The channels and inputs, as last read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<App>,
    /// How often each channel was opened from homeCore.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launches: BTreeMap<String, u32>,
}

/// Where `media_position` was true, and when. A screen counts on from here
/// while it plays, so the position is republished only when it jumps.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub pos: u64,
    pub at: Instant,
    pub iso: String,
    pub playing: bool,
}

/// Keep the anchor while the position is where it should be; a new one when
/// it jumped (a scan, a new video), stopped or started moving.
pub fn anchor(
    prev: Option<&Anchor>,
    pos: Option<u64>,
    playing: bool,
    now: Instant,
    now_iso: impl FnOnce() -> String,
) -> Option<Anchor> {
    let pos = pos?;
    if let Some(a) = prev.filter(|a| a.playing == playing) {
        let moved = if playing {
            now.saturating_duration_since(a.at).as_secs_f64()
        } else {
            0.0
        };
        if (pos as f64 - (a.pos as f64 + moved)).abs() <= POSITION_SLACK_SECS {
            return Some(a.clone());
        }
    }
    Some(Anchor {
        pos,
        at: now,
        iso: now_iso(),
        playing,
    })
}

/// One device the plugin is managing.
pub struct DeviceEntry {
    pub hc_id: String,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub area: Option<String>,
    pub serial: Option<String>,
    /// From `[[devices]]` rather than discovery. Configured devices are
    /// never retired automatically — the operator asked for them.
    pub configured: bool,
    pub poll_interval: Duration,
    pub client: EcpClient,
    pub tx: mpsc::Sender<Job>,

    // ── Live, written by its worker ──────────────────────────────────
    /// `None` until the first poll; then whether it answered.
    pub online: Option<bool>,
    pub snapshot: RokuSnapshot,
    pub caps: Caps,
    pub playback_state: String,
    /// Mute as the plugin last left it. ECP cannot report mute, so this is
    /// tracked from the VolumeMute presses sent.
    pub muted: bool,
    pub last_published: Option<Value>,
    pub apps_refreshed_at: Option<Instant>,
    pub catalog_error: Option<String>,
    /// Model and firmware last registered: re-registered only on a change.
    pub hardware: Option<(String, String)>,
    pub schema_published: Option<Value>,
    pub last_error: Option<String>,
    /// A command was refused with a 403 since the last that went through.
    pub control_refused: bool,
    pub alerts_sent: Option<Vec<String>>,
    pub anchor: Option<Anchor>,
    pub launches: BTreeMap<String, u32>,
    pub macs: Vec<String>,
    pub model: Option<String>,
    /// Its address answers as another Roku: it is being looked for.
    pub moved: bool,
    pub last_seen: Option<DateTime<Utc>>,
}

impl DeviceEntry {
    fn address(&self) -> String {
        address(&self.host, self.port)
    }
}

fn address(host: &str, port: u16) -> String {
    if port == crate::ecp::DEFAULT_PORT {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

pub type Devices = Arc<RwLock<HashMap<String, DeviceEntry>>>;

pub struct Bridge {
    cfg: RokuConfig,
    publisher: DevicePublisher,
    devices: Devices,
    state_writer: PluginStateWriter,
    /// serial → what is remembered, mirrored to the learned-state document.
    known: RwLock<BTreeMap<String, Known>>,
    learned: watch::Sender<bool>,
    notices: PluginNotices,
    icons: Arc<Icons>,
    persist_now: Notify,
    sweep_now: Notify,
    /// A sweep was asked for and has not run yet: several asks in a row
    /// (two Rokus that swapped addresses each ask twice) make one sweep.
    sweep_wanted: std::sync::atomic::AtomicBool,
    last_sweep: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl Bridge {
    pub fn new(
        cfg: RokuConfig,
        publisher: DevicePublisher,
        state_writer: PluginStateWriter,
        notices: PluginNotices,
        files_dir: &std::path::Path,
    ) -> Self {
        Self {
            cfg,
            publisher,
            devices: Arc::new(RwLock::new(HashMap::new())),
            state_writer,
            known: RwLock::new(BTreeMap::new()),
            learned: watch::channel(false).0,
            notices,
            icons: Arc::new(Icons::new(files_dir)),
            persist_now: Notify::new(),
            sweep_now: Notify::new(),
            sweep_wanted: std::sync::atomic::AtomicBool::new(false),
            last_sweep: std::sync::Mutex::new(None),
        }
    }

    /// Raise or clear the "nothing to control" notice from the live device
    /// map. Call after anything that can change what is managed.
    pub async fn refresh_device_notice(&self) {
        if self.devices.read().await.is_empty() {
            self.notices.raise(
                PluginNotice::warning(
                    "no_devices_configured",
                    "No Roku devices are configured or discovered, so this plugin \
                     publishes nothing.",
                )
                .with_remedy(
                    "Run the Discover Rokus action, which sweeps over SSDP. If it finds \
                     nothing and homeCore runs in a container on a bridge network, that \
                     is expected — SSDP is multicast and does not cross the bridge. Add \
                     each Roku by IP under Configuration instead.",
                ),
            );
        } else {
            self.notices.clear("no_devices_configured");
        }
    }

    /// Apply the durable learned-state document core hands back on connect
    /// (`homecore/plugins/{id}/state`). Read once: after that, this process's
    /// own copy is the newer one.
    pub async fn apply_learned_state(&self, doc: &Value) {
        if *self.learned.borrow() {
            return;
        }
        if let Some(map) = doc.get("devices").and_then(Value::as_object) {
            let mut known = self.known.write().await;
            for (serial, entry) in map {
                match serde_json::from_value::<Known>(entry.clone()) {
                    Ok(k) if !k.hc_id.is_empty() => {
                        known.insert(serial.clone(), k);
                    }
                    _ => warn!(serial, "Unreadable learned Roku entry; skipped"),
                }
            }
            info!(count = known.len(), "Applied learned Roku identities");
        }
        self.learned.send_replace(true);
    }

    // ── Running ──────────────────────────────────────────────────────

    /// Drive the bridge: register what is configured and remembered, then
    /// route commands to each Roku's worker until the channel closes.
    pub async fn run(self: Arc<Self>, mut cmd_rx: mpsc::Receiver<(String, Value)>) {
        // The learned map arrives over MQTT shortly after connect; registering
        // before it lands would mint fresh ids for Rokus we know. None at all
        // (a first start) is fine after a moment.
        let mut rx = self.learned.subscribe();
        if tokio::time::timeout(LEARNED_WAIT, rx.wait_for(|v| *v))
            .await
            .is_err()
        {
            debug!("No learned state arrived; starting without");
            self.learned.send_replace(true);
        }

        // Configured devices first: they carry pinned ids.
        let configured: Vec<DeviceConfig> = self.cfg.devices.clone();
        for dev in &configured {
            self.add_configured(dev).await;
        }
        // With discovery on, every Roku remembered is registered now — a TV
        // that is fully off included, so it shows (offline) and Wake-on-LAN can
        // reach it. With discovery off, the configured list is the inventory.
        if self.cfg.roku.discovery_enabled {
            let known: Vec<(String, Known)> = self
                .known
                .read()
                .await
                .iter()
                .map(|(s, k)| (s.clone(), k.clone()))
                .collect();
            for (serial, k) in known {
                let have = self.devices.read().await.contains_key(&k.hc_id);
                if !have {
                    self.add_known(&serial, &k).await;
                }
            }
        } else {
            let live: HashSet<String> = configured.iter().map(|d| d.hc_id.clone()).collect();
            match self.publisher.reconcile_devices(live).await {
                Ok(report) if !report.stale_unregistered.is_empty() => {
                    info!(
                        retired = ?report.stale_unregistered,
                        "Retired devices no longer present in config",
                    );
                }
                Err(e) => warn!(error = %e, "reconcile_devices failed"),
                _ => {}
            }
        }

        let p = Arc::clone(&self);
        tokio::spawn(async move { p.persist_loop().await });
        let s = Arc::clone(&self);
        tokio::spawn(async move { s.sweep_loop().await });
        self.refresh_device_notice().await;

        info!(
            configured = configured.len(),
            discovery = self.cfg.roku.discovery_enabled,
            "hc-roku bridge running"
        );

        while let Some((hc_id, cmd)) = cmd_rx.recv().await {
            self.enqueue(&hc_id, cmd, None).await;
        }
        warn!("hc-roku bridge loop exited — the command channel closed");
    }

    /// Hand a command to its Roku's worker. A full queue is said, not dropped.
    async fn enqueue(
        self: &Arc<Self>,
        hc_id: &str,
        cmd: Value,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    ) {
        let tx = self.devices.read().await.get(hc_id).map(|d| d.tx.clone());
        let Some(tx) = tx else {
            warn!(hc_id, "Command for unknown device");
            if let Some(r) = reply {
                let _ = r.send(Err(format!("unknown device '{hc_id}'")));
            }
            return;
        };
        let op = operation_of(&cmd);
        let job = Job::Command {
            cmd,
            at: Instant::now(),
            reply,
        };
        if let Err(e) = tx.try_send(job) {
            let why = "too many commands are waiting for this Roku; this one was not sent";
            if let mpsc::error::TrySendError::Full(Job::Command { reply: Some(r), .. }) = e {
                let _ = r.send(Err(why.into()));
            }
            self.report(hc_id, &op, Err(why.to_string())).await;
        }
    }

    // ── Registration ─────────────────────────────────────────────────

    fn client_for(&self, host: &str, port: u16) -> Result<EcpClient> {
        EcpClient::new(
            host,
            port,
            Duration::from_secs(self.cfg.roku.request_timeout_secs.max(1)),
        )
    }

    fn new_entry(
        &self,
        hc_id: &str,
        host: &str,
        port: u16,
        name: &str,
        configured: bool,
        k: Option<(&str, &Known)>,
    ) -> Result<(DeviceEntry, mpsc::Receiver<Job>)> {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let poll = configured
            .then(|| {
                self.cfg
                    .devices
                    .iter()
                    .find(|d| d.hc_id == hc_id)
                    .and_then(|d| d.poll_interval_secs)
            })
            .flatten()
            .unwrap_or(self.cfg.roku.poll_interval_secs)
            .max(1);
        let known = k.map(|(_, k)| k);
        Ok((
            DeviceEntry {
                hc_id: hc_id.to_string(),
                host: host.to_string(),
                port,
                name: name.to_string(),
                area: None,
                serial: k.map(|(s, _)| s.to_string()),
                configured,
                poll_interval: Duration::from_secs(poll),
                client: self.client_for(host, port)?,
                tx,
                online: None,
                snapshot: RokuSnapshot {
                    apps: known.map(|k| k.apps.clone()).unwrap_or_default(),
                    ..Default::default()
                },
                caps: known.and_then(|k| k.caps.clone()).unwrap_or_default(),
                playback_state: "idle".into(),
                muted: false,
                last_published: None,
                apps_refreshed_at: None,
                catalog_error: None,
                hardware: None,
                schema_published: None,
                last_error: None,
                control_refused: false,
                alerts_sent: None,
                anchor: None,
                launches: known.map(|k| k.launches.clone()).unwrap_or_default(),
                macs: known.map(|k| k.macs.clone()).unwrap_or_default(),
                model: known.and_then(|k| k.model.clone()),
                moved: false,
                last_seen: None,
            },
            rx,
        ))
    }

    async fn add_configured(self: &Arc<Self>, dev: &DeviceConfig) {
        let port = dev.port.unwrap_or(crate::ecp::DEFAULT_PORT);
        let known = self
            .known
            .read()
            .await
            .iter()
            .find(|(_, k)| k.hc_id == dev.hc_id)
            .map(|(s, k)| (s.clone(), k.clone()));
        let made = self.new_entry(
            &dev.hc_id,
            &dev.host,
            port,
            &dev.name,
            true,
            known.as_ref().map(|(s, k)| (s.as_str(), k)),
        );
        match made {
            Ok((mut entry, rx)) => {
                entry.area = dev.area.clone();
                self.register(entry, rx).await;
            }
            Err(e) => error!(host = %dev.host, error = %e, "Could not build ECP client"),
        }
    }

    async fn add_known(self: &Arc<Self>, serial: &str, k: &Known) {
        let name = k
            .name
            .clone()
            .unwrap_or_else(|| format!("Roku {}", k.hc_id));
        let port = k.port.unwrap_or(crate::ecp::DEFAULT_PORT);
        match self.new_entry(&k.hc_id, &k.host, port, &name, false, Some((serial, k))) {
            Ok((entry, rx)) => self.register(entry, rx).await,
            Err(e) => error!(host = %k.host, error = %e, "Could not build ECP client"),
        }
    }

    /// Insert the device, register it with homeCore, publish its schema, and
    /// start its worker.
    async fn register(self: &Arc<Self>, entry: DeviceEntry, rx: mpsc::Receiver<Job>) {
        let hc_id = entry.hc_id.clone();
        let (name, area) = (entry.name.clone(), entry.area.clone());
        info!(hc_id = %hc_id, host = %entry.host, name = %name, "Registering Roku");
        self.devices.write().await.insert(hc_id.clone(), entry);

        // `area` is passed only when the operator set it in config: room
        // assignment belongs to homeCore's device registry.
        if let Err(e) = self
            .publisher
            .register_device_full(&hc_id, &name, Some("media_player"), area.as_deref(), None)
            .await
        {
            warn!(hc_id = %hc_id, error = %e, "Failed to register device");
        }
        self.publish_schema(&hc_id).await;
        if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
            error!(hc_id = %hc_id, error = %e, "Failed to subscribe to commands");
        }
        let b = Arc::clone(self);
        tokio::spawn(async move { b.worker(hc_id, rx).await });
    }

    /// Publish its schema (attributes + actions, from what it is) when it
    /// differs from the one last published.
    async fn publish_schema(&self, hc_id: &str) {
        let schema = {
            let mut devs = self.devices.write().await;
            let Some(d) = devs.get_mut(hc_id) else { return };
            let schema = crate::schema::published(&d.caps);
            if d.schema_published.as_ref() == Some(&schema) {
                return;
            }
            d.schema_published = Some(schema.clone());
            schema
        };
        if let Err(e) = self
            .publisher
            .register_device_schema_json(hc_id, &schema)
            .await
        {
            warn!(hc_id, error = %e, "Failed to publish device schema");
        }
    }

    // ── The worker ───────────────────────────────────────────────────

    async fn worker(self: Arc<Self>, hc_id: String, mut rx: mpsc::Receiver<Job>) {
        let mut next = Instant::now();
        let mut cause: Option<Value> = None;
        loop {
            tokio::select! {
                biased;
                job = rx.recv() => match job {
                    None => return,
                    Some(Job::Command { cmd, at, reply }) => {
                        self.run_command(&hc_id, &cmd, at, reply).await;
                        cause = Some(cmd);
                        next = Instant::now() + POST_COMMAND_SETTLE;
                        // Roku's UI drops presses that come faster than it
                        // redraws: a gap before the next one already waiting.
                        if !rx.is_empty() {
                            tokio::time::sleep(KEY_GAP).await;
                        }
                    }
                    Some(Job::PollNow) => next = Instant::now(),
                    Some(Job::Refresh { reply }) => {
                        if let Some(d) = self.devices.write().await.get_mut(&hc_id) {
                            d.apps_refreshed_at = None;
                        }
                        let wait = self.poll_once(&hc_id, cause.take().as_ref()).await;
                        let _ = reply.send(self.catalogue_said(&hc_id).await);
                        match wait {
                            Some(w) => next = Instant::now() + w,
                            None => return,
                        }
                    }
                },
                _ = tokio::time::sleep_until(next) => {
                    match self.poll_once(&hc_id, cause.take().as_ref()).await {
                        Some(w) => next = Instant::now() + w,
                        None => return,
                    }
                }
            }
        }
    }

    async fn catalogue_said(&self, hc_id: &str) -> Value {
        let devs = self.devices.read().await;
        let Some(d) = devs.get(hc_id) else {
            return json!({ "error": "no longer managed" });
        };
        match &d.catalog_error {
            Some(e) => json!({ "error": e }),
            None => {
                let inputs = d.snapshot.apps.iter().filter(|a| a.is_input()).count();
                json!({ "channels": d.snapshot.apps.len() - inputs, "inputs": inputs })
            }
        }
    }

    fn standby_interval(&self) -> Duration {
        Duration::from_secs(self.cfg.roku.standby_poll_interval_secs.max(5))
    }

    /// One command, start to finish: run, keep what it changed, say how it went.
    async fn run_command(
        self: &Arc<Self>,
        hc_id: &str,
        cmd: &Value,
        at: Instant,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    ) {
        let op = operation_of(cmd);
        if at.elapsed() > STALE_AFTER {
            let why = format!(
                "not sent: it waited more than {}s for the Roku",
                STALE_AFTER.as_secs()
            );
            if let Some(r) = reply {
                let _ = r.send(Err(why.clone()));
            }
            self.report(hc_id, &op, Err(why)).await;
            return;
        }
        let Some(c) = self.context_for(hc_id).await else {
            return;
        };
        let ctx = CommandContext {
            apps: &c.apps,
            caps: &c.caps,
            playback_state: &c.playback,
            muted: c.muted,
            reachable: AtomicBool::new(c.online),
            powered: AtomicBool::new(c.powered),
            on_input: c.on_input,
            wake_on_lan: self.cfg.roku.wake_on_lan,
            macs: &c.macs,
            host: &c.host,
            default_hold: Duration::from_millis(
                self.cfg.roku.key_hold_ms.min(commands::MAX_HOLD_MS),
            ),
            type_delay: Duration::from_millis(self.cfg.roku.type_delay_ms),
            wake_timeout: if c.online { WAKE_TIMEOUT } else { WOL_TIMEOUT },
            power_settle: POWER_SETTLE,
        };
        let result = commands::execute(&c.client, cmd, &ctx).await;
        let (said, forbidden, ok_value) = match result {
            Ok(v) => {
                let failed = commands::failures(&v);
                let forbidden = v.get("forbidden").and_then(Value::as_bool) == Some(true);
                (failed, forbidden, Some(v))
            }
            Err(e) => (Some(e.to_string()), commands::is_forbidden(&e), None),
        };

        let mut persist = false;
        {
            let mut devs = self.devices.write().await;
            if let Some(d) = devs.get_mut(hc_id) {
                if let Some(v) = &ok_value {
                    if let Some(m) = v.get("muted").and_then(Value::as_bool) {
                        d.muted = m;
                    }
                    // "Most used" counts what was opened from homeCore —
                    // channels, not inputs.
                    if let Some(id) = v.get("launched").and_then(Value::as_str) {
                        let is_channel =
                            d.snapshot.apps.iter().any(|a| a.id == id && !a.is_input());
                        if is_channel {
                            *d.launches.entry(id.to_string()).or_insert(0) += 1;
                            persist = true;
                        }
                    }
                }
                if forbidden {
                    d.control_refused = true;
                } else if said.is_none() {
                    d.control_refused = false;
                }
            }
        }
        if persist {
            self.remember(hc_id).await;
        }
        if let Some(r) = reply {
            let _ = r.send(match (&said, ok_value) {
                (None, Some(v)) => Ok(v),
                (Some(e), _) => Err(e.clone()),
                (None, None) => Err("no result".into()),
            });
        }
        let outcome = match said {
            Some(e) => Err(e),
            None => Ok(()),
        };
        self.report(hc_id, &op, outcome).await;
        self.refresh_alerts(hc_id).await;
    }

    async fn context_for(&self, hc_id: &str) -> Option<CommandSnapshot> {
        let devs = self.devices.read().await;
        let d = devs.get(hc_id)?;
        let info = d.snapshot.device_info.as_ref();
        let on_input = d
            .snapshot
            .active
            .as_ref()
            .filter(|a| !a.is_home())
            .and_then(|a| a.app.as_ref())
            .is_some_and(App::is_input);
        Some(CommandSnapshot {
            client: d.client.clone(),
            apps: d.snapshot.apps.clone(),
            caps: d.caps.clone(),
            playback: d.playback_state.clone(),
            muted: d.muted,
            online: d.online == Some(true),
            powered: info.is_some_and(DeviceInfo::is_powered_on),
            on_input,
            macs: d.macs.clone(),
            host: d.host.clone(),
        })
    }

    /// Say how a command went: on the device (`last_command_error`, null once
    /// one succeeds) and as a `plugin_command_result` event.
    async fn report(&self, hc_id: &str, operation: &str, result: Result<(), String>) {
        if let Err(e) = &result {
            warn!(hc_id, operation, error = %e, "Command failed");
        }
        let now = result.as_ref().err().cloned();
        let changed = {
            let mut devs = self.devices.write().await;
            match devs.get_mut(hc_id) {
                Some(d) => {
                    let changed = d.last_error != now;
                    d.last_error = now.clone();
                    changed
                }
                None => false,
            }
        };
        // The whole document, not a partial: a partial's null *removes* a key,
        // and `last_command_error` is null — present — once one succeeds.
        if changed {
            self.publish_doc(hc_id, None).await;
        }
        let mut payload = json!({
            "plugin_id": self.publisher.plugin_id(),
            "device_id": hc_id,
            "operation": operation,
            "success": result.is_ok(),
        });
        if let Err(e) = &result {
            payload["error"] = json!(e);
            payload["error_code"] = json!(error_code(e));
        }
        if let Err(e) = self
            .publisher
            .publish_event("plugin_command_result", &payload)
            .await
        {
            debug!(hc_id, error = %e, "Could not publish the command result");
        }
    }

    /// The `ecp_control_disabled` alert: while the Roku says "Control by
    /// mobile apps" is disabled, or refused a command with a 403 since the
    /// last that went through. Current state, set only when it changes.
    async fn refresh_alerts(&self, hc_id: &str) {
        let alerts = {
            let mut devs = self.devices.write().await;
            let Some(d) = devs.get_mut(hc_id) else { return };
            let disabled = d
                .snapshot
                .device_info
                .as_ref()
                .is_some_and(|i| !i.ecp_control_enabled());
            let on = d.online == Some(true) && (disabled || d.control_refused);
            let alerts: Vec<PluginAlert> = if on {
                vec![PluginAlert::new(
                    "ecp_control_disabled",
                    ECP_DISABLED_REASON,
                )]
            } else {
                Vec::new()
            };
            let topics: Vec<String> = alerts.iter().map(|a| a.topic.clone()).collect();
            if d.alerts_sent.as_ref() == Some(&topics) {
                return;
            }
            d.alerts_sent = Some(topics);
            alerts
        };
        if let Err(e) = self.publisher.set_alerts(hc_id, &alerts).await {
            warn!(hc_id, error = %e, "Failed to publish alerts");
        }
    }

    // ── Polling ──────────────────────────────────────────────────────

    /// One poll. Returns how long until the next, or `None` once the device
    /// is no longer managed (its worker then ends).
    async fn poll_once(self: &Arc<Self>, hc_id: &str, cause: Option<&Value>) -> Option<Duration> {
        let (client, base, want_apps, serial, host, port, configured) = {
            let devs = self.devices.read().await;
            let d = devs.get(hc_id)?;
            let stale = d.snapshot.apps.is_empty()
                || d.apps_refreshed_at
                    .map(|t| {
                        t.elapsed()
                            >= Duration::from_secs(self.cfg.roku.app_refresh_interval_secs.max(60))
                    })
                    .unwrap_or(true);
            (
                d.client.clone(),
                d.poll_interval,
                stale,
                d.serial.clone(),
                d.host.clone(),
                d.port,
                d.configured,
            )
        };

        // device-info is the gate: the only query that reports the power
        // state, and the one that says whether the device is reachable.
        let info = match client.device_info().await {
            Ok(i) => i,
            Err(e) => {
                self.went_quiet(hc_id, &e.to_string()).await;
                return self
                    .devices
                    .read()
                    .await
                    .contains_key(hc_id)
                    .then(|| self.standby_interval());
            }
        };

        // Is it still the Roku we think it is? (M3)
        if let Some(s) = info.serial() {
            match &serial {
                Some(mine) if mine != s => {
                    self.answered_as_another(hc_id, s, &host, port).await;
                    return Some(self.standby_interval());
                }
                None if !self.adopt_serial(hc_id, s, configured).await => {
                    return Some(self.standby_interval());
                }
                _ => {}
            }
        }

        let powered = info.is_powered_on();
        let caps = Caps::from_info(&info);
        let mut snap = RokuSnapshot {
            device_info: Some(info.clone()),
            ..Default::default()
        };
        // Everything below only has an answer while the device is on. In
        // standby the app queries return the last-running app.
        if powered {
            match client.active_app().await {
                Ok(a) => snap.active = Some(a),
                Err(e) => debug!(hc_id, error = %e, "active-app query failed"),
            }
            match client.media_player().await {
                Ok(p) => snap.player = Some(p),
                // Pre-9.4 firmware has no media-player endpoint.
                Err(e) => debug!(hc_id, error = %e, "media-player query failed"),
            }
            if caps.is_tv
                && snap
                    .active
                    .as_ref()
                    .and_then(|a| a.app.as_ref())
                    .is_some_and(|app| app.id == "tvinput.dtv")
            {
                match client.tv_active_channel().await {
                    Ok(c) => snap.tv_channel = c,
                    Err(e) => debug!(hc_id, error = %e, "tv-active-channel query failed"),
                }
            }
        }

        // Catalogues: whenever it answers — standby included — on their
        // own slow cadence, carried forward between cycles.
        let (mut apps, mut tv_channels) = {
            let devs = self.devices.read().await;
            let d = devs.get(hc_id)?;
            (d.snapshot.apps.clone(), d.snapshot.tv_channels.clone())
        };
        let mut fetched = None;
        let mut apps_changed = false;
        if want_apps {
            match client.apps().await {
                Ok(a) => {
                    apps_changed = a != apps;
                    apps = a;
                    fetched = Some(None);
                }
                Err(e) => {
                    debug!(hc_id, error = %e, "apps query failed");
                    fetched = Some(Some(e.to_string()));
                }
            }
            if caps.is_tv {
                match client.tv_channels().await {
                    Ok(c) => tv_channels = c,
                    Err(e) => debug!(hc_id, error = %e, "tv-channels query failed"),
                }
            }
        }
        snap.apps = apps;
        snap.tv_channels = tv_channels;

        let playback = state::playback_state(&snap).to_string();
        let macs = info.wake_macs();
        let model = info.model_words();
        let hw = (
            model.clone().unwrap_or_default(),
            info.software_version().unwrap_or_default().to_string(),
        );
        let (was, register_hw, remember, name, want_icons) = {
            let mut devs = self.devices.write().await;
            let d = devs.get_mut(hc_id)?;
            let was = d.online;
            d.online = Some(true);
            d.moved = false;
            d.last_seen = Some(Utc::now());
            let caps_changed = d.caps != caps;
            d.caps = caps;
            d.snapshot = snap;
            d.playback_state = playback;
            if let Some(err) = &fetched {
                d.catalog_error = err.clone();
                if err.is_none() {
                    d.apps_refreshed_at = Some(Instant::now());
                }
            }
            let macs_changed = !macs.is_empty() && d.macs != macs;
            if macs_changed {
                d.macs = macs;
            }
            let model_changed = model.is_some() && d.model != model;
            if model_changed {
                d.model = model;
            }
            let register_hw = d.hardware.as_ref() != Some(&hw) && !hw.0.is_empty();
            if register_hw {
                d.hardware = Some(hw.clone());
            }
            let want_icons = !d.control_refused
                && (apps_changed || fetched.is_some())
                && !self.icons.missing(&d.snapshot.apps).is_empty();
            (
                was,
                register_hw,
                caps_changed || macs_changed || model_changed || apps_changed,
                d.name.clone(),
                want_icons,
            )
        };

        if was != Some(true) {
            info!(hc_id, model = ?info.get("model-name"), "Roku answering");
            let _ = self.publisher.publish_availability(hc_id, true).await;
            if !info.ecp_control_enabled() {
                warn!(
                    hc_id,
                    mode = ?info.get("ecp-setting-mode"),
                    "Roku will report state but reject all control — {ECP_DISABLED_REASON}",
                );
            }
        }
        self.publish_schema(hc_id).await;
        // What the box is: registered once, and again only when the model or
        // firmware changes (H4) — not every poll.
        if register_hw {
            let hardware = plugin_sdk_rs::DeviceHardware::new()
                .manufacturer(
                    info.get("vendor-name")
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Roku"),
                )
                .model(hw.0.clone())
                .sw_version(hw.1.clone());
            if let Err(e) = self
                .publisher
                .register_device_detailed(
                    hc_id,
                    &name,
                    Some("media_player"),
                    None,
                    None,
                    Some(&hardware),
                )
                .await
            {
                debug!(hc_id, error = %e, "identity re-register failed");
            }
        }
        if remember || was.is_none() {
            self.remember(hc_id).await;
        }
        if want_icons {
            self.fetch_icons(hc_id);
        }
        self.publish_doc(hc_id, cause).await;
        self.refresh_alerts(hc_id).await;

        Some(if powered {
            base
        } else {
            self.standby_interval()
        })
    }

    /// The first answer with a serial, for a device that had none (configured
    /// and never seen). Returns false when the serial is another live
    /// device's and this one stands down.
    async fn adopt_serial(self: &Arc<Self>, hc_id: &str, serial: &str, configured: bool) -> bool {
        let other = self.live_with_serial(serial, Some(hc_id)).await;
        if let Some((other_id, other_configured)) = other {
            if configured && !other_configured {
                // The configured entry pins the id: the discovered copy of the
                // same Roku stops being polled. Never two copies of one serial.
                warn!(hc_id, other = %other_id, serial, "Same Roku registered twice; keeping the configured one");
                self.devices.write().await.remove(&other_id);
            } else {
                warn!(hc_id, other = %other_id, serial, "This address answers as another managed Roku");
                self.went_quiet(hc_id, "its address answers as another Roku")
                    .await;
                return false;
            }
        }
        if let Some(d) = self.devices.write().await.get_mut(hc_id) {
            d.serial = Some(serial.to_string());
        }
        true
    }

    /// A live entry with this serial, other than `except`: `(hc_id, configured)`.
    async fn live_with_serial(&self, serial: &str, except: Option<&str>) -> Option<(String, bool)> {
        self.devices
            .read()
            .await
            .values()
            .find(|d| d.serial.as_deref() == Some(serial) && Some(d.hc_id.as_str()) != except)
            .map(|d| (d.hc_id.clone(), d.configured))
    }

    /// Its address now answers as another Roku (they swapped addresses, or
    /// DHCP gave this one's away). The other is re-homed here; this one is
    /// offline and looked for.
    async fn answered_as_another(
        self: &Arc<Self>,
        hc_id: &str,
        other: &str,
        host: &str,
        port: u16,
    ) {
        warn!(hc_id, at = %address(host, port), answers_as = other, "A Roku's address now answers as another");
        if let Some(d) = self.devices.write().await.get_mut(hc_id) {
            d.moved = true;
        }
        self.went_quiet(hc_id, "its address answers as another Roku")
            .await;
        // The other, if it is one we manage, lives here now. One we don't is
        // left to the sweep — which adds it only if discovery may.
        if let Some((other_id, _)) = self.live_with_serial(other, Some(hc_id)).await {
            self.rehome(&other_id, host, port).await;
        }
        self.request_sweep();
    }

    /// A Roku is at a new address. Anything else that was polled there was
    /// wrong about it and is looked for in turn.
    async fn rehome(self: &Arc<Self>, hc_id: &str, host: &str, port: u16) {
        let Ok(client) = self.client_for(host, port) else {
            return;
        };
        let tx = {
            let mut devs = self.devices.write().await;
            for d in devs.values_mut() {
                if d.hc_id != hc_id && d.host == host && d.port == port {
                    d.moved = true;
                }
            }
            let Some(d) = devs.get_mut(hc_id) else { return };
            if d.host == host && d.port == port && !d.moved {
                return;
            }
            info!(hc_id, from = %d.address(), to = %address(host, port), "Roku found at a new address");
            d.host = host.to_string();
            d.port = port;
            d.client = client;
            d.moved = false;
            d.tx.clone()
        };
        self.remember(hc_id).await;
        let _ = tx.try_send(Job::PollNow);
    }

    /// It stopped answering: offline, with what it was doing cleared and what
    /// it is kept (M1). Never removed. A Roku that was answering until now is
    /// looked for at once, in case it moved.
    async fn went_quiet(self: &Arc<Self>, hc_id: &str, why: &str) {
        let prev = {
            let mut devs = self.devices.write().await;
            let Some(d) = devs.get_mut(hc_id) else { return };
            let prev = d.online;
            d.online = Some(false);
            d.snapshot.device_info = None;
            d.snapshot.active = None;
            d.snapshot.player = None;
            d.snapshot.tv_channel = None;
            d.playback_state = "idle".into();
            d.anchor = None;
            prev
        };
        if prev == Some(false) {
            return;
        }
        if prev == Some(true) {
            warn!(hc_id, why, "Roku not answering");
        } else {
            info!(hc_id, why, "Roku not answering at startup");
        }
        self.publish_doc(hc_id, None).await;
        self.refresh_alerts(hc_id).await;
        if prev == Some(true) {
            self.request_sweep();
        }
    }

    /// Publish the state document — always for a command (its
    /// acknowledgement), otherwise only when it changed.
    async fn publish_doc(&self, hc_id: &str, cause: Option<&Value>) {
        let (doc, online) = {
            let mut devs = self.devices.write().await;
            let Some(d) = devs.get_mut(hc_id) else { return };
            let icons = Arc::clone(&self.icons);
            let icon = move |a: &App| icons.path_for(a);
            let mut doc = state::to_json(
                &d.snapshot,
                &state::Extras {
                    caps: &d.caps,
                    muted: d.muted,
                    last_error: d.last_error.as_deref(),
                    serial: d.serial.as_deref(),
                    icon: &icon,
                    launches: &d.launches,
                },
            );
            let playing = doc["state"] == "playing";
            d.anchor = anchor(
                d.anchor.as_ref(),
                doc["media_position"].as_u64(),
                playing,
                Instant::now(),
                || Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
            if let Some(a) = &d.anchor {
                doc["media_position"] = json!(a.pos);
                doc["media_position_updated_at"] = json!(a.iso);
            }
            let changed = d.last_published.as_ref() != Some(&doc);
            if !changed && cause.is_none() {
                return;
            }
            d.last_published = Some(doc.clone());
            (doc, d.online == Some(true))
        };
        let published = match cause {
            Some(cmd) => {
                self.publisher
                    .publish_state_for_command(hc_id, &doc, cmd, "hc-roku")
                    .await
            }
            None => self.publisher.publish_state(hc_id, &doc).await,
        };
        if let Err(e) = published {
            warn!(hc_id, error = %e, "Failed to publish state");
        }
        // A state publish is homeCore's sign that a device is there.
        if !online {
            let _ = self.publisher.publish_availability(hc_id, false).await;
        }
    }

    /// Fetch the icons it is missing, one at a time, then republish.
    fn fetch_icons(self: &Arc<Self>, hc_id: &str) {
        let b = Arc::clone(self);
        let hc_id = hc_id.to_string();
        tokio::spawn(async move {
            let (client, missing, tx) = {
                let devs = b.devices.read().await;
                let Some(d) = devs.get(&hc_id) else { return };
                (
                    d.client.clone(),
                    b.icons.missing(&d.snapshot.apps),
                    d.tx.clone(),
                )
            };
            let mut got = 0usize;
            for app in &missing {
                match b.icons.fetch(&client, app).await {
                    Ok(true) => got += 1,
                    Ok(false) => {}
                    Err(e) => {
                        let refused = e
                            .downcast_ref::<EcpError>()
                            .is_some_and(|x| matches!(x, EcpError::Forbidden));
                        debug!(hc_id, app = %app.id, error = %e, "Icon not fetched");
                        if refused {
                            break;
                        }
                    }
                }
            }
            if got > 0 {
                info!(hc_id, got, "Channel icons kept");
                let _ = tx.try_send(Job::PollNow);
            }
        });
    }

    // ── Remembering ──────────────────────────────────────────────────

    /// Copy what is known about a device into the learned map and write it
    /// soon.
    async fn remember(&self, hc_id: &str) {
        let k = {
            let devs = self.devices.read().await;
            let Some(d) = devs.get(hc_id) else { return };
            let Some(serial) = d.serial.clone() else {
                return;
            };
            (
                serial,
                Known {
                    hc_id: d.hc_id.clone(),
                    host: d.host.clone(),
                    port: (d.port != crate::ecp::DEFAULT_PORT).then_some(d.port),
                    macs: d.macs.clone(),
                    name: Some(d.name.clone()),
                    model: d.model.clone(),
                    caps: Some(d.caps.clone()),
                    apps: d.snapshot.apps.clone(),
                    launches: d.launches.clone(),
                },
            )
        };
        let mut known = self.known.write().await;
        if known.get(&k.0) != Some(&k.1) {
            known.insert(k.0, k.1);
            self.persist_now.notify_one();
        }
    }

    /// Write the learned map a moment after it last changed. The **whole**
    /// map, every time: core merges only the document's top level, so a
    /// delta naming one Roku under `devices` would forget the others.
    async fn persist_loop(self: Arc<Self>) {
        loop {
            self.persist_now.notified().await;
            tokio::time::sleep(PERSIST_DEBOUNCE).await;
            let doc = {
                let known = self.known.read().await;
                json!({ "devices": &*known })
            };
            if let Err(e) = self.state_writer.persist(&doc).await {
                warn!(error = %e, "Failed to keep what was learnt about the Rokus");
            }
        }
    }

    // ── Discovery ────────────────────────────────────────────────────

    fn request_sweep(&self) {
        self.sweep_wanted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.sweep_now.notify_one();
    }

    async fn sweep_loop(self: Arc<Self>) {
        let periodic = self.cfg.roku.discovery_enabled;
        let interval = Duration::from_secs(self.cfg.roku.discovery_interval_secs.max(60));
        if periodic {
            self.discovery_sweep().await;
        }
        let mut last = Instant::now();
        loop {
            let timer = tokio::time::sleep(interval);
            tokio::select! {
                _ = self.sweep_now.notified() => {
                    let since = last.elapsed();
                    if since < SWEEP_GAP {
                        tokio::time::sleep(SWEEP_GAP - since).await;
                    }
                    // Every ask up to now is answered by this one sweep.
                    if !self
                        .sweep_wanted
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        continue;
                    }
                    self.discovery_sweep().await;
                }
                _ = timer, if periodic => {
                    self.discovery_sweep().await;
                }
            }
            last = Instant::now();
        }
    }

    /// Candidate addresses for one sweep: SSDP's answers (only with
    /// discovery on), every `manual_hosts` entry, and every address a Roku
    /// was configured at or last seen at — the last two are how a Roku that
    /// moved is found again without multicast.
    pub async fn sweep_candidates(&self) -> Vec<SsdpHit> {
        let mut hits = if self.cfg.roku.discovery_enabled {
            let timeout = Duration::from_secs(self.cfg.roku.discovery_timeout_secs.clamp(1, 30));
            match discovery::ssdp_search(timeout, 3).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(error = %e, "SSDP sweep failed");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut add = |host: &str, port: u16| {
            if !hits.iter().any(|h| h.host == host && h.port == port) {
                hits.push(SsdpHit {
                    host: host.to_string(),
                    port,
                    serial: None,
                    location: String::new(),
                });
            }
        };
        for host in &self.cfg.roku.manual_hosts {
            add(host, crate::ecp::DEFAULT_PORT);
        }
        for d in &self.cfg.devices {
            add(&d.host, d.port.unwrap_or(crate::ecp::DEFAULT_PORT));
        }
        for k in self.known.read().await.values() {
            add(&k.host, k.port.unwrap_or(crate::ecp::DEFAULT_PORT));
        }
        for d in self.devices.read().await.values() {
            add(&d.host, d.port);
        }
        hits
    }

    /// Devices already managed, as `(hc_id, host, port)`.
    pub async fn managed_hosts(&self) -> Vec<(String, String, u16)> {
        let devs = self.devices.read().await;
        let mut v: Vec<(String, String, u16)> = devs
            .values()
            .map(|d| (d.hc_id.clone(), d.host.clone(), d.port))
            .collect();
        v.sort();
        v
    }

    /// One full sweep: find candidates, then place each — in parallel.
    pub async fn discovery_sweep(self: &Arc<Self>) -> Vec<SsdpHit> {
        let hits = self.sweep_candidates().await;
        debug!(count = hits.len(), "Sweep");
        let jobs = hits.iter().cloned().map(|h| {
            let b = Arc::clone(self);
            async move { b.integrate_hit(h).await }
        });
        futures_util::future::join_all(jobs).await;
        if let Ok(mut l) = self.last_sweep.lock() {
            *l = Some(Utc::now());
        }
        self.refresh_device_notice().await;
        hits
    }

    /// Place one answering address: the Roku managed with its serial (moved
    /// there, if it was elsewhere), a remembered one, or — when discovery is
    /// on and may add — a new one. Returns the device id when it is managed.
    pub async fn integrate_hit(self: &Arc<Self>, hit: SsdpHit) -> Option<String> {
        // Held by a device answering there now: nothing to do.
        let held = {
            let devs = self.devices.read().await;
            devs.values()
                .find(|d| {
                    d.host == hit.host && d.port == hit.port && d.online == Some(true) && !d.moved
                })
                .map(|d| d.hc_id.clone())
        };
        if let Some(id) = held {
            return Some(id);
        }
        let client = self.client_for(&hit.host, hit.port).ok()?;
        let info = match client.device_info().await {
            Ok(i) => i,
            Err(e) => {
                debug!(host = %hit.host, error = %e, "Address did not answer ECP");
                return None;
            }
        };
        let serial = info.serial().map(str::to_string).or(hit.serial.clone());

        if let Some(s) = &serial {
            // One serial, one device: the managed one moves here.
            if let Some((id, _)) = self.live_with_serial(s, None).await {
                self.rehome(&id, &hit.host, hit.port).await;
                return Some(id);
            }
            // Remembered from an earlier run but not registered now (discovery
            // was off then, or it was forgotten from the map): only with
            // discovery on, which is what lets the plugin add Rokus at all.
            let known = self.known.read().await.get(s).cloned();
            if let Some(mut k) = known {
                if !self.cfg.roku.discovery_enabled {
                    return None;
                }
                k.host = hit.host.clone();
                k.port = (hit.port != crate::ecp::DEFAULT_PORT).then_some(hit.port);
                let id = k.hc_id.clone();
                self.add_known(s, &k).await;
                return Some(id);
            }
        }

        if !self.cfg.roku.discovery_enabled || !self.cfg.roku.auto_add_discovered {
            debug!(host = %hit.host, "A Roku answered, but this plugin adds none of its own");
            return None;
        }
        let hc_id = mint_device_id(serial.as_deref(), &hit.host);
        if self.devices.read().await.contains_key(&hc_id) {
            return Some(hc_id);
        }
        let name = info
            .display_name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("Roku {hc_id}"));
        info!(hc_id = %hc_id, host = %hit.host, serial = ?serial, "Discovered new Roku");
        let made = self.new_entry(&hc_id, &hit.host, hit.port, &name, false, None);
        let Ok((mut entry, rx)) = made else {
            return None;
        };
        entry.serial = serial;
        entry.caps = Caps::from_info(&info);
        self.register(entry, rx).await;
        self.remember(&hc_id).await;
        Some(hc_id)
    }

    // ── Management actions ───────────────────────────────────────────

    pub fn discovery_enabled(&self) -> bool {
        self.cfg.roku.discovery_enabled
    }

    /// The plugin page's view: each Roku from memory, nothing asked of any.
    pub async fn status(&self) -> Value {
        let devs = self.devices.read().await;
        let mut list: Vec<Value> = devs
            .values()
            .map(|d| {
                let info = d.snapshot.device_info.as_ref();
                let inputs = d.snapshot.apps.iter().filter(|a| a.is_input()).count();
                let on_screen = d.last_published.as_ref();
                json!({
                    "hc_id": d.hc_id,
                    "name": d.name,
                    "address": d.address(),
                    "host": d.host,
                    "port": d.port,
                    "configured": d.configured,
                    "online": d.online,
                    "moved": d.moved,
                    "network": info.and_then(|i| i.get("network-type")),
                    "model": d.model,
                    "roku_os": info.and_then(DeviceInfo::software_version),
                    "is_tv": d.caps.is_tv,
                    "find_remote": d.caps.find_remote,
                    "phone_control": info.map(|i| if i.ecp_control_enabled() && !d.control_refused { "allowed" } else { "disabled" }),
                    "power_mode": info.map(DeviceInfo::power_mode),
                    "channels": d.snapshot.apps.len() - inputs,
                    "inputs": inputs,
                    "can_wake": self.cfg.roku.wake_on_lan && !d.macs.is_empty(),
                    "now": {
                        "state": on_screen.map(|p| p["state"].clone()),
                        "source": on_screen.map(|p| p["source"].clone()),
                        "app_id": on_screen.map(|p| p["app_id"].clone()),
                    },
                    "last_error": d.last_error,
                    "last_seen": d.last_seen,
                })
            })
            .collect();
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let last_sweep = self.last_sweep.lock().ok().and_then(|l| *l);
        json!({
            "status": "ok",
            "rokus": list,
            "discovery": self.cfg.roku.discovery_enabled,
            "discovery_interval_secs": self.cfg.roku.discovery_interval_secs,
            "wake_on_lan": self.cfg.roku.wake_on_lan,
            "last_sweep": last_sweep,
        })
    }

    pub async fn list_devices(&self) -> Value {
        let devs = self.devices.read().await;
        let mut list: Vec<Value> = devs
            .values()
            .map(|d| {
                json!({
                    "hc_id": d.hc_id,
                    "host": d.host,
                    "port": d.port,
                    "url": d.client.base_url(),
                    "name": d.name,
                    "serial": d.serial,
                    "configured": d.configured,
                    "online": d.online == Some(true),
                    "state": d.playback_state,
                })
            })
            .collect();
        list.sort_by(|a, b| a["hc_id"].as_str().cmp(&b["hc_id"].as_str()));
        json!({ "status": "ok", "count": list.len(), "devices": list })
    }

    /// The ids an action names (`hc_id`), or every Roku.
    async fn targets(&self, cmd: &Value) -> Result<Vec<String>, String> {
        let devs = self.devices.read().await;
        match cmd
            .get("hc_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(id) if devs.contains_key(id) => Ok(vec![id.to_string()]),
            Some(id) => Err(format!("unknown device '{id}'")),
            None => Ok(devs.keys().cloned().collect()),
        }
    }

    /// Read the channel lists again now — through each Roku's worker, so it
    /// waits its turn behind commands — all Rokus at once.
    pub async fn refresh_catalog(self: &Arc<Self>, cmd: &Value) -> Value {
        let ids = match self.targets(cmd).await {
            Ok(ids) => ids,
            Err(e) => return json!({ "status": "error", "error": e }),
        };
        let asks = ids.into_iter().map(|id| {
            let b = Arc::clone(self);
            async move {
                let tx = b.devices.read().await.get(&id).map(|d| d.tx.clone());
                let (reply, rx) = oneshot::channel();
                let said = match tx {
                    Some(tx) if tx.try_send(Job::Refresh { reply }).is_ok() => {
                        match tokio::time::timeout(Duration::from_secs(25), rx).await {
                            Ok(Ok(v)) => v,
                            _ => json!({ "error": "no answer in time" }),
                        }
                    }
                    _ => json!({ "error": "busy" }),
                };
                (id, said)
            }
        });
        let per: serde_json::Map<String, Value> = futures_util::future::join_all(asks)
            .await
            .into_iter()
            .collect();
        json!({ "status": "ok", "devices": per })
    }

    /// Raw `query/device-info` from each Roku (MACs and all — a diagnostic,
    /// never state), asked in parallel.
    pub async fn device_info(&self, cmd: &Value) -> Value {
        let ids = match self.targets(cmd).await {
            Ok(ids) => ids,
            Err(e) => return json!({ "status": "error", "error": e }),
        };
        let clients: Vec<(String, EcpClient)> = {
            let devs = self.devices.read().await;
            ids.iter()
                .filter_map(|id| devs.get(id).map(|d| (id.clone(), d.client.clone())))
                .collect()
        };
        let asks = clients.into_iter().map(|(id, c)| async move {
            let v = match c.device_info().await {
                Ok(info) => Value::Object(
                    info.fields
                        .iter()
                        .map(|(k, v)| (k.replace('-', "_"), json!(v)))
                        .collect(),
                ),
                Err(e) => json!({ "error": e.to_string() }),
            };
            (id, v)
        });
        let per: serde_json::Map<String, Value> = futures_util::future::join_all(asks)
            .await
            .into_iter()
            .collect();
        json!({ "status": "ok", "devices": per })
    }

    /// One command with a real answer: queued behind the Roku's others.
    pub async fn send_command(self: &Arc<Self>, cmd: &Value) -> Value {
        let Some(hc_id) = cmd["hc_id"].as_str() else {
            return json!({ "status": "error", "error": "send_command requires 'hc_id'" });
        };
        let Some(inner) = cmd.get("command").cloned() else {
            return json!({ "status": "error", "error": "send_command requires 'command'" });
        };
        let (reply, rx) = oneshot::channel();
        self.enqueue(hc_id, inner, Some(reply)).await;
        match tokio::time::timeout(Duration::from_secs(40), rx).await {
            Ok(Ok(Ok(v))) => json!({ "status": "ok", "result": v }),
            Ok(Ok(Err(e))) => json!({ "status": "error", "error": e }),
            _ => json!({ "status": "error", "error": "the Roku did not answer in time" }),
        }
    }

    /// "Forget it": a Roku gone for good stops being managed, its device is
    /// removed, and it is forgotten. One listed in the settings is the
    /// operator's to remove there.
    pub async fn forget_device(&self, cmd: &Value) -> Value {
        let Some(hc_id) = cmd["hc_id"].as_str().or(cmd["device_id"].as_str()) else {
            return json!({ "status": "error", "error": "forget_device requires 'hc_id'" });
        };
        let serial = {
            let devs = self.devices.read().await;
            match devs.get(hc_id) {
                None => {
                    return json!({ "status": "error", "error": format!("unknown device '{hc_id}'") })
                }
                Some(d) if d.configured => {
                    return json!({ "status": "error", "error": "it is listed in the plugin's settings — remove it there" })
                }
                Some(d) => d.serial.clone(),
            }
        };
        self.devices.write().await.remove(hc_id);
        if let Some(s) = serial {
            self.known.write().await.remove(&s);
        } else {
            self.known.write().await.retain(|_, k| k.hc_id != hc_id);
        }
        self.persist_now.notify_one();
        let _ = self.publisher.set_alerts(hc_id, &[]).await;
        if let Err(e) = self
            .publisher
            .unregister_device(self.publisher.plugin_id(), hc_id)
            .await
        {
            return json!({ "status": "error", "error": e.to_string() });
        }
        self.refresh_device_notice().await;
        info!(hc_id, "Roku forgotten");
        json!({ "status": "ok", "forgotten": hc_id })
    }

    /// Every discovered Roku not answering now, forgotten — asked in parallel.
    pub async fn forget_stale_devices(&self) -> Value {
        let targets: Vec<(String, bool, EcpClient)> = self
            .devices
            .read()
            .await
            .values()
            .map(|d| (d.hc_id.clone(), d.configured, d.client.clone()))
            .collect();
        let asks = targets.into_iter().map(|(id, configured, c)| async move {
            (id, configured, c.device_info().await.is_ok())
        });
        let mut retired = Vec::new();
        for (id, configured, answered) in futures_util::future::join_all(asks).await {
            if !answered
                && !configured
                && self.forget_device(&json!({ "hc_id": id })).await["status"] == "ok"
            {
                retired.push(id);
            }
        }
        json!({ "status": "ok", "retired": retired })
    }
}

/// What a command runs against, copied out of the device map.
struct CommandSnapshot {
    client: EcpClient,
    apps: Vec<App>,
    caps: Caps,
    playback: String,
    muted: bool,
    online: bool,
    powered: bool,
    on_input: bool,
    macs: Vec<String>,
    host: String,
}

/// What a command asked, for `plugin_command_result`: its action, or its
/// keys without the change metadata (`_hc`, `correlation_id`).
fn operation_of(cmd: &Value) -> String {
    if let Some(a) = cmd.get("action").and_then(Value::as_str) {
        return a.to_string();
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

/// A failure's kind, for `plugin_command_result`.
pub fn error_code(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.contains("http 403") || l.contains("control by mobile apps") {
        "control_disabled"
    } else if l.contains("not sent") || l.contains("waited") || l.contains("too many commands") {
        "stale"
    } else if l.contains("request to") && l.contains("failed")
        || l.contains("isn't answering")
        || l.contains("wake-on-lan")
    {
        "unreachable"
    } else if l.contains("no installed app") || l.contains("unknown device") {
        "not_found"
    } else if l.contains("has no") || l.contains("no absolute") || l.contains("does not implement")
    {
        "unsupported"
    } else if l.contains("requires")
        || l.contains("unknown roku key")
        || l.contains("longer than")
        || l.contains("ambiguous")
        || l.contains("unsupported roku action")
    {
        "invalid_params"
    } else {
        "rejected"
    }
}

/// Derive a stable homeCore device id.
///
/// The serial is preferred because it survives a DHCP move; the host is
/// the fallback for devices that answered without one, at the cost of the
/// id changing if the address does.
fn mint_device_id(serial: Option<&str>, host: &str) -> String {
    let basis = serial.filter(|s| !s.is_empty()).unwrap_or(host);
    let slug: String = basis
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("roku_{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The house's ids are `roku_<serial lowercase>`: they must never change.
    #[test]
    fn device_id_prefers_the_serial() {
        assert_eq!(
            mint_device_id(Some("YL0001073725"), "10.0.10.69"),
            "roku_yl0001073725"
        );
    }

    /// Without a serial the address is all we have, and it has to be
    /// sanitised — dots are not valid in a homeCore device id.
    #[test]
    fn device_id_falls_back_to_the_host() {
        assert_eq!(mint_device_id(None, "10.0.10.40"), "roku_10_0_10_40");
        assert_eq!(mint_device_id(Some(""), "10.0.10.40"), "roku_10_0_10_40");
    }

    /// M10: published again only when it jumps — not every poll it plays.
    #[test]
    fn the_position_is_anchored_until_it_jumps() {
        let t0 = Instant::now();
        let a = anchor(None, Some(100), true, t0, || "t0".into()).unwrap();
        // Ten seconds on, ten seconds further: the same anchor.
        let b = anchor(
            Some(&a),
            Some(110),
            true,
            t0 + Duration::from_secs(10),
            || "t1".into(),
        )
        .unwrap();
        assert_eq!(b.iso, "t0");
        // A scan forward: a new one.
        let c = anchor(
            Some(&b),
            Some(200),
            true,
            t0 + Duration::from_secs(11),
            || "t2".into(),
        )
        .unwrap();
        assert_eq!((c.pos, c.iso.as_str()), (200, "t2"));
        // Paused and still: the same.
        let p = anchor(
            Some(&c),
            Some(200),
            false,
            t0 + Duration::from_secs(12),
            || "t3".into(),
        )
        .unwrap();
        let q = anchor(
            Some(&p),
            Some(200),
            false,
            t0 + Duration::from_secs(40),
            || "t4".into(),
        )
        .unwrap();
        assert_eq!(q.iso, "t3");
        assert!(anchor(Some(&q), None, false, t0, || "x".into()).is_none());
    }

    /// The old learned document (`{hc_id, host}` per serial) still reads,
    /// and the new one keeps everything a restart needs.
    #[test]
    fn learned_entries_old_and_new_both_read() {
        let old: Known =
            serde_json::from_value(json!({ "hc_id": "roku_x1", "host": "10.0.10.40" })).unwrap();
        assert_eq!(old.hc_id, "roku_x1");
        assert!(old.apps.is_empty() && old.macs.is_empty());
        let new = Known {
            hc_id: "roku_x1".into(),
            host: "10.0.10.40".into(),
            port: None,
            macs: vec!["02:00:00:00:00:01".into()],
            name: Some("Office TV".into()),
            model: Some("TCL 32S327".into()),
            caps: Some(Caps {
                is_tv: true,
                ..Default::default()
            }),
            apps: vec![App {
                id: "837".into(),
                name: "YouTube".into(),
                app_type: Some("appl".into()),
                version: Some("2.23".into()),
            }],
            launches: BTreeMap::from([("837".into(), 4)]),
        };
        let back: Known = serde_json::from_value(serde_json::to_value(&new).unwrap()).unwrap();
        assert_eq!(back, new);
    }

    #[test]
    fn failures_have_kinds() {
        assert_eq!(
            error_code(&EcpError::Forbidden.to_string()),
            "control_disabled"
        );
        assert_eq!(
            error_code("ECP request to http://x/keypress/Home failed: connection refused"),
            "unreachable"
        );
        assert_eq!(
            error_code("source: no installed app matches 'X'"),
            "not_found"
        );
        assert_eq!(error_code("this Roku has no tuner"), "unsupported");
        assert_eq!(
            error_code("text is longer than 200 characters"),
            "invalid_params"
        );
    }
}

/// The bridge against stand-in Rokus: what each fix promises, end to end.
#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::testutil::{paths, scripted, Log};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    /// A Roku as `device-info` shows it.
    fn info_xml(serial: &str, power: &str, ecp: &str) -> String {
        format!(
            "<device-info><serial-number>{serial}</serial-number><vendor-name>TCL</vendor-name>\
             <model-name>32S327</model-name><model-number>8116X</model-number>\
             <software-version>15.3.4</software-version><is-tv>true</is-tv>\
             <network-type>wifi</network-type><wifi-mac>02:00:00:00:00:02</wifi-mac>\
             <power-mode>{power}</power-mode><ecp-setting-mode>{ecp}</ecp-setting-mode>\
             <supports-find-remote>false</supports-find-remote></device-info>"
        )
    }
    const APPS: &str =
        "<apps><app id=\"tvinput.hdmi1\" type=\"tvin\" version=\"1.0.0\">HDMI\u{a0}1</app>\
         <app id=\"837\" type=\"appl\" version=\"2.23\">YouTube</app>\
         <app id=\"12\" type=\"appl\" version=\"5.1\">Netflix</app></apps>";
    const ON_HDMI: &str =
        "<active-app><app id=\"tvinput.hdmi1\" type=\"tvin\">HDMI\u{a0}1</app></active-app>";

    /// A stand-in whose answers a test can change.
    #[derive(Clone)]
    struct Roku {
        serial: Arc<StdMutex<String>>,
        power: Arc<StdMutex<&'static str>>,
        refuse_keys: Arc<AtomicBool>,
        dead: Arc<AtomicBool>,
    }

    async fn roku(serial: &str, power: &'static str) -> (Roku, String, u16, Log) {
        let r = Roku {
            serial: Arc::new(StdMutex::new(serial.into())),
            power: Arc::new(StdMutex::new(power)),
            refuse_keys: Arc::new(AtomicBool::new(false)),
            dead: Arc::new(AtomicBool::new(false)),
        };
        let s = r.clone();
        let (client, log) = scripted(move |_, path| {
            if s.dead.load(Ordering::SeqCst) {
                return (500, String::new());
            }
            match path {
                "/query/device-info" => {
                    let ecp = if s.refuse_keys.load(Ordering::SeqCst) {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    (
                        200,
                        info_xml(&s.serial.lock().unwrap(), &s.power.lock().unwrap(), ecp),
                    )
                }
                "/query/apps" => (200, APPS.into()),
                "/query/active-app" => (200, ON_HDMI.into()),
                "/query/media-player" => (200, "<player error=\"false\" state=\"none\"/>".into()),
                "/query/tv-channels" => (200, "<tv-channels/>".into()),
                p if p.starts_with("/query/icon/") => (200, "PNG".into()),
                p if p.starts_with("/keypress/") && s.refuse_keys.load(Ordering::SeqCst) => {
                    (403, String::new())
                }
                _ => (200, String::new()),
            }
        })
        .await;
        let url = client.base_url().trim_start_matches("http://").to_string();
        let (host, port) = url.rsplit_once(':').unwrap();
        (r, host.to_string(), port.parse().unwrap(), log)
    }

    fn bridge(devices: &[(&str, &str, u16)]) -> Arc<Bridge> {
        let mut toml = String::from("[roku]\ndiscovery_enabled = false\nwake_on_lan = false\n");
        for (id, host, port) in devices {
            toml.push_str(&format!(
                "[[devices]]\nhc_id = \"{id}\"\nname = \"{id}\"\nhost = \"{host}\"\nport = {port}\n"
            ));
        }
        let cfg: RokuConfig = toml::from_str(&toml).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "hc-roku-bridge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b = Arc::new(Bridge::new(
            cfg,
            DevicePublisher::test_instance("plugin.roku"),
            PluginStateWriter::test_instance("plugin.roku"),
            PluginNotices::test_instance(),
            &dir,
        ));
        b.learned.send_replace(true);
        b
    }

    async fn start(b: &Arc<Bridge>) {
        for d in b.cfg.devices.clone() {
            b.add_configured(&d).await;
        }
    }

    async fn until<F, Fut>(what: &str, f: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f().await {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn online(b: &Arc<Bridge>, id: &str) -> bool {
        b.devices.read().await.get(id).and_then(|d| d.online) == Some(true)
    }

    /// H1: presses go one after another, in the order sent, and a burst is
    /// followed by one re-poll — not one per press.
    #[tokio::test]
    async fn presses_arrive_in_order_with_one_poll_after_the_burst() {
        let (_r, host, port, log) = roku("YK00XF435811", "PowerOn").await;
        let b = bridge(&[("roku_yk00xf435811", &host, port)]);
        start(&b).await;
        until("first poll", || online(&b, "roku_yk00xf435811")).await;
        log.lock().unwrap().clear();
        for key in ["Down", "Down", "Right", "Select", "Back"] {
            b.enqueue(
                "roku_yk00xf435811",
                json!({ "action": "key", "key": key }),
                None,
            )
            .await;
        }
        until("the re-poll", || async {
            paths(&log).iter().any(|p| p == "/query/device-info")
        })
        .await;
        tokio::time::sleep(POST_COMMAND_SETTLE).await;
        let seen = paths(&log);
        let keys: Vec<&String> = seen
            .iter()
            .filter(|p| p.starts_with("/keypress/"))
            .collect();
        assert_eq!(
            keys,
            [
                "/keypress/Down",
                "/keypress/Down",
                "/keypress/Right",
                "/keypress/Select",
                "/keypress/Back"
            ]
        );
        let first_poll = seen.iter().position(|p| p == "/query/device-info").unwrap();
        assert!(
            first_poll
                > seen
                    .iter()
                    .rposition(|p| p.starts_with("/keypress/"))
                    .unwrap(),
            "polled in the middle of the burst: {seen:?}"
        );
        assert_eq!(
            seen.iter().filter(|p| *p == "/query/device-info").count(),
            1,
            "{seen:?}"
        );
    }

    /// H3: the channel list is read from a Roku in standby (ECP answers in
    /// Ready), and remembered — so a restart has it before the TV answers.
    #[tokio::test]
    async fn the_catalogue_is_read_in_standby_and_remembered() {
        let (_r, host, port, log) = roku("X00300CR4TPA", "Ready").await;
        let b = bridge(&[("roku_x00300cr4tpa", &host, port)]);
        start(&b).await;
        until("first poll", || online(&b, "roku_x00300cr4tpa")).await;
        assert!(paths(&log).contains(&"/query/apps".to_string()));
        let doc = b.devices.read().await["roku_x00300cr4tpa"]
            .last_published
            .clone()
            .unwrap();
        assert_eq!(doc["on"], false);
        assert_eq!(doc["source"], Value::Null);
        assert_eq!(doc["available_inputs"][0]["name"], "HDMI 1");
        assert_eq!(doc["available_apps"].as_array().unwrap().len(), 2);
        assert!(
            !paths(&log).contains(&"/query/active-app".to_string()),
            "asked a TV in standby what it shows"
        );
        let known = b.known.read().await;
        let k = &known["X00300CR4TPA"];
        assert_eq!((k.hc_id.as_str(), k.apps.len()), ("roku_x00300cr4tpa", 3));
        assert_eq!(k.macs, vec!["02:00:00:00:00:02".to_string()]);
    }

    /// H2: a 403 is said on the device — `last_command_error` — and raises
    /// the alert with the fix; a command that goes through clears both.
    #[tokio::test]
    async fn a_refusal_is_said_and_raises_the_alert() {
        let (r, host, port, _log) = roku("YL0001073725", "PowerOn").await;
        let b = bridge(&[("roku_yl0001073725", &host, port)]);
        start(&b).await;
        until("first poll", || online(&b, "roku_yl0001073725")).await;
        r.refuse_keys.store(true, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        b.enqueue("roku_yl0001073725", json!({ "action": "home" }), Some(tx))
            .await;
        let said = rx.await.unwrap().unwrap_err();
        assert!(said.contains("Control by mobile apps"), "{said}");
        {
            let devs = b.devices.read().await;
            let d = &devs["roku_yl0001073725"];
            assert!(d
                .last_error
                .as_deref()
                .unwrap()
                .contains("Control by mobile apps"));
            assert_eq!(
                d.alerts_sent.as_deref(),
                Some(&["ecp_control_disabled".to_string()][..])
            );
        }
        r.refuse_keys.store(false, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        b.enqueue("roku_yl0001073725", json!({ "action": "home" }), Some(tx))
            .await;
        rx.await.unwrap().unwrap();
        let devs = b.devices.read().await;
        assert_eq!(devs["roku_yl0001073725"].last_error, None);
        assert_eq!(
            devs["roku_yl0001073725"].alerts_sent.as_deref(),
            Some(&[][..])
        );
    }

    /// M1: one that stops answering is offline with what it was doing
    /// cleared and its channels kept — never removed.
    #[tokio::test]
    async fn a_roku_that_stops_answering_is_cleared_not_removed() {
        let (r, host, port, _log) = roku("YK00XF435811", "PowerOn").await;
        let b = bridge(&[("roku_yk00xf435811", &host, port)]);
        start(&b).await;
        until("first poll", || online(&b, "roku_yk00xf435811")).await;
        r.dead.store(true, Ordering::SeqCst);
        let tx = b.devices.read().await["roku_yk00xf435811"].tx.clone();
        tx.send(Job::PollNow).await.unwrap();
        until("offline", || async {
            b.devices.read().await["roku_yk00xf435811"].online == Some(false)
        })
        .await;
        let doc = b.devices.read().await["roku_yk00xf435811"]
            .last_published
            .clone()
            .unwrap();
        assert_eq!(
            (doc["state"].clone(), doc["source"].clone()),
            (Value::Null, Value::Null)
        );
        assert_eq!(doc["available_apps"].as_array().unwrap().len(), 2);
    }

    /// M3: two Rokus swap addresses — each is found by its serial at the
    /// other's, keeps its id, and no address is two devices.
    #[tokio::test]
    async fn an_address_swap_is_followed_by_serial() {
        let (a, host_a, port_a, _) = roku("YK00XF435811", "PowerOn").await;
        let (c, host_c, port_c, _) = roku("YL0001073725", "PowerOn").await;
        let b = bridge(&[
            ("roku_yk00xf435811", &host_a, port_a),
            ("roku_yl0001073725", &host_c, port_c),
        ]);
        start(&b).await;
        until("first polls", || async {
            online(&b, "roku_yk00xf435811").await && online(&b, "roku_yl0001073725").await
        })
        .await;
        *a.serial.lock().unwrap() = "YL0001073725".into();
        *c.serial.lock().unwrap() = "YK00XF435811".into();
        for id in ["roku_yk00xf435811", "roku_yl0001073725"] {
            let tx = b.devices.read().await[id].tx.clone();
            tx.send(Job::PollNow).await.unwrap();
        }
        until("re-homed", || async {
            let devs = b.devices.read().await;
            let office = &devs["roku_yk00xf435811"];
            let bar = &devs["roku_yl0001073725"];
            office.port == port_c
                && bar.port == port_a
                && office.online == Some(true)
                && bar.online == Some(true)
        })
        .await;
        assert_eq!(b.devices.read().await.len(), 2);
    }

    /// H4: model and firmware are registered once, not every poll.
    #[tokio::test]
    async fn the_hardware_is_registered_once() {
        let (_r, host, port, _log) = roku("YK00XF435811", "PowerOn").await;
        let b = bridge(&[("roku_yk00xf435811", &host, port)]);
        start(&b).await;
        until("first poll", || online(&b, "roku_yk00xf435811")).await;
        let first = b.devices.read().await["roku_yk00xf435811"].hardware.clone();
        assert_eq!(
            first,
            Some(("TCL 32S327".to_string(), "15.3.4".to_string()))
        );
        // A second poll with the same answer changes nothing to register.
        assert!(b.poll_once("roku_yk00xf435811", None).await.is_some());
        assert_eq!(b.devices.read().await["roku_yk00xf435811"].hardware, first);
    }
}
