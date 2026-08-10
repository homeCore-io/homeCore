//! `plugin-sdk-rs` — Rust SDK for HomeCore device plugins.
//!
//! Provides:
//! - [`PluginClient`] — connects to the broker, handles registration, typed
//!   publish/subscribe helpers, and a command callback loop.
//! - [`DevicePublisher`] — cloneable handle for publishing state from spawned tasks.
//! - [`ManagementHandle`] — enable heartbeat + remote config/log management.
//!
//! ## Re-exports — single dependency for plugins
//!
//! The [`types`] and [`logging`] modules re-export the surface plugins
//! need from `hc-types` and `hc-logging`. Plugins should consume these
//! through `plugin-sdk-rs` rather than depending on `hc-types` /
//! `hc-logging` directly — that keeps the SDK as the single
//! upstream-homeCore dep, with one SemVer to track and one Cargo.lock
//! conflict surface to manage. Component versioning plan, Phase C.
//!
//! Direct dependencies on `hc-types` / `hc-logging` from existing plugin
//! repos are still supported (additive change — nothing was renamed or
//! removed). New plugins should prefer the re-exports; existing ones
//! migrate as they're touched.

/// Typed authoring for a plugin's config descriptor. The vocabulary lives in
/// `hc-types` because core describes `homecore.toml` with the same builders and
/// checks it with the same [`missing_schema_coverage`](hc_types::config_descriptor::missing_schema_coverage)
/// rule; core cannot depend on this crate without a cycle. Re-exported
/// unchanged, so `plugin_sdk_rs::config_descriptor::*` keeps working and no
/// plugin source changes.
pub use hc_types::config_descriptor;

pub mod device_actions;
pub mod mqtt_log_layer;
pub mod streaming;

/// Re-exports of the `hc-types` surface plugins use directly. Plugins
/// should `use plugin_sdk_rs::types::X;` rather than depending on
/// `hc-types` directly. Keeps the SDK as the single upstream-homeCore
/// dep. The whole `hc_types::schema` module is re-exported so plugins
/// can grow new uses (e.g. `schema::Range`) without an SDK PR per item.
pub mod types {
    pub use hc_types::device::{with_command_change_metadata, DeviceChange};
    pub use hc_types::plugin_capabilities::{
        Action, Capabilities, Concurrency, ItemOp, RequiresRole,
    };
    // Needed to construct anything for `PluginClient::notices()`. Without this
    // a plugin would have to depend on hc-types directly, which is exactly what
    // this module exists to avoid — and 0.3.7 shipped the handle without it,
    // making the feature uncallable.
    pub use hc_types::schema;
    pub use hc_types::schema::{
        AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
    };
    pub use hc_types::{NoticeLevel, PluginNotice};
}

/// Log rotation, compression, retention, and the plugin's tracing setup.
/// Also re-exports the `hc-logging` items plugins use directly.
pub mod logging;

use anyhow::{Context, Result};
use hc_types::device::{change_from_command_payload, with_state_change_metadata, DeviceChange};
use hc_types::PluginNotice;
use rumqttc::{AsyncClient, EventLoop, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

pub use streaming::{StreamContext, StreamingAction};

/// Shared tracker of MQTT topics this plugin has subscribed to.
/// On reconnect (ConnAck), all tracked topics are re-subscribed.
type SubscriptionTracker = Arc<Mutex<HashSet<String>>>;

/// No-op state callback used when the plugin doesn't need to observe other
/// devices (most plugins). Passed to `run_inner` from `run` / `run_managed`.
fn noop_state_cb(_device_id: String, _state: Value) {}

/// Inner state of the device tracker. Holds the set of device_ids this
/// plugin has registered with HomeCore plus an optional path to mirror
/// the set onto disk so it survives plugin restarts.
///
/// Persistence is opt-in via [`PluginClient::with_device_persistence`]
/// or [`DevicePublisher::enable_persistence`]. When enabled, every
/// register/unregister mutation is followed by a synchronous JSON
/// write — fine for the typical scale (dozens to a few hundred
/// devices) and avoids any reconnect/restart races.
#[derive(Default)]
pub(crate) struct DeviceTrackerInner {
    set: HashSet<String>,
    persist_path: Option<std::path::PathBuf>,
}

/// Insert `plugin_id` into a device-snapshot filename so two plugins sharing a
/// config directory cannot share a snapshot:
/// `.published-device-ids.json` → `.published-device-ids.plugin.hue.json`.
///
/// Idempotent: a path that already carries this plugin's id is returned
/// unchanged, so repeated calls cannot keep extending the name.
fn scoped_device_snapshot_path(path: &std::path::Path, plugin_id: &str) -> std::path::PathBuf {
    // Plugin ids contain dots ("plugin.hue"), and so does the base filename, so
    // work with the full file name rather than Path::file_stem/extension —
    // otherwise "plugin.hue" would be mistaken for an extension.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return path.to_path_buf();
    };
    let scoped = match name.strip_suffix(".json") {
        Some(base) if base.ends_with(plugin_id) => name.to_string(),
        Some(base) => format!("{base}.{plugin_id}.json"),
        None => format!("{name}.{plugin_id}"),
    };
    path.with_file_name(scoped)
}

impl DeviceTrackerInner {
    fn enable_persistence(&mut self, path: std::path::PathBuf) {
        // This snapshot is the only record of devices registered in *earlier*
        // runs, so it is what lets `reconcile_devices` retire a device that has
        // since been dropped from config.  If it fails to load we silently lose
        // that ability and the device lingers in homeCore forever, still
        // accepting commands nothing will execute — so never fail quietly here.
        match std::fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<Vec<String>>(&body) {
                Ok(ids) => {
                    debug!(
                        path = %path.display(),
                        count = ids.len(),
                        "Loaded published-device snapshot"
                    );
                    self.set.extend(ids);
                }
                Err(e) => warn!(
                    path = %path.display(), error = %e,
                    "Published-device snapshot is corrupt — devices registered in \
                     earlier runs cannot be reconciled and will linger in homeCore"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "No published-device snapshot yet — first run");
            }
            Err(e) => warn!(
                path = %path.display(), error = %e,
                "Cannot read published-device snapshot — devices registered in earlier \
                 runs cannot be reconciled and will linger in homeCore"
            ),
        }
        self.persist_path = Some(path);
    }

    fn insert(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.save();
        }
    }

    fn remove(&mut self, id: &str) {
        if self.set.remove(id) {
            self.save();
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.set.len()
    }

    fn snapshot(&self) -> HashSet<String> {
        self.set.clone()
    }

    fn save(&self) {
        let Some(p) = &self.persist_path else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut sorted: Vec<&String> = self.set.iter().collect();
        sorted.sort();
        if let Ok(body) = serde_json::to_vec_pretty(&sorted) {
            if let Err(e) = std::fs::write(p, body) {
                warn!(path = %p.display(), error = %e, "device tracker persistence write failed");
            }
        }
    }
}

/// Shared tracker of device IDs this plugin has registered with HomeCore.
/// Shared between `PluginClient` and `DevicePublisher` so registrations
/// from spawned tasks are reflected in the heartbeat's `device_count`
/// and in `reconcile_devices`.
type DeviceTracker = Arc<Mutex<DeviceTrackerInner>>;

/// Shared set of notices this plugin is currently reporting about itself.
/// Cloned into the heartbeat task and into every [`PluginNotices`] handle, so
/// a condition detected deep in a spawned task reaches core on the next beat.
type NoticeTracker = Arc<Mutex<Vec<PluginNotice>>>;

/// Handle for reporting what is wrong with this plugin.
///
/// A plugin that starts cleanly but cannot do its job is the case this exists
/// for — a receiver bound to an address nothing can reach, a credential that
/// is absent, a gateway that has never once answered. Logging it is not enough:
/// the operator is looking at the plugin page, where the plugin reads `active`.
///
/// The notices you hold are **current state**, republished in full on every
/// heartbeat. Core replaces its copy each time, so:
///
/// - raise a notice when you detect the condition,
/// - clear it when it resolves,
/// - and never worry about a stale one lingering — if you stop reporting it,
///   it disappears from the UI on the next beat.
///
/// Obtain one with [`PluginClient::notices`] *before* calling `run()`, since
/// `run()` consumes the client. The handle is `Clone` and cheap to move into
/// tasks.
///
/// ```rust,ignore
/// let notices = client.notices();
/// if bind_is_loopback && cfg.gateway_ip.is_none() {
///     notices.raise(
///         PluginNotice::warning(
///             "receiver_unreachable",
///             "The receiver is bound to loopback, so uploads from a gateway \
///              elsewhere on the network are dropped before they arrive.",
///         )
///         .with_remedy(r#"Set [ecowitt].bind_addr = "0.0.0.0"."#),
///     );
/// } else {
///     notices.clear("receiver_unreachable");
/// }
/// ```
#[derive(Clone)]
pub struct PluginNotices {
    inner: NoticeTracker,
}

impl PluginNotices {
    /// Create a detached handle for use in unit tests.
    ///
    /// Notices raised on it are held in memory and never reach a heartbeat,
    /// which is what a test wants: constructing whatever owns the handle
    /// should not require a broker. Deliberately a named constructor rather
    /// than a `Default` impl, so a detached handle cannot be created by
    /// accident somewhere that expected notices to actually be delivered.
    pub fn test_instance() -> Self {
        Self {
            inner: NoticeTracker::default(),
        }
    }

