//! `hc-state` — device registry and state persistence.
//!
//! Two storage back-ends:
//! - **redb** — device registry, rules, scenes, areas, users (single shared `Database`).
//! - **SQLite/rusqlite** — append-only time-series history.
//!
//! All public methods are async wrappers around `spawn_blocking`.

use anyhow::{Context, Result};
use hc_auth::User;
use hc_types::device::{Area, DeviceState};
use hc_types::event::Event;
use hc_types::rule::{Rule, Scene};
use redb::Database;
use std::sync::{Arc, OnceLock};
use tracing::info;
use uuid::Uuid;

pub mod alert_rule_store;
pub mod api_key_store;
pub mod asset_store;
pub mod audit_store;
pub mod battery_store;
pub mod device_store;
pub mod history;
pub mod plugin_runtime_store;
pub mod plugin_state_store;
pub mod refresh_token_store;
pub mod rule_store;
pub mod schema_store;
pub mod user_store;

use alert_rule_store::AlertRuleStore;
use api_key_store::ApiKeyStore;
use asset_store::AssetStore;
use audit_store::AuditStore;
use battery_store::BatteryStore;
use device_store::{DeviceStore, DeviceWrite};
use history::HistoryStore;
use plugin_state_store::PluginStateStore;
use refresh_token_store::RefreshTokenStore;
use rule_store::RuleStore;
use schema_store::SchemaStore;
use user_store::UserStore;

pub use api_key_store::ApiKeyRecord;
pub use asset_store::AssetRecord;
pub use audit_store::{AuditActorType, AuditEntry, AuditQuery, AuditResult};
pub use battery_store::{BatteryEdge, BatteryRecord};
pub use plugin_runtime_store::{
    EnrollTokenRecord, PlacementRecord, PluginRuntimeStore, RuntimeRecord, RuntimeStatus,
};
pub use refresh_token_store::RefreshTokenRecord;

/// Combined handle to both storage back-ends.
#[derive(Clone)]
pub struct StateStore {
    devices: Arc<DeviceStore>,
    rules: Arc<RuleStore>,
    history: Arc<HistoryStore>,
    schemas: Arc<SchemaStore>,
    users: Arc<UserStore>,
    api_keys: Arc<ApiKeyStore>,
    refresh_tokens: Arc<RefreshTokenStore>,
    audit: Arc<AuditStore>,
    battery: Arc<BatteryStore>,
    alert_rules: Arc<AlertRuleStore>,
    plugin_state: Arc<PluginStateStore>,
    plugin_runtimes: Arc<PluginRuntimeStore>,
    assets: Arc<AssetStore>,
    /// Where changes to the house's catalogue are announced. See [`on_change`].
    ///
    /// [`on_change`]: StateStore::on_change
    changes: Arc<OnceLock<ChangeSink>>,
}

/// Receives the events the store raises. See [`StateStore::on_change`].
type ChangeSink = Arc<dyn Fn(Event) + Send + Sync>;

impl StateStore {
    /// Announce changes to devices and scenes through `sink` from now on.
    ///
    /// **The store raises these, rather than each edit path, because the edit
    /// paths are many.** Devices are written by registration, by every state
    /// report, by a dozen API handlers, by the timer, switch and mode
    /// managers and by room edits; they are deleted from eight places. An
    /// event raised at each of those would be missing from whichever one was
    /// written next. Here, every write is seen once.
    ///
    /// Raises [`Event::DeviceAdded`], [`Event::DeviceMetadataChanged`] (not
    /// for a write that only moved state — that has its own event),
    /// [`Event::DeviceRemoved`], [`Event::SceneChanged`] and
    /// [`Event::SceneRemoved`]. Set once, at startup; later calls are ignored.
    /// A store with no sink — a test, a tool — raises nothing.
    pub fn on_change(&self, sink: impl Fn(Event) + Send + Sync + 'static) {
        let _ = self.changes.set(Arc::new(sink));
    }

