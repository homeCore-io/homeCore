//! `plugin-sdk-rs` — Rust SDK for HomeCore device plugins.
//!
//! Provides:
//! - [`PluginClient`] — connects to the broker, handles registration, typed
//!   publish/subscribe helpers, and a command callback loop.
//! - [`DeviceRegistration`] — fluent builder for capability schemas.

use anyhow::{Context, Result};
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
}

impl DevicePublisher {
    /// Publish a full device state to `homecore/devices/{device_id}/state` (retained).
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = serde_json::to_vec(state)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    /// Publish `"online"` or `"offline"` to the device's availability topic (retained).
    /// Mirrors [`PluginClient::set_available`] for use from spawned tasks that hold
    /// only a [`DevicePublisher`] handle.
    pub async fn set_available(&self, device_id: &str, available: bool) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/availability");
        let payload = if available { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("DevicePublisher::set_available failed")
    }

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
            .context("DevicePublisher::register_device_schema failed")
    }
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

    /// Publish a full device state update (retained so new subscribers see it).
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = serde_json::to_vec(state)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    /// Publish a partial state update (JSON merge-patch, not retained).
    pub async fn publish_state_partial(&self, device_id: &str, patch: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state/partial");
        let payload = serde_json::to_vec(patch)?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, payload)
            .await
            .context("publish_state_partial failed")
    }

    /// Publish `"online"` to the device's availability topic.
    pub async fn set_available(&self, device_id: &str, available: bool) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/availability");
        let payload = if available { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("set_available failed")
    }

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
    /// `"light"`, `"light_color"`, `"switch"`, `"temperature_sensor"`,
    /// `"power_monitor"`, `"cover"`, `"lock"`, `"climate"`, …
    pub async fn register_device_typed(
        &self,
        device_id: &str,
        name: &str,
        device_type: &str,
        area: Option<&str>,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.config.plugin_id);
        let mut payload = serde_json::json!({
            "device_id":   device_id,
            "plugin_id":   self.config.plugin_id,
            "name":        name,
            "device_type": device_type,
        });
        if let Some(a) = area {
            payload["area"] = serde_json::Value::String(a.to_string());
        }
        self.client
            .publish(
                &topic,
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&payload)?,
            )
            .await
            .context("register_device_typed failed")?;
        info!(device_id, device_type, "Device registered (typed)");
        Ok(())
    }

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

    /// Return a [`DevicePublisher`] that can publish state concurrently with `run()`.
    ///
    /// Call this **before** `run()` — `run()` consumes `self`, so any handles
    /// must be obtained first.  The returned publisher is `Clone`.
    pub fn device_publisher(&self) -> DevicePublisher {
        DevicePublisher {
            client: self.client.clone(),
        }
    }

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

    /// Drive the MQTT event loop, calling `on_command` whenever a `cmd`
    /// message arrives for any subscribed device.
    ///
    /// This method blocks until the connection is lost or an error occurs.
    pub async fn run<F>(mut self, on_command: F) -> Result<()>
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        info!(plugin_id = %self.config.plugin_id, "Plugin event loop starting");
        loop {
            match self.eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                    info!("Plugin connected to broker");
                }
                Ok(rumqttc::Event::Incoming(Packet::Publish(p))) => {
                    // homecore/devices/{id}/cmd
                    let parts: Vec<&str> = p.topic.splitn(4, '/').collect();
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