    /// Raise a notice, replacing any existing one with the same `code`.
    ///
    /// Keying on `code` is what makes this safe to call from a polling loop:
    /// re-raising the same condition every 30 seconds updates in place instead
    /// of accumulating duplicates.
    pub fn raise(&self, notice: PluginNotice) {
        let mut set = self.inner.lock().unwrap();
        match set.iter_mut().find(|n| n.code == notice.code) {
            Some(existing) => *existing = notice,
            None => set.push(notice),
        }
    }

    /// Clear the notice with this `code`, if present. Idempotent — clearing a
    /// condition that was never raised is a no-op, so callers can clear
    /// unconditionally on the healthy branch.
    pub fn clear(&self, code: &str) {
        self.inner.lock().unwrap().retain(|n| n.code != code);
    }

    /// Replace the entire set. For plugins that recompute every condition in
    /// one pass and would rather state the result than diff it.
    pub fn set(&self, notices: Vec<PluginNotice>) {
        *self.inner.lock().unwrap() = notices;
    }

    /// Drop every notice.
    pub fn clear_all(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// Snapshot of what would be sent on the next heartbeat.
    pub fn current(&self) -> Vec<PluginNotice> {
        self.inner.lock().unwrap().clone()
    }
}

/// Outcome of [`DevicePublisher::reconcile_devices`].
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Device IDs that were registered before this reconcile but are
    /// not in the supplied `live` set, so were unregistered.
    pub stale_unregistered: Vec<String>,
    /// Device IDs that were in the `live` set but had not been
    /// registered. Usually empty — non-empty means the caller passed
    /// a live set that includes devices it never registered with the
    /// SDK. Logged for diagnostic value, no action taken.
    pub unknown_in_live: Vec<String>,
}

/// A cloneable handle for publishing device state from outside the `run()` loop.
///
/// Obtained via [`PluginClient::device_publisher`] before calling `run()`.
#[derive(Clone)]
pub struct DevicePublisher {
    client: AsyncClient,
    plugin_id: String,
    subscriptions: SubscriptionTracker,
    devices: DeviceTracker,
}

pub fn change_from_command(command_payload: &Value, fallback_source: &str) -> DeviceChange {
    change_from_command_payload(command_payload, fallback_source)
}

impl DevicePublisher {
    /// Return the plugin ID this publisher was created with.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    async fn clear_retained_topic(&self, topic: &str) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
            .await
            .with_context(|| format!("clear retained topic failed: {topic}"))
    }

    // ── Full state publishing ────────────────────────────────────────────

    /// Publish a full device state to `homecore/devices/{device_id}/state` (retained).
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        self.publish_state_with_change(device_id, state, None).await
    }

    /// Publish a full device state with explicit provenance metadata.
    pub async fn publish_state_with_change(
        &self,
        device_id: &str,
        state: &Value,
        change: Option<&DeviceChange>,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = match change {
            Some(change) => serde_json::to_vec(&with_state_change_metadata(state.clone(), change))?,
            None => serde_json::to_vec(state)?,
        };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    /// Publish a full device state caused by an inbound HomeCore command.
    pub async fn publish_state_for_command(
        &self,
        device_id: &str,
        state: &Value,
        command_payload: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let change = change_from_command(command_payload, fallback_source);
        self.publish_state_with_change(device_id, state, Some(&change))
            .await
    }

    // ── Partial state publishing ─────────────────────────────────────────

    /// Publish a partial state update (JSON merge-patch, not retained).
    pub async fn publish_state_partial(&self, device_id: &str, patch: &Value) -> Result<()> {
        self.publish_state_partial_with_change(device_id, patch, None)
            .await
    }

    /// Publish a partial state update with explicit provenance metadata.
    pub async fn publish_state_partial_with_change(
        &self,
        device_id: &str,
        patch: &Value,
        change: Option<&DeviceChange>,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state/partial");
        let payload = match change {
            Some(change) => serde_json::to_vec(&with_state_change_metadata(patch.clone(), change))?,
            None => serde_json::to_vec(patch)?,
        };
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, payload)
            .await
            .context("publish_state_partial failed")
    }

    /// Publish a partial state update caused by an inbound HomeCore command.
    pub async fn publish_state_partial_for_command(
        &self,
        device_id: &str,
        patch: &Value,
        command_payload: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let change = change_from_command(command_payload, fallback_source);
        self.publish_state_partial_with_change(device_id, patch, Some(&change))
            .await
    }

    // ── Availability ─────────────────────────────────────────────────────

    /// Publish `"online"` or `"offline"` to the device's availability topic (retained).
    pub async fn set_available(&self, device_id: &str, available: bool) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/availability");
        let payload = if available { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("set_available failed")
    }

    /// Alias for [`set_available`] — matches the naming used by most plugins.
    pub async fn publish_availability(&self, device_id: &str, online: bool) -> Result<()> {
        self.set_available(device_id, online).await
    }

    // ── Schema ───────────────────────────────────────────────────────────

    /// Publish a device capability schema (retained) so HomeCore stores it and
    /// API clients can retrieve it via `GET /api/v1/devices/{id}/schema`.
    pub async fn register_device_schema(
        &self,
        device_id: &str,
        schema: &hc_types::DeviceSchema,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/schema");
        let payload = serde_json::to_vec(schema).context("serialising device schema")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("register_device_schema failed")
    }

    /// Publish a device capability schema built as JSON.
    ///
    /// The route for a schema carrying **action declarations** — see
    /// [`device_actions::with_actions`](crate::device_actions::with_actions).
    /// Deliberately separate from [`Self::register_device_schema`]: the typed
    /// `DeviceSchema` this SDK compiles against is whatever `hc-types` on
    /// `main` says it is, so a plugin declaring actions would otherwise wait on
    /// a core release, an SDK repin and a plugin repin before it could say
    /// anything new. The `Value` costs compile-time checking of the shape,
    /// which the builders give back.
    pub async fn register_device_schema_json(&self, device_id: &str, schema: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/schema");
        let payload = serde_json::to_vec(schema).context("serialising device schema")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("register_device_schema_json failed")
    }

    // ── Unregister ───────────────────────────────────────────────────────

    /// Retire a device from HomeCore by clearing retained topics and publishing
    /// a plugin-scoped unregister command.
    pub async fn unregister_device(&self, plugin_id: &str, device_id: &str) -> Result<()> {
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/state"))
            .await?;
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/availability"))
            .await?;
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/schema"))
            .await?;
        self.client
            .publish(
                format!("homecore/plugins/{plugin_id}/unregister"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&serde_json::json!({ "device_id": device_id }))?,
            )
            .await
            .context("unregister_device failed")?;
        self.devices.lock().unwrap().remove(device_id);
        Ok(())
    }

    // ── Plugin status ────────────────────────────────────────────────────

    /// Publish plugin status (`"active"`, `"degraded"`, `"offline"`) to
    /// `homecore/plugins/{id}/status` (retained).
    pub async fn publish_plugin_status(&self, status: &str) -> Result<()> {
        let topic = format!("homecore/plugins/{}/status", self.plugin_id);
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, status.as_bytes())
            .await
            .context("publish_plugin_status failed")
    }

    // ── Events ───────────────────────────────────────────────────────────

    /// Publish a structured event to `homecore/events/{event_type}`.
    pub async fn publish_event(&self, event_type: &str, payload: &Value) -> Result<()> {
        let topic = format!("homecore/events/{event_type}");
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(payload)?,
            )
            .await
            .context("publish_event failed")
    }

    // ── Dynamic registration (for plugins that discover devices at runtime) ─

    /// Register a device with all optional fields via the publisher.
    ///
    /// This mirrors [`PluginClient::register_device_full`] but can be called
    /// from spawned tasks that only hold a `DevicePublisher` handle (after the
    /// `PluginClient` has been consumed by `run_managed`).
    pub async fn register_device_full(
        &self,
        device_id: &str,
        name: &str,
        device_type: Option<&str>,
        area: Option<&str>,
        capabilities: Option<Value>,
    ) -> Result<()> {
        self.register_device_detailed(device_id, name, device_type, area, capabilities, None)
            .await
    }

    /// Register a device, including what the hardware actually is and what it
    /// sits behind. See [`PluginClient::register_device_detailed`].
    pub async fn register_device_detailed(
        &self,
        device_id: &str,
        name: &str,
        device_type: Option<&str>,
        area: Option<&str>,
        capabilities: Option<Value>,
        hardware: Option<&DeviceHardware>,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.plugin_id);
        let mut payload = serde_json::json!({
            "device_id": device_id,
            "plugin_id": self.plugin_id,
            "name": name,
        });
        if let Some(dt) = device_type {
            payload["device_type"] = Value::String(dt.to_string());
        }
        if let Some(a) = area {
            payload["area"] = Value::String(a.to_string());
        }
        if let Some(c) = capabilities {
            payload["capabilities"] = c;
        }
        if let Some(hw) = hardware {
            for (key, value) in [
                ("manufacturer", &hw.manufacturer),
                ("model", &hw.model),
                ("sw_version", &hw.sw_version),
                ("parent_device_id", &hw.parent_device_id),
            ] {
                if let Some(v) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    payload[key] = Value::String(v.to_string());
                }
            }
        }
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&payload)?,
            )
            .await
            .context("DevicePublisher::register_device_full failed")?;
        self.devices.lock().unwrap().insert(device_id);
        Ok(())
    }

    /// Enable cross-restart persistence for the device tracker.
    ///
    /// Loads any previously-saved device IDs from `path` into the
    /// in-memory tracker, then mirrors every register/unregister
    /// mutation back to disk. Combined with [`Self::reconcile_devices`]
    /// this gives plugins a "set what's live this cycle, SDK cleans
    /// up everything else" workflow that survives plugin restarts.
    ///
    /// Idempotent — call once at startup, before the first
    /// `register_device_full` of the session. Multiple calls re-load
    /// from the same path which is harmless but pointless.
    ///
    /// Path is typically `<config_dir>/.published-device-ids.json`. As with
    /// [`PluginClient::with_device_persistence`], the plugin id is inserted
    /// into the filename so plugins sharing a config directory cannot share a
    /// snapshot and unregister each other's devices.
    pub fn enable_persistence(&self, path: std::path::PathBuf) {
        let scoped = scoped_device_snapshot_path(&path, &self.plugin_id);
        self.devices.lock().unwrap().enable_persistence(scoped);
    }

    /// Reconcile the live device set against everything this plugin
    /// has ever registered (both in-session via `register_device_full`
    /// and across restarts when persistence is enabled).
    ///
    /// `live` should be the set of device_ids the plugin's authoritative
    /// upstream (Hue bridge, Z-Wave network, YoLink cloud, etc.)
    /// reports as currently existing. Anything previously registered
    /// but absent from `live` gets `unregister_device`d, removed from
    /// the tracker, and (if persistence is on) the new live set is
    /// written to disk.
    ///
    /// Caller is responsible for not invoking this when an upstream
    /// fetch failed — a partial live set would otherwise wipe legit
    /// devices behind a temporarily-unreachable upstream. Typical
    /// pattern: track an `all_bridges_succeeded` flag, only call
    /// `reconcile_devices` when true.
    ///
    /// New devices in `live` that the plugin hasn't yet registered
    /// are reported in `unknown_in_live` but otherwise ignored — call
    /// `register_device_full` for them first to bring them into the
    /// tracker.
    pub async fn reconcile_devices(&self, live: HashSet<String>) -> Result<ReconcileReport> {
        let known = self.devices.lock().unwrap().snapshot();
        let stale: Vec<String> = known.difference(&live).cloned().collect();
        let unknown_in_live: Vec<String> = live.difference(&known).cloned().collect();
        let mut unregistered = Vec::with_capacity(stale.len());
        for id in &stale {
            match self.unregister_device(&self.plugin_id, id).await {
                Ok(()) => {
                    unregistered.push(id.clone());
                    info!(plugin_id = %self.plugin_id, device_id = %id, "Unregistered stale device");
                }
                Err(e) => {
                    warn!(plugin_id = %self.plugin_id, device_id = %id, error = %e, "Failed to unregister stale device");
                }
            }
        }
        if !unknown_in_live.is_empty() {
            debug!(
                plugin_id = %self.plugin_id,
                count = unknown_in_live.len(),
                "reconcile_devices saw live ids not yet registered with the SDK; \
                 caller should `register_device_full` first"
            );
        }
        Ok(ReconcileReport {
            stale_unregistered: unregistered,
            unknown_in_live,
        })
    }

    /// Subscribe to command messages for a device and track the subscription
    /// so it is restored on MQTT reconnect.
    ///
    /// This mirrors [`PluginClient::subscribe_commands`] but can be called
    /// from spawned tasks that only hold a `DevicePublisher` handle.
    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("DevicePublisher::subscribe_commands failed")?;
        self.subscriptions.lock().unwrap().insert(topic);
        Ok(())
    }

    /// Subscribe to state updates for a device this plugin does **not** own.
    /// Tracked in the shared subscription set so reconnect restores it.
    ///
    /// Mirrors [`PluginClient::subscribe_state`].
    pub async fn subscribe_state(&self, device_id: &str) -> Result<()> {
        let topic_full = format!("homecore/devices/{device_id}/state");
        let topic_partial = format!("homecore/devices/{device_id}/state/partial");
        self.client
            .subscribe(&topic_full, QoS::AtLeastOnce)
            .await
            .context("DevicePublisher::subscribe_state (full) failed")?;
        self.client
            .subscribe(&topic_partial, QoS::AtLeastOnce)
            .await
            .context("DevicePublisher::subscribe_state (partial) failed")?;
        let mut subs = self.subscriptions.lock().unwrap();
        subs.insert(topic_full);
        subs.insert(topic_partial);
        Ok(())
    }

    /// Remove a subscription previously added with [`subscribe_state`].
    pub async fn unsubscribe_state(&self, device_id: &str) -> Result<()> {
        let topic_full = format!("homecore/devices/{device_id}/state");
        let topic_partial = format!("homecore/devices/{device_id}/state/partial");
        let _ = self.client.unsubscribe(&topic_full).await;
        let _ = self.client.unsubscribe(&topic_partial).await;
        let mut subs = self.subscriptions.lock().unwrap();
        subs.remove(&topic_full);
        subs.remove(&topic_partial);
        Ok(())
    }

    /// Create a `DevicePublisher` for use in unit tests.
    ///
    /// The underlying MQTT client is connected to `127.0.0.1:1883` and will
    /// not actually send messages unless a broker is running.
    pub fn test_instance(plugin_id: &str) -> Self {
        use rumqttc::MqttOptions;
        use std::time::Duration;
        let mut opts = MqttOptions::new(format!("{plugin_id}-test"), "127.0.0.1", 1883);
        opts.set_keep_alive(Duration::from_secs(30));
        let (client, _eventloop) = AsyncClient::new(opts, 8);
        Self {
            client,
            plugin_id: plugin_id.to_string(),
            subscriptions: Arc::new(Mutex::new(HashSet::new())),
            devices: Arc::new(Mutex::new(DeviceTrackerInner::default())),
        }
    }
}