    fn announce(&self, event: impl FnOnce() -> Event) {
        if let Some(sink) = self.changes.get() {
            sink(event());
        }
    }

    /// Open (or create) the databases at the given paths. The audit log
    /// defaults to `<parent-of-history_db_path>/audit.db` — use
    /// [`open_with_audit`] to override.
    pub async fn open(state_db_path: &str, history_db_path: &str) -> Result<Self> {
        let audit_path = std::path::Path::new(history_db_path)
            .parent()
            .map(|d| d.join("audit.db"))
            .unwrap_or_else(|| std::path::PathBuf::from("audit.db"));
        Self::open_with_audit(state_db_path, history_db_path, audit_path.to_str().unwrap()).await
    }

    /// Open all stores with an explicit audit log path.
    pub async fn open_with_audit(
        state_db_path: &str,
        history_db_path: &str,
        audit_db_path: &str,
    ) -> Result<Self> {
        info!(%state_db_path, %history_db_path, %audit_db_path, "Opening state store");
        let state_path = state_db_path.to_string();
        let history_path = history_db_path.to_string();
        let audit_path = audit_db_path.to_string();

        let (
            devices,
            rules,
            history,
            schemas,
            users,
            api_keys,
            refresh_tokens,
            audit,
            battery,
            alert_rules,
            plugin_state,
            plugin_runtimes,
            assets,
        ) = tokio::task::spawn_blocking(move || {
            // Ensure parent directories exist before opening databases.
            if let Some(parent) = std::path::Path::new(&state_path).parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create state DB directory: {}", parent.display())
                })?;
            }
            if let Some(parent) = std::path::Path::new(&history_path).parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create history DB directory: {}",
                        parent.display()
                    )
                })?;
            }
            if let Some(parent) = std::path::Path::new(&audit_path).parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create audit DB directory: {}", parent.display())
                })?;
            }

            // Single redb::Database shared between DeviceStore, RuleStore, and UserStore.
            let db = Arc::new(
                Database::create(&state_path).map_err(|e| explain_open_failure(e, &state_path))?,
            );
            let devices = DeviceStore::new(Arc::clone(&db))?;
            let rules = RuleStore::new(Arc::clone(&db))?;
            let history = HistoryStore::open(&history_path)?;
            let schemas = SchemaStore::new(Arc::clone(&db))?;
            let users = UserStore::new(Arc::clone(&db))?;
            let api_keys = ApiKeyStore::new(Arc::clone(&db))?;
            let refresh_tokens = RefreshTokenStore::new(Arc::clone(&db))?;
            let audit = AuditStore::open(std::path::Path::new(&audit_path))?;
            let battery = BatteryStore::new(Arc::clone(&db))?;
            let alert_rules = AlertRuleStore::new(Arc::clone(&db))?;
            let plugin_state = PluginStateStore::new(Arc::clone(&db))?;
            let plugin_runtimes = PluginRuntimeStore::new(Arc::clone(&db))?;
            // Blobs live beside the state DB rather than inside it, and the
            // path is derived rather than configured — the same trick
            // `jwt_secret_file` and `audit.db` already use.
            let assets_dir = std::path::Path::new(&state_path)
                .parent()
                .map(|d| d.join("assets"))
                .unwrap_or_else(|| std::path::PathBuf::from("assets"));
            let assets = AssetStore::new(Arc::clone(&db), assets_dir)?;
            Ok::<_, anyhow::Error>((
                devices,
                rules,
                history,
                schemas,
                users,
                api_keys,
                refresh_tokens,
                audit,
                battery,
                alert_rules,
                plugin_state,
                plugin_runtimes,
                assets,
            ))
        })
        .await??;

        Ok(Self {
            devices: Arc::new(devices),
            rules: Arc::new(rules),
            history: Arc::new(history),
            schemas: Arc::new(schemas),
            users: Arc::new(users),
            api_keys: Arc::new(api_keys),
            refresh_tokens: Arc::new(refresh_tokens),
            audit: Arc::new(audit),
            battery: Arc::new(battery),
            alert_rules: Arc::new(alert_rules),
            plugin_state: Arc::new(plugin_state),
            plugin_runtimes: Arc::new(plugin_runtimes),
            assets: Arc::new(assets),
            changes: Arc::new(OnceLock::new()),
        })
    }

    // --- Plugin-learned state (D8 split, leg 2) ---

    /// Full learned-state document a plugin has persisted, or `None`.
    /// Enrollment records for container-hosted plugin runtimes.
    ///
    /// Handed out rather than wrapped in async helpers like the device store,
    /// because every caller is an HTTP handler that already has an executor and
    /// the operations are single small reads.
    pub fn plugin_runtimes(&self) -> Arc<PluginRuntimeStore> {
        Arc::clone(&self.plugin_runtimes)
    }

    /// Content-addressed blob storage for the files a dashboard points at.
    ///
    /// Handed out rather than wrapped, like the runtime store above: every
    /// caller is an HTTP handler, and the one operation heavy enough to matter
    /// — writing bytes — is put on a blocking thread by that handler.
    pub fn assets(&self) -> Arc<AssetStore> {
        Arc::clone(&self.assets)
    }

    pub async fn plugin_state_get(&self, plugin_id: &str) -> Result<Option<serde_json::Value>> {
        let store = Arc::clone(&self.plugin_state);
        let id = plugin_id.to_string();
        tokio::task::spawn_blocking(move || store.get(&id)).await?
    }

    /// Replace a plugin's entire learned-state document.
    pub async fn plugin_state_put(&self, plugin_id: &str, state: &serde_json::Value) -> Result<()> {
        let store = Arc::clone(&self.plugin_state);
        let id = plugin_id.to_string();
        let state = state.clone();
        tokio::task::spawn_blocking(move || store.put(&id, &state)).await?
    }

    /// Shallow-merge a delta into a plugin's learned state; returns the merged
    /// document. See [`plugin_state_store::PluginStateStore::merge`].
    pub async fn plugin_state_merge(
        &self,
        plugin_id: &str,
        delta: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let store = Arc::clone(&self.plugin_state);
        let id = plugin_id.to_string();
        let delta = delta.clone();
        tokio::task::spawn_blocking(move || store.merge(&id, &delta)).await?
    }

    /// Drop a plugin's learned state (e.g. on deregistration).
    pub async fn plugin_state_delete(&self, plugin_id: &str) -> Result<bool> {
        let store = Arc::clone(&self.plugin_state);
        let id = plugin_id.to_string();
        tokio::task::spawn_blocking(move || store.delete(&id)).await?
    }

    /// All plugin ids with persisted learned state (for boot-time re-publish).
    pub async fn plugin_state_list_ids(&self) -> Result<Vec<String>> {
        let store = Arc::clone(&self.plugin_state);
        tokio::task::spawn_blocking(move || store.list_ids()).await?
    }

    // --- Battery latch ---

    /// Apply a new battery reading. Returns `Some(edge)` when the latch
    /// state machine transitions, `None` for noise inside the band.
    pub async fn evaluate_battery(
        &self,
        device_id: &str,
        pct: f64,
        threshold: f64,
        recover_band: f64,
    ) -> Result<Option<BatteryEdge>> {
        let store = Arc::clone(&self.battery);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.evaluate(&id, pct, threshold, recover_band))
            .await?
    }

    // --- Alert time rules (H4) ---

    pub async fn list_alert_rules(&self) -> Result<Vec<hc_types::alert::AlertRule>> {
        let store = Arc::clone(&self.alert_rules);
        tokio::task::spawn_blocking(move || store.list()).await?
    }

    pub async fn get_alert_rule(&self, id: &str) -> Result<Option<hc_types::alert::AlertRule>> {
        let store = Arc::clone(&self.alert_rules);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.get(&id)).await?
    }

    pub async fn upsert_alert_rule(&self, rule: hc_types::alert::AlertRule) -> Result<()> {
        let store = Arc::clone(&self.alert_rules);
        tokio::task::spawn_blocking(move || store.upsert(&rule)).await?
    }

    /// Whether there was such a rule.
    pub async fn delete_alert_rule(&self, id: &str) -> Result<bool> {
        let store = Arc::clone(&self.alert_rules);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.delete(&id)).await?
    }

    // --- Alert ignores ---

    pub async fn list_alert_ignores(&self) -> Result<Vec<hc_types::alert::AlertIgnore>> {
        let store = Arc::clone(&self.alert_rules);
        tokio::task::spawn_blocking(move || store.list_ignores()).await?
    }

    pub async fn upsert_alert_ignore(&self, ignore: hc_types::alert::AlertIgnore) -> Result<()> {
        let store = Arc::clone(&self.alert_rules);
        tokio::task::spawn_blocking(move || store.upsert_ignore(&ignore)).await?
    }

    /// Whether there was such an ignore.
    pub async fn delete_alert_ignore(&self, id: &str) -> Result<bool> {
        let store = Arc::clone(&self.alert_rules);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.delete_ignore(&id)).await?
    }

    /// Drop the battery latch entry for a device (called when the device
    /// is deleted from the registry).
    pub async fn forget_battery(&self, device_id: &str) -> Result<()> {
        let store = Arc::clone(&self.battery);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.forget(&id)).await?
    }

    // --- Audit log ---

    /// Record an audit event. Returns the inserted rowid. Best-effort —
    /// failures are surfaced to callers but should be logged and not crash
    /// the originating operation.
    pub async fn record_audit(&self, entry: &AuditEntry) -> Result<i64> {
        let store = Arc::clone(&self.audit);
        let e = entry.clone();
        tokio::task::spawn_blocking(move || store.record(&e)).await?
    }

    pub async fn query_audit(&self, query: &AuditQuery) -> Result<Vec<AuditEntry>> {
        let store = Arc::clone(&self.audit);
        let q = query.clone();
        tokio::task::spawn_blocking(move || store.query(&q)).await?
    }

    pub async fn prune_audit_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> Result<usize> {
        let store = Arc::clone(&self.audit);
        tokio::task::spawn_blocking(move || store.prune_before(cutoff)).await?
    }

    pub async fn audit_count(&self) -> Result<i64> {
        let store = Arc::clone(&self.audit);
        tokio::task::spawn_blocking(move || store.count()).await?
    }

    // --- Refresh tokens ---

    pub async fn create_refresh_token(&self, record: &RefreshTokenRecord) -> Result<()> {
        let store = Arc::clone(&self.refresh_tokens);
        let r = record.clone();
        tokio::task::spawn_blocking(move || store.create(&r)).await?
    }

    pub async fn refresh_prefix_exists(&self, prefix: &str) -> Result<bool> {
        let store = Arc::clone(&self.refresh_tokens);
        let p = prefix.to_string();
        tokio::task::spawn_blocking(move || store.prefix_exists(&p)).await?
    }

    pub async fn get_refresh_by_prefix(&self, prefix: &str) -> Result<Option<RefreshTokenRecord>> {
        let store = Arc::clone(&self.refresh_tokens);
        let p = prefix.to_string();
        tokio::task::spawn_blocking(move || store.get_by_prefix(&p)).await?
    }

    pub async fn get_refresh_by_id(&self, id: Uuid) -> Result<Option<RefreshTokenRecord>> {
        let store = Arc::clone(&self.refresh_tokens);
        tokio::task::spawn_blocking(move || store.get_by_id(id)).await?
    }

    pub async fn list_refresh_by_user(&self, user_id: Uuid) -> Result<Vec<RefreshTokenRecord>> {
        let store = Arc::clone(&self.refresh_tokens);
        tokio::task::spawn_blocking(move || store.list_by_user(user_id)).await?
    }

    pub async fn mark_refresh_used(&self, id: Uuid) -> Result<()> {
        let store = Arc::clone(&self.refresh_tokens);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.mark_used(id, now)).await?
    }

    pub async fn revoke_refresh_chain(&self, id: Uuid) -> Result<usize> {
        let store = Arc::clone(&self.refresh_tokens);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.revoke_chain(id, now)).await?
    }

    pub async fn revoke_refresh(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.refresh_tokens);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.revoke(id, now)).await?
    }

    pub async fn prune_refresh_tokens(&self) -> Result<usize> {
        let store = Arc::clone(&self.refresh_tokens);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.prune(now)).await?
    }

    // --- API keys ---

    pub async fn create_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
        let store = Arc::clone(&self.api_keys);
        let r = record.clone();
        tokio::task::spawn_blocking(move || store.create(&r)).await?
    }

    pub async fn api_key_prefix_exists(&self, prefix: &str) -> Result<bool> {
        let store = Arc::clone(&self.api_keys);
        let p = prefix.to_string();
        tokio::task::spawn_blocking(move || store.prefix_exists(&p)).await?
    }

    pub async fn get_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKeyRecord>> {
        let store = Arc::clone(&self.api_keys);
        let p = prefix.to_string();
        tokio::task::spawn_blocking(move || store.get_by_prefix(&p)).await?
    }

    pub async fn get_api_key_by_id(&self, id: Uuid) -> Result<Option<ApiKeyRecord>> {
        let store = Arc::clone(&self.api_keys);
        tokio::task::spawn_blocking(move || store.get_by_id(id)).await?
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>> {
        let store = Arc::clone(&self.api_keys);
        tokio::task::spawn_blocking(move || store.list()).await?
    }

    pub async fn list_api_keys_by_owner(&self, owner_uid: Uuid) -> Result<Vec<ApiKeyRecord>> {
        let store = Arc::clone(&self.api_keys);
        tokio::task::spawn_blocking(move || store.list_by_owner(owner_uid)).await?
    }

    pub async fn update_api_key(&self, record: &ApiKeyRecord) -> Result<()> {
        let store = Arc::clone(&self.api_keys);
        let r = record.clone();
        tokio::task::spawn_blocking(move || store.update(&r)).await?
    }

    pub async fn revoke_api_key(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.api_keys);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.revoke(id, now)).await?
    }

    pub async fn touch_api_key_last_used(&self, id: Uuid) -> Result<()> {
        let store = Arc::clone(&self.api_keys);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.touch_last_used(id, now)).await?
    }

    pub async fn replace_api_key_secret(
        &self,
        id: Uuid,
        new_prefix: String,
        new_hash: String,
    ) -> Result<Option<ApiKeyRecord>> {
        let store = Arc::clone(&self.api_keys);
        let now = chrono::Utc::now();
        tokio::task::spawn_blocking(move || store.replace_secret(id, new_prefix, new_hash, now))
            .await?
    }

    // --- Device registry ---

    pub async fn get_device(&self, device_id: &str) -> Result<Option<DeviceState>> {
        let store = Arc::clone(&self.devices);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.get(&id)).await?
    }

    pub async fn upsert_device(&self, state: &DeviceState) -> Result<()> {
        let store = Arc::clone(&self.devices);
        let s = state.clone();
        let kind = tokio::task::spawn_blocking(move || store.upsert(&s)).await??;
        let timestamp = chrono::Utc::now();
        match kind {
            DeviceWrite::Added => self.announce(|| Event::DeviceAdded {
                timestamp,
                device: Box::new(state.clone()),
            }),
            DeviceWrite::MetadataChanged => self.announce(|| Event::DeviceMetadataChanged {
                timestamp,
                device: Box::new(state.clone()),
            }),
            DeviceWrite::StateOnly => {}
        }
        Ok(())
    }

    pub async fn delete_device(&self, device_id: &str) -> Result<bool> {
        let store = Arc::clone(&self.devices);
        let id = device_id.to_string();
        let removed = tokio::task::spawn_blocking(move || store.delete(&id)).await??;
        if removed {
            self.announce(|| Event::DeviceRemoved {
                timestamp: chrono::Utc::now(),
                device_id: device_id.to_string(),
            });
        }
        Ok(removed)
    }

    pub async fn list_devices(&self) -> Result<Vec<DeviceState>> {
        let store = Arc::clone(&self.devices);
        tokio::task::spawn_blocking(move || store.list()).await?
    }

    // --- Device schemas ---

    pub async fn upsert_device_schema(
        &self,
        device_id: &str,
        schema: &hc_types::DeviceSchema,
    ) -> Result<()> {
        let store = Arc::clone(&self.schemas);
        let id = device_id.to_string();
        let s = schema.clone();
        tokio::task::spawn_blocking(move || store.upsert(&id, &s)).await?
    }

    pub async fn get_device_schema(
        &self,
        device_id: &str,
    ) -> Result<Option<hc_types::DeviceSchema>> {
        let store = Arc::clone(&self.schemas);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.get(&id)).await?
    }

    pub async fn list_device_schemas(&self) -> Result<Vec<(String, hc_types::DeviceSchema)>> {
        let store = Arc::clone(&self.schemas);
        tokio::task::spawn_blocking(move || store.list()).await?
    }

    pub async fn delete_device_schema(&self, device_id: &str) -> Result<bool> {
        let store = Arc::clone(&self.schemas);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.delete(&id)).await?
    }

    // --- Rules ---

    pub async fn get_rule(&self, id: Uuid) -> Result<Option<Rule>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.get_rule(id)).await?
    }

    pub async fn upsert_rule(&self, rule: &Rule) -> Result<()> {
        let store = Arc::clone(&self.rules);
        let r = rule.clone();
        tokio::task::spawn_blocking(move || store.upsert_rule(&r)).await?
    }

    pub async fn delete_rule(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.delete_rule(id)).await?
    }

    pub async fn list_rules(&self) -> Result<Vec<Rule>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.list_rules()).await?
    }

    // --- Scenes ---

    pub async fn get_scene(&self, id: Uuid) -> Result<Option<Scene>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.get_scene(id)).await?
    }

    pub async fn upsert_scene(&self, scene: &Scene) -> Result<()> {
        let store = Arc::clone(&self.rules);
        let s = scene.clone();
        tokio::task::spawn_blocking(move || store.upsert_scene(&s)).await??;
        self.announce(|| Event::SceneChanged {
            timestamp: chrono::Utc::now(),
            scene: scene.clone(),
        });
        Ok(())
    }

    pub async fn delete_scene(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.rules);
        let removed = tokio::task::spawn_blocking(move || store.delete_scene(id)).await??;
        if removed {
            self.announce(|| Event::SceneRemoved {
                timestamp: chrono::Utc::now(),
                scene_id: id,
            });
        }
        Ok(removed)
    }

    pub async fn list_scenes(&self) -> Result<Vec<Scene>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.list_scenes()).await?
    }

    // --- Areas ---

    pub async fn get_area(&self, id: Uuid) -> Result<Option<Area>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.get_area(id)).await?
    }

    pub async fn upsert_area(&self, area: &Area) -> Result<()> {
        let store = Arc::clone(&self.rules);
        let a = area.clone();
        tokio::task::spawn_blocking(move || store.upsert_area(&a)).await?
    }

    pub async fn delete_area(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.delete_area(id)).await?
    }

    pub async fn list_areas(&self) -> Result<Vec<Area>> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.list_areas()).await?
    }

    // --- History ---

    pub async fn append_history(
        &self,
        device_id: &str,
        attribute: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let store = Arc::clone(&self.history);
        let did = device_id.to_string();
        let attr = attribute.to_string();
        let val = value.clone();
        tokio::task::spawn_blocking(move || store.append(&did, &attr, &val)).await?
    }

    pub async fn query_history(
        &self,
        device_id: &str,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
        attribute: Option<&str>,
        limit: u32,
    ) -> Result<Vec<history::HistoryEntry>> {
        let store = Arc::clone(&self.history);
        let did = device_id.to_string();
        let attr = attribute.map(str::to_string);
        tokio::task::spawn_blocking(move || store.query(&did, from, to, attr.as_deref(), limit))
            .await?
    }

    // --- Rule fire history ---

    pub async fn append_rule_firing(
        &self,
        rule_id: String,
        fired_at: String,
        record_json: String,
    ) -> Result<()> {
        let store = Arc::clone(&self.history);
        tokio::task::spawn_blocking(move || {
            store.append_rule_firing(&rule_id, &fired_at, &record_json)
        })
        .await?
    }

    pub async fn load_recent_rule_firings(
        &self,
        limit_per_rule: usize,
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let store = Arc::clone(&self.history);
        let lim = limit_per_rule as i64;
        tokio::task::spawn_blocking(move || store.load_recent_per_rule(lim)).await?
    }

    // --- Users ---

    pub async fn create_user(&self, user: &User) -> Result<()> {
        let store = Arc::clone(&self.users);
        let u = user.clone();
        tokio::task::spawn_blocking(move || store.create_user(&u)).await?
    }

    pub async fn get_user_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let store = Arc::clone(&self.users);
        tokio::task::spawn_blocking(move || store.get_user_by_id(id)).await?
    }

    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let store = Arc::clone(&self.users);
        let u = username.to_string();
        tokio::task::spawn_blocking(move || store.get_user_by_username(&u)).await?
    }

    pub async fn update_user(&self, user: &User) -> Result<()> {
        let store = Arc::clone(&self.users);
        let u = user.clone();
        tokio::task::spawn_blocking(move || store.update_user(&u)).await?
    }

    pub async fn delete_user(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.users);
        tokio::task::spawn_blocking(move || store.delete_user(id)).await?
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        let store = Arc::clone(&self.users);
        tokio::task::spawn_blocking(move || store.list_users()).await?
    }

    pub async fn user_count(&self) -> Result<usize> {
        let store = Arc::clone(&self.users);
        tokio::task::spawn_blocking(move || store.user_count()).await?
    }
}

