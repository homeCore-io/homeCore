//! `plugin-sdk-rs` — Rust SDK for HomeCore device plugins.
//!
//! Provides:
//! - [`PluginClient`] — connects to the broker, handles registration, typed
//!   publish/subscribe helpers, and a command callback loop.
//! - [`DevicePublisher`] — cloneable handle for publishing state from spawned tasks.
//! - [`ManagementHandle`] — enable heartbeat + remote config/log management.

use anyhow::{Context, Result};
use hc_types::device::{change_from_command_payload, with_state_change_metadata, DeviceChange};
use rumqttc::{AsyncClient, EventLoop, MqttOptions, Packet, QoS};
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// A cloneable handle for publishing device state from outside the `run()` loop.
///
/// Obtained via [`PluginClient::device_publisher`] before calling `run()`.
#[derive(Clone)]
pub struct DevicePublisher {
    client: AsyncClient,
    plugin_id: String,
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
            .context("unregister_device failed")
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
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, serde_json::to_vec(&payload)?)
            .await
            .context("DevicePublisher::register_device_full failed")
    }

    /// Subscribe to command messages for a device.
    ///
    /// This mirrors [`PluginClient::subscribe_commands`] but can be called
    /// from spawned tasks that only hold a `DevicePublisher` handle.
    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("DevicePublisher::subscribe_commands failed")
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
        }
    }
}

/// Handle returned by [`PluginClient::enable_management`].
///
/// Pass this to [`PluginClient::run_managed`] to automatically handle
/// `get_config`, `set_config`, and `set_log_level` management commands.
#[derive(Clone)]
pub struct ManagementHandle {
    plugin_id: String,
    config_path: Option<String>,
    log_level_handle: Option<hc_logging::LogLevelHandle>,
}

/// Connection configuration for a plugin.
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
        if !config.password.is_empty() {
            opts.set_credentials(&config.plugin_id, &config.password);
        }

        let (client, eventloop) = AsyncClient::new(opts, 64);
        info!(plugin_id = %config.plugin_id, "Plugin connecting");
        Ok(Self {
            client,
            eventloop,
            config,
        })
    }

    /// Return the plugin ID.
    pub fn plugin_id(&self) -> &str {
        &self.config.plugin_id
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
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&payload)?,
            )
            .await
            .context("register_device_full failed")?;
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
        // Subscribe to management command topic.
        let topic = format!("homecore/plugins/{}/manage/cmd", self.config.plugin_id);
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("subscribe management/cmd failed")?;

        // Spawn heartbeat publisher.
        let hb_client = self.client.clone();
        let hb_plugin_id = self.config.plugin_id.clone();
        let hb_version = version.clone();
        let started_at = std::time::Instant::now();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                let uptime_secs = started_at.elapsed().as_secs();
                let payload = serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "version": hb_version,
                    "uptime_secs": uptime_secs,
                });
                let topic = format!("homecore/plugins/{hb_plugin_id}/heartbeat");
                let _ = hb_client
                    .publish(&topic, QoS::AtMostOnce, false, serde_json::to_vec(&payload).unwrap_or_default())
                    .await;
            }
        });

        info!(plugin_id = %self.config.plugin_id, "Management protocol enabled (heartbeat every {interval_secs}s)");
        Ok(ManagementHandle {
            plugin_id: self.config.plugin_id.clone(),
            config_path,
            log_level_handle,
        })
    }

    // ── Command subscriptions ────────────────────────────────────────────

    /// Subscribe to command messages for a device.
    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("subscribe_commands failed")?;
        debug!(device_id, "Subscribed to commands");
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
        self.run_inner(on_command, None).await
    }

    /// Like [`run`], but also handles management protocol commands (heartbeat
    /// responses, config read/write, log level changes).
    ///
    /// Pass the [`ManagementHandle`] returned by [`enable_management`].
    pub async fn run_managed<F>(mut self, on_command: F, mgmt: ManagementHandle) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.run_inner(on_command, Some(mgmt)).await
    }

    async fn run_inner<F>(
        &mut self,
        on_command: F,
        mgmt: Option<ManagementHandle>,
    ) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        let plugin_id = self.config.plugin_id.clone();
        info!(plugin_id = %plugin_id, "Plugin event loop starting");
        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    info!("Plugin connected to broker");
                }
                Ok(rumqttc::Event::Incoming(Packet::Publish(p))) => {
                    let parts: Vec<&str> = p.topic.split('/').collect();

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

                    // homecore/plugins/{id}/manage/cmd
                    if let Some(ref mgmt) = mgmt {
                        if parts.len() == 5
                            && parts[0] == "homecore"
                            && parts[1] == "plugins"
                            && parts[3] == "manage"
                            && parts[4] == "cmd"
                        {
                            if let Ok(cmd) = serde_json::from_slice::<Value>(&p.payload) {
                                let resp = handle_management_cmd(mgmt, &cmd);
                                let resp_topic = format!(
                                    "homecore/plugins/{}/manage/response",
                                    mgmt.plugin_id
                                );
                                let _ = self.client
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
                    let toml_val: toml::Value = match serde_json::from_value(Value::Object(obj.clone())) {
                        Ok(v) => v,
                        Err(e) => return serde_json::json!({
                            "request_id": request_id,
                            "status": "error",
                            "error": format!("invalid config: {e}"),
                        }),
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
                info!(level, "Management: log level change requested (no reload handle; requires restart)");
                serde_json::json!({
                    "request_id": request_id,
                    "status": "ok",
                    "note": "log level change acknowledged; restart required to take effect",
                })
            }
        }
        _ => serde_json::json!({
            "request_id": request_id,
            "status": "error",
            "error": format!("unknown action: {action}"),
        }),
    }
}
