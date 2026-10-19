//! What the bridge, the slow refresher and the plugin's actions share: the
//! controllers, their firmware, which restarts were asked for, and how to say
//! a device's alerts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plugin_sdk_rs::types::PluginAlert;
use plugin_sdk_rs::DevicePublisher;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::warn;

use crate::config::{DeviceConfig, WledConfig};
use crate::firmware;

/// How long an unexpected restart stays an alert.
pub const RESTART_ALERT_FOR: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone)]
pub struct Runtime {
    pub cfg: WledConfig,
    pub config_path: String,
    pub publisher: DevicePublisher,
    pub firmware: firmware::Statuses,
    /// When this plugin last restarted a controller itself (a reboot, an
    /// update), so the refresher doesn't call that one unexpected.
    pub expected_restart: Arc<Mutex<HashMap<String, Instant>>>,
    /// An unexpected restart, by device: when it was seen, for its alert.
    pub restarted: Arc<Mutex<HashMap<String, (Instant, String)>>>,
    /// Wakes a device's refresher early — after its presets change, say.
    pub refresh: Arc<HashMap<String, Arc<Notify>>>,
    /// Each controller's schema as last published, with its own names
    /// (`schema::schema_named`) — what a republish sends again.
    pub schemas: Arc<Mutex<HashMap<String, Value>>>,
}

impl Runtime {
    pub fn new(cfg: WledConfig, config_path: &str, publisher: DevicePublisher) -> Self {
        let refresh = cfg
            .devices
            .iter()
            .map(|d| (d.hc_id.clone(), Arc::new(Notify::new())))
            .collect();
        Self {
            cfg,
            config_path: config_path.to_string(),
            publisher,
            firmware: Arc::default(),
            expected_restart: Arc::default(),
            restarted: Arc::default(),
            refresh: Arc::new(refresh),
            schemas: Arc::default(),
        }
    }

    pub fn device(&self, hc_id: &str) -> Option<&DeviceConfig> {
        self.cfg.devices.iter().find(|d| d.hc_id == hc_id)
    }

    /// The device a plugin action names by `device_id` (its homeCore id) or
    /// `host`; the only one, when there is only one and none is named.
    pub fn named(&self, params: &Value) -> Result<&DeviceConfig, String> {
        let want = params
            .get("device_id")
            .or_else(|| params.get("host"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match want {
            Some(w) => self
                .cfg
                .devices
                .iter()
                .find(|d| d.hc_id == w || d.host == w)
                .ok_or_else(|| format!("no WLED controller is {w}")),
            None if self.cfg.devices.len() == 1 => Ok(&self.cfg.devices[0]),
            None => Err("say which controller: `device_id`".into()),
        }
    }

    /// The schema to (re)publish for a controller: its named one, once the
    /// refresher has made it.
    pub fn schema_for(&self, hc_id: &str) -> Value {
        self.schemas
            .lock()
            .unwrap()
            .get(hc_id)
            .cloned()
            .unwrap_or_else(crate::schema::schema_json)
    }

    pub fn wake(&self, hc_id: &str) {
        if let Some(n) = self.refresh.get(hc_id) {
            n.notify_one();
        }
    }

    pub fn expect_restart(&self, hc_id: &str) {
        self.expected_restart
            .lock()
            .unwrap()
            .insert(hc_id.to_string(), Instant::now());
    }

    /// Whether this plugin restarted it in the last ten minutes.
    pub fn restart_was_expected(&self, hc_id: &str) -> bool {
        self.expected_restart
            .lock()
            .unwrap()
            .get(hc_id)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
    }

    /// Everything true of the device now, as alerts, said to homeCore whole.
    pub async fn publish_alerts(&self, hc_id: &str) {
        let mut alerts: Vec<PluginAlert> = self
            .firmware
            .lock()
            .unwrap()
            .get(hc_id)
            .map(|s| s.alerts())
            .unwrap_or_default();
        {
            let mut r = self.restarted.lock().unwrap();
            if r.get(hc_id)
                .is_some_and(|(at, _)| at.elapsed() > RESTART_ALERT_FOR)
            {
                r.remove(hc_id);
            }
            if let Some((_, when)) = r.get(hc_id) {
                alerts.push(PluginAlert::new("restarted", "Restarted by itself").key(when.clone()));
            }
        }
        if let Err(e) = self.publisher.set_alerts(hc_id, &alerts).await {
            warn!(hc_id, error = %e, "publishing alerts failed");
        }
    }

    /// The firmware attributes, published on the device.
    pub async fn publish_firmware(&self, hc_id: &str) {
        let attrs = self
            .firmware
            .lock()
            .unwrap()
            .get(hc_id)
            .map(|s| s.attributes());
        if let Some(a) = attrs {
            let _ = self.publisher.publish_state_partial(hc_id, &a).await;
        }
        self.publish_alerts(hc_id).await;
    }
}