/// Turn a failure to open the state database into something an operator can act on.
///
/// The case worth naming is `UpgradeRequired`: redb 3 changed its on-disk
/// format, and redb 4 refuses a file written by redb 2 with
/// `Expected file format version 3, but file is version 2`. That is accurate
/// and tells you nothing about what to do.
///
/// homeCore 0.1.23 shipped a one-time converter for exactly this, and it was
/// removed once the only installation had run it. Anyone who later restores a
/// pre-0.1.23 backup lands here, and the way out is to run that version once —
/// not to delete the database, which is the guess the raw message invites.
fn explain_open_failure(err: redb::DatabaseError, path: &str) -> anyhow::Error {
    match err {
        redb::DatabaseError::UpgradeRequired(found) => anyhow::anyhow!(
            "{path} is a redb v{found} database and this build reads v3.\n\
             \n\
             This is a state database from homeCore 0.1.22 or earlier. Install \
             0.1.23 and start it once — it converts the file in place and keeps \
             the original as {path}.v2-backup — then upgrade to this version. \
             Do not delete the file; it is the device registry, users, API keys \
             and rules."
        ),
        other => anyhow::Error::new(other).context(format!("failed to open state DB at {path}")),
    }
}

#[cfg(test)]
mod open_failure_tests {
    use super::*;

