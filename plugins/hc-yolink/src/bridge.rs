use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::config::TemperatureUnit;
use crate::devices::DeviceKind;
use crate::yolink::{
    api::YolinkApi,
    types::{is_device_unreachable, is_transient, DeviceInfo, YolinkReport},
};
use plugin_sdk_rs::DevicePublisher;

// ---------------------------------------------------------------------------
// Device record — pairs the YoLink device info with its resolved kind
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Device {
    info: DeviceInfo,
    kind: DeviceKind,
    /// HomeCore device ID: "yolink_{yolink_device_id}"
    hc_id: String,
    retired: bool,
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    devices: Vec<Device>,
    /// YoLink device_id → index in `devices`, for O(1) lookup on MQTT events
    index: HashMap<String, usize>,
    yolink_api: Arc<YolinkApi>,
    publisher: DevicePublisher,
    temp_unit: TemperatureUnit,
    poll_interval: Duration,
    inventory_interval: Duration,
    /// Delay between successive per-device getState calls to avoid hub rate limits.
    poll_device_delay: Duration,
    /// Delay before the one-shot background initial state fetch. Zero = disabled.
    initial_fetch_delay: Duration,
    /// Notified to trigger an immediate `sync_inventory()` on demand
    /// (e.g. from the `rescan_devices` management command).
    rescan: Arc<Notify>,
}

/// Tuning parameters for [`Bridge::new`]. Grouped to keep the constructor's
/// signature within clippy's `too_many_arguments` threshold and to give
/// callers a single place to thread cadence/timing config.
pub struct BridgeOptions {
    pub temp_unit: TemperatureUnit,
    pub poll_interval_secs: u64,
    pub inventory_interval_secs: u64,
    pub poll_device_delay_ms: u64,
    pub initial_fetch_delay_secs: u64,
}