/// Boxed handler for management actions the built-in dispatcher does not
/// recognise. Installed via [`ManagementHandle::with_custom_handler`].
type CustomActionHandler = Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

/// Handle returned by [`PluginClient::enable_management`].
///
/// Pass this to [`PluginClient::run_managed`] to automatically handle
/// `get_config`, `set_config`, and `set_log_level` management commands.
#[derive(Clone)]
pub struct ManagementHandle {
    plugin_id: String,
    config_path: Option<String>,
    log_level_handle: Option<hc_logging::LogLevelHandle>,
    custom_handler: Option<CustomActionHandler>,
    /// Capability manifest, published retained on
    /// `homecore/plugins/{id}/capabilities` after the first CONNACK.
    capabilities: Option<hc_types::Capabilities>,
    /// JSON Schema for the plugin's operator config, injected into the
    /// published capability manifest as `config_schema`. Core extracts it and
    /// serves it at `GET /plugins/{id}/config/schema` so the config editor can
    /// render a typed form. `None` → no schema published (editor uses raw TOML).
    config_schema: Option<Value>,
    /// The plugin's own config *descriptor*, injected into the published
    /// capability manifest as `config_descriptor`. Core serves it at
    /// `GET /plugins/{id}/config/descriptor`; the editor renders it directly
    /// instead of guessing a form from the schema. `None` → the client
    /// auto-derives a baseline descriptor from `config_schema`.
    config_descriptor: Option<Value>,
    /// Callback invoked with the plugin's durable learned-state document
    /// (`homecore/plugins/{id}/state`, retained + owned by core) whenever it
    /// arrives — once shortly after connect, and again on any core-side change.
    /// Set via [`ManagementHandle::with_state_handler`]; when present the SDK
    /// subscribes to that topic on connect.
    state_handler: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    /// Registered streaming action handlers, indexed by action id.
    streaming_actions: Arc<HashMap<String, StreamingAction>>,
    /// Live streams, keyed by `request_id`. Entries are added on
    /// dispatch and removed after the action closure exits.
    active_streams: streaming::ActiveStreams,
}

impl ManagementHandle {
    /// Install a plugin-specific handler for actions not recognised by the
    /// built-in dispatcher (`ping`, `get_config`, `set_config`, `set_log_level`).
    ///
    /// Return `Some(response)` to handle the action (the SDK fills in
    /// `request_id` automatically), or `None` to fall through to the standard
    /// "unknown action" error.
    pub fn with_custom_handler<F>(mut self, f: F) -> Self
    where
        F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    {
        self.custom_handler = Some(Arc::new(f));
        self
    }

    /// Declare the plugin's capability manifest. The SDK publishes it
    /// retained on `homecore/plugins/{id}/capabilities` after each CONNACK
    /// so reconnects refresh the cached manifest.
    ///
    /// `spec` and `plugin_id` are set by the SDK if empty — callers only
    /// need to provide the `actions` list in most cases.
    pub fn with_capabilities(mut self, mut caps: hc_types::Capabilities) -> Self {
        if caps.spec.is_empty() {
            caps.spec = "1".into();
        }
        if caps.plugin_id.is_empty() {
            caps.plugin_id = self.plugin_id.clone();
        }
        self.capabilities = Some(caps);
        self
    }

    /// Declare the JSON Schema of the plugin's operator config. The SDK injects
    /// it into the published capability manifest as `config_schema`; core serves
    /// it at `GET /plugins/{id}/config/schema` so the config editor can render a
    /// typed form instead of a raw textarea.
    ///
    /// Requires a capability manifest — the schema rides on it, so call
    /// [`with_capabilities`](Self::with_capabilities) too (an empty manifest is
    /// fine). Typically `serde_json::to_value(schemars::schema_for!(MyConfig))`.
    pub fn with_config_schema(mut self, schema: Value) -> Self {
        self.config_schema = Some(schema);
        self
    }

