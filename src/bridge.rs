//! Device lifecycle, polling, and command routing.
//!
//! ECP has no push channel and no subscription mechanism, so everything
//! homeCore knows about a Roku comes from asking it on a timer. The loop
//! is deliberately cheap: `query/device-info` every cycle (it is the only
//! way to see the power state), the app and player queries only while the
//! device is awake, and the installed-channel catalogue on its own slow
//! cadence because it changes when someone installs a channel, not when
//! they press a button.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices, PluginStateWriter};
use serde_json::{json, Value};
use tokio::sync::{mpsc, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::commands::{self, CommandContext};
use crate::config::{DeviceConfig, RokuConfig};
use crate::discovery::{self, SsdpHit};
use crate::ecp::{DeviceInfo, EcpClient};
use crate::state::{self, RokuSnapshot};

/// How long after a command to re-poll. Roku applies a keypress
/// asynchronously — polling immediately reads the state *before* the
/// press landed and publishes a value that the next cycle contradicts.
const POST_COMMAND_SETTLE: Duration = Duration::from_millis(900);

/// One device the plugin is managing.
#[derive(Debug, Clone)]
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

    // ── Live, written by the poll task ───────────────────────────────
    pub online: bool,
    pub snapshot: RokuSnapshot,
    pub playback_state: String,
    /// Last full state document published, for change detection. ECP
    /// answers identically between polls far more often than not, and
    /// republishing a retained topic every 10 s for every device is pure
    /// broker noise.
    pub last_published: Option<Value>,
    pub apps_refreshed_at: Option<Instant>,
}

impl DeviceEntry {
    fn summary(&self) -> Value {
        json!({
            "hc_id": self.hc_id,
            "host": self.host,
            "port": self.port,
            "url": self.client.base_url(),
            "name": self.name,
            "serial": self.serial,
            "configured": self.configured,
            "online": self.online,
            "state": self.playback_state,
        })
    }
}

pub type Devices = Arc<RwLock<HashMap<String, DeviceEntry>>>;

/// A management-protocol request handed from the SDK's synchronous
/// handler to the bridge, which owns the device map and the runtime.
pub struct MgmtRequest {
    pub cmd: Value,
    pub reply: std::sync::mpsc::SyncSender<Value>,
}

pub struct Bridge {
    cfg: RokuConfig,
    publisher: DevicePublisher,
    devices: Devices,
    state_writer: PluginStateWriter,
    /// serial → hc_id, seeded from the durable plugin-state document core
    /// keeps for us. This is what stops a discovered Roku from getting a
    /// new device id (and losing its rules, name and room) every time
    /// DHCP moves it or the plugin restarts.
    known_ids: Arc<RwLock<HashMap<String, String>>>,
    /// Whether this plugin has anything to control, said on the plugin page.
    /// Re-evaluated from the live device map rather than decided once at
    /// startup — devices arrive from discovery minutes later, and a notice is
    /// current state, not a record of how things looked at boot.
    notices: PluginNotices,
}