impl Bridge {
    pub fn new(
        raw: Vec<(DeviceInfo, DeviceKind)>,
        yolink_api: Arc<YolinkApi>,
        publisher: DevicePublisher,
        rescan: Arc<Notify>,
        opts: BridgeOptions,
    ) -> Self {
        let mut devices = Vec::with_capacity(raw.len());
        let mut index = HashMap::new();

        for (info, kind) in raw {
            let hc_id = format!("yolink_{}", info.device_id);
            index.insert(info.device_id.clone(), devices.len());
            devices.push(Device {
                info,
                kind,
                hc_id,
                retired: false,
            });
        }

        Self {
            devices,
            index,
            yolink_api,
            publisher,
            temp_unit: opts.temp_unit,
            poll_interval: Duration::from_secs(opts.poll_interval_secs),
            inventory_interval: Duration::from_secs(opts.inventory_interval_secs),
            poll_device_delay: Duration::from_millis(opts.poll_device_delay_ms),
            initial_fetch_delay: Duration::from_secs(opts.initial_fetch_delay_secs),
            rescan,
        }
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    pub async fn run(
        mut self,
        mut yolink_rx: mpsc::Receiver<YolinkReport>,
        mut homecore_rx: mpsc::Receiver<(String, Value)>,
    ) -> Result<()> {
        // The initial getState sweep runs as a background task below.
        // Skip the first poll tick so we don't double-fetch at startup.
        let mut poll_timer = tokio::time::interval(self.poll_interval);
        poll_timer.tick().await;

        // Inventory sync runs on its own cadence so users can rescan often
        // without paying the cost of a full per-device state refresh.
        let mut inv_timer = tokio::time::interval(self.inventory_interval);
        inv_timer.tick().await;

        let rescan = Arc::clone(&self.rescan);

        // --- Background initial state fetch ----------------------------------
        if !self.initial_fetch_delay.is_zero() {
            let devices: Vec<Device> = self.devices.clone();
            let api = Arc::clone(&self.yolink_api);
            let publisher = self.publisher.clone();
            let temp_unit = self.temp_unit.clone();
            let device_delay = self.poll_device_delay;
            let startup_delay = self.initial_fetch_delay;

            tokio::spawn(async move {
                info!(
                    delay_secs = startup_delay.as_secs(),
                    "Initial state fetch: waiting before starting"
                );
                tokio::time::sleep(startup_delay).await;
                info!(count = devices.len(), "Initial state fetch: starting");

                for dev in &devices {
                    if dev.retired || !dev.kind.is_supported() {
                        continue;
                    }

                    if !device_delay.is_zero() {
                        tokio::time::sleep(device_delay).await;
                    }

                    match get_state_retrying(&api, &dev.info).await {
                        Ok(data) => {
                            let online = data["online"].as_bool().unwrap_or(true);
                            let _ = publisher.publish_availability(&dev.hc_id, online).await;
                            if let Some(state) = dev.kind.translate_state(&data, &temp_unit) {
                                if let Err(e) = publisher.publish_state(&dev.hc_id, &state).await {
                                    warn!(hc_id = %dev.hc_id, error = %e,
                                        "Initial fetch: failed to publish state");
                                }
                            }
                        }
                        Err(e) if is_device_unreachable(&e) => {
                            warn!(hc_id = %dev.hc_id, error = %e,
                                "Initial fetch: device still unreachable after retries, marking offline");
                            let _ = publisher.publish_availability(&dev.hc_id, false).await;
                        }
                        Err(e) => {
                            warn!(hc_id = %dev.hc_id, error = %e,
                                "Initial fetch: getState failed");
                        }
                    }
                }
                info!("Initial state fetch complete");
            });
        }

        info!("Bridge event loop running ({} devices)", self.devices.len());

        loop {
            tokio::select! {
                // Real-time device report from YoLink MQTT
                Some(report) = yolink_rx.recv() => {
                    self.handle_yolink_report(report).await;
                }

                // Command from HomeCore (rule engine / user API)
                Some((hc_id, cmd)) = homecore_rx.recv() => {
                    self.handle_homecore_command(hc_id, cmd).await;
                }

                // Periodic true-up: full-state refresh of every device.
                _ = poll_timer.tick() => {
                    self.poll_all_devices().await;
                }

                // Periodic inventory reconciliation (detects newly-paired
                // or removed devices).  Runs on its own cadence.
                _ = inv_timer.tick() => {
                    self.sync_inventory().await;
                }

                // On-demand rescan (e.g. "Rescan devices" button in the
                // Leptos admin UI via the `rescan_devices` management cmd).
                _ = rescan.notified() => {
                    info!("Manual rescan requested");
                    self.sync_inventory().await;
                }
            }
        }
    }

    async fn sync_inventory(&mut self) {
        let fresh = match self.yolink_api.get_device_list().await {
            Ok(list) => list,
            Err(e) => {
                warn!(error = %e, "Inventory sync: get_device_list failed");
                return;
            }
        };

        let mut seen = HashSet::new();

        for info in fresh {
            let kind = DeviceKind::from_yolink_type(&info.device_type);
            if !kind.is_supported() {
                continue;
            }

            let device_id = info.device_id.clone();
            seen.insert(device_id.clone());

            if let Some(&idx) = self.index.get(&device_id) {
                let (hc_id, old_name, needs_reregister) = {
                    let dev = &self.devices[idx];
                    (
                        dev.hc_id.clone(),
                        dev.info.name.clone(),
                        dev.info.name != info.name
                            || dev.kind.homecore_device_type() != kind.homecore_device_type(),
                    )
                };

                if needs_reregister {
                    info!(
                        hc_id = %hc_id,
                        old_name = %old_name,
                        new_name = %info.name,
                        "YoLink device metadata changed; re-registering with HomeCore"
                    );
                    if let Err(e) = self
                        .publisher
                        .register_device_full(
                            &hc_id,
                            &info.name,
                            Some(kind.homecore_device_type()),
                            None,
                            None,
                        )
                        .await
                    {
                        warn!(
                            hc_id = %hc_id,
                            error = %e,
                            "Inventory sync: failed to re-register device metadata"
                        );
                        continue;
                    }
                }

                if let Err(e) = crate::schema::publish(&self.publisher, &hc_id, &kind).await {
                    warn!(hc_id = %hc_id, error = %e, "Inventory sync: publish schema failed");
                }

                let dev = &mut self.devices[idx];
                dev.info = info;
                dev.kind = kind;
                dev.retired = false;
                continue;
            }

            let hc_id = format!("yolink_{device_id}");
            info!(hc_id = %hc_id, name = %info.name, "New YoLink device discovered; registering");
            if let Err(e) = self
                .publisher
                .register_device_full(
                    &hc_id,
                    &info.name,
                    Some(kind.homecore_device_type()),
                    None,
                    None,
                )
                .await
            {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: register_device failed");
                continue;
            }
            if let Err(e) = crate::schema::publish(&self.publisher, &hc_id, &kind).await {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: publish schema failed");
            }
            if let Err(e) = self.publisher.subscribe_commands(&hc_id).await {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: subscribe_commands failed");
            }

            // Rate-limit new device state fetch to avoid overwhelming the hub.
            if !self.poll_device_delay.is_zero() {
                tokio::time::sleep(self.poll_device_delay).await;
            }

            match self.get_state_retrying(&info).await {
                Ok(data) => {
                    let online = data["online"].as_bool().unwrap_or(true);
                    let _ = self.publisher.publish_availability(&hc_id, online).await;
                    if let Some(state) = kind.translate_state(&data, &self.temp_unit) {
                        let _ = self.publisher.publish_state(&hc_id, &state).await;
                    }
                }
                Err(e) if is_device_unreachable(&e) => {
                    debug!(hc_id = %hc_id, error = %e,
                        "Inventory sync: new device unreachable after retries, registering as offline");
                    let _ = self.publisher.publish_availability(&hc_id, false).await;
                }
                Err(e) => {
                    warn!(hc_id = %hc_id, error = %e, "Inventory sync: initial state fetch failed");
                    let _ = self.publisher.publish_availability(&hc_id, false).await;
                }
            }

            self.index.insert(device_id, self.devices.len());
            self.devices.push(Device {
                info,
                kind,
                hc_id,
                retired: false,
            });
        }

        let missing: Vec<(String, usize)> = self
            .index
            .iter()
            .filter_map(|(device_id, &idx)| {
                (!seen.contains(device_id.as_str())).then_some((device_id.clone(), idx))
            })
            .collect();

        for (device_id, idx) in missing {
            self.index.remove(&device_id);
            if self.devices[idx].retired {
                continue;
            }
            let hc_id = self.devices[idx].hc_id.clone();
            info!(hc_id = %hc_id, "YoLink device missing from inventory; unregistering");
            if let Err(e) = self
                .publisher
                .unregister_device(self.publisher.plugin_id(), &hc_id)
                .await
            {
                warn!(hc_id = %hc_id, error = %e, "Inventory sync: unregister_device failed");
                self.index.insert(device_id, idx);
                continue;
            }
            self.devices[idx].retired = true;
        }

        // Cross-restart cleanup: tell the SDK what's live this cycle so
        // it can prune anything from a prior session whose device_id
        // isn't in `seen` and which the in-memory loop above couldn't
        // catch (because self.index starts empty after restart).
        let live: std::collections::HashSet<String> = seen
            .iter()
            .map(|device_id| format!("yolink_{device_id}"))
            .collect();
        if let Err(e) = self.publisher.reconcile_devices(live).await {
            warn!(error = %e, "reconcile_devices failed");
        }
    }

    // -----------------------------------------------------------------------
    // Handlers
    // -----------------------------------------------------------------------

    async fn handle_yolink_report(&self, report: YolinkReport) {
        let Some(dev) = self.find_by_yolink_id(&report.device_id) else {
            debug!(device_id = %report.device_id, "Report for unknown device, ignoring");
            return;
        };

        debug!(
            hc_id = %dev.hc_id,
            yolink_device_id = %report.device_id,
            event = %report.event,
            kind = ?dev.kind,
            raw = %report.data,
            "YoLink report received"
        );

        // Publish availability if present in the report
        if let Some(online) = report.data["online"].as_bool() {
            debug!(
                hc_id = %dev.hc_id,
                yolink_device_id = %report.device_id,
                event = %report.event,
                online,
                "Publishing availability from YoLink report"
            );
            if let Err(e) = self
                .publisher
                .publish_availability(&dev.hc_id, online)
                .await
            {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
            }
        }

        // Translate and publish state as a partial update (merge-patch)
        if let Some(patch) = dev
            .kind
            .translate_report(&report.event, &report.data, &self.temp_unit)
        {
            debug!(
                hc_id = %dev.hc_id,
                yolink_device_id = %report.device_id,
                event = %report.event,
                kind = ?dev.kind,
                patch = %patch,
                "Publishing state patch from YoLink report"
            );
            if let Err(e) = self
                .publisher
                .publish_state_partial(&dev.hc_id, &patch)
                .await
            {
                warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state partial");
            }
        } else {
            warn!(
                hc_id = %dev.hc_id,
                event = %report.event,
                kind  = ?dev.kind,
                raw   = %report.data,
                "MQTT report: translate_state returned None — raw report data logged above"
            );
        }
    }

    async fn handle_homecore_command(&self, hc_id: String, cmd: Value) {
        // HomeCore device IDs are "yolink_{yolink_device_id}"
        let yolink_id = hc_id.strip_prefix("yolink_").unwrap_or(&hc_id);

        let Some(dev) = self.find_by_yolink_id(yolink_id) else {
            warn!(hc_id = %hc_id, "Command for unknown device");
            return;
        };

        debug!(
            hc_id = %hc_id,
            yolink_device_id = %yolink_id,
            kind = ?dev.kind,
            command = %cmd,
            "HomeCore command received for YoLink device"
        );

        match dev.kind.translate_command(&cmd) {
            Ok((method_suffix, params)) => {
                debug!(
                    hc_id = %hc_id,
                    yolink_device_id = %yolink_id,
                    kind = ?dev.kind,
                    method_suffix,
                    params = %params,
                    "Translated HomeCore command to YoLink request"
                );
                // All current controllable types use setState
                if let Err(e) = self.yolink_api.set_device_state(&dev.info, params).await {
                    warn!(hc_id = %hc_id, error = %e, "YoLink command failed");
                } else {
                    debug!(hc_id = %hc_id, "Command sent to YoLink");
                }
            }
            Err(e) => {
                warn!(hc_id = %hc_id, error = %e, "Cannot translate HomeCore command");
            }
        }
    }

    async fn poll_all_devices(&self) {
        info!("Polling {} devices for state true-up", self.devices.len());

        for dev in &self.devices {
            if dev.retired {
                continue;
            }
            if !dev.kind.is_supported() {
                continue;
            }

            if !self.poll_device_delay.is_zero() {
                tokio::time::sleep(self.poll_device_delay).await;
            }

            match self.get_state_retrying(&dev.info).await {
                Ok(data) => {
                    debug!(
                        hc_id = %dev.hc_id,
                        yolink_device_id = %dev.info.device_id,
                        kind = ?dev.kind,
                        raw = %data,
                        "YoLink getState snapshot received"
                    );

                    // Publish availability
                    let online = data["online"].as_bool().unwrap_or(true);
                    debug!(
                        hc_id = %dev.hc_id,
                        yolink_device_id = %dev.info.device_id,
                        online,
                        "Publishing availability from YoLink getState snapshot"
                    );
                    let _ = self
                        .publisher
                        .publish_availability(&dev.hc_id, online)
                        .await;

                    // Publish full state (retained — this is a ground-truth refresh)
                    if let Some(state) = dev.kind.translate_state(&data, &self.temp_unit) {
                        debug!(
                            hc_id = %dev.hc_id,
                            yolink_device_id = %dev.info.device_id,
                            kind = ?dev.kind,
                            state = %state,
                            "Publishing full state snapshot from YoLink getState"
                        );
                        if let Err(e) = self.publisher.publish_state(&dev.hc_id, &state).await {
                            warn!(hc_id = %dev.hc_id, error = %e, "Poll: failed to publish state");
                        }
                    } else {
                        warn!(
                            hc_id = %dev.hc_id,
                            kind  = ?dev.kind,
                            raw   = %data,
                            "Poll: translate_state returned None — raw getState response logged above"
                        );
                    }
                }
                Err(e) if is_device_unreachable(&e) => {
                    // Still unreachable after the retries above, so the busy-radio
                    // explanation has run out and this is most likely a device
                    // with flat batteries or out of range. Say so through
                    // availability, which is what feeds the "needs attention"
                    // list.
                    //
                    // Before this, an unreachable device produced a warning and
                    // nothing else: core went on believing it was available, so
                    // a lock with dead batteries sat there looking healthy and
                    // never appeared in any alert. The only trace was a log line
                    // once an hour, which is exactly where nobody looks.
                    warn!(hc_id = %dev.hc_id, error = %e,
                        "Poll: device still unreachable after retries, marking offline");
                    let _ = self.publisher.publish_availability(&dev.hc_id, false).await;
                }
                Err(e) => {
                    warn!(hc_id = %dev.hc_id, error = %e, "Poll: getState failed");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Fetch with retry
    // -----------------------------------------------------------------------

    /// `getState`, retrying the errors that clear on their own.
    ///
    /// `000201` ("cannot connect to the device") is usually the LoRa channel
    /// being busy rather than anything wrong with the device — the more devices
    /// on the hub, the more often it happens, and a retry a second later
    /// normally succeeds. The rate-limit codes behave the same way.
    ///
    /// This matters because the caller uses exhausted retries as evidence the
    /// device is actually dead. Without the retries that inference is wrong,
    /// and a busy radio would flap a healthy lock to "offline" and raise an
    /// alert about it.
    ///
    /// Backoff is 1s, 2s, 4s. Short, because a poll of a large install is
    /// already sequential and slow, and the device delay between polls gives
    /// the channel room anyway.
    async fn get_state_retrying(&self, info: &DeviceInfo) -> Result<Value, anyhow::Error> {
        get_state_retrying(&self.yolink_api, info).await
    }

    // -----------------------------------------------------------------------
    // Lookup helpers
    // -----------------------------------------------------------------------

    fn find_by_yolink_id(&self, yolink_id: &str) -> Option<&Device> {
        self.index.get(yolink_id).map(|&i| &self.devices[i])
    }
}

/// Free function so the detached startup-fetch task, which owns an `Arc<YolinkApi>`
/// rather than a `&Bridge`, retries on exactly the same terms as the poll loop.
async fn get_state_retrying(api: &YolinkApi, info: &DeviceInfo) -> Result<Value, anyhow::Error> {
    const ATTEMPTS: u32 = 3;

    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        match api.get_device_state(info).await {
            Ok(v) => return Ok(v),
            Err(e) if is_transient(&e) => {
                debug!(
                    device_id = %info.device_id,
                    attempt = attempt + 1,
                    error = %e,
                    "getState hit a transient error, retrying"
                );
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("loop runs at least once and only stores on error"))
}