    /// Declare the plugin's own **config descriptor** — an expressive
    /// description of its configuration (sections, field kinds, conditionals,
    /// data sources) that the config editor renders directly, rather than
    /// guessing a form from the JSON Schema.
    ///
    /// Rides on the capability manifest exactly like
    /// [`with_config_schema`](Self::with_config_schema) (so call
    /// [`with_capabilities`](Self::with_capabilities) too); core serves it at
    /// `GET /plugins/{id}/config/descriptor`. Publish the schema as well — the
    /// schema stays authoritative for *existence* and core-side validation,
    /// while the descriptor annotates *intent*.
    pub fn with_config_descriptor(mut self, descriptor: Value) -> Self {
        self.config_descriptor = Some(descriptor);
        self
    }

    /// Register a handler for the plugin's durable learned-state document
    /// (`homecore/plugins/{id}/state`) — vendor secrets the plugin discovers at
    /// runtime (Hue `app_key`s, OAuth tokens, published-device-ids), which core
    /// persists and hands back on connect. The SDK subscribes to the topic and
    /// invokes `f` with the parsed document each time it arrives (once shortly
    /// after connect via the retained value, then on any change).
    ///
    /// Persist updates with [`PluginClient::persist_state`] /
    /// [`PluginStateWriter::persist`].
    pub fn with_state_handler<F>(mut self, f: F) -> Self
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        self.state_handler = Some(Arc::new(f));
        self
    }

    /// Register a handler for a streaming action declared in the
    /// capability manifest. When `homecore/plugins/{id}/manage/cmd`
    /// receives a command whose `action` matches `action.id()`, the SDK
    /// replies `status:"accepted"` and spawns the closure with a fresh
    /// [`StreamContext`]. The closure must emit exactly one terminal
    /// stage before returning.
    pub fn with_streaming_action(mut self, action: StreamingAction) -> Self {
        // Arc<HashMap<_,_>> is immutable after clone; rebuild on add.
        let mut map: HashMap<String, StreamingAction> = (*self.streaming_actions).clone();
        map.insert(action.id.clone(), action);
        self.streaming_actions = Arc::new(map);
        self
    }
}

/// Connection configuration for a plugin.
/// What a device *is*, as its own system reports it.
///
/// Every field optional, because a plugin usually learns them at different
/// times — a bridge names the manufacturer at discovery and the firmware only
/// after the first poll. Absent leaves whatever core already knows alone.
///
/// Nothing in homeCore branches on these. They exist so an operator staring at
/// a device that stopped working can tell which one it is, what it is running,
/// and whether the other three like it are the same model.
#[derive(Debug, Clone, Default)]
pub struct DeviceHardware {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// Firmware, as the device reports it — not any homeCore version.
    pub sw_version: Option<String>,
    /// The device this one sits behind — a bulb's bridge, a node's
    /// controller, an outlet's strip. Advisory; nothing routes through it.
    pub parent_device_id: Option<String>,
}

impl DeviceHardware {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn manufacturer(mut self, v: impl Into<String>) -> Self {
        self.manufacturer = Some(v.into());
        self
    }
    pub fn model(mut self, v: impl Into<String>) -> Self {
        self.model = Some(v.into());
        self
    }
    pub fn sw_version(mut self, v: impl Into<String>) -> Self {
        self.sw_version = Some(v.into());
        self
    }
    /// Say what this device sits behind. The id must be one this plugin also
    /// registers — a bridge, a controller, a strip.
    pub fn behind(mut self, device_id: impl Into<String>) -> Self {
        self.parent_device_id = Some(device_id.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub plugin_id: String,
    pub password: String,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            broker_host: "127.0.0.1".into(),
            broker_port: 1883,
            plugin_id: "plugin.unnamed".into(),
            password: String::new(),
        }
    }
}

/// Callback type invoked when a command arrives for a device.
pub type CommandHandler = Box<dyn Fn(String, Value) + Send + Sync + 'static>;

/// A connected plugin client.
pub struct PluginClient {
    client: AsyncClient,
    eventloop: EventLoop,
    config: PluginConfig,
    subscriptions: SubscriptionTracker,
    devices: DeviceTracker,
    notices: NoticeTracker,
}

/// Cloneable handle for persisting plugin learned-state deltas without holding
/// the `PluginClient` (which `run_managed` consumes). Obtain via
/// [`PluginClient::state_writer`]; safe to move into callbacks.
#[derive(Clone)]
pub struct PluginStateWriter {
    client: AsyncClient,
    plugin_id: String,
}

impl PluginStateWriter {
    /// Create a `PluginStateWriter` for use in unit tests. The underlying MQTT
    /// client points at `127.0.0.1:1883` and won't actually send unless a broker
    /// is running.
    pub fn test_instance(plugin_id: &str) -> Self {
        let mut opts = MqttOptions::new(format!("{plugin_id}-state-test"), "127.0.0.1", 1883);
        opts.set_keep_alive(Duration::from_secs(30));
        let (client, _eventloop) = AsyncClient::new(opts, 8);
        Self {
            client,
            plugin_id: plugin_id.to_string(),
        }
    }

    /// Publish a learned-state delta to `homecore/plugins/{id}/state/set`
    /// (non-retained). Core merges it and re-publishes the retained
    /// authoritative `homecore/plugins/{id}/state`.
    pub async fn persist(&self, delta: &Value) -> Result<()> {
        let topic = format!("homecore/plugins/{}/state/set", self.plugin_id);
        let bytes = serde_json::to_vec(delta).context("serialise state delta")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, bytes)
            .await
            .with_context(|| format!("publish {topic} failed"))
    }
}