impl Bridge {
    pub fn new(
        cfg: RokuConfig,
        publisher: DevicePublisher,
        state_writer: PluginStateWriter,
        devices: Devices,
        notices: PluginNotices,
    ) -> Self {
        Self {
            cfg,
            publisher,
            devices,
            state_writer,
            known_ids: Arc::new(RwLock::new(HashMap::new())),
            notices,
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

    /// Apply the durable learned-state document core hands back on
    /// connect (`homecore/plugins/{id}/state`).
    pub async fn apply_learned_state(&self, doc: &Value) {
        let Some(map) = doc.get("devices").and_then(Value::as_object) else {
            return;
        };
        let mut known = self.known_ids.write().await;
        for (serial, entry) in map {
            if let Some(hc_id) = entry.get("hc_id").and_then(Value::as_str) {
                known.insert(serial.clone(), hc_id.to_string());
            }
        }
        debug!(count = known.len(), "Applied learned Roku identities");
    }

    /// Drive the bridge. Takes `Arc<Self>` because the poll tasks it
    /// spawns outlive any single call and need their own handle.
    pub async fn run(
        self: Arc<Self>,
        mut cmd_rx: mpsc::Receiver<(String, Value)>,
        mut mgmt_rx: mpsc::Receiver<MgmtRequest>,
    ) {
        let this = self;

        // Configured devices first: they carry pinned ids, so registering
        // them before discovery runs means a discovered device recognises
        // itself as already-known by host and doesn't mint a second id.
        for dev in &this.cfg.devices {
            this.add_configured(dev).await;
        }

        // With discovery off, the configured list is the complete
        // inventory, so anything left over from a previous run that is no
        // longer in the file can be retired. With discovery on it is not
        // complete — a device that is merely unplugged would be wrongly
        // retired — so cleanup is left to the explicit
        // `forget_stale_devices` action.
        if !this.cfg.roku.discovery_enabled {
            let live: HashSet<String> = this.cfg.devices.iter().map(|d| d.hc_id.clone()).collect();
            match this.publisher.reconcile_devices(live).await {
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

        if this.cfg.roku.discovery_enabled {
            let d = Arc::clone(&this);
            tokio::spawn(async move { d.discovery_loop().await });
        }

        // Covers the discovery-disabled case, where no sweep will ever run.
        this.refresh_device_notice().await;

        info!(
            configured = this.cfg.devices.len(),
            discovery = this.cfg.roku.discovery_enabled,
            "hc-roku bridge running"
        );

        loop {
            tokio::select! {
                Some((hc_id, cmd)) = cmd_rx.recv() => {
                    let b = Arc::clone(&this);
                    tokio::spawn(async move { b.handle_command(hc_id, cmd).await });
                }
                Some(req) = mgmt_rx.recv() => {
                    let b = Arc::clone(&this);
                    tokio::spawn(async move { b.handle_mgmt(req).await });
                }
                else => break,
            }
        }
        warn!("hc-roku bridge loop exited — all channels closed");
    }

    // ── Device registration ──────────────────────────────────────────

    async fn add_configured(self: &Arc<Self>, dev: &DeviceConfig) {
        let port = dev.port.unwrap_or(crate::ecp::DEFAULT_PORT);
        let poll = Duration::from_secs(
            dev.poll_interval_secs
                .unwrap_or(self.cfg.roku.poll_interval_secs)
                .max(1),
        );
        let client = match self.client_for(&dev.host, port) {
            Ok(c) => c,
            Err(e) => {
                error!(host = %dev.host, error = %e, "Could not build ECP client");
                return;
            }
        };
        let entry = DeviceEntry {
            hc_id: dev.hc_id.clone(),
            host: dev.host.clone(),
            port,
            name: dev.name.clone(),
            area: dev.area.clone(),
            serial: None,
            configured: true,
            poll_interval: poll,
            client,
            online: false,
            snapshot: RokuSnapshot::default(),
            playback_state: "unavailable".into(),
            last_published: None,
            apps_refreshed_at: None,
        };
        self.register(entry).await;
    }

    fn client_for(&self, host: &str, port: u16) -> Result<EcpClient> {
        EcpClient::new(
            host,
            port,
            Duration::from_secs(self.cfg.roku.request_timeout_secs.max(1)),
        )
    }

    /// Insert the device, register it with homeCore, and start its poll
    /// task. Idempotent: a device already present just has its address
    /// refreshed, which is how a DHCP move is absorbed.
    async fn register(self: &Arc<Self>, entry: DeviceEntry) {
        {
            let mut devs = self.devices.write().await;
            if let Some(existing) = devs.get_mut(&entry.hc_id) {
                if existing.host != entry.host || existing.port != entry.port {
                    info!(
                        hc_id = %entry.hc_id,
                        from = %existing.host, to = %entry.host,
                        "Roku address changed; updating",
                    );
                    existing.host = entry.host.clone();
                    existing.port = entry.port;
                    existing.client = entry.client.clone();
                }
                if entry.serial.is_some() && existing.serial.is_none() {
                    existing.serial = entry.serial.clone();
                }
                return;
            }
            devs.insert(entry.hc_id.clone(), entry.clone());
        }

        info!(hc_id = %entry.hc_id, host = %entry.host, name = %entry.name, "Registering Roku");

        // `area` is passed only on first registration and only when the
        // operator set it: room assignment belongs to homeCore's device
        // registry, and re-asserting a config value here would overwrite
        // what someone changed in the UI on every restart.
        if let Err(e) = self
            .publisher
            .register_device_full(
                &entry.hc_id,
                &entry.name,
                Some("media_player"),
                entry.area.as_deref(),
                None,
            )
            .await
        {
            warn!(hc_id = %entry.hc_id, error = %e, "Failed to register device");
        }
        // Attributes and actions travel together on the one retained schema
        // topic — what can be written, and what can be done.
        let schema = plugin_sdk_rs::device_actions::with_actions(
            &crate::schema::device_schema(),
            crate::actions::device_actions(),
        );
        if let Err(e) = self
            .publisher
            .register_device_schema_json(&entry.hc_id, &schema)
            .await
        {
            warn!(hc_id = %entry.hc_id, error = %e, "Failed to publish device schema");
        }
        if let Err(e) = self.publisher.subscribe_commands(&entry.hc_id).await {
            error!(hc_id = %entry.hc_id, error = %e, "Failed to subscribe to commands");
        }

        let bridge = Arc::clone(self);
        let hc_id = entry.hc_id.clone();
        tokio::spawn(async move { bridge.poll_loop(hc_id).await });
    }

    // ── Discovery ────────────────────────────────────────────────────

    async fn discovery_loop(self: Arc<Self>) {
        // The learned-identity map arrives over MQTT shortly after
        // connect. Sweeping before it lands would mint fresh ids for
        // devices we already know, so give it a moment — a one-off
        // second at startup against ids that must stay stable forever.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let interval = Duration::from_secs(self.cfg.roku.discovery_interval_secs.max(60));
        loop {
            self.discovery_sweep().await;
            tokio::time::sleep(interval).await;
        }
    }

    /// Candidate addresses for one sweep: everything SSDP answered with,
    /// plus every `manual_hosts` entry it didn't already cover.
    ///
    /// Split out from [`Self::discovery_sweep`] so the streaming
    /// discovery action can report each device as it resolves rather than
    /// after the whole sweep finishes.
    pub async fn ssdp_only_sweep(&self) -> Vec<SsdpHit> {
        let timeout = Duration::from_secs(self.cfg.roku.discovery_timeout_secs.clamp(1, 30));
        let mut hits = match discovery::ssdp_search(timeout, 3).await {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "SSDP sweep failed");
                Vec::new()
            }
        };

        // Manual hosts are additive, not an alternative: they exist for
        // devices multicast cannot reach, and a host that SSDP also found
        // must not be probed twice.
        for host in &self.cfg.roku.manual_hosts {
            if hits.iter().any(|h| &h.host == host) {
                continue;
            }
            hits.push(SsdpHit {
                host: host.clone(),
                port: crate::ecp::DEFAULT_PORT,
                serial: None,
                location: String::new(),
            });
        }
        hits
    }

    /// Devices already managed, as `(hc_id, host)`. The streaming
    /// discovery action reports these alongside what the sweep found:
    /// SSDP over Wi-Fi drops probes, and a sweep that misses a Roku the
    /// plugin is actively polling should not be presented as "nothing on
    /// the network".
    pub async fn managed_hosts(&self) -> Vec<(String, String)> {
        let devs = self.devices.read().await;
        let mut v: Vec<(String, String)> = devs
            .values()
            .map(|d| (d.hc_id.clone(), d.host.clone()))
            .collect();
        v.sort();
        v
    }

    /// One full sweep: find candidates, then register anything new.
    pub async fn discovery_sweep(self: &Arc<Self>) -> Vec<SsdpHit> {
        let hits = self.ssdp_only_sweep().await;
        debug!(count = hits.len(), "Discovery sweep complete");
        for hit in &hits {
            self.integrate_hit(hit.clone()).await;
        }
        // A sweep is exactly when "nothing to control" can stop being true.
        self.refresh_device_notice().await;
        hits
    }

    /// Turn an SSDP hit into a registered device, if it isn't one already.
    ///
    /// Returns the resolved hc_id when the device is (now) managed.
    pub async fn integrate_hit(self: &Arc<Self>, hit: SsdpHit) -> Option<String> {
        // Already managed at this address? Nothing to do — the poll task
        // owns it from here.
        let at_this_address = {
            let devs = self.devices.read().await;
            devs.values()
                .find(|d| d.host == hit.host)
                .map(|d| d.hc_id.clone())
        };
        if let Some(id) = at_this_address {
            return Some(id);
        }

        // Probe before deciding anything: the serial and the friendly
        // name both come from ECP, and a device that doesn't answer isn't
        // worth registering.
        let client = self.client_for(&hit.host, hit.port).ok()?;
        let info = match client.device_info().await {
            Ok(i) => i,
            Err(e) => {
                debug!(host = %hit.host, error = %e, "Discovered host did not answer ECP");
                return None;
            }
        };
        let serial = info
            .serial()
            .map(str::to_string)
            .or_else(|| hit.serial.clone());

        // Serial-first matching: a device we already manage keeps its id
        // even though its address changed. This is the whole reason the
        // serial is chased down before anything else happens.
        let known_id = match &serial {
            Some(s) => self.known_ids.read().await.get(s).cloned(),
            None => None,
        };
        if let Some(hc_id) = &known_id {
            let existing = self.devices.read().await.get(hc_id).cloned();
            if let Some(mut updated) = existing {
                updated.host = hit.host.clone();
                updated.port = hit.port;
                updated.client = client;
                updated.serial = serial.clone();
                let id = updated.hc_id.clone();
                self.register(updated).await;
                return Some(id);
            }
        }

        if !self.cfg.roku.auto_add_discovered {
            debug!(host = %hit.host, "Discovered a Roku but auto-add is off");
            return None;
        }

        // A known id with no live entry means the device was registered
        // in an earlier run: reuse the id rather than minting a second
        // one for the same hardware.
        let hc_id = known_id.unwrap_or_else(|| mint_device_id(serial.as_deref(), &hit.host));
        let name = info
            .display_name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("Roku {hc_id}"));

        let entry = DeviceEntry {
            hc_id: hc_id.clone(),
            host: hit.host.clone(),
            port: hit.port,
            name,
            // Discovered devices get no area: homeCore owns room
            // assignment and guessing one would fight the operator.
            area: None,
            serial: serial.clone(),
            configured: false,
            poll_interval: Duration::from_secs(self.cfg.roku.poll_interval_secs.max(1)),
            client,
            online: true,
            snapshot: RokuSnapshot::default(),
            playback_state: "unavailable".into(),
            last_published: None,
            apps_refreshed_at: None,
        };
        if !info.ecp_control_enabled() {
            warn!(
                host = %hit.host,
                mode = ?info.get("ecp-setting-mode"),
                "Discovered Roku has \"Control by mobile apps\" disabled; it will report \
                 state but reject every command until that setting is changed on the device",
            );
        }
        info!(hc_id = %hc_id, host = %hit.host, serial = ?serial, "Discovered new Roku");
        self.register(entry).await;

        // Remember the identity so the id survives the next address
        // change and the next restart.
        if let Some(s) = serial {
            self.known_ids
                .write()
                .await
                .insert(s.clone(), hc_id.clone());
            self.persist_identity(&s, &hc_id, &hit.host).await;
        }
        Some(hc_id)
    }

    async fn persist_identity(&self, serial: &str, hc_id: &str, host: &str) {
        let delta = json!({
            "devices": { serial: { "hc_id": hc_id, "host": host } }
        });
        if let Err(e) = self.state_writer.persist(&delta).await {
            warn!(error = %e, "Failed to persist Roku identity");
        }
    }

    // ── Polling ──────────────────────────────────────────────────────

    async fn poll_loop(self: Arc<Self>, hc_id: String) {
        loop {
            let interval = self.poll_once(&hc_id, None).await;
            match interval {
                Some(d) => tokio::time::sleep(d).await,
                // Device gone from the map — its task retires with it.
                None => return,
            }
        }
    }

    /// One poll cycle. Returns how long to wait before the next, or
    /// `None` if the device has been removed.
    ///
    /// `cause` is the command that prompted this cycle, when there was
    /// one. It rides along as provenance on the published state so the UI
    /// can attribute the change to the command rather than to a
    /// background refresh — and passing it here rather than publishing a
    /// second time afterwards keeps it to one retained message.
    async fn poll_once(&self, hc_id: &str, cause: Option<&Value>) -> Option<Duration> {
        let (client, base_interval, want_apps, was_online, is_configured) = {
            let devs = self.devices.read().await;
            let d = devs.get(hc_id)?;
            let stale = d
                .apps_refreshed_at
                .map(|t| {
                    t.elapsed()
                        >= Duration::from_secs(self.cfg.roku.app_refresh_interval_secs.max(60))
                })
                .unwrap_or(true);
            (
                d.client.clone(),
                d.poll_interval,
                stale,
                d.online,
                d.configured,
            )
        };

        // device-info is the gate: it is the only query that reports the
        // power state, and it is the one that says whether the device is
        // reachable at all.
        let info = match client.device_info().await {
            Ok(i) => i,
            Err(e) => {
                if was_online {
                    warn!(hc_id, error = %e, "Roku unreachable");
                    let _ = self.publisher.publish_availability(hc_id, false).await;
                }
                {
                    let mut devs = self.devices.write().await;
                    // Gone from the map means the device was retired
                    // while this cycle was in flight; `?` ends the poll
                    // task along with it.
                    let d = devs.get_mut(hc_id)?;
                    d.online = false;
                    d.playback_state = "unavailable".into();
                }
                // Back off to the standby cadence while unreachable: a
                // powered-off TV is the common case, and hammering it at
                // the active interval buys nothing.
                return Some(Duration::from_secs(
                    self.cfg.roku.standby_poll_interval_secs.max(5),
                ));
            }
        };

        if !was_online {
            info!(hc_id, model = ?info.get("model-name"), "Roku online");
            let _ = self.publisher.publish_availability(hc_id, true).await;
            // Say this once, on the transition, rather than every cycle:
            // the device is about to look perfectly healthy while
            // rejecting every command, and the fix is a setting on the
            // Roku that nothing in homeCore can reach.
            if !info.ecp_control_enabled() {
                warn!(
                    hc_id,
                    mode = ?info.get("ecp-setting-mode"),
                    "Roku will report state but reject all control — set Settings → System → \
                     Advanced system settings → Control by mobile apps → Network access to \
                     \"Default\" or \"Permissive\" on the device",
                );
            }
        }

        let powered = info.is_powered_on();
        let is_tv = info.is_tv();
        let mut snap = RokuSnapshot {
            device_info: Some(info.clone()),
            ..Default::default()
        };

        // Everything below only has an answer while the device is awake.
        // In standby the app queries return the last-running app, which
        // would leave `source` reporting Netflix on a TV that has been
        // off for a week.
        if powered {
            match client.active_app().await {
                Ok(a) => snap.active = Some(a),
                Err(e) => debug!(hc_id, error = %e, "active-app query failed"),
            }
            match client.media_player().await {
                Ok(p) => snap.player = Some(p),
                // Pre-9.4 firmware has no media-player endpoint; the rest
                // of the state is still good, so this is not an error.
                Err(e) => debug!(hc_id, error = %e, "media-player query failed"),
            }
            if is_tv
                && snap
                    .active
                    .as_ref()
                    .is_some_and(|a| a.app.as_ref().is_some_and(|app| app.id == "tvinput.dtv"))
            {
                match client.tv_active_channel().await {
                    Ok(c) => snap.tv_channel = c,
                    Err(e) => debug!(hc_id, error = %e, "tv-active-channel query failed"),
                }
            }
        }

        // Catalogues: slow cadence, carried forward between cycles.
        let (mut apps, mut tv_channels) = {
            let devs = self.devices.read().await;
            let d = devs.get(hc_id)?;
            (d.snapshot.apps.clone(), d.snapshot.tv_channels.clone())
        };
        let mut refreshed_at = None;
        if powered && want_apps {
            match client.apps().await {
                Ok(a) => {
                    apps = a;
                    refreshed_at = Some(Instant::now());
                }
                Err(e) => debug!(hc_id, error = %e, "apps query failed"),
            }
            if is_tv {
                match client.tv_channels().await {
                    Ok(c) => tv_channels = c,
                    Err(e) => debug!(hc_id, error = %e, "tv-channels query failed"),
                }
            }
        }
        snap.apps = apps;
        snap.tv_channels = tv_channels;

        let doc = state::to_json(&snap);
        let playback = state::playback_state(&snap).to_string();

        let should_publish = {
            let mut devs = self.devices.write().await;
            let d = devs.get_mut(hc_id)?;
            d.online = true;
            d.snapshot = snap;
            d.playback_state = playback;
            if let Some(t) = refreshed_at {
                d.apps_refreshed_at = Some(t);
            }
            if d.serial.is_none() {
                d.serial = info.serial().map(str::to_string);
            }
            let changed = d.last_published.as_ref() != Some(&doc);
            if changed {
                d.last_published = Some(doc.clone());
            }
            changed
        };

        // A command always republishes, even when the resulting state is
        // byte-identical: the caller needs the acknowledgement, and a
        // command that legitimately changes nothing observable (a volume
        // key, say) would otherwise look like it was dropped.
        if should_publish || cause.is_some() {
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
        }

        // Learn the identity of a configured device the first time it
        // answers, so discovery can match it by serial later instead of
        // registering it a second time under a minted id.
        if is_configured {
            if let Some(serial) = info.serial() {
                let known = self.known_ids.read().await.contains_key(serial);
                if !known {
                    self.known_ids
                        .write()
                        .await
                        .insert(serial.to_string(), hc_id.to_string());
                }
            }
        }

        Some(if powered {
            base_interval
        } else {
            Duration::from_secs(self.cfg.roku.standby_poll_interval_secs.max(5))
        })
    }

    // ── Commands ─────────────────────────────────────────────────────

    async fn handle_command(&self, hc_id: String, cmd: Value) {
        let Some(entry) = self.devices.read().await.get(&hc_id).cloned() else {
            warn!(hc_id, "Command for unknown device");
            return;
        };

        let ctx = CommandContext {
            apps: &entry.snapshot.apps,
            device_info: entry.snapshot.device_info.as_ref(),
            playback_state: &entry.playback_state,
            reachable: entry.online,
            wake_on_lan: self.cfg.roku.wake_on_lan,
            default_hold: Duration::from_millis(self.cfg.roku.key_hold_ms),
            type_delay: Duration::from_millis(self.cfg.roku.type_delay_ms),
        };

        match commands::execute(&entry.client, &cmd, &ctx).await {
            Ok(result) => {
                debug!(hc_id, ?result, "Command executed");
                // Re-poll so homeCore reflects the change without waiting
                // for the next tick, tagged with the command that caused
                // it.
                tokio::time::sleep(POST_COMMAND_SETTLE).await;
                self.poll_once(&hc_id, Some(&cmd)).await;
            }
            Err(e) => warn!(hc_id, error = %e, cmd = ?cmd, "Command failed"),
        }
    }

    // ── Management actions ───────────────────────────────────────────

    async fn handle_mgmt(self: &Arc<Self>, req: MgmtRequest) {
        let action = req.cmd["action"].as_str().unwrap_or("").to_string();
        let result = self.run_mgmt_action(&action, &req.cmd).await;
        let payload = match result {
            Ok(mut v) => {
                if v.get("status").is_none() {
                    v["status"] = json!("ok");
                }
                v
            }
            Err(e) => json!({ "status": "error", "error": e.to_string() }),
        };
        // The requester may have given up (its recv timed out); a failed
        // send just means nobody is listening any more.
        let _ = req.reply.send(payload);
    }

    async fn run_mgmt_action(self: &Arc<Self>, action: &str, cmd: &Value) -> Result<Value> {
        match action {
            "list_devices" => {
                let devs = self.devices.read().await;
                let mut list: Vec<Value> = devs.values().map(DeviceEntry::summary).collect();
                list.sort_by(|a, b| a["hc_id"].as_str().cmp(&b["hc_id"].as_str()));
                Ok(json!({ "devices": list, "count": list.len() }))
            }

            "refresh_catalog" => {
                let targets: Vec<DeviceEntry> =
                    self.devices.read().await.values().cloned().collect();
                let mut per_device = serde_json::Map::new();
                for d in targets {
                    let apps = d.client.apps().await;
                    let channels = if d
                        .snapshot
                        .device_info
                        .as_ref()
                        .map(DeviceInfo::is_tv)
                        .unwrap_or(false)
                    {
                        d.client.tv_channels().await.ok()
                    } else {
                        None
                    };
                    match apps {
                        Ok(apps) => {
                            per_device.insert(
                                d.hc_id.clone(),
                                json!({
                                    "apps": apps.iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                                    "tv_channels": channels.as_ref().map(|c| {
                                        c.iter().map(|x| x.to_json()).collect::<Vec<_>>()
                                    }),
                                }),
                            );
                            let mut devs = self.devices.write().await;
                            if let Some(e) = devs.get_mut(&d.hc_id) {
                                e.snapshot.apps = apps;
                                if let Some(c) = channels {
                                    e.snapshot.tv_channels = c;
                                }
                                e.apps_refreshed_at = Some(Instant::now());
                                // Force a republish next cycle so the new
                                // catalogue reaches homeCore immediately.
                                e.last_published = None;
                            }
                        }
                        Err(e) => {
                            per_device.insert(d.hc_id.clone(), json!({ "error": e.to_string() }));
                        }
                    }
                }
                Ok(json!({ "devices": per_device }))
            }

            "device_info" => {
                let targets: Vec<DeviceEntry> =
                    self.devices.read().await.values().cloned().collect();
                let mut per_device = serde_json::Map::new();
                for d in targets {
                    match d.client.device_info().await {
                        Ok(info) => {
                            let mut m = serde_json::Map::new();
                            for (k, v) in &info.fields {
                                m.insert(k.replace('-', "_"), json!(v));
                            }
                            per_device.insert(d.hc_id.clone(), Value::Object(m));
                        }
                        Err(e) => {
                            per_device.insert(d.hc_id.clone(), json!({ "error": e.to_string() }));
                        }
                    }
                }
                Ok(json!({ "devices": per_device }))
            }

            // A device-scoped command issued through the management
            // channel, so callers get a synchronous result. The device
            // `cmd` topic is fire-and-forget and cannot report failure.
            "send_command" => {
                let hc_id = cmd["hc_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("send_command requires 'hc_id'"))?;
                let inner = cmd
                    .get("command")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("send_command requires 'command'"))?;
                let entry = self
                    .devices
                    .read()
                    .await
                    .get(hc_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown device '{hc_id}'"))?;
                let ctx = CommandContext {
                    apps: &entry.snapshot.apps,
                    device_info: entry.snapshot.device_info.as_ref(),
                    playback_state: &entry.playback_state,
                    reachable: entry.online,
                    wake_on_lan: self.cfg.roku.wake_on_lan,
                    default_hold: Duration::from_millis(self.cfg.roku.key_hold_ms),
                    type_delay: Duration::from_millis(self.cfg.roku.type_delay_ms),
                };
                let result = commands::execute(&entry.client, &inner, &ctx).await?;
                // Refresh in the background: the caller wants the command
                // acknowledged now, not after the settle delay.
                let bridge = Arc::clone(self);
                let hc_id = hc_id.to_string();
                let cmd = inner.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(POST_COMMAND_SETTLE).await;
                    bridge.poll_once(&hc_id, Some(&cmd)).await;
                });
                Ok(json!({ "result": result }))
            }

            "app_icon" => {
                let hc_id = cmd["hc_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("app_icon requires 'hc_id'"))?;
                let app_id = cmd["app_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("app_icon requires 'app_id'"))?;
                let entry = self
                    .devices
                    .read()
                    .await
                    .get(hc_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown device '{hc_id}'"))?;
                let (mime, bytes) = entry.client.icon(app_id).await?;
                // Returned as a data URI so a UI can render it without a
                // second round trip through a binary endpoint homeCore
                // does not have.
                Ok(json!({
                    "app_id": app_id,
                    "content_type": mime,
                    "data_uri": format!("data:{mime};base64,{}", base64(&bytes)),
                    "bytes": bytes.len(),
                }))
            }

            // Explicit cleanup: retire every registered device that does
            // not answer right now. Deliberately manual — doing it
            // automatically would delete a Roku that is merely unplugged.
            "forget_stale_devices" => {
                let targets: Vec<DeviceEntry> =
                    self.devices.read().await.values().cloned().collect();
                let mut live = HashSet::new();
                let mut retired = Vec::new();
                for d in targets {
                    if d.client.device_info().await.is_ok() {
                        live.insert(d.hc_id.clone());
                    } else if d.configured {
                        // Configured devices are the operator's stated
                        // intent; unreachable is not the same as unwanted.
                        live.insert(d.hc_id.clone());
                    } else {
                        retired.push(d.hc_id.clone());
                    }
                }
                for hc_id in &retired {
                    self.devices.write().await.remove(hc_id);
                }
                let report = self.publisher.reconcile_devices(live).await?;
                Ok(json!({
                    "retired": report.stale_unregistered,
                    "unreachable": retired,
                }))
            }

            other => anyhow::bail!("unknown action '{other}'"),
        }
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

/// Minimal base64 for the app-icon data URI. One caller, no padding
/// subtleties beyond the standard alphabet — not worth a dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_prefers_the_serial() {
        assert_eq!(
            mint_device_id(Some("P0A070000007"), "10.0.10.40"),
            "roku_p0a070000007"
        );
    }

    /// Without a serial the address is all we have, and it has to be
    /// sanitised — dots are not valid in a homeCore device id.
    #[test]
    fn device_id_falls_back_to_the_host() {
        assert_eq!(mint_device_id(None, "10.0.10.40"), "roku_10_0_10_40");
        assert_eq!(mint_device_id(Some(""), "10.0.10.40"), "roku_10_0_10_40");
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }
}
