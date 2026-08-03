//! Dynamic device registry — auto-registers devices with HomeCore on first sight.

use anyhow::Result;
use plugin_sdk_rs::DevicePublisher;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::parser::DeviceUpdate;

/// Tracks which devices have been registered with HomeCore.
/// On first sight of a device_id, registers it automatically.
pub struct DeviceRegistry {
    registered: HashSet<String>,
    /// The attribute names last published in each device's schema.
    ///
    /// The schema is derived from what a device actually reports, and that set
    /// grows as sensors are paired. Republishing on every reading would churn a
    /// retained topic several times a minute for no change, so it is published
    /// only when the set of names differs from last time.
    published_attrs: std::collections::HashMap<String, Vec<String>>,
    publisher: DevicePublisher,
    #[allow(dead_code)]
    plugin_id: String,
    cache_path: PathBuf,
}

impl DeviceRegistry {
    pub fn new(publisher: DevicePublisher, plugin_id: String, config_path: &str) -> Self {
        let cache_path = Path::new(config_path)
            .parent()
            .unwrap_or(Path::new("."))
            .join(".published-device-ids.json");

        Self {
            registered: HashSet::new(),
            published_attrs: std::collections::HashMap::new(),
            publisher,
            plugin_id,
            cache_path,
        }
    }

    /// Process a batch of device updates: register new devices, publish state for all.
    pub async fn process_updates(&mut self, updates: Vec<DeviceUpdate>) {
        for update in &updates {
            if !self.registered.contains(&update.device_id) {
                self.register_device(update).await;
            }
        }

        for update in updates {
            self.publish_schema_if_changed(&update).await;
            let _ = self
                .publisher
                .publish_state(&update.device_id, &update.state)
                .await;
        }
    }

    /// Publish this device's schema when the set of attributes it reports has
    /// changed since last time — first sighting included.
    async fn publish_schema_if_changed(&mut self, update: &DeviceUpdate) {
        let Some(obj) = update.state.as_object() else {
            return;
        };
        let mut names: Vec<String> = obj.keys().cloned().collect();
        names.sort();
        if self.published_attrs.get(&update.device_id) == Some(&names) {
            return;
        }
        if let Err(e) =
            crate::schema::publish(&self.publisher, &update.device_id, &update.state).await
        {
            warn!(device_id = %update.device_id, error = %e, "Failed to publish device schema");
            return;
        }
        self.published_attrs.insert(update.device_id.clone(), names);
    }

    async fn register_device(&mut self, update: &DeviceUpdate) {
        if let Err(e) = self
            .publisher
            .register_device_full(
                &update.device_id,
                &update.name,
                Some(update.device_type),
                None, // area — not known from sensor data
                None,
            )
            .await
        {
            warn!(device_id = %update.device_id, error = %e, "Failed to register device");
            return;
        }

        if let Err(e) = self.publisher.subscribe_commands(&update.device_id).await {
            warn!(device_id = %update.device_id, error = %e, "Failed to subscribe commands");
        }

        let _ = self
            .publisher
            .publish_availability(&update.device_id, true)
            .await;

        info!(device_id = %update.device_id, device_type = update.device_type, name = %update.name, "Auto-registered new device");
        self.registered.insert(update.device_id.clone());
        let _ = self.save_cache();
    }

    /// Clean up devices that were registered previously but are no longer seen.
    #[allow(dead_code)]
    pub async fn cleanup_stale(&mut self) {
        let previous = self.load_cache();
        for stale_id in previous.iter().filter(|id| !self.registered.contains(*id)) {
            if let Err(e) = self
                .publisher
                .unregister_device(&self.plugin_id, stale_id)
                .await
            {
                warn!(device_id = %stale_id, error = %e, "Failed to unregister stale device");
            } else {
                info!(device_id = %stale_id, "Unregistered stale device");
            }
        }
    }

    #[allow(dead_code)]
    fn load_cache(&self) -> Vec<String> {
        std::fs::read_to_string(&self.cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_cache(&self) -> Result<()> {
        let ids: Vec<&String> = self.registered.iter().collect();
        let payload = serde_json::to_vec_pretty(&ids)?;
        std::fs::write(&self.cache_path, payload)?;
        Ok(())
    }
}