impl PluginClient {
    async fn clear_retained_topic(&self, topic: &str) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
            .await
            .with_context(|| format!("clear retained topic failed: {topic}"))
    }

    /// Connect to the HomeCore broker and return a ready client.
    pub async fn connect(config: PluginConfig) -> Result<Self> {
        let mut opts = MqttOptions::new(&config.plugin_id, &config.broker_host, config.broker_port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        // The default max packet size (~10 KB) silently drops a large capability
        // manifest (rich config schema + actions) at the eventloop — the plugin
        // stays connected (heartbeats are tiny) but its schema never publishes.
        // 1 MiB covers any realistic manifest / device payload.
        opts.set_max_packet_size(1024 * 1024, 1024 * 1024);
        if !config.password.is_empty() {
            opts.set_credentials(&config.plugin_id, &config.password);
        }

        let (client, eventloop) = AsyncClient::new(opts, 64);
        info!(plugin_id = %config.plugin_id, "Plugin connecting");
        Ok(Self {
            client,
            eventloop,
            config,
            subscriptions: Arc::new(Mutex::new(HashSet::new())),
            devices: Arc::new(Mutex::new(DeviceTrackerInner::default())),
            notices: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Return the plugin ID.
    pub fn plugin_id(&self) -> &str {
        &self.config.plugin_id
    }

    /// Return a clone of the underlying MQTT client handle.
    pub fn mqtt_client(&self) -> AsyncClient {
        self.client.clone()
    }

    /// Enable cross-restart persistence for the device tracker.
    ///
    /// Builder-style — call once after `connect`, before any
    /// `register_device_full` calls. Loads any previously-saved
    /// device IDs into the in-memory tracker so
    /// [`DevicePublisher::reconcile_devices`] can clean up devices
    /// that disappeared while the plugin was offline.
    ///
    /// Path is typically `<config_dir>/.published-device-ids.json`. The
    /// **plugin id is inserted into the filename** — the caller does not have
    /// to, and must not rely on getting back exactly the path it passed.
    ///
    /// That scoping is load-bearing, not tidiness. Every plugin derives this
    /// path as a sibling of its own config file, and a real deployment keeps
    /// all plugin configs in one directory — so nine plugins were sharing a
    /// single `.published-device-ids.json`. Each start-up read the previous
    /// plugin's device ids, concluded they were its own and now stale,
    /// unregistered them, and wrote the file back containing only its own. The
    /// plugins deleted each other's devices in a loop, which looked like
    /// devices randomly disappearing and coming back.
    pub fn with_device_persistence(self, path: std::path::PathBuf) -> Self {
        let scoped = scoped_device_snapshot_path(&path, self.plugin_id());
        self.devices.lock().unwrap().enable_persistence(scoped);
        self
    }

    // ── Plugin learned-state (D8 write-back) ─────────────────────────────

    /// Persist a learned-state delta to core. Publishes `delta` to
    /// `homecore/plugins/{id}/state/set`; core shallow-merges it into the
    /// durable store and re-publishes the authoritative retained
    /// `homecore/plugins/{id}/state` (delivered to your
    /// [`ManagementHandle::with_state_handler`]). Top-level keys set to `null`
    /// are deleted. Not retained — a one-shot write, not a source of truth.
    pub async fn persist_state(&self, delta: &Value) -> Result<()> {
        self.state_writer().persist(delta).await
    }

    /// A cloneable handle for persisting learned state from anywhere — e.g. a
    /// callback, after `run_managed` has consumed the client by value.
    /// Handle for reporting conditions that stop this plugin working.
    /// Take it before `run()` consumes the client. See [`PluginNotices`].
    pub fn notices(&self) -> PluginNotices {
        PluginNotices {
            inner: Arc::clone(&self.notices),
        }
    }

    pub fn state_writer(&self) -> PluginStateWriter {
        PluginStateWriter {
            client: self.client.clone(),
            plugin_id: self.config.plugin_id.clone(),
        }
    }

    // ── Full state publishing ────────────────────────────────────────────

    /// Publish a full device state update (retained so new subscribers see it).
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        self.publish_state_with_change(device_id, state, None).await
    }

    /// Publish a full device state update with explicit provenance metadata.
    pub async fn publish_state_with_change(
        &self,
        device_id: &str,
        state: &Value,
        change: Option<&DeviceChange>,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = match change {
            Some(change) => serde_json::to_vec(&with_state_change_metadata(state.clone(), change))?,
            None => serde_json::to_vec(state)?,
        };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    /// Publish a full device state caused by an inbound HomeCore command.
    pub async fn publish_state_for_command(
        &self,
        device_id: &str,
        state: &Value,
        command_payload: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let change = change_from_command(command_payload, fallback_source);
        self.publish_state_with_change(device_id, state, Some(&change))
            .await
    }

    // ── Partial state publishing ─────────────────────────────────────────

    /// Publish a partial state update (JSON merge-patch, not retained).
    pub async fn publish_state_partial(&self, device_id: &str, patch: &Value) -> Result<()> {
        self.publish_state_partial_with_change(device_id, patch, None)
            .await
    }

    /// Publish a partial state update with explicit provenance metadata.
    pub async fn publish_state_partial_with_change(
        &self,
        device_id: &str,
        patch: &Value,
        change: Option<&DeviceChange>,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state/partial");
        let payload = match change {
            Some(change) => serde_json::to_vec(&with_state_change_metadata(patch.clone(), change))?,
            None => serde_json::to_vec(patch)?,
        };
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, payload)
            .await
            .context("publish_state_partial failed")
    }

    /// Publish a partial state update caused by an inbound HomeCore command.
    pub async fn publish_state_partial_for_command(
        &self,
        device_id: &str,
        patch: &Value,
        command_payload: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let change = change_from_command(command_payload, fallback_source);
        self.publish_state_partial_with_change(device_id, patch, Some(&change))
            .await
    }

    // ── Availability ─────────────────────────────────────────────────────

    /// Publish `"online"` or `"offline"` to the device's availability topic.
    pub async fn set_available(&self, device_id: &str, available: bool) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/availability");
        let payload = if available { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("set_available failed")
    }

    /// Alias for [`set_available`] — matches the naming used by most plugins.
    pub async fn publish_availability(&self, device_id: &str, online: bool) -> Result<()> {
        self.set_available(device_id, online).await
    }

    // ── Device registration ──────────────────────────────────────────────

    /// Register a device with its capability schema.
    pub async fn register_device(
        &self,
        device_id: &str,
        name: &str,
        capabilities: Value,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.config.plugin_id);
        let payload = serde_json::json!({
            "device_id": device_id,
            "plugin_id": self.config.plugin_id,
            "name": name,
            "capabilities": capabilities,
        });
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&payload)?,
            )
            .await
            .context("register_device failed")?;
        self.devices.lock().unwrap().insert(device_id);
        info!(device_id, "Device registered");
        Ok(())
    }

    /// Register a device by type name.
    ///
    /// Instead of providing a full capability schema, supply a `device_type` string
    /// that HomeCore resolves against its built-in device-type catalog (loaded from
    /// `config/profiles/examples/device-types.toml`).  This is the recommended
    /// registration path for well-known device categories.
    ///
    /// # Example types
    /// `"light"`, `"switch"`, `"motion_sensor"`, `"contact_sensor"`,
    /// `"temperature_sensor"`, `"power_monitor"`, `"cover"`, `"lock"`,
    /// `"climate"`, `"virtual_switch"`, …
    pub async fn register_device_typed(
        &self,
        device_id: &str,
        name: &str,
        device_type: &str,
        area: Option<&str>,
    ) -> Result<()> {
        self.register_device_full(device_id, name, Some(device_type), area, None)
            .await
    }

    /// Register a device with all optional fields: device_type, area, and capabilities.
    ///
    /// This is the most flexible registration method. Use it when you need to
    /// combine a device_type with custom capabilities, or when you need to set
    /// the area alongside capabilities.
    pub async fn register_device_full(
        &self,
        device_id: &str,
        name: &str,
        device_type: Option<&str>,
        area: Option<&str>,
        capabilities: Option<Value>,
    ) -> Result<()> {
        self.register_device_detailed(device_id, name, device_type, area, capabilities, None)
            .await
    }

    /// Register a device, including what the hardware actually is.
    ///
    /// The same as [`Self::register_device_full`] plus [`DeviceHardware`] —
    /// manufacturer, model and firmware, as the upstream system reports them.
    /// homeCore acts on none of it; it is there for the operator looking at a
    /// device that has stopped working and needing to know which one it is and
    /// what it is running.
    ///
    /// A separate method rather than three more arguments, because
    /// `register_device_full` is called by every plugin in the fleet and a
    /// changed signature is a fleet-wide edit for the plugins that have
    /// nothing to add.
    ///
    /// Absent fields leave whatever core already stored alone, so reporting
    /// firmware only after the first poll — the common case — is fine.
    pub async fn register_device_detailed(
        &self,
        device_id: &str,
        name: &str,
        device_type: Option<&str>,
        area: Option<&str>,
        capabilities: Option<Value>,
        hardware: Option<&DeviceHardware>,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.config.plugin_id);
        let mut payload = serde_json::json!({
            "device_id":   device_id,
            "plugin_id":   self.config.plugin_id,
            "name":        name,
        });
        if let Some(dt) = device_type {
            payload["device_type"] = serde_json::Value::String(dt.to_string());
        }
        if let Some(a) = area {
            payload["area"] = serde_json::Value::String(a.to_string());
        }
        if let Some(c) = capabilities {
            payload["capabilities"] = c;
        }
        if let Some(hw) = hardware {
            for (key, value) in [
                ("manufacturer", &hw.manufacturer),
                ("model", &hw.model),
                ("sw_version", &hw.sw_version),
                ("parent_device_id", &hw.parent_device_id),
            ] {
                if let Some(v) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    payload[key] = serde_json::Value::String(v.to_string());
                }
            }
        }
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&payload)?,
            )
            .await
            .context("register_device_full failed")?;
        self.devices.lock().unwrap().insert(device_id);
        info!(device_id, "Device registered");
        Ok(())
    }

    // ── Schema ───────────────────────────────────────────────────────────

    /// Publish a device capability schema (retained) so HomeCore stores it and
    /// API clients can retrieve it via `GET /api/v1/devices/{id}/schema`.
    pub async fn register_device_schema(
        &self,
        device_id: &str,
        schema: &hc_types::DeviceSchema,
    ) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/schema");
        let payload = serde_json::to_vec(schema).context("serialising device schema")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("register_device_schema failed")
    }

    /// Publish a device capability schema built as JSON.
    ///
    /// The route for a schema carrying **action declarations** — see
    /// [`device_actions::with_actions`](crate::device_actions::with_actions).
    /// Deliberately separate from [`Self::register_device_schema`]: the typed
    /// `DeviceSchema` this SDK compiles against is whatever `hc-types` on
    /// `main` says it is, so a plugin declaring actions would otherwise wait on
    /// a core release, an SDK repin and a plugin repin before it could say
    /// anything new. The `Value` costs compile-time checking of the shape,
    /// which the builders give back.
    pub async fn register_device_schema_json(&self, device_id: &str, schema: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/schema");
        let payload = serde_json::to_vec(schema).context("serialising device schema")?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("register_device_schema_json failed")
    }

    // ── Unregister ───────────────────────────────────────────────────────

    /// Retire a device from HomeCore by clearing retained topics and publishing
    /// a plugin-scoped unregister command.
    pub async fn unregister_device(&self, device_id: &str) -> Result<()> {
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/state"))
            .await?;
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/availability"))
            .await?;
        self.clear_retained_topic(&format!("homecore/devices/{device_id}/schema"))
            .await?;
        self.client
            .publish(
                format!("homecore/plugins/{}/unregister", self.config.plugin_id),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&serde_json::json!({ "device_id": device_id }))?,
            )
            .await
            .context("unregister_device failed")?;
        self.devices.lock().unwrap().remove(device_id);
        info!(device_id, "Device unregistered");
        Ok(())
    }

    // ── Plugin status ────────────────────────────────────────────────────

    /// Publish plugin status (`"active"`, `"degraded"`, `"offline"`) to
    /// `homecore/plugins/{id}/status` (retained).
    pub async fn publish_plugin_status(&self, status: &str) -> Result<()> {
        let topic = format!("homecore/plugins/{}/status", self.config.plugin_id);
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, status.as_bytes())
            .await
            .context("publish_plugin_status failed")
    }

    // ── Events ───────────────────────────────────────────────────────────

    /// Publish a structured event to `homecore/events/{event_type}`.
    pub async fn publish_event(&self, event_type: &str, payload: &Value) -> Result<()> {
        let topic = format!("homecore/events/{event_type}");
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(payload)?,
            )
            .await
            .context("publish_event failed")
    }

    // ── Publisher handle ─────────────────────────────────────────────────

    /// Return a [`DevicePublisher`] that can publish state concurrently with `run()`.
    ///
    /// Call this **before** `run()` — `run()` consumes `self`, so any handles
    /// must be obtained first.  The returned publisher is `Clone`.
    pub fn device_publisher(&self) -> DevicePublisher {
        DevicePublisher {
            client: self.client.clone(),
            plugin_id: self.config.plugin_id.clone(),
            subscriptions: Arc::clone(&self.subscriptions),
            devices: Arc::clone(&self.devices),
        }
    }

    // ── Management ───────────────────────────────────────────────────────

    /// Enable the management protocol: heartbeat publisher + command listener.
    ///
    /// Call this **before** `run()`.  The heartbeat is published every
    /// `interval_secs` seconds to `homecore/plugins/{id}/heartbeat`.
    /// Management commands arrive on `homecore/plugins/{id}/manage/cmd` and are
    /// dispatched inside `run()` via the provided callbacks.
    ///
    /// `config_path` is the plugin's config file path — used to implement
    /// `get_config` and `set_config` commands automatically.
    pub async fn enable_management(
        &self,
        interval_secs: u64,
        version: Option<String>,
        config_path: Option<String>,
        log_level_handle: Option<hc_logging::LogLevelHandle>,
    ) -> Result<ManagementHandle> {
        // Track management subscription for reconnect.
        let mgmt_topic = format!("homecore/plugins/{}/manage/cmd", self.config.plugin_id);
        self.client
            .subscribe(&mgmt_topic, QoS::AtLeastOnce)
            .await
            .context("subscribe management/cmd failed")?;
        self.subscriptions.lock().unwrap().insert(mgmt_topic);

        // Spawn heartbeat publisher.
        let hb_client = self.client.clone();
        let hb_plugin_id = self.config.plugin_id.clone();
        let hb_version = version.clone();
        let hb_devices = Arc::clone(&self.devices);
        let hb_notices = Arc::clone(&self.notices);
        let started_at = std::time::Instant::now();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                let uptime_secs = started_at.elapsed().as_secs();
                let device_count = hb_devices.lock().unwrap().len() as u64;
                let payload = serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "version": hb_version,
                    // This crate's own version. Informational only: it tells
                    // an operator which SDK to rebuild against, and appears in
                    // core's logs beside any protocol complaint.
                    //
                    // It is deliberately NOT what core checks compatibility on.
                    // It used to be, compared against hc_types::PROTOCOL_VERSION
                    // — but this crate and hc-types are versioned independently
                    // (0.3.x against 0.1.x), and below 1.0 a differing MINOR
                    // reads as breaking, so the check could never pass and
                    // warned about every plugin forever.
                    "sdk_version": env!("CARGO_PKG_VERSION"),
                    // The wire protocol this plugin was compiled against — the
                    // hc-types version, which is what actually decides whether
                    // core and this plugin agree on the shape of a device, an
                    // event, or a command. Core compares it against its own.
                    "protocol_version": hc_types::PROTOCOL_VERSION,
                    "uptime_secs": uptime_secs,
                    "device_count": device_count,
                    // Full current set every beat — core replaces rather than
                    // merges, so a cleared condition disappears on its own and
                    // there is nothing to expire.
                    "notices": hb_notices.lock().unwrap().clone(),
                });
                let topic = format!("homecore/plugins/{hb_plugin_id}/heartbeat");
                let _ = hb_client
                    .publish(
                        &topic,
                        QoS::AtMostOnce,
                        false,
                        serde_json::to_vec(&payload).unwrap_or_default(),
                    )
                    .await;
            }
        });

        info!(plugin_id = %self.config.plugin_id, "Management protocol enabled (heartbeat every {interval_secs}s)");
        Ok(ManagementHandle {
            plugin_id: self.config.plugin_id.clone(),
            config_path,
            log_level_handle,
            custom_handler: None,
            capabilities: None,
            config_schema: None,
            config_descriptor: None,
            state_handler: None,
            streaming_actions: Arc::new(HashMap::new()),
            active_streams: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    // ── Command subscriptions ────────────────────────────────────────────

    /// Subscribe to command messages for a device.
    ///
    /// The subscription is tracked and automatically restored on MQTT reconnect
    /// (clean_session=true loses subscriptions on disconnect).
    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("subscribe_commands failed")?;
        self.subscriptions.lock().unwrap().insert(topic);
        debug!(device_id, "Subscribed to commands");
        Ok(())
    }

    // ── External state subscription (cross-device consumer plugins) ─────

    /// Subscribe to state updates for a device this plugin does **not** own.
    ///
    /// Used by cross-device consumer plugins (e.g. thermostats observing
    /// external temperature sensors). The subscription is tracked and
    /// automatically restored on MQTT reconnect, just like
    /// [`subscribe_commands`].
    ///
    /// Use [`run_managed_with_state`] to receive these messages in a callback.
    pub async fn subscribe_state(&self, device_id: &str) -> Result<()> {
        let topic_full = format!("homecore/devices/{device_id}/state");
        let topic_partial = format!("homecore/devices/{device_id}/state/partial");
        self.client
            .subscribe(&topic_full, QoS::AtLeastOnce)
            .await
            .context("subscribe_state (full) failed")?;
        self.client
            .subscribe(&topic_partial, QoS::AtLeastOnce)
            .await
            .context("subscribe_state (partial) failed")?;
        {
            let mut subs = self.subscriptions.lock().unwrap();
            subs.insert(topic_full);
            subs.insert(topic_partial);
        }
        debug!(
            device_id,
            "Subscribed to external device state (full + partial)"
        );
        Ok(())
    }

    /// Remove a subscription previously added with [`subscribe_state`].
    pub async fn unsubscribe_state(&self, device_id: &str) -> Result<()> {
        let topic_full = format!("homecore/devices/{device_id}/state");
        let topic_partial = format!("homecore/devices/{device_id}/state/partial");
        let _ = self.client.unsubscribe(&topic_full).await;
        let _ = self.client.unsubscribe(&topic_partial).await;
        let mut subs = self.subscriptions.lock().unwrap();
        subs.remove(&topic_full);
        subs.remove(&topic_partial);
        debug!(device_id, "Unsubscribed from external device state");
        Ok(())
    }

    // ── Event loop ───────────────────────────────────────────────────────

    /// Drive the MQTT event loop, calling `on_command` whenever a `cmd`
    /// message arrives for any subscribed device.
    ///
    /// This method blocks until the connection is lost or an error occurs.
    pub async fn run<F>(mut self, on_command: F) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.run_inner(on_command, noop_state_cb, None).await
    }

    /// Like [`run`], but also handles management protocol commands (heartbeat
    /// responses, config read/write, log level changes).
    ///
    /// Pass the [`ManagementHandle`] returned by [`enable_management`].
    pub async fn run_managed<F>(mut self, on_command: F, mgmt: ManagementHandle) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.run_inner(on_command, noop_state_cb, Some(mgmt)).await
    }

    /// Like [`run_managed`], but additionally delivers state updates for any
    /// device subscribed to via [`subscribe_state`] into `on_state`.
    ///
    /// Use for cross-device consumer plugins (e.g. thermostat observes sensors).
    pub async fn run_managed_with_state<F, S>(
        mut self,
        on_command: F,
        on_state: S,
        mgmt: ManagementHandle,
    ) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
        S: Fn(String, Value) + Send + Sync + 'static,
    {
        self.run_inner(on_command, on_state, Some(mgmt)).await
    }

    async fn run_inner<F, S>(
        &mut self,
        on_command: F,
        on_state: S,
        mgmt: Option<ManagementHandle>,
    ) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
        S: Fn(String, Value) + Send + Sync + 'static,
    {
        let plugin_id = self.config.plugin_id.clone();
        let subs = Arc::clone(&self.subscriptions);
        info!(plugin_id = %plugin_id, "Plugin event loop starting");
        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    info!("Plugin connected to broker");
                    // Subscribe to the system-wide TZ topic. Retained, so the
                    // payload arrives within ms of connect; the message
                    // handler below applies it via `hc_time::init`. Plugin
                    // tracing init runs before this point, so the very first
                    // log lines render in UTC and auto-swap once the message
                    // lands. Use QoS 0 — late delivery is fine.
                    if let Err(e) = self
                        .client
                        .subscribe("homecore/system/tz", QoS::AtMostOnce)
                        .await
                    {
                        warn!(error = %e, "Failed to subscribe homecore/system/tz");
                    }
                    // Re-subscribe to all tracked topics on every (re)connect.
                    // With clean_session=true, subscriptions are lost on reconnect.
                    let topics: Vec<String> = subs.lock().unwrap().iter().cloned().collect();
                    for topic in &topics {
                        if let Err(e) = self
                            .client
                            .subscribe(topic.as_str(), QoS::AtLeastOnce)
                            .await
                        {
                            error!(topic, error = %e, "Failed to re-subscribe on reconnect");
                        }
                    }
                    if !topics.is_empty() {
                        info!(
                            count = topics.len(),
                            "Re-subscribed to {} topics",
                            topics.len()
                        );
                    }
                    // Republish capability manifest retained, if declared.
                    // Retained so late-joining core instances still see it.
                    if let Some(ref mgmt) = mgmt {
                        // Publish the manifest when the plugin declared capabilities
                        // OR a config schema — the schema rides on the manifest, so a
                        // schema-only plugin (no actions) still needs it published.
                        if mgmt.capabilities.is_some()
                            || mgmt.config_schema.is_some()
                            || mgmt.config_descriptor.is_some()
                        {
                            let topic = format!("homecore/plugins/{}/capabilities", mgmt.plugin_id);
                            // Synthesize an empty manifest for a schema-only plugin.
                            let synthesized;
                            let caps = match mgmt.capabilities {
                                Some(ref c) => c,
                                None => {
                                    synthesized = hc_types::Capabilities {
                                        spec: "1".into(),
                                        plugin_id: mgmt.plugin_id.clone(),
                                        actions: Vec::new(),
                                    };
                                    &synthesized
                                }
                            };
                            // The config schema rides on the manifest JSON (core
                            // extracts it from the raw payload).
                            let manifest = build_capability_manifest(
                                caps,
                                mgmt.config_schema.as_ref(),
                                mgmt.config_descriptor.as_ref(),
                            );
                            if !manifest.is_null() {
                                let bytes = serde_json::to_vec(&manifest).unwrap_or_default();
                                if let Err(e) = self
                                    .client
                                    .publish(&topic, QoS::AtLeastOnce, true, bytes)
                                    .await
                                {
                                    warn!(error = %e, "Failed to publish capabilities");
                                }
                            } else {
                                warn!("Failed to serialise capabilities");
                            }
                        }

                        // Learned-state: subscribe to the retained doc core owns.
                        if mgmt.state_handler.is_some() {
                            let topic = format!("homecore/plugins/{}/state", mgmt.plugin_id);
                            if let Err(e) = self.client.subscribe(&topic, QoS::AtLeastOnce).await {
                                warn!(error = %e, "Failed to subscribe plugin state");
                            }
                        }
                    }
                }
                Ok(rumqttc::Event::Incoming(Packet::Publish(p))) => {
                    let parts: Vec<&str> = p.topic.split('/').collect();

                    // homecore/system/tz — retained IANA zone name from
                    // core. Apply via hc_time::init so the tracing
                    // subscriber's ConfiguredTzTime formatter starts
                    // emitting in the operator's zone on the next log
                    // event (no subscriber rebuild needed — hc-time uses
                    // a RwLock<Tz> updated in place).
                    if p.topic == "homecore/system/tz" {
                        match std::str::from_utf8(&p.payload) {
                            Ok(name) => {
                                let trimmed = name.trim();
                                match hc_time::parse_iana(trimmed) {
                                    Ok(tz) => {
                                        hc_time::init(tz);
                                        info!(tz = trimmed, "Applied TZ from homecore/system/tz");
                                    }
                                    Err(e) => {
                                        warn!(payload = trimmed, error = %e, "Bad TZ in homecore/system/tz")
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "Non-UTF8 homecore/system/tz payload"),
                        }
                        continue;
                    }

                    // homecore/devices/{id}/cmd
                    if parts.len() == 4
                        && parts[0] == "homecore"
                        && parts[1] == "devices"
                        && parts[3] == "cmd"
                    {
                        let device_id = parts[2].to_string();
                        match serde_json::from_slice::<Value>(&p.payload) {
                            Ok(cmd) => on_command(device_id, cmd),
                            Err(e) => warn!(topic = %p.topic, error = %e, "Non-JSON cmd payload"),
                        }
                        continue;
                    }

                    // homecore/devices/{id}/state (full state, for subscribe_state consumers)
                    if parts.len() == 4
                        && parts[0] == "homecore"
                        && parts[1] == "devices"
                        && parts[3] == "state"
                    {
                        let device_id = parts[2].to_string();
                        match serde_json::from_slice::<Value>(&p.payload) {
                            Ok(state) => on_state(device_id, state),
                            Err(e) => warn!(topic = %p.topic, error = %e, "Non-JSON state payload"),
                        }
                        continue;
                    }

                    // homecore/devices/{id}/state/partial (merge patch — delivered
                    // to the same callback so cross-device consumers that read
                    // specific attributes see updates between full-state pushes).
                    if parts.len() == 5
                        && parts[0] == "homecore"
                        && parts[1] == "devices"
                        && parts[3] == "state"
                        && parts[4] == "partial"
                    {
                        let device_id = parts[2].to_string();
                        match serde_json::from_slice::<Value>(&p.payload) {
                            Ok(state) => on_state(device_id, state),
                            Err(e) => {
                                warn!(topic = %p.topic, error = %e, "Non-JSON state/partial payload")
                            }
                        }
                        continue;
                    }

                    // homecore/plugins/{id}/state — durable learned-state doc,
                    // owned + retained by core. Delivered to the state handler.
                    if let Some(ref mgmt) = mgmt {
                        if parts.len() == 4
                            && parts[0] == "homecore"
                            && parts[1] == "plugins"
                            && parts[2] == mgmt.plugin_id
                            && parts[3] == "state"
                        {
                            if let Some(ref handler) = mgmt.state_handler {
                                // Empty retained payload = core cleared it
                                // (e.g. on deregister) — nothing to deliver.
                                if !p.payload.is_empty() {
                                    match serde_json::from_slice::<Value>(&p.payload) {
                                        Ok(doc) => handler(doc),
                                        Err(e) => {
                                            warn!(topic = %p.topic, error = %e, "Non-JSON plugin state payload")
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                    }

                    // homecore/plugins/{id}/manage/cmd
                    if let Some(ref mgmt) = mgmt {
                        if parts.len() == 5
                            && parts[0] == "homecore"
                            && parts[1] == "plugins"
                            && parts[3] == "manage"
                            && parts[4] == "cmd"
                        {
                            if let Ok(cmd) = serde_json::from_slice::<Value>(&p.payload) {
                                let resp = dispatch_management_cmd(mgmt, &self.client, &cmd).await;
                                let resp_topic =
                                    format!("homecore/plugins/{}/manage/response", mgmt.plugin_id);
                                let _ = self
                                    .client
                                    .publish(
                                        &resp_topic,
                                        QoS::AtLeastOnce,
                                        false,
                                        serde_json::to_vec(&resp).unwrap_or_default(),
                                    )
                                    .await;
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "Plugin MQTT error; retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

/// Serialise the capability manifest and inject `config_schema` (which rides on
/// the manifest JSON, not the frozen `Capabilities` type). Returns `Value::Null`
/// if `caps` fails to serialise.
fn build_capability_manifest(
    caps: &hc_types::Capabilities,
    config_schema: Option<&Value>,
    config_descriptor: Option<&Value>,
) -> Value {
    match serde_json::to_value(caps) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                if let Some(schema) = config_schema {
                    obj.insert("config_schema".into(), schema.clone());
                }
                if let Some(descriptor) = config_descriptor {
                    obj.insert("config_descriptor".into(), descriptor.clone());
                }
            }
            v
        }
        Err(_) => Value::Null,
    }
}

/// Top-level management-cmd dispatcher. Handles streaming actions
/// (`action` matches a registered `StreamingAction`), `cancel`, and
/// `respond` before falling through to the sync built-in handler.
async fn dispatch_management_cmd(
    mgmt: &ManagementHandle,
    client: &AsyncClient,
    cmd: &Value,
) -> Value {
    let action = cmd["action"].as_str().unwrap_or("").to_string();
    let request_id = cmd["request_id"].as_str().unwrap_or("").to_string();

    // `cancel` — flip the cancel flag on the targeted active stream.
    if action == "cancel" {
        let Some(target) = cmd["target_request_id"].as_str() else {
            return json!({
                "request_id": request_id,
                "status": "error",
                "error": "cancel requires target_request_id",
            });
        };
        let found = {
            let map = mgmt.active_streams.lock().unwrap();
            if let Some(entry) = map.get(target) {
                entry
                    .cancel
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                true
            } else {
                false
            }
        };
        if found {
            return json!({ "request_id": request_id, "status": "ok" });
        } else {
            return json!({
                "request_id": request_id,
                "status": "error",
                "error": "no active stream for target_request_id",
            });
        }
    }

    // `respond` — deliver response payload to the targeted active stream.
    if action == "respond" {
        let Some(target) = cmd["target_request_id"].as_str() else {
            return json!({
                "request_id": request_id,
                "status": "error",
                "error": "respond requires target_request_id",
            });
        };
        let response = cmd
            .get("response")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let delivered = {
            let map = mgmt.active_streams.lock().unwrap();
            match map.get(target) {
                Some(entry) => entry.respond_tx.send(response).is_ok(),
                None => false,
            }
        };
        if delivered {
            return json!({ "request_id": request_id, "status": "ok" });
        } else {
            return json!({
                "request_id": request_id,
                "status": "error",
                "error": "no active awaiting_user stream for target_request_id",
            });
        }
    }

    // Streaming action match?
    if let Some(streaming_action) = mgmt.streaming_actions.get(&action) {
        return dispatch_streaming_action(mgmt, client, streaming_action, cmd).await;
    }

    // Fall through to synchronous built-ins + custom handler.
    handle_management_cmd(mgmt, cmd)
}

/// Dispatch a streaming action: register state, spawn the closure, and
/// return the sync `accepted` reply with the stream topic.
async fn dispatch_streaming_action(
    mgmt: &ManagementHandle,
    client: &AsyncClient,
    action: &StreamingAction,
    cmd: &Value,
) -> Value {
    let request_id = cmd["request_id"].as_str().unwrap_or("").to_string();
    if request_id.is_empty() {
        return json!({
            "status": "error",
            "error": "streaming action requires request_id on the command",
        });
    }

    // Extract params: everything except action/request_id/target_request_id.
    let params = {
        let mut p = cmd.clone();
        if let Some(obj) = p.as_object_mut() {
            obj.remove("action");
            obj.remove("request_id");
            obj.remove("target_request_id");
        }
        p
    };

    let stream_topic = format!(
        "homecore/plugins/{}/commands/{}/events",
        mgmt.plugin_id, request_id
    );
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let terminal = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (respond_tx, respond_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

    {
        let mut map = mgmt.active_streams.lock().unwrap();
        map.insert(
            request_id.clone(),
            streaming::ActiveStreamEntry {
                cancel: Arc::clone(&cancel),
                respond_tx,
            },
        );
    }

    let ctx = StreamContext::new(
        request_id.clone(),
        mgmt.plugin_id.clone(),
        action.id.clone(),
        client.clone(),
        Arc::clone(&cancel),
        Arc::clone(&terminal),
        respond_rx,
    );

    let handler = Arc::clone(&action.handler);
    let registry = Arc::clone(&mgmt.active_streams);
    let rid_clone = request_id.clone();
    let topic_clone = stream_topic.clone();
    let terminal_clone = Arc::clone(&terminal);
    let client_clone = client.clone();

    tokio::spawn(async move {
        let result = handler(ctx, params).await;
        streaming::finalize_stream(
            &client_clone,
            &topic_clone,
            &rid_clone,
            &terminal_clone,
            result,
        )
        .await;
        // Clean up registry entry after finalize — any late cancel/respond
        // after terminal becomes a no-op.
        let mut map = registry.lock().unwrap();
        map.remove(&rid_clone);
    });

    json!({
        "request_id": request_id,
        "status": "accepted",
        "stream_topic": stream_topic,
    })
}

/// Handle a management command and return a JSON response.
fn handle_management_cmd(mgmt: &ManagementHandle, cmd: &Value) -> Value {
    let action = cmd["action"].as_str().unwrap_or("");
    let request_id = cmd["request_id"].as_str().unwrap_or("").to_string();

    match action {
        "ping" => serde_json::json!({
            "request_id": request_id,
            "status": "ok",
        }),
        "get_config" => {
            if let Some(ref path) = mgmt.config_path {
                match std::fs::read_to_string(path) {
                    Ok(content) => serde_json::json!({
                        "request_id": request_id,
                        "status": "ok",
                        "data": content,
                    }),
                    Err(e) => serde_json::json!({
                        "request_id": request_id,
                        "status": "error",
                        "error": format!("failed to read config: {e}"),
                    }),
                }
            } else {
                serde_json::json!({
                    "request_id": request_id,
                    "status": "error",
                    "error": "no config path configured",
                })
            }
        }
        "set_config" => {
            if let Some(ref path) = mgmt.config_path {
                let config_str = if let Some(s) = cmd["config"].as_str() {
                    s.to_string()
                } else if let Some(obj) = cmd["config"].as_object() {
                    // JSON object → TOML
                    let toml_val: toml::Value =
                        match serde_json::from_value(Value::Object(obj.clone())) {
                            Ok(v) => v,
                            Err(e) => {
                                return serde_json::json!({
                                    "request_id": request_id,
                                    "status": "error",
                                    "error": format!("invalid config: {e}"),
                                })
                            }
                        };
                    toml::to_string_pretty(&toml_val).unwrap_or_default()
                } else {
                    return serde_json::json!({
                        "request_id": request_id,
                        "status": "error",
                        "error": "missing 'config' field",
                    });
                };
                match std::fs::write(path, &config_str) {
                    Ok(()) => serde_json::json!({
                        "request_id": request_id,
                        "status": "ok",
                    }),
                    Err(e) => serde_json::json!({
                        "request_id": request_id,
                        "status": "error",
                        "error": format!("failed to write config: {e}"),
                    }),
                }
            } else {
                serde_json::json!({
                    "request_id": request_id,
                    "status": "error",
                    "error": "no config path configured",
                })
            }
        }
        "set_log_level" => {
            let level = cmd["level"].as_str().unwrap_or("info");
            if let Some(ref handle) = mgmt.log_level_handle {
                match handle.set_level(level) {
                    Ok(()) => {
                        info!(level, "Management: log level changed dynamically");
                        serde_json::json!({
                            "request_id": request_id,
                            "status": "ok",
                        })
                    }
                    Err(e) => serde_json::json!({
                        "request_id": request_id,
                        "status": "error",
                        "error": e,
                    }),
                }
            } else {
                info!(
                    level,
                    "Management: log level change requested (no reload handle; requires restart)"
                );
                serde_json::json!({
                    "request_id": request_id,
                    "status": "ok",
                    "note": "log level change acknowledged; restart required to take effect",
                })
            }
        }
        _ => {
            if let Some(ref h) = mgmt.custom_handler {
                if let Some(mut resp) = h(cmd) {
                    resp["request_id"] = Value::String(request_id);
                    return resp;
                }
            }
            serde_json::json!({
                "request_id": request_id,
                "status": "error",
                "error": format!("unknown action: {action}"),
            })
        }
    }
}

#[cfg(test)]
mod device_snapshot_path_tests {
    use super::*;
    use std::path::Path;

    /// The bug this guards: every plugin derives its snapshot path as a sibling
    /// of its own config, and real deployments keep all plugin configs in one
    /// directory — so the unscoped name collided and plugins unregistered each
    /// other's devices.
    #[test]
    fn two_plugins_in_one_config_dir_get_different_files() {
        let shared = Path::new("/config/plugins/.published-device-ids.json");
        let hue = scoped_device_snapshot_path(shared, "plugin.hue");
        let sonos = scoped_device_snapshot_path(shared, "plugin.sonos");
        assert_ne!(hue, sonos);
        assert_eq!(
            hue,
            Path::new("/config/plugins/.published-device-ids.plugin.hue.json")
        );
    }

    /// Plugin ids contain dots, so a file_stem/extension implementation would
    /// treat "hue" as the extension and mangle the name.
    #[test]
    fn dotted_plugin_id_is_not_mistaken_for_an_extension() {
        let p =
            scoped_device_snapshot_path(Path::new("/c/.published-device-ids.json"), "plugin.hue");
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("json"));
    }

    #[test]
    fn scoping_is_idempotent() {
        let once =
            scoped_device_snapshot_path(Path::new("/c/.published-device-ids.json"), "plugin.hue");
        let twice = scoped_device_snapshot_path(&once, "plugin.hue");
        assert_eq!(once, twice);
    }
}

#[cfg(test)]
mod config_schema_tests {
    use super::*;

    fn caps() -> hc_types::Capabilities {
        hc_types::Capabilities {
            spec: "1".into(),
            plugin_id: "plugin.hue".into(),
            actions: vec![],
        }
    }

    #[test]
    fn manifest_injects_config_schema_when_present() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "poll_interval_secs": { "type": "integer" } }
        });
        let m = build_capability_manifest(&caps(), Some(&schema), None);
        assert_eq!(m["plugin_id"], "plugin.hue");
        assert_eq!(m["spec"], "1");
        assert_eq!(m["config_schema"], schema);
    }

    #[test]
    fn manifest_omits_config_schema_when_absent() {
        let m = build_capability_manifest(&caps(), None, None);
        assert!(m.get("config_schema").is_none());
        // Base manifest still intact.
        assert_eq!(m["plugin_id"], "plugin.hue");
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;
    use hc_types::NoticeLevel;

    fn handle() -> PluginNotices {
        PluginNotices {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn raise_dedupes_on_code() {
        // The motivating shape: a plugin re-detects the same condition on every
        // poll and re-raises it. That must update in place, not accumulate — a
        // 30-second heartbeat would otherwise grow the payload without bound.
        let n = handle();
        n.raise(PluginNotice::warning("receiver_unreachable", "first"));
        n.raise(PluginNotice::warning("receiver_unreachable", "second"));
        n.raise(PluginNotice::error("other", "x"));
        let cur = n.current();
        assert_eq!(cur.len(), 2);
        let first = cur
            .iter()
            .find(|x| x.code == "receiver_unreachable")
            .unwrap();
        assert_eq!(first.message, "second", "re-raise must replace, not append");
    }

    #[test]
    fn clear_is_idempotent_and_targeted() {
        // Callers clear unconditionally on the healthy branch, so clearing
        // something never raised must be a no-op rather than a panic.
        let n = handle();
        n.clear("never_raised");
        n.raise(PluginNotice::warning("a", "m"));
        n.raise(PluginNotice::warning("b", "m"));
        n.clear("a");
        n.clear("a");
        let codes: Vec<String> = n.current().into_iter().map(|x| x.code).collect();
        assert_eq!(codes, vec!["b"]);
    }

    #[test]
    fn set_replaces_wholesale() {
        let n = handle();
        n.raise(PluginNotice::warning("old", "m"));
        n.set(vec![PluginNotice::info("new", "m")]);
        let cur = n.current();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].code, "new");
        assert_eq!(cur[0].level, NoticeLevel::Info);
    }

    #[test]
    fn clones_share_one_set() {
        // The handle is cloned into spawned tasks; a condition detected there
        // has to reach the heartbeat task holding a different clone.
        let a = handle();
        let b = a.clone();
        b.raise(PluginNotice::error("from_task", "m"));
        assert_eq!(a.current().len(), 1);
        a.clear("from_task");
        assert!(b.current().is_empty());
    }

    #[test]
    fn empty_set_serialises_as_an_empty_array() {
        // What every plugin that never raises anything sends. Core reads it as
        // "nothing to report", identical to omitting the field.
        let n = handle();
        assert_eq!(serde_json::to_string(&n.current()).unwrap(), "[]");
    }

    #[test]
    fn remedy_survives_the_wire() {
        let n = handle();
        n.raise(PluginNotice::warning("receiver_unreachable", "msg").with_remedy("set bind_addr"));
        let json = serde_json::to_string(&n.current()).unwrap();
        let back: Vec<PluginNotice> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].remedy.as_deref(), Some("set bind_addr"));
    }
}
