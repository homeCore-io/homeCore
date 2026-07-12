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
use crate::hue::registry::{HueRegistry, RegisteredLight};
use crate::sync::{self, EventApplyOutcome, SyncConfig};
use crate::translator;
use plugin_sdk_rs::DevicePublisher;

const FALLBACK_REFRESH_COOLDOWN_SECS: u64 = 15;

/// Prefix shared by every device_id this plugin publishes
/// (`hue_{bridge_id}_{kind}_{rid}` — see `BridgeTarget` in `hue::models`).
const HUE_DEVICE_ID_PREFIX: &str = "hue_";

/// Request sent by the management `refresh_devices` (or
/// `cleanup_stale_devices`) action. Carries an optional progress
/// sink so the streaming action can forward bridge-by-bridge events
/// to the operator while the refresh is in flight.
pub struct RefreshRequest {
    pub progress: Option<mpsc::Sender<RefreshEvent>>,
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

pub struct Bridge {
    cfg: HuePluginConfig,
    config_path: String,
    publisher: DevicePublisher,
    sync_cfg: SyncConfig,
    apis: Vec<HueApiClient>,
    registry: HueRegistry,
    command_started_at: Option<Instant>,
    metrics: EventstreamMetrics,
}

impl Bridge {
    pub fn new(
        cfg: HuePluginConfig,
        config_path: String,
        bridges: Vec<BridgeTarget>,
        publisher: DevicePublisher,
    ) -> Self {
        let apis = bridges.into_iter().map(HueApiClient::new).collect();
        let sync_cfg = SyncConfig {
            display: cfg.hue.display.clone(),
            compact_motion_facets: cfg.hue.compact_motion_facets,
            publish_grouped_lights: cfg.hue.publish_grouped_lights,
            publish_grouped_lights_for: cfg.hue.publish_grouped_lights_for.clone(),
            skip_grouped_lights_for: cfg.hue.skip_grouped_lights_for.clone(),
            publish_bridge_home: cfg.hue.publish_bridge_home,
            publish_entertainment_configurations: cfg.hue.publish_entertainment_configurations,
        };
        Self {
            cfg,
            config_path,
            publisher,
            sync_cfg,
            apis,
            registry: HueRegistry::default(),
            command_started_at: None,
            metrics: EventstreamMetrics::default(),
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

    async fn refresh(&mut self, api: &HueApiClient) -> Result<()> {
        // Borrow individual fields to avoid borrowing all of `self` — callers
        // often hold an `api` reference from `self.apis` at the same time.
        sync::refresh_bridge_state(&self.publisher, &mut self.registry, api, &self.sync_cfg).await
    }

    pub async fn run(
        mut self,
        mut hc_rx: mpsc::Receiver<(String, Value)>,
        mut refresh_rx: mpsc::Receiver<RefreshRequest>,
        mut new_bridge_rx: mpsc::Receiver<crate::hue::models::BridgeTarget>,
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
                                    &self.sync_cfg,
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
                            info!(
                                bridge_id = %target.bridge_id,
                                host = %target.host,
                                "New bridge added at runtime via pair action"
                            );
                            let api = HueApiClient::new(target.clone());
                            self.apis.push(api.clone());
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
                        None => {
                            // Sender dropped — fine.
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
        if let Some(api) = self
            .apis
            .iter()
            .find(|api| api.target().device_id() == device_id)
            .cloned()
        {
            match cmd {
                PluginCommand::Refresh => {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "refresh", true, None)
                        .await;
                }
                PluginCommand::SetAvailability(online) => {
                    if let Err(e) = self.publisher.publish_availability(device_id, online).await {
                        self.observe_command_result(
                            device_id,
                            "set_availability",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_availability", true, None)
                        .await;
                }
                PluginCommand::PairBridge => {
                    info!(
                        device_id,
                        bridge_id = %api.target().bridge_id,
                        host = %api.target().host,
                        "Hue bridge pairing requested"
                    );
                    self.publish_bridge_pairing_progress(
                        device_id,
                        &api.target().bridge_id,
                        "started",
                        true,
                        None,
                    )
                    .await;
                    let max_attempts: u8 = 15;
                    let retry_delay = Duration::from_secs(2);
                    let mut paired = false;
                    let mut final_error: Option<String> = None;

                    for attempt in 1..=max_attempts {
                        match api.pair_bridge("homecore#hc_hue").await {
                            Ok(app_key) => {
                                paired = true;
                                // Persist the new app_key to config so it survives restarts.
                                self.cfg.upsert_bridge_app_key(api.target(), &app_key);
                                match self.cfg.save(&self.config_path) {
                                    Ok(()) => info!(
                                        bridge_id = %api.target().bridge_id,
                                        config = %self.config_path,
                                        "Bridge app_key saved to config"
                                    ),
                                    Err(e) => warn!(
                                        bridge_id = %api.target().bridge_id,
                                        error = %e,
                                        "Failed to persist bridge app_key; re-pairing will be needed after restart"
                                    ),
                                }
                                break;
                            }
                            Err(e) => {
                                let err_text = e.to_string();

                                // Keep polling for a short window so users can press the bridge button
                                // after triggering pairing from the TUI.
                                if Self::is_link_button_not_pressed_error(&err_text)
                                    && attempt < max_attempts
                                {
                                    info!(
                                        device_id,
                                        bridge_id = %api.target().bridge_id,
                                        attempt,
                                        max_attempts,
                                        "Waiting for Hue link button press"
                                    );
                                    self.publish_bridge_pairing_progress(
                                        device_id,
                                        &api.target().bridge_id,
                                        "waiting_link_button",
                                        true,
                                        Some("Press the link button on the Hue bridge"),
                                    )
                                    .await;
                                    tokio::time::sleep(retry_delay).await;
                                    continue;
                                }

                                final_error = Some(err_text);
                                break;
                            }
                        }
                    }

                    if !paired {
                        let err_text = final_error
                            .unwrap_or_else(|| "Hue pairing failed after retry window".to_string());
                        warn!(
                            device_id,
                            bridge_id = %api.target().bridge_id,
                            error = %err_text,
                            "Hue bridge pairing failed"
                        );
                        self.publish_bridge_pairing_progress(
                            device_id,
                            &api.target().bridge_id,
                            "failed",
                            false,
                            Some(&err_text),
                        )
                        .await;
                        self.observe_command_result(
                            device_id,
                            "pair_bridge",
                            false,
                            Some(&err_text),
                        )
                        .await;
                        return Ok(());
                    }

                    info!(
                        device_id,
                        bridge_id = %api.target().bridge_id,
                        "Hue bridge pairing succeeded"
                    );
                    self.publish_bridge_pairing_progress(
                        device_id,
                        &api.target().bridge_id,
                        "paired",
                        false,
                        None,
                    )
                    .await;
                    self.observe_command_result(device_id, "pair_bridge", true, None)
                        .await;
                    if let Err(e) = self.refresh(&api).await {
                        warn!(
                            device_id,
                            bridge_id = %api.target().bridge_id,
                            error = %e,
                            "Hue bridge post-pair refresh failed"
                        );
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::Raw(raw) => {
                    if let Err(e) = api.execute_raw_command(&raw).await {
                        self.observe_command_result(device_id, "raw", false, Some(&e.to_string()))
                            .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "raw", true, None)
                        .await;
                }
                PluginCommand::SetLightState(_) => {
                    warn!(
                        device_id,
                        "Ignoring light-state command sent to bridge device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_light_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::ActivateScene { .. } => {
                    warn!(
                        device_id,
                        "Ignoring scene activation command sent to bridge device"
                    );
                    self.observe_command_result(
                        device_id,
                        "activate_scene",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetAccessoryState(_) => {
                    warn!(
                        device_id,
                        "Ignoring accessory-state command sent to bridge device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_accessory_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetEntertainmentActive { .. } => {
                    warn!(
                        device_id,
                        "Ignoring entertainment command sent to bridge device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_entertainment_active",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
            }

            return Ok(());
        }

        if let Some(binding) = self.registry.get_light_binding(device_id) {
            let Some(api) = self
                .apis
                .iter()
                .find(|api| api.target().bridge_id == binding.bridge_id)
                .cloned()
            else {
                warn!(device_id, bridge_id = %binding.bridge_id, "No Hue API client for light's bridge");
                return Ok(());
            };

            match cmd {
                PluginCommand::SetLightState(light_cmd) => {
                    if let Err(e) = Self::validate_light_command(binding, &light_cmd) {
                        self.observe_command_result(
                            device_id,
                            "set_light_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Err(e) = api
                        .execute_light_command(&binding.light_rid, &light_cmd)
                        .await
                    {
                        self.observe_command_result(
                            device_id,
                            "set_light_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_light_state", true, None)
                        .await;
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::Refresh => {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "refresh", true, None)
                        .await;
                }
                PluginCommand::Raw(raw) => {
                    if let Err(e) = api.execute_raw_command(&raw).await {
                        self.observe_command_result(device_id, "raw", false, Some(&e.to_string()))
                            .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "raw", true, None)
                        .await;
                }
                PluginCommand::SetAvailability(online) => {
                    if let Err(e) = self.publisher.publish_availability(device_id, online).await {
                        self.observe_command_result(
                            device_id,
                            "set_availability",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_availability", true, None)
                        .await;
                }
                PluginCommand::ActivateScene { scene_id } => {
                    if let Some(scene_rid) = scene_id {
                        if let Err(e) = api.activate_scene(&scene_rid).await {
                            self.observe_command_result(
                                device_id,
                                "activate_scene",
                                false,
                                Some(&e.to_string()),
                            )
                            .await;
                            return Ok(());
                        }
                        self.observe_command_result(device_id, "activate_scene", true, None)
                            .await;
                        self.emit_scene_activated_event(
                            device_id,
                            &scene_rid,
                            "light_device_command",
                        )
                        .await;
                        if let Err(e) = self.refresh(&api).await {
                            self.observe_command_result(
                                device_id,
                                "refresh_after_command",
                                false,
                                Some(&e.to_string()),
                            )
                            .await;
                        }
                    } else {
                        warn!(
                            device_id,
                            "activate_scene requires scene_id when sent to light device"
                        );
                        self.observe_command_result(
                            device_id,
                            "activate_scene",
                            false,
                            Some("missing scene_id"),
                        )
                        .await;
                    }
                }
                PluginCommand::SetAccessoryState(_) => {
                    warn!(
                        device_id,
                        "Ignoring accessory-state command sent to light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_accessory_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetEntertainmentActive { .. } => {
                    warn!(
                        device_id,
                        "Ignoring entertainment command sent to light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_entertainment_active",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::PairBridge => {
                    warn!(
                        device_id,
                        "Ignoring pair_bridge command sent to light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "pair_bridge",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
            }

            return Ok(());
        }

        if let Some(binding) = self.registry.get_group_binding(device_id) {
            let Some(api) = self
                .apis
                .iter()
                .find(|api| api.target().bridge_id == binding.bridge_id)
                .cloned()
            else {
                warn!(device_id, bridge_id = %binding.bridge_id, "No Hue API client for grouped-light bridge");
                return Ok(());
            };

            match cmd {
                PluginCommand::SetLightState(group_cmd) => {
                    if let Err(e) = Self::validate_grouped_light_command(&group_cmd) {
                        self.observe_command_result(
                            device_id,
                            "set_group_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Err(e) = api
                        .execute_grouped_light_command(&binding.group_rid, &group_cmd)
                        .await
                    {
                        self.observe_command_result(
                            device_id,
                            "set_group_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_group_state", true, None)
                        .await;
                    self.emit_group_action_event(
                        device_id,
                        &binding.group_rid,
                        "group_device_command",
                    )
                    .await;
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::Refresh => {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "refresh", true, None)
                        .await;
                }
                PluginCommand::SetAvailability(online) => {
                    if let Err(e) = self.publisher.publish_availability(device_id, online).await {
                        self.observe_command_result(
                            device_id,
                            "set_availability",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_availability", true, None)
                        .await;
                }
                PluginCommand::Raw(raw) => {
                    if let Err(e) = api.execute_raw_command(&raw).await {
                        self.observe_command_result(device_id, "raw", false, Some(&e.to_string()))
                            .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "raw", true, None)
                        .await;
                }
                PluginCommand::ActivateScene { scene_id } => {
                    if let Some(scene_rid) = scene_id {
                        if let Err(e) = api.activate_scene(&scene_rid).await {
                            self.observe_command_result(
                                device_id,
                                "activate_scene",
                                false,
                                Some(&e.to_string()),
                            )
                            .await;
                            return Ok(());
                        }
                        self.observe_command_result(device_id, "activate_scene", true, None)
                            .await;
                        self.emit_scene_activated_event(
                            device_id,
                            &scene_rid,
                            "group_device_command",
                        )
                        .await;
                        if let Err(e) = self.refresh(&api).await {
                            self.observe_command_result(
                                device_id,
                                "refresh_after_command",
                                false,
                                Some(&e.to_string()),
                            )
                            .await;
                        }
                    } else {
                        warn!(
                            device_id,
                            "activate_scene requires scene_id when sent to grouped-light device"
                        );
                        self.observe_command_result(
                            device_id,
                            "activate_scene",
                            false,
                            Some("missing scene_id"),
                        )
                        .await;
                    }
                }
                PluginCommand::SetAccessoryState(_) => {
                    warn!(
                        device_id,
                        "Ignoring accessory-state command sent to grouped-light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_accessory_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetEntertainmentActive { .. } => {
                    warn!(
                        device_id,
                        "Ignoring entertainment command sent to grouped-light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_entertainment_active",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::PairBridge => {
                    warn!(
                        device_id,
                        "Ignoring pair_bridge command sent to grouped-light device"
                    );
                    self.observe_command_result(
                        device_id,
                        "pair_bridge",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
            }

            return Ok(());
        }

        if let Some(binding) = self.registry.get_scene_binding(device_id) {
            let Some(api) = self
                .apis
                .iter()
                .find(|api| api.target().bridge_id == binding.bridge_id)
                .cloned()
            else {
                warn!(device_id, bridge_id = %binding.bridge_id, "No Hue API client for scene bridge");
                return Ok(());
            };

            match cmd {
                PluginCommand::ActivateScene { scene_id } => {
                    let target_scene = scene_id.unwrap_or_else(|| binding.scene_rid.clone());
                    if let Err(e) = api.activate_scene(&target_scene).await {
                        self.observe_command_result(
                            device_id,
                            "activate_scene",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "activate_scene", true, None)
                        .await;
                    self.emit_scene_activated_event(
                        device_id,
                        &target_scene,
                        "scene_device_command",
                    )
                    .await;
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::Refresh => {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "refresh", true, None)
                        .await;
                }
                PluginCommand::Raw(raw) => {
                    if let Err(e) = api.execute_raw_command(&raw).await {
                        self.observe_command_result(device_id, "raw", false, Some(&e.to_string()))
                            .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "raw", true, None)
                        .await;
                }
                PluginCommand::SetAvailability(online) => {
                    if let Err(e) = self.publisher.publish_availability(device_id, online).await {
                        self.observe_command_result(
                            device_id,
                            "set_availability",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_availability", true, None)
                        .await;
                }
                PluginCommand::SetLightState(_) => {
                    warn!(
                        device_id,
                        "Ignoring light-state command sent to scene device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_light_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetAccessoryState(_) => {
                    warn!(
                        device_id,
                        "Ignoring accessory-state command sent to scene device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_accessory_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetEntertainmentActive { .. } => {
                    warn!(
                        device_id,
                        "Ignoring entertainment command sent to scene device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_entertainment_active",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::PairBridge => {
                    warn!(
                        device_id,
                        "Ignoring pair_bridge command sent to scene device"
                    );
                    self.observe_command_result(
                        device_id,
                        "pair_bridge",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
            }

            return Ok(());
        }

        if let Some(binding) = self.registry.get_aux_binding(device_id) {
            let Some(api) = self
                .apis
                .iter()
                .find(|api| api.target().bridge_id == binding.bridge_id)
                .cloned()
            else {
                warn!(device_id, bridge_id = %binding.bridge_id, "No Hue API client for auxiliary resource bridge");
                return Ok(());
            };

            match cmd {
                PluginCommand::SetAccessoryState(accessory_cmd) => {
                    if let Err(e) =
                        Self::validate_accessory_command(&binding.resource_type, &accessory_cmd)
                    {
                        self.observe_command_result(
                            device_id,
                            "set_accessory_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Err(e) = api
                        .execute_accessory_command(
                            &binding.resource_type,
                            &binding.resource_rid,
                            &accessory_cmd,
                        )
                        .await
                    {
                        self.observe_command_result(
                            device_id,
                            "set_accessory_state",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_accessory_state", true, None)
                        .await;
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::Refresh => {
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "refresh", true, None)
                        .await;
                }
                PluginCommand::Raw(raw) => {
                    if let Err(e) = api.execute_raw_command(&raw).await {
                        self.observe_command_result(device_id, "raw", false, Some(&e.to_string()))
                            .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "raw", true, None)
                        .await;
                }
                PluginCommand::SetAvailability(online) => {
                    if let Err(e) = self.publisher.publish_availability(device_id, online).await {
                        self.observe_command_result(
                            device_id,
                            "set_availability",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.observe_command_result(device_id, "set_availability", true, None)
                        .await;
                }
                PluginCommand::SetLightState(_) => {
                    warn!(
                        device_id,
                        "Ignoring light-state command sent to auxiliary device"
                    );
                    self.observe_command_result(
                        device_id,
                        "set_light_state",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::ActivateScene { .. } => {
                    warn!(
                        device_id,
                        "Ignoring scene activation command sent to auxiliary device"
                    );
                    self.observe_command_result(
                        device_id,
                        "activate_scene",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
                PluginCommand::SetEntertainmentActive { config_id, active } => {
                    if let Err(e) = Self::validate_entertainment_target(&binding.resource_type) {
                        let target_config = config_id
                            .as_deref()
                            .unwrap_or(&binding.resource_rid)
                            .to_string();
                        self.publish_entertainment_command_context(
                            device_id,
                            &target_config,
                            active,
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        self.observe_command_result(
                            device_id,
                            "set_entertainment_active",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Err(e) = Self::validate_entertainment_command(config_id.as_deref()) {
                        let target_config = config_id
                            .as_deref()
                            .unwrap_or(&binding.resource_rid)
                            .to_string();
                        self.publish_entertainment_command_context(
                            device_id,
                            &target_config,
                            active,
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        self.observe_command_result(
                            device_id,
                            "set_entertainment_active",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    let target_config = config_id.unwrap_or_else(|| binding.resource_rid.clone());
                    if let Err(e) = api
                        .execute_entertainment_command(&target_config, active)
                        .await
                    {
                        self.publish_entertainment_command_context(
                            device_id,
                            &target_config,
                            active,
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        self.observe_command_result(
                            device_id,
                            "set_entertainment_active",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                        return Ok(());
                    }
                    self.publish_entertainment_command_context(
                        device_id,
                        &target_config,
                        active,
                        true,
                        None,
                    )
                    .await;
                    self.observe_command_result(device_id, "set_entertainment_active", true, None)
                        .await;
                    self.emit_entertainment_action_event(
                        device_id,
                        &target_config,
                        active,
                        "entertainment_device_command",
                    )
                    .await;
                    if let Err(e) = self.refresh(&api).await {
                        self.observe_command_result(
                            device_id,
                            "refresh_after_command",
                            false,
                            Some(&e.to_string()),
                        )
                        .await;
                    }
                }
                PluginCommand::PairBridge => {
                    warn!(
                        device_id,
                        "Ignoring pair_bridge command sent to auxiliary device"
                    );
                    self.observe_command_result(
                        device_id,
                        "pair_bridge",
                        false,
                        Some("unsupported target type"),
                    )
                    .await;
                }
            }

            return Ok(());
        }

        // A device_id carrying our own `hue_` prefix that reaches this point is
        // one we published in an earlier run but no longer sync — e.g. a grouped
        // light whose `publish_grouped_lights` config was turned off while its
        // registration stayed retained in homeCore.  Such a device still accepts
        // commands from rules and the UI, and every one of them lands here.
        // Dropping it as "not ours" makes the command vanish with no error
        // anywhere, so say so loudly.
        if Self::is_own_device_id(device_id) {
            warn!(
                device_id,
                "Command for an unmanaged Hue device — it is still registered in \
                 homeCore but this plugin has no binding for it, so the command was \
                 dropped.  Check publish_grouped_lights / publish_bridge_home, or \
                 remove the stale device."
            );
            // Deliberately not `observe_command_result` — that publishes a state
            // patch, which would refresh this device's `last_seen` and disguise the
            // very staleness that marks it as unmanaged.  Emit the event only.
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
            return Ok(());
        }

        // Ignore commands for devices this plugin doesn't own.  With the
        // SDK wildcard subscription (homecore/devices/+/cmd), commands for
        // other plugins' devices arrive here too — silently skip them.
        debug!(device_id, "Ignoring command for non-Hue device");

        Ok(())
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

    fn validate_light_command(binding: &RegisteredLight, command: &LightCommand) -> Result<()> {
        if command.on.is_none()
            && command.brightness_pct.is_none()
            && command.color_temp_mirek.is_none()
            && command.color_xy.is_none()
            && command.effect.is_none()
            && command.dynamic_speed.is_none()
            && command.gradient_points.is_none()
            && command.identify.is_none()
        {
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

    fn validate_grouped_light_command(command: &LightCommand) -> Result<()> {
        if command.on.is_none() && command.brightness_pct.is_none() {
            anyhow::bail!("no writable fields for grouped_light command");
        }

        if command.color_temp_mirek.is_some()
            || command.color_xy.is_some()
            || command.effect.is_some()
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
            "config/config.toml".to_string(),
            vec![bridge],
            publisher,
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
