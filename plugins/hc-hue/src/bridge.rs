use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::commands::{parse_homecore_command, PluginCommand};
use crate::config::HuePluginConfig;
use crate::hue::api::{EventstreamSignal, HueApiClient};
use crate::hue::models::{AccessoryCommand, BridgeTarget, LightCommand};
use crate::hue::registry::{
    HueRegistry, RegisteredAux, RegisteredGroup, RegisteredLight, RegisteredScene,
};
use crate::sync::{self, EventApplyOutcome, SyncConfig};
use crate::translator;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};

/// A refresh the event stream asks for waits at least this long after the
/// last one; one asked for meanwhile runs when it is up, not never.
const FALLBACK_REFRESH_COOLDOWN_SECS: u64 = 3;

/// Prefix shared by every device_id this plugin publishes
/// (`hue_{bridge_id}_{kind}_{rid}` — see `BridgeTarget` in `hue::models`).
const HUE_DEVICE_ID_PREFIX: &str = "hue_";

/// Request sent by the management `refresh_devices` (or
/// `cleanup_stale_devices`) action. Carries an optional progress
/// sink so the streaming action can forward bridge-by-bridge events
/// to the operator while the refresh is in flight.
pub struct RefreshRequest {
    pub progress: Option<mpsc::Sender<RefreshEvent>>,
    /// Republish every device's registration and schema, not only the ones
    /// this process has not seen before.
    ///
    /// True when a person asked. *Refresh devices* is what somebody presses
    /// when a device looks wrong, and until this existed it could not restore
    /// a schema core had lost — only restarting the plugin could, because that
    /// is what emptied the in-memory registry. False for the periodic loop,
    /// where republishing everything on every tick would be pure churn and,
    /// since `device_schema_changed` exists, an event per device per tick.
    pub force_republish: bool,
}

/// Request sent by the management `unpair_bridge` action to forget one bridge:
/// drop it from the runtime, unregister every device it owns, and report back
/// how many were removed. The runtime replies exactly once through `resp`.
pub struct UnpairRequest {
    pub bridge_id: String,
    pub resp: tokio::sync::oneshot::Sender<UnpairOutcome>,
}

/// The result of an [`UnpairRequest`], sent back to the streaming action.
pub struct UnpairOutcome {
    /// Whether a matching runtime bridge was found and dropped (false if it was
    /// only in config/learned state, or already gone).
    pub matched: bool,
    /// Number of homeCore devices unregistered as a result.
    pub devices_removed: usize,
}

/// Per-bridge progress emitted during a refresh. The streaming
/// action turns these into `ctx.progress` / `ctx.item_add` calls;
/// nothing else listens.
#[derive(Debug, Clone)]
pub enum RefreshEvent {
    /// Emitted once before any `BridgeStart` so the streaming action
    /// knows how many bridges to expect — needed to compute progress
    /// percentages while the walk is in flight.
    AllStart {
        total_bridges: usize,
    },
    BridgeStart {
        bridge_id: String,
        name: String,
    },
    BridgeDone {
        bridge_id: String,
        name: String,
        elapsed_ms: u64,
        ok: bool,
        error: Option<String>,
    },
    AllDone {
        bridges: usize,
        ok: usize,
        failed: usize,
    },
}

#[derive(Default)]
struct Counter {
    total: u64,
    recent: u64,
    by_bridge: HashMap<String, u64>,
    recent_by_bridge: HashMap<String, u64>,
    by_reason: HashMap<String, u64>,
    recent_by_reason: HashMap<String, u64>,
}

impl Counter {
    fn record(&mut self, bridge_id: &str, reason: &str) {
        self.total += 1;
        self.recent += 1;
        *self.by_bridge.entry(bridge_id.to_string()).or_default() += 1;
        *self
            .recent_by_bridge
            .entry(bridge_id.to_string())
            .or_default() += 1;
        *self.by_reason.entry(reason.to_string()).or_default() += 1;
        *self.recent_by_reason.entry(reason.to_string()).or_default() += 1;
    }

    fn reset_recent(&mut self) {
        self.recent = 0;
        self.recent_by_bridge.clear();
        self.recent_by_reason.clear();
    }
}

#[derive(Default)]
struct EventstreamMetrics {
    refresh_signal: Counter,
    incremental_applied: Counter,
    fallback_refresh: Counter,
    refresh_ignored: Counter,
    fallback_refresh_skipped: Counter,
    last_fallback_by_bridge: HashMap<String, Instant>,
}

/// The device a command is for.
enum Target {
    Bridge,
    Light(RegisteredLight),
    Group(RegisteredGroup),
    Scene(RegisteredScene),
    Aux(RegisteredAux),
}

pub struct Bridge {
    cfg: HuePluginConfig,
    publisher: DevicePublisher,
    sync_cfg: SyncConfig,
    apis: Vec<HueApiClient>,
    registry: HueRegistry,
    command_started_at: Option<Instant>,
    metrics: EventstreamMetrics,
    /// Core's durable learned-state view + writer, for persisting app_keys
    /// discovered by the device-command-triggered pairing path (D8) — the same
    /// channel the streaming `pair_bridge` action uses.
    learned_state: crate::pairing::LearnedState,
    state_writer: plugin_sdk_rs::PluginStateWriter,
    /// What the operator sees when a bridge stops answering. A Hue bridge that
    /// goes away leaves its lights registered but frozen, and the plugin keeps
    /// reading "active" — the log line alone scrolls past.
    notices: PluginNotices,
    /// Bridge id → whether its event stream is up. While it is, the bridge
    /// reports every change itself and nothing needs reading back.
    live: HashMap<String, bool>,
    /// Paired bridges go to the runtime through here, from the device
    /// command's pairing as from the actions'.
    new_bridge_tx: mpsc::Sender<BridgeTarget>,
    /// Bridges whose event stream asked for a refresh while one was too
    /// recent: they are read once the cooldown is up.
    refresh_owed: std::collections::HashSet<String>,
}

impl Bridge {
    pub fn new(
        cfg: HuePluginConfig,
        bridges: Vec<BridgeTarget>,
        publisher: DevicePublisher,
        learned_state: crate::pairing::LearnedState,
        state_writer: plugin_sdk_rs::PluginStateWriter,
        notices: PluginNotices,
        new_bridge_tx: mpsc::Sender<BridgeTarget>,
    ) -> Self {
        let apis = bridges.into_iter().map(HueApiClient::new).collect();
        let sync_cfg = SyncConfig {
            compact_motion_facets: cfg.hue.compact_motion_facets,
            publish_grouped_lights: cfg.hue.publish_grouped_lights,
            publish_grouped_lights_for: cfg.hue.publish_grouped_lights_for.clone(),
            skip_grouped_lights_for: cfg.hue.skip_grouped_lights_for.clone(),
            publish_bridge_home: cfg.hue.publish_bridge_home,
            publish_entertainment_configurations: cfg.hue.publish_entertainment_configurations,
        };
        Self {
            cfg,
            publisher,
            sync_cfg,
            apis,
            registry: HueRegistry::default(),
            command_started_at: None,
            metrics: EventstreamMetrics::default(),
            learned_state,
            state_writer,
            notices,
            live: HashMap::new(),
            new_bridge_tx,
            refresh_owed: Default::default(),
        }
    }