    /// The message has to name the version that converts the file. Without it
    /// the operator gets redb's "expected file format version 3" and a
    /// database they are tempted to delete.
    #[test]
    fn an_old_format_database_is_explained_not_just_reported() {
        let e = explain_open_failure(
            redb::DatabaseError::UpgradeRequired(2),
            "/var/lib/state.redb",
        );
        let msg = e.to_string();
        assert!(
            msg.contains("0.1.23"),
            "must name the converting release: {msg}"
        );
        assert!(
            msg.contains("v2-backup"),
            "must say a backup is kept: {msg}"
        );
        assert!(
            msg.contains("Do not delete"),
            "must warn against deleting: {msg}"
        );
        assert!(
            msg.contains("/var/lib/state.redb"),
            "must name the file: {msg}"
        );
    }

    /// Anything else keeps redb's own wording — inventing an explanation for a
    /// failure we do not understand would be worse than passing it through.
    #[test]
    fn other_failures_are_passed_through_with_context() {
        let e = explain_open_failure(redb::DatabaseError::DatabaseAlreadyOpen, "/tmp/state.redb");
        let msg = format!("{e:#}");
        assert!(msg.contains("/tmp/state.redb"));
        assert!(
            !msg.contains("0.1.23"),
            "must not claim a migration is needed: {msg}"
        );
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;
    use std::sync::Mutex;

    async fn store_with_sink() -> (tempfile::TempDir, StateStore, Arc<Mutex<Vec<Event>>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(
            dir.path().join("state.redb").to_str().unwrap(),
            dir.path().join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        store.on_change(move |e| sink.lock().unwrap().push(e));
        (dir, store, seen)
    }

    fn names(seen: &Mutex<Vec<Event>>) -> Vec<&'static str> {
        seen.lock()
            .unwrap()
            .drain(..)
            .map(|e| match e {
                Event::DeviceAdded { .. } => "added",
                Event::DeviceMetadataChanged { .. } => "metadata",
                Event::DeviceRemoved { .. } => "removed",
                Event::SceneChanged { .. } => "scene",
                Event::SceneRemoved { .. } => "scene_removed",
                _ => "other",
            })
            .collect()
    }

    /// Every write is seen, whoever made it; a state report — by far the
    /// commonest write — announces nothing here, since it has its own event.
    #[tokio::test]
    async fn device_writes_are_announced_by_what_they_changed() {
        let (_dir, store, seen) = store_with_sink().await;
        let mut lamp = DeviceState::new("lamp", "Lamp", "plugin.test");
        store.upsert_device(&lamp).await.unwrap();
        assert_eq!(names(&seen), ["added"]);

        lamp.attributes.insert("on".into(), serde_json::json!(true));
        lamp.last_seen = chrono::Utc::now();
        lamp.available = false;
        store.upsert_device(&lamp).await.unwrap();
        assert!(names(&seen).is_empty(), "a state report is not metadata");

        lamp.area_override = Some("den".into());
        store.upsert_device(&lamp).await.unwrap();
        lamp.ui_hint = Some("light".into());
        store.upsert_device(&lamp).await.unwrap();
        assert_eq!(names(&seen), ["metadata", "metadata"]);

        assert!(store.delete_device("lamp").await.unwrap());
        assert!(!store.delete_device("lamp").await.unwrap());
        assert_eq!(names(&seen), ["removed"], "only what was there is removed");
    }

    #[tokio::test]
    async fn scene_writes_are_announced() {
        let (_dir, store, seen) = store_with_sink().await;
        let scene = Scene {
            id: Uuid::new_v4(),
            name: "Movie".into(),
            states: Default::default(),
        };
        store.upsert_scene(&scene).await.unwrap();
        store.delete_scene(scene.id).await.unwrap();
        assert_eq!(names(&seen), ["scene", "scene_removed"]);
    }
}