    /// Reconcile the SDK's persisted device_id snapshot against the
    /// in-memory registry and unregister anything that's gone stale
    /// (registered before but not currently live). Skipped when
    /// `all_bridges_succeeded` is false — partial failures could
    /// otherwise wipe live devices belonging to a temporarily
    /// unreachable bridge.
    async fn reconcile_published_ids(&self, all_bridges_succeeded: bool) {
        if !all_bridges_succeeded {
            // Skipping is the safe choice, but a bridge that fails every sync
            // means stale devices are never cleaned up and nothing says so.
            warn!(
                "Skipping cross-restart reconcile — not all bridges responded. \
                 Devices dropped from config stay registered in homeCore until a \
                 sync in which every bridge succeeds."
            );
            return;
        }
        let live = self.registry.all_device_ids();
        match self.publisher.reconcile_devices(live).await {
            Ok(report) => {
                if !report.stale_unregistered.is_empty() {
                    info!(
                        count = report.stale_unregistered.len(),
                        devices = ?report.stale_unregistered,
                        "Cleaned up stale Hue devices via SDK reconcile"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "SDK reconcile_devices failed");
            }
        }
    }

    /// Read one bridge and publish what it has. A bridge that does not
    /// answer is said so — as a notice on the plugin page and an alert on the
    /// bridge — and both clear when it answers again, on whatever refresh
    /// that is: startup, the periodic resync, or one a person asked for.
    async fn refresh(&mut self, api: &HueApiClient) -> Result<()> {
        // Borrow individual fields to avoid borrowing all of `self` — callers
        // often hold an `api` reference from `self.apis` at the same time.
        let result =
            sync::refresh_bridge_state(&self.publisher, &mut self.registry, api, &self.sync_cfg)
                .await;
        let target = api.target();
        let notice = format!(
            "bridge_unreachable_{}",
            target.bridge_id.to_ascii_lowercase()
        );
        match &result {
            Ok(()) => {
                // The refresh says the bridge's whole state; whether its event
                // stream is up is the runtime's to add.
                let live = self.live.get(&target.bridge_id).copied().unwrap_or(false);
                let _ = self
                    .publisher
                    .publish_state_partial(&target.device_id(), &json!({ "live": live }))
                    .await;
                self.notices.clear(&notice);
                // The notice used to carry no bridge in its id.
                self.notices.clear("bridge_unreachable");
            }
            Err(e) => {
                warn!(bridge = %target.bridge_id, error = %e, "Hue bridge refresh failed");
                self.notices.raise(
                    PluginNotice::error(
                        notice,
                        format!(
                            "Cannot reach the Hue bridge {} at {} — {e}. Its lights stay \
                             listed but will not respond or report state.",
                            target.name, target.host
                        ),
                    )
                    .with_remedy(
                        "Check the bridge is powered and on the network. If its address has \
                         changed, correct it on this page; if it was factory reset, add it \
                         again — the stored app key will no longer be accepted.",
                    ),
                );
            }
        }
        if api.has_app_key() {
            let update = self
                .registry
                .bridge_update_state(&target.device_id())
                .map(str::to_string);
            self.registry
                .set_alerts_if_changed(
                    &self.publisher,
                    &target.device_id(),
                    crate::alerts::for_bridge(result.is_ok(), &target.host, update.as_deref()),
                )
                .await;
        }
        crate::ui_actions::set_rooms(self.registry.rooms());
        result
    }

    pub async fn run(
        mut self,
        mut hc_rx: mpsc::Receiver<(String, Value)>,
        mut refresh_rx: mpsc::Receiver<RefreshRequest>,
        mut new_bridge_rx: mpsc::Receiver<crate::hue::models::BridgeTarget>,
        mut unpair_rx: mpsc::Receiver<UnpairRequest>,
    ) -> Result<()> {
        info!(bridges = self.apis.len(), "Hue bridge runtime started");

        let (event_tx, mut event_rx) = mpsc::channel::<EventstreamSignal>(128);

        if self.cfg.hue.eventstream_enabled {
            for api in &self.apis {
                if !api.has_app_key() {
                    continue;
                }
                let api = api.clone();
                let tx = event_tx.clone();
                let reconnect_secs = self.cfg.hue.eventstream_reconnect_secs;
                tokio::spawn(async move {
                    let _ = api.run_eventstream(tx, reconnect_secs).await;
                });
            }
        }

        let mut all_bridges_succeeded = true;
        for api in self.apis.clone() {
            if let Err(e) = self.refresh(&api).await {
                warn!(bridge = %api.target().bridge_id, error = %e, "Initial bridge refresh failed");
                all_bridges_succeeded = false;
            }
        }
        // Cross-restart cleanup runs once after the startup sync. If a
        // bridge was unreachable we hold off — better to leave a few
        // ghosts in homeCore than risk wiping legit devices that just
        // happened to be temporarily offline.
        self.reconcile_published_ids(all_bridges_succeeded).await;

        let mut resync_tick = tokio::time::interval(Duration::from_secs(
            self.cfg.hue.resync_interval_secs.max(5),
        ));
        let mut heartbeat_tick =
            tokio::time::interval(Duration::from_secs(self.cfg.hue.heartbeat_secs.max(5)));
        let mut owed_tick =
            tokio::time::interval(Duration::from_secs(FALLBACK_REFRESH_COOLDOWN_SECS));

        loop {
            tokio::select! {
                msg = hc_rx.recv() => {
                    let Some((device_id, payload)) = msg else {
                        info!("HomeCore command channel closed; stopping hc-hue runtime");
                        break;
                    };
                    if let Err(e) = self.handle_homecore_command(&device_id, payload).await {
                        warn!(device_id, error = %e, "Command handling failed");
                    }
                }
                signal_msg = event_rx.recv() => {
                    let Some(signal) = signal_msg else {
                        continue;
                    };
                    match signal {
                        EventstreamSignal::Refresh { bridge_id, reason } => {
                            self.metrics.refresh_signal.record(&bridge_id, &reason);
                            if let Some(api) = self
                                .apis
                                .iter()
                                .find(|a| a.target().bridge_id == bridge_id)
                                .cloned()
                            {
                                self.refresh_bridge_with_reason(
                                    &bridge_id,
                                    &reason,
                                    &api,
                                ).await?;
                            }
                        }
                        EventstreamSignal::Live { bridge_id, live } => {
                            let was = self.live.insert(bridge_id.clone(), live).unwrap_or(false);
                            if let Some(api) = self.apis.iter().find(|a| a.target().bridge_id == bridge_id).cloned() {
                                let device_id = api.target().device_id();
                                let _ = self
                                    .publisher
                                    .publish_state_partial(&device_id, &json!({ "live": live }))
                                    .await;
                                // Back after a gap: whatever changed meanwhile
                                // was said to nobody, so read it.
                                if live && !was {
                                    if let Err(e) = self.refresh(&api).await {
                                        warn!(bridge = %bridge_id, error = %e, "Refresh after the event stream came back failed");
                                    }
                                }
                            }
                        }
                        EventstreamSignal::Data { bridge_id, payload } => {
                            if let Some(api) = self
                                .apis
                                .iter()
                                .find(|a| a.target().bridge_id == bridge_id)
                                .cloned()
                            {
                                match sync::apply_eventstream_update(
                                    &self.publisher,
                                    &self.registry,
                                    &bridge_id,
                                    &payload,
                                ).await? {
                                    EventApplyOutcome::Applied => {
                                        self.metrics.incremental_applied.record(&bridge_id, "applied");
                                    }
                                    EventApplyOutcome::Ignored { reason } => {
                                        self.metrics.refresh_ignored.record("", &reason);
                                    }
                                    EventApplyOutcome::NeedsRefresh { reason } => {
                                        if self.should_run_fallback_refresh(&bridge_id) {
                                            self.metrics.fallback_refresh.record(&bridge_id, &reason);
                                            self.refresh_bridge_with_reason(&bridge_id, &reason, &api).await?;
                                        } else {
                                            self.metrics.fallback_refresh_skipped.record(&bridge_id, &reason);
                                            self.refresh_owed.insert(bridge_id.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ = resync_tick.tick() => {
                    let mut all_ok = true;
                    for api in self.apis.clone() {
                        if let Err(e) = self.refresh(&api).await {
                            warn!(bridge = %api.target().bridge_id, error = %e, "Resync refresh failed");
                            all_ok = false;
                        }
                    }
                    self.reconcile_published_ids(all_ok).await;
                }
                sig = refresh_rx.recv() => {
                    match sig {
                        Some(req) => {
                            info!("Manual refresh requested via manifest action");
                            let progress = req.progress;
                            if req.force_republish {
                                // A person asked, so say everything again —
                                // see `forget_publications`.
                                self.registry.forget_publications();
                            }
                            let mut ok_count = 0usize;
                            let mut failed_count = 0usize;
                            let total = self.apis.len();
                            if let Some(p) = &progress {
                                let _ = p
                                    .send(RefreshEvent::AllStart {
                                        total_bridges: total,
                                    })
                                    .await;
                            }
                            for api in self.apis.clone() {
                                let target = api.target().clone();
                                if let Some(p) = &progress {
                                    let _ = p
                                        .send(RefreshEvent::BridgeStart {
                                            bridge_id: target.bridge_id.clone(),
                                            name: target.name.clone(),
                                        })
                                        .await;
                                }
                                let started = Instant::now();
                                let result = self.refresh(&api).await;
                                let elapsed_ms = started.elapsed().as_millis() as u64;
                                let (ok, error) = match &result {
                                    Ok(()) => (true, None),
                                    Err(e) => (false, Some(e.to_string())),
                                };
                                if ok { ok_count += 1; } else { failed_count += 1; }
                                if let Err(e) = result {
                                    warn!(bridge = %target.bridge_id, error = %e, "Manual refresh failed for bridge");
                                }
                                if let Some(p) = &progress {
                                    let _ = p
                                        .send(RefreshEvent::BridgeDone {
                                            bridge_id: target.bridge_id,
                                            name: target.name,
                                            elapsed_ms,
                                            ok,
                                            error,
                                        })
                                        .await;
                                }
                            }
                            self.reconcile_published_ids(failed_count == 0).await;
                            if let Some(p) = progress {
                                let _ = p
                                    .send(RefreshEvent::AllDone {
                                        bridges: total,
                                        ok: ok_count,
                                        failed: failed_count,
                                    })
                                    .await;
                            }
                        }
                        None => {
                            // Sender dropped — fine, keep the loop alive.
                        }
                    }
                }
                new = new_bridge_rx.recv() => {
                    match new {
                        Some(target) => {
                            // Dedup: re-pairing an already-running bridge (possibly
                            // with a different-case bridge_id, or targeted by host)
                            // must not add a second runtime bridge / device.
                            // Distinct new bridges (no match by id or host) are
                            // still added — multi-bridge.
                            let running = self.apis.iter().position(|a| {
                                let t = a.target();
                                crate::config::same_bridge(
                                    &t.bridge_id, &t.host, &target.bridge_id, &target.host,
                                )
                            });
                            if let Some(idx) = running {
                                // "Already running" does not mean "already working":
                                // discovery registers every bridge it finds, including
                                // unpaired ones (auth_required, no app_key). That is
                                // the normal case for a first pairing, so the existing
                                // entry must ADOPT the new key — dropping it here left
                                // the bridge unauthorized with no devices until the
                                // plugin was restarted, which looked like pairing had
                                // silently failed.
                                let api = self.apis[idx].clone();
                                let was_authorized = api.has_app_key();
                                if let Some(app_key) = target.app_key.clone() {
                                    api.set_app_key(app_key);
                                }
                                info!(
                                    bridge_id = %target.bridge_id,
                                    host = %target.host,
                                    newly_authorized = !was_authorized && api.has_app_key(),
                                    "Re-paired an already-running bridge; adopted the new app key"
                                );
                                // Going unauthorized → authorized is the moment this
                                // bridge becomes usable: start its eventstream and pull
                                // its devices now rather than waiting for the next
                                // resync tick.
                                if !was_authorized && api.has_app_key() {
                                    if self.cfg.hue.eventstream_enabled {
                                        let api_for_es = api.clone();
                                        let tx = event_tx.clone();
                                        let reconnect_secs =
                                            self.cfg.hue.eventstream_reconnect_secs;
                                        tokio::spawn(async move {
                                            let _ = api_for_es
                                                .run_eventstream(tx, reconnect_secs)
                                                .await;
                                        });
                                    }
                                    if let Err(e) = self.refresh(&api).await {
                                        warn!(
                                            error = %e,
                                            "Initial refresh of newly-authorized bridge failed"
                                        );
                                    }
                                }
                            } else {
                                info!(
                                    bridge_id = %target.bridge_id,
                                    host = %target.host,
                                    "New bridge added at runtime via pair action"
                                );
                                let api = HueApiClient::new(target.clone());
                                self.apis.push(api.clone());
                                crate::register_bridge_device(&self.publisher, api.target()).await;
                                // Spawn eventstream task for the new bridge if enabled.
                                if self.cfg.hue.eventstream_enabled && api.has_app_key() {
                                    let api_for_es = api.clone();
                                    let tx = event_tx.clone();
                                    let reconnect_secs = self.cfg.hue.eventstream_reconnect_secs;
                                    tokio::spawn(async move {
                                        let _ = api_for_es.run_eventstream(tx, reconnect_secs).await;
                                    });
                                }
                                if let Err(e) = self.refresh(&api).await {
                                    warn!(error = %e, "Initial refresh of newly-paired bridge failed");
                                }
                            }
                        }
                        None => {
                            // Sender dropped — fine.
                        }
                    }
                }
                req = unpair_rx.recv() => {
                    if let Some(req) = req {
                        let requested = req.bridge_id;
                        // Find the runtime bridge (by id or host, case-insensitive)
                        // and drop it so it stops syncing + eventstreaming. Its
                        // spawned eventstream task keeps its own api clone until the
                        // config-PUT restart that follows an unpair tears it down.
                        let idx = self.apis.iter().position(|a| {
                            let t = a.target();
                            crate::config::same_bridge(&t.bridge_id, &t.host, &requested, "")
                        });
                        let target = match idx {
                            Some(i) => self.apis.remove(i).target().clone(),
                            // Not running (config-only, or already gone). Build a
                            // target from the requested id so we can still compute
                            // its device id and clear any lingering registry rows.
                            None => crate::hue::models::BridgeTarget {
                                name: String::new(),
                                bridge_id: requested.clone(),
                                host: String::new(),
                                app_key: None,
                                verify_tls: true,
                                allow_self_signed: true,
                            },
                        };
                        let bridge_device_id = target.device_id();
                        let removed = self
                            .registry
                            .remove_all_for_bridge(&target.bridge_id, &bridge_device_id);
                        let devices_removed = removed.len();
                        info!(
                            bridge_id = %target.bridge_id,
                            matched = idx.is_some(),
                            devices_removed,
                            "Unpair: dropped bridge and removed its devices"
                        );
                        // Reconcile so the SDK unregisters the now-absent devices
                        // from homeCore and persists the smaller published set.
                        self.reconcile_published_ids(true).await;
                        let _ = req.resp.send(UnpairOutcome {
                            matched: idx.is_some(),
                            devices_removed,
                        });
                    }
                    // Sender dropped → this arm just won't fire again.
                }
                _ = owed_tick.tick(), if !self.refresh_owed.is_empty() => {
                    for bridge_id in self.refresh_owed.clone() {
                        if !self.should_run_fallback_refresh(&bridge_id) {
                            continue;
                        }
                        self.refresh_owed.remove(&bridge_id);
                        if let Some(api) = self.apis.iter().find(|a| a.target().bridge_id == bridge_id).cloned() {
                            self.refresh_bridge_with_reason(&bridge_id, "owed", &api).await?;
                        }
                    }
                }
                _ = heartbeat_tick.tick() => {
                    self.publisher.publish_plugin_status("active").await?;
                    let m = &self.metrics;
                    let fallback_ratio = Self::fallback_ratio_pct(
                        m.incremental_applied.total,
                        m.fallback_refresh.total,
                    );
                    let recent_fallback_ratio = Self::fallback_ratio_pct(
                        m.incremental_applied.recent,
                        m.fallback_refresh.recent,
                    );
                    debug!(
                        bridges_total = self.registry.bridge_count(),
                        bridges_online = self.registry.online_count(),
                        lights_total = self.registry.light_count(),
                        groups_total = self.registry.group_count(),
                        scenes_total = self.registry.scene_count(),
                        aux_total = self.registry.aux_count(),
                        eventstream_enabled = self.cfg.hue.eventstream_enabled,
                        eventstream_refresh_signal_total = m.refresh_signal.total,
                        eventstream_incremental_applied_total = m.incremental_applied.total,
                        eventstream_incremental_applied_bridges = m.incremental_applied.by_bridge.len(),
                        eventstream_fallback_refresh_total = m.fallback_refresh.total,
                        eventstream_fallback_ratio_pct = fallback_ratio,
                        eventstream_fallback_refresh_bridges = m.fallback_refresh.by_bridge.len(),
                        eventstream_refresh_ignored_total = m.refresh_ignored.total,
                        eventstream_fallback_refresh_skipped_total = m.fallback_refresh_skipped.total,
                        eventstream_refresh_signal_recent = m.refresh_signal.recent,
                        eventstream_incremental_applied_recent = m.incremental_applied.recent,
                        eventstream_incremental_applied_recent_bridges = m.incremental_applied.recent_by_bridge.len(),
                        eventstream_fallback_refresh_recent = m.fallback_refresh.recent,
                        eventstream_fallback_refresh_recent_bridges = m.fallback_refresh.recent_by_bridge.len(),
                        eventstream_refresh_ignored_recent = m.refresh_ignored.recent,
                        eventstream_fallback_refresh_skipped_recent = m.fallback_refresh_skipped.recent,
                        eventstream_fallback_ratio_recent_pct = recent_fallback_ratio,
                        state_bytes = self.registry.total_summary_bytes(),
                        "Hue plugin heartbeat"
                    );

                    let metrics = json!({
                        "plugin_id": self.publisher.plugin_id(),
                        "eventstream_enabled": self.cfg.hue.eventstream_enabled,
                        "eventstream_refresh_signal_total": m.refresh_signal.total,
                        "eventstream_incremental_applied_total": m.incremental_applied.total,
                        "eventstream_incremental_applied_by_bridge": m.incremental_applied.by_bridge,
                        "eventstream_fallback_refresh_total": m.fallback_refresh.total,
                        "eventstream_fallback_ratio_pct": fallback_ratio,
                        "eventstream_fallback_refresh_by_bridge": m.fallback_refresh.by_bridge,
                        "eventstream_fallback_refresh_reason_total": m.fallback_refresh.by_reason,
                        "eventstream_refresh_ignored_total": m.refresh_ignored.total,
                        "eventstream_refresh_ignored_reason_total": m.refresh_ignored.by_reason,
                        "eventstream_refresh_signal_reason_total": m.refresh_signal.by_reason,
                        "eventstream_fallback_refresh_skipped_total": m.fallback_refresh_skipped.total,
                        "eventstream_fallback_refresh_skipped_by_bridge": m.fallback_refresh_skipped.by_bridge,
                        "eventstream_fallback_refresh_skipped_reason_total": m.fallback_refresh_skipped.by_reason,
                        "eventstream_refresh_signal_recent": m.refresh_signal.recent,
                        "eventstream_incremental_applied_recent": m.incremental_applied.recent,
                        "eventstream_incremental_applied_recent_by_bridge": m.incremental_applied.recent_by_bridge,
                        "eventstream_fallback_refresh_recent": m.fallback_refresh.recent,
                        "eventstream_fallback_refresh_recent_by_bridge": m.fallback_refresh.recent_by_bridge,
                        "eventstream_fallback_refresh_reason_recent": m.fallback_refresh.recent_by_reason,
                        "eventstream_refresh_ignored_recent": m.refresh_ignored.recent,
                        "eventstream_refresh_ignored_reason_recent": m.refresh_ignored.recent_by_reason,
                        "eventstream_refresh_signal_reason_recent": m.refresh_signal.recent_by_reason,
                        "eventstream_fallback_refresh_skipped_recent": m.fallback_refresh_skipped.recent,
                        "eventstream_fallback_refresh_skipped_recent_by_bridge": m.fallback_refresh_skipped.recent_by_bridge,
                        "eventstream_fallback_refresh_skipped_reason_recent": m.fallback_refresh_skipped.recent_by_reason,
                        "eventstream_fallback_ratio_recent_pct": recent_fallback_ratio,
                    });
                    if let Err(e) = self.publisher.publish_event("plugin_metrics", &metrics).await {
                        warn!(error = %e, "Failed to publish plugin_metrics event");
                    }

                    self.metrics.refresh_signal.reset_recent();
                    self.metrics.incremental_applied.reset_recent();
                    self.metrics.fallback_refresh.reset_recent();
                    self.metrics.refresh_ignored.reset_recent();
                    self.metrics.fallback_refresh_skipped.reset_recent();
                }
            }
        }

        self.publisher.publish_plugin_status("offline").await?;
        Ok(())
    }

    async fn handle_homecore_command(&mut self, device_id: &str, payload: Value) -> Result<()> {
        let cmd = parse_homecore_command(payload);
        self.command_started_at = Some(Instant::now());

        let Some((target, api)) = self.target_of(device_id) else {
            self.report_unrouted(device_id).await;
            return Ok(());
        };
        let bridge_id = api.target().bridge_id.clone();

        // Commands every kind of device takes.
        match &cmd {
            PluginCommand::Raw(raw) => {
                let r = api.execute_raw_command(raw).await;
                return self.finish(device_id, "raw", r, None).await;
            }
            PluginCommand::SetAvailability(online) => {
                let r = self
                    .publisher
                    .publish_availability(device_id, *online)
                    .await;
                return self.finish(device_id, "set_availability", r, None).await;
            }
            PluginCommand::Refresh => {
                let r = self.refresh(&api).await;
                self.observe(device_id, "refresh", &r).await;
                return Ok(());
            }
            _ => {}
        }

        let (operation, result) = match (&target, cmd) {
            (Target::Bridge, PluginCommand::PairBridge) => {
                self.spawn_device_pairing(device_id, &api).await;
                return Ok(());
            }
            (Target::Light(binding), PluginCommand::SetLightState(c)) => (
                "set_light_state",
                match Self::validate_light_command(binding, &c) {
                    Ok(()) => api.execute_light_command(&binding.light_rid, &c).await,
                    Err(e) => Err(e),
                },
            ),
            (Target::Group(binding), PluginCommand::SetLightState(c)) => {
                let r = match Self::validate_grouped_light_command(&c) {
                    Ok(()) => {
                        api.execute_grouped_light_command(&binding.group_rid, &c)
                            .await
                    }
                    Err(e) => Err(e),
                };
                if r.is_ok() {
                    self.emit_group_action_event(
                        device_id,
                        &binding.group_rid,
                        "group_device_command",
                    )
                    .await;
                }
                ("set_group_state", r)
            }
            (
                Target::Light(_) | Target::Group(_) | Target::Scene(_),
                PluginCommand::ActivateScene { scene_id, recall },
            ) => {
                let scene_rid = match (&target, scene_id) {
                    // The Hue scene's own id, or its homeCore device id.
                    (_, Some(id)) => self
                        .registry
                        .get_scene_binding(&id)
                        .map(|b| b.scene_rid.clone())
                        .unwrap_or(id),
                    (Target::Scene(b), None) => b.scene_rid.clone(),
                    _ => {
                        let r = Err(anyhow::anyhow!("missing scene_id"));
                        return self.finish(device_id, "activate_scene", r, None).await;
                    }
                };
                let r = match &target {
                    Target::Scene(b) if b.smart => api.activate_smart_scene(&scene_rid, true).await,
                    _ => api.activate_scene(&scene_rid, &recall).await,
                };
                if r.is_ok() {
                    let source = match target {
                        Target::Scene(_) => "scene_device_command",
                        Target::Group(_) => "group_device_command",
                        _ => "light_device_command",
                    };
                    self.emit_scene_activated_event(device_id, &scene_rid, source)
                        .await;
                }
                ("activate_scene", r)
            }
            (Target::Aux(binding), PluginCommand::SetAccessoryState(c)) => (
                "set_accessory_state",
                match Self::validate_accessory_command(&binding.resource_type, &c) {
                    Ok(()) => {
                        api.execute_accessory_command(
                            &binding.resource_type,
                            &binding.resource_rid,
                            &c,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                },
            ),
            (Target::Aux(binding), PluginCommand::SetEntertainmentActive { config_id, active }) => {
                let config = config_id
                    .clone()
                    .unwrap_or_else(|| binding.resource_rid.clone());
                let r = match Self::validate_entertainment_target(&binding.resource_type)
                    .and_then(|()| Self::validate_entertainment_command(config_id.as_deref()))
                {
                    Ok(()) => api.execute_entertainment_command(&config, active).await,
                    Err(e) => Err(e),
                };
                let err = r.as_ref().err().map(|e| e.to_string());
                self.publish_entertainment_command_context(
                    device_id,
                    &config,
                    active,
                    r.is_ok(),
                    err.as_deref(),
                )
                .await;
                if r.is_ok() {
                    self.emit_entertainment_action_event(
                        device_id,
                        &config,
                        active,
                        "entertainment_device_command",
                    )
                    .await;
                }
                ("set_entertainment_active", r)
            }
            (Target::Group(b), PluginCommand::SaveScene { name }) => {
                let r = match &b.owner_rid {
                    Some(owner) => api.save_scene(owner, &name).await,
                    None => Err(anyhow::anyhow!(
                        "this group has no room or zone to save a scene in"
                    )),
                };
                ("save_scene", r)
            }
            (Target::Scene(b), PluginCommand::RenameScene { name }) if !b.smart => {
                ("rename_scene", api.rename_scene(&b.scene_rid, &name).await)
            }
            (Target::Scene(b), PluginCommand::DeleteScene) if !b.smart => {
                ("delete_scene", api.delete_scene(&b.scene_rid).await)
            }
            (_, other) => {
                warn!(device_id, command = ?other, "Command not supported by this kind of Hue device");
                let operation = match other {
                    PluginCommand::SetLightState(_) => "set_light_state",
                    PluginCommand::ActivateScene { .. } => "activate_scene",
                    PluginCommand::SetAccessoryState(_) => "set_accessory_state",
                    PluginCommand::SetEntertainmentActive { .. } => "set_entertainment_active",
                    PluginCommand::PairBridge => "pair_bridge",
                    PluginCommand::SaveScene { .. } => "save_scene",
                    PluginCommand::RenameScene { .. } => "rename_scene",
                    PluginCommand::DeleteScene => "delete_scene",
                    _ => "command",
                };
                let r = Err(anyhow::anyhow!("unsupported target type"));
                return self.finish(device_id, operation, r, None).await;
            }
        };

        self.finish(device_id, operation, result, Some(&bridge_id))
            .await
    }

    /// Report a command's outcome and, after a change, catch up with the
    /// bridge — but only when its event stream is not already telling us:
    /// with the stream up, the bridge reports the change by itself, and
    /// reading every resource after each command was twenty requests
    /// between one tap and the next.
    async fn finish(
        &mut self,
        device_id: &str,
        operation: &str,
        result: Result<()>,
        changed_bridge: Option<&str>,
    ) -> Result<()> {
        let ok = result.is_ok();
        self.observe(device_id, operation, &result).await;
        if let (true, Some(bridge_id)) = (ok, changed_bridge) {
            if !self.live.get(bridge_id).copied().unwrap_or(false) {
                if let Some(api) = self
                    .apis
                    .iter()
                    .find(|a| a.target().bridge_id == bridge_id)
                    .cloned()
                {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe(device_id, "refresh_after_command", &Err(e))
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn observe(&self, device_id: &str, operation: &str, result: &Result<()>) {
        let err = result.as_ref().err().map(|e| format!("{e:#}"));
        self.observe_command_result(device_id, operation, result.is_ok(), err.as_deref())
            .await;
    }

    /// Which device a command is for, and the bridge that has it.
    fn target_of(&self, device_id: &str) -> Option<(Target, HueApiClient)> {
        let api_for = |bridge_id: &str| {
            self.apis
                .iter()
                .find(|a| a.target().bridge_id == bridge_id)
                .cloned()
        };
        if let Some(api) = self
            .apis
            .iter()
            .find(|a| a.target().device_id() == device_id)
        {
            return Some((Target::Bridge, api.clone()));
        }
        if let Some(b) = self.registry.get_light_binding(device_id) {
            return Some((Target::Light(b.clone()), api_for(&b.bridge_id)?));
        }
        if let Some(b) = self.registry.get_group_binding(device_id) {
            return Some((Target::Group(b.clone()), api_for(&b.bridge_id)?));
        }
        if let Some(b) = self.registry.get_scene_binding(device_id) {
            return Some((Target::Scene(b.clone()), api_for(&b.bridge_id)?));
        }
        if let Some(b) = self.registry.get_aux_binding(device_id) {
            return Some((Target::Aux(b.clone()), api_for(&b.bridge_id)?));
        }
        None
    }

    /// Pairing asked of the bridge device. It waits up to half a minute for
    /// the link button, so it runs on its own: the runtime used to sleep in
    /// here, and every light stopped answering until somebody pressed it.
    async fn spawn_device_pairing(&self, device_id: &str, api: &HueApiClient) {
        info!(device_id, bridge_id = %api.target().bridge_id, "Hue bridge pairing requested");
        self.publish_bridge_pairing_progress(
            device_id,
            &api.target().bridge_id,
            "started",
            true,
            None,
        )
        .await;
        let api = api.clone();
        let device_id = device_id.to_string();
        let publisher = self.publisher.clone();
        let learned = self.learned_state.clone();
        let writer = self.state_writer.clone();
        let new_bridge_tx = self.new_bridge_tx.clone();
        tokio::spawn(async move {
            let mut outcome = Err("Nobody pressed the link button in time.".to_string());
            for _ in 0..15 {
                match api.pair_bridge("homecore#hc_hue").await {
                    Ok(key) => {
                        outcome = Ok(key);
                        break;
                    }
                    Err(e) if Self::is_link_button_not_pressed_error(&e.to_string()) => {
                        let _ = publisher
                            .publish_state_partial(
                                &device_id,
                                &json!({ "pairing_last_phase": "waiting_link_button" }),
                            )
                            .await;
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        outcome = Err(e.to_string());
                        break;
                    }
                }
            }
            let patch = match outcome {
                Ok(app_key) => {
                    let delta = {
                        let mut cell = learned.lock().unwrap();
                        let current = cell.clone().unwrap_or_else(|| json!({}));
                        let delta = crate::config::build_bridge_state_delta(
                            &current,
                            api.target(),
                            &app_key,
                        );
                        *cell = Some(delta.clone());
                        delta
                    };
                    if let Err(e) = writer.persist(&delta).await {
                        warn!(error = %e, "Failed to persist bridge app_key; re-pairing will be needed after restart");
                    }
                    let mut paired = api.target().clone();
                    paired.app_key = Some(app_key);
                    let _ = new_bridge_tx.send(paired).await;
                    json!({ "pairing_in_progress": false, "pairing_status": "paired",
                            "pairing_last_phase": "paired", "pairing_last_result": "success",
                            "pairing_last_error": null })
                }
                Err(e) => {
                    warn!(device_id, error = %e, "Hue bridge pairing failed");
                    json!({ "pairing_in_progress": false, "pairing_status": "failed",
                            "pairing_last_phase": "failed", "pairing_last_result": "failed",
                            "pairing_last_error": e })
                }
            };
            let _ = publisher.publish_state_partial(&device_id, &patch).await;
        });
    }

    /// A command for a device with our prefix but no binding.
    async fn report_unrouted(&self, device_id: &str) {
        // A device_id carrying our own `hue_` prefix but with no registry
        // binding — we published it at some point and have since stopped
        // managing it. Dropping it as "not ours" makes the command vanish with
        // no error anywhere, so say so loudly. It cannot catch a device
        // orphaned across a restart (the SDK subscribes per device, so its
        // commands never reach us); core detects that one instead — see
        // `GET /api/v1/devices/orphaned`.
        if !Self::is_own_device_id(device_id) {
            debug!(device_id, "Ignoring command for non-Hue device");
            return;
        }
        warn!(
            device_id,
            "Command for an unmanaged Hue device — it is still registered in \
             homeCore but this plugin has no binding for it, so the command was \
             dropped.  Check publish_grouped_lights / publish_bridge_home, or \
             remove the stale device."
        );
        // Not `observe_command_result`: its state patch would refresh this
        // device's `last_seen` and disguise the staleness that marks it.
        let payload = translator::command_result_event(translator::CommandResult {
            plugin_id: self.publisher.plugin_id(),
            device_id,
            operation: "unrouted",
            success: false,
            error: Some("device not managed by this plugin"),
            error_code: Some("unmanaged_device"),
            latency_ms: 0,
            retry_count: 0,
        });
        if let Err(e) = self
            .publisher
            .publish_event("plugin_command_result", &payload)
            .await
        {
            warn!(device_id, error = %e, "Failed to publish unrouted-command event");
        }
    }

    fn validate_accessory_command(resource_type: &str, command: &AccessoryCommand) -> Result<()> {
        if command.enabled.is_none() && command.motion_sensitivity.is_none() {
            anyhow::bail!("empty accessory command");
        }

        if command.motion_sensitivity.is_some() && resource_type != "motion" {
            anyhow::bail!(
                "motion_sensitivity is only supported for motion resources (target type: {resource_type})"
            );
        }

        if command.enabled.is_some()
            && !matches!(
                resource_type,
                "motion" | "temperature" | "light_level" | "contact"
            )
        {
            anyhow::bail!("enabled is not supported for accessory resource type: {resource_type}");
        }

        Ok(())
    }

    /// Whether a command asks for anything at all.
    fn is_empty_light_command(c: &LightCommand) -> bool {
        c.on.is_none()
            && c.brightness_pct.is_none()
            && c.brightness_step.is_none()
            && c.color_temp_mirek.is_none()
            && c.color_xy.is_none()
            && c.effect.is_none()
            && c.dynamic_speed.is_none()
            && c.gradient_points.is_none()
            && c.identify.is_none()
            && !c.breathe
            && c.signal.is_none()
    }

    fn validate_light_command(binding: &RegisteredLight, command: &LightCommand) -> Result<()> {
        if Self::is_empty_light_command(command) {
            anyhow::bail!("empty light command");
        }

        if command.gradient_points.is_some() && !binding.supports_gradient {
            anyhow::bail!("gradient_points not supported for this light");
        }

        if command.identify == Some(true) && !binding.supports_identify {
            anyhow::bail!("identify not supported for this light");
        }

        if command.identify == Some(false) {
            anyhow::bail!("identify must be true when provided");
        }

        if let Some(effect) = &command.effect {
            if !binding.effect_values.is_empty()
                && !binding.effect_values.iter().any(|v| v == effect)
            {
                anyhow::bail!("effect '{effect}' not supported for this light");
            }
        }

        Ok(())
    }

    /// A room or zone takes what all its lights share — power, brightness,
    /// white, colour, a fade, a pulse, a flash — but not what only one lamp
    /// has: an effect, a gradient, being identified.
    fn validate_grouped_light_command(command: &LightCommand) -> Result<()> {
        if Self::is_empty_light_command(command) {
            anyhow::bail!("no writable fields for grouped_light command");
        }

        if command.effect.is_some()
            || command.dynamic_speed.is_some()
            || command.gradient_points.is_some()
            || command.identify.is_some()
        {
            anyhow::bail!("grouped_light does not support advanced light fields");
        }

        Ok(())
    }

    fn validate_entertainment_target(resource_type: &str) -> Result<()> {
        if resource_type != "entertainment_configuration" {
            anyhow::bail!(
                "entertainment command is only supported for entertainment_configuration resources (target type: {resource_type})"
            );
        }
        Ok(())
    }

    fn validate_entertainment_command(config_id: Option<&str>) -> Result<()> {
        if let Some(id) = config_id {
            if id.trim().is_empty() {
                anyhow::bail!("missing entertainment config_id");
            }
        }
        Ok(())
    }

    fn should_run_fallback_refresh(&mut self, bridge_id: &str) -> bool {
        let now = Instant::now();
        match self.metrics.last_fallback_by_bridge.get(bridge_id) {
            Some(last)
                if now.duration_since(*last)
                    < Duration::from_secs(FALLBACK_REFRESH_COOLDOWN_SECS) =>
            {
                false
            }
            _ => {
                self.metrics
                    .last_fallback_by_bridge
                    .insert(bridge_id.to_string(), now);
                true
            }
        }
    }

    async fn refresh_bridge_with_reason(
        &mut self,
        bridge_id: &str,
        reason: &str,
        api: &HueApiClient,
    ) -> Result<()> {
        debug!(bridge_id, reason, "Refreshing Hue bridge state");
        self.refresh(api).await
    }

    fn fallback_ratio_pct(incremental_applied_total: u64, fallback_refresh_total: u64) -> f64 {
        let base = incremental_applied_total + fallback_refresh_total;
        if base == 0 {
            0.0
        } else {
            (fallback_refresh_total as f64 / base as f64) * 100.0
        }
    }

    async fn observe_command_result(
        &self,
        device_id: &str,
        operation: &str,
        success: bool,
        error: Option<&str>,
    ) {
        let error_code = Self::classify_command_error(error);
        let latency_ms = self
            .command_started_at
            .as_ref()
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        let retry_count = 0;
        let patch = translator::command_result_patch_with_metrics(
            operation,
            success,
            error,
            error_code,
            latency_ms,
            retry_count,
        );
        if let Err(e) = self
            .publisher
            .publish_state_partial(device_id, &patch)
            .await
        {
            warn!(device_id, operation, error = %e, "Failed to publish command result state patch");
        }

        let payload = translator::command_result_event(translator::CommandResult {
            plugin_id: self.publisher.plugin_id(),
            device_id,
            operation,
            success,
            error,
            error_code,
            latency_ms,
            retry_count,
        });
        if let Err(e) = self
            .publisher
            .publish_event("plugin_command_result", &payload)
            .await
        {
            warn!(device_id, operation, error = %e, "Failed to publish command result event");
        }
    }

    /// Whether `device_id` is one this plugin publishes.  True even for devices
    /// it no longer has a registry binding for — that gap is exactly what makes
    /// an unroutable command worth reporting rather than silently dropping.
    fn is_own_device_id(device_id: &str) -> bool {
        device_id.starts_with(HUE_DEVICE_ID_PREFIX)
    }

    fn classify_command_error(error: Option<&str>) -> Option<&'static str> {
        let err = error?.to_ascii_lowercase();

        if err.contains("unknown device_id") {
            return Some("unknown_device");
        }
        if err.contains("unsupported target type") {
            return Some("unsupported_target_type");
        }
        if err.contains("missing scene_id") {
            return Some("missing_required_field");
        }
        if err.contains("missing entertainment config_id") {
            return Some("missing_required_field");
        }
        if err.contains("empty accessory command") {
            return Some("empty_command");
        }
        if err.contains("empty light command") {
            return Some("empty_command");
        }
        if err.contains("only supported for") || err.contains("not supported for") {
            return Some("unsupported_field_for_resource");
        }
        if err.contains("must be true") {
            return Some("invalid_field_value");
        }
        if err.contains("no writable fields") {
            return Some("no_writable_fields");
        }
        if err.contains("link button") {
            return Some("pairing_button_required");
        }
        if err.contains("failed") || err.contains("request") {
            return Some("execution_failed");
        }

        Some("command_error")
    }

    fn is_link_button_not_pressed_error(error: &str) -> bool {
        error
            .to_ascii_lowercase()
            .contains("link button not pressed")
    }

    async fn emit_scene_activated_event(&self, device_id: &str, scene_id: &str, source: &str) {
        let payload = translator::scene_activated_event(
            self.publisher.plugin_id(),
            device_id,
            scene_id,
            source,
        );
        if let Err(e) = self
            .publisher
            .publish_event("scene_activated", &payload)
            .await
        {
            warn!(device_id, scene_id, error = %e, "Failed to publish scene_activated event");
        }
    }

    async fn emit_group_action_event(&self, device_id: &str, group_id: &str, source: &str) {
        let payload =
            translator::group_action_event(self.publisher.plugin_id(), device_id, group_id, source);
        if let Err(e) = self
            .publisher
            .publish_event("group_action_applied", &payload)
            .await
        {
            warn!(device_id, group_id, error = %e, "Failed to publish group_action_applied event");
        }
    }

    async fn emit_entertainment_action_event(
        &self,
        device_id: &str,
        config_id: &str,
        active: bool,
        source: &str,
    ) {
        let payload = translator::entertainment_action_event(
            self.publisher.plugin_id(),
            device_id,
            config_id,
            active,
            source,
        );
        if let Err(e) = self
            .publisher
            .publish_event("entertainment_action_applied", &payload)
            .await
        {
            warn!(device_id, config_id, error = %e, "Failed to publish entertainment_action_applied event");
        }
    }

    async fn publish_entertainment_command_context(
        &self,
        device_id: &str,
        config_id: &str,
        active: bool,
        success: bool,
        error: Option<&str>,
    ) {
        let action = if active { "start" } else { "stop" };
        let mut patch = json!({
            "last_entertainment_command_config_id": config_id,
            "last_entertainment_command_action": action,
            "last_entertainment_command_active": active,
            "last_entertainment_command_success": success,
        });
        if let Some(err) = error {
            patch["last_entertainment_command_error"] = json!(err);
        } else {
            patch["last_entertainment_command_error"] = Value::Null;
        }

        if let Err(e) = self
            .publisher
            .publish_state_partial(device_id, &patch)
            .await
        {
            warn!(device_id, config_id, error = %e, "Failed to publish entertainment command context patch");
        }
    }

    async fn publish_bridge_pairing_progress(
        &self,
        device_id: &str,
        bridge_id: &str,
        phase: &str,
        in_progress: bool,
        error: Option<&str>,
    ) {
        let status = if in_progress {
            "in_progress"
        } else if error.is_some() {
            "failed"
        } else {
            "paired"
        };

        let mut patch = json!({
            "pairing_in_progress": in_progress,
            "pairing_status": status,
            "pairing_last_phase": phase,
            "pairing_last_result": if error.is_some() { "failed" } else if in_progress { "in_progress" } else { "success" },
        });

        if let Some(err) = error {
            patch["pairing_last_error"] = json!(err);
        } else {
            patch["pairing_last_error"] = Value::Null;
        }

        if let Err(e) = self
            .publisher
            .publish_state_partial(device_id, &patch)
            .await
        {
            warn!(device_id, bridge_id, error = %e, "Failed to publish bridge pairing state patch");
        }

        let event = translator::bridge_pairing_event(
            self.publisher.plugin_id(),
            device_id,
            bridge_id,
            phase,
            if error.is_some() {
                Some(false)
            } else if in_progress {
                None
            } else {
                Some(true)
            },
            error,
        );
        if let Err(e) = self
            .publisher
            .publish_event("bridge_pairing_status", &event)
            .await
        {
            warn!(device_id, bridge_id, error = %e, "Failed to publish bridge_pairing_status event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bridge() -> Bridge {
        let publisher = DevicePublisher::test_instance("plugin.hue");
        let bridge = BridgeTarget {
            name: "Test Bridge".to_string(),
            bridge_id: "bridge-1".to_string(),
            host: "127.0.0.1".to_string(),
            app_key: None,
            verify_tls: true,
            allow_self_signed: true,
        };
        Bridge::new(
            HuePluginConfig::default(),
            vec![bridge],
            publisher,
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            plugin_sdk_rs::PluginStateWriter::test_instance("plugin.hue"),
            plugin_sdk_rs::PluginNotices::test_instance(),
            mpsc::channel(1).0,
        )
    }

    #[test]
    fn validates_motion_sensitivity_only_for_motion() {
        let cmd = AccessoryCommand {
            enabled: None,
            motion_sensitivity: Some(1),
        };

        assert!(Bridge::validate_accessory_command("motion", &cmd).is_ok());
        assert!(Bridge::validate_accessory_command("temperature", &cmd).is_err());
    }

    #[test]
    fn validates_enabled_resource_support() {
        let cmd = AccessoryCommand {
            enabled: Some(true),
            motion_sensitivity: None,
        };

        assert!(Bridge::validate_accessory_command("motion", &cmd).is_ok());
        assert!(Bridge::validate_accessory_command("light_level", &cmd).is_ok());
        assert!(Bridge::validate_accessory_command("device_power", &cmd).is_err());
    }

    #[test]
    fn rejects_empty_accessory_command() {
        let cmd = AccessoryCommand {
            enabled: None,
            motion_sensitivity: None,
        };
        assert!(Bridge::validate_accessory_command("motion", &cmd).is_err());
    }

    #[test]
    fn recognizes_own_device_ids_even_without_a_binding() {
        // A grouped light dropped from config keeps its registration in
        // homeCore and still routes commands here.  It must be recognized as
        // ours so the command is reported rather than silently discarded.
        assert!(Bridge::is_own_device_id(
            "hue_001788fffe6841b3_group_f891081e"
        ));
        assert!(Bridge::is_own_device_id(
            "hue_001788fffe6841b3_light_03113f8a"
        ));
        assert!(!Bridge::is_own_device_id("lutron_72"));
        assert!(!Bridge::is_own_device_id("caseta_10"));
    }

    #[test]
    fn classifies_common_command_errors() {
        assert_eq!(
            Bridge::classify_command_error(Some("unsupported target type")),
            Some("unsupported_target_type")
        );
        assert_eq!(
            Bridge::classify_command_error(Some("unknown device_id")),
            Some("unknown_device")
        );
        assert_eq!(
            Bridge::classify_command_error(Some(
                "motion_sensitivity is only supported for motion resources"
            )),
            Some("unsupported_field_for_resource")
        );
        assert_eq!(
            Bridge::classify_command_error(Some("empty light command")),
            Some("empty_command")
        );
        assert_eq!(
            Bridge::classify_command_error(Some("identify must be true when provided")),
            Some("invalid_field_value")
        );
        assert_eq!(
            Bridge::classify_command_error(Some("missing entertainment config_id")),
            Some("missing_required_field")
        );
        assert_eq!(Bridge::classify_command_error(None), None);
    }

    #[test]
    fn fallback_ratio_pct_handles_edge_and_normal_cases() {
        assert_eq!(Bridge::fallback_ratio_pct(0, 0), 0.0);
        assert_eq!(Bridge::fallback_ratio_pct(10, 0), 0.0);
        assert_eq!(Bridge::fallback_ratio_pct(0, 5), 100.0);
        assert_eq!(Bridge::fallback_ratio_pct(3, 1), 25.0);
    }

    #[test]
    fn detects_link_button_not_pressed_errors() {
        assert!(Bridge::is_link_button_not_pressed_error(
            "Hue pairing failed: link button not pressed"
        ));
        assert!(Bridge::is_link_button_not_pressed_error(
            "LINK BUTTON NOT PRESSED"
        ));
        assert!(!Bridge::is_link_button_not_pressed_error(
            "connection timeout"
        ));
    }

    #[test]
    fn fallback_refresh_is_debounced_per_bridge() {
        let mut bridge = test_bridge();

        assert!(bridge.should_run_fallback_refresh("bridge-1"));
        assert!(!bridge.should_run_fallback_refresh("bridge-1"));
        assert!(bridge.should_run_fallback_refresh("bridge-2"));
    }

    #[test]
    fn validates_entertainment_target_type() {
        assert!(Bridge::validate_entertainment_target("entertainment_configuration").is_ok());
        assert!(Bridge::validate_entertainment_target("motion").is_err());
    }

    #[test]
    fn validates_entertainment_command_config_id() {
        assert!(Bridge::validate_entertainment_command(None).is_ok());
        assert!(Bridge::validate_entertainment_command(Some("cfg-1")).is_ok());
        assert!(Bridge::validate_entertainment_command(Some("   ")).is_err());
    }

    #[test]
    fn validates_light_command_non_empty() {
        let binding = RegisteredLight {
            bridge_id: "b1".to_string(),
            light_rid: "l1".to_string(),
            supports_gradient: true,
            supports_identify: true,
            effect_values: vec!["candle".to_string()],
        };

        let empty = LightCommand::default();
        assert!(Bridge::validate_light_command(&binding, &empty).is_err());

        let with_effect = LightCommand {
            effect: Some("candle".to_string()),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &with_effect).is_ok());

        let with_identify = LightCommand {
            identify: Some(true),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &with_identify).is_ok());
    }

    #[test]
    fn validates_light_command_against_supported_capabilities() {
        let binding = RegisteredLight {
            bridge_id: "b1".to_string(),
            light_rid: "l1".to_string(),
            supports_gradient: false,
            supports_identify: false,
            effect_values: vec!["prism".to_string()],
        };

        let gradient = LightCommand {
            gradient_points: Some(vec![(0.1, 0.2)]),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &gradient).is_err());

        let identify = LightCommand {
            identify: Some(true),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &identify).is_err());

        let invalid_effect = LightCommand {
            effect: Some("candle".to_string()),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &invalid_effect).is_err());

        let invalid_identify = LightCommand {
            identify: Some(false),
            ..Default::default()
        };
        assert!(Bridge::validate_light_command(&binding, &invalid_identify).is_err());
    }

    #[test]
    fn validates_grouped_light_command_supported_fields_only() {
        let ok = LightCommand {
            on: Some(true),
            ..Default::default()
        };
        assert!(Bridge::validate_grouped_light_command(&ok).is_ok());

        let unsupported = LightCommand {
            effect: Some("candle".to_string()),
            ..Default::default()
        };
        assert!(Bridge::validate_grouped_light_command(&unsupported).is_err());
    }
}
