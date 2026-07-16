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
use hc_types::rule::{Rule, Scene};
use redb::Database;
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

pub mod api_key_store;
pub mod audit_store;
pub mod battery_store;
pub mod device_store;
pub mod history;
pub mod plugin_state_store;
pub mod refresh_token_store;
pub mod rule_store;
pub mod schema_store;
pub mod user_store;

use api_key_store::ApiKeyStore;
use audit_store::AuditStore;
use battery_store::BatteryStore;
use device_store::DeviceStore;
use history::HistoryStore;
use plugin_state_store::PluginStateStore;
use refresh_token_store::RefreshTokenStore;
use rule_store::RuleStore;
use schema_store::SchemaStore;
use user_store::UserStore;

pub use api_key_store::ApiKeyRecord;
pub use audit_store::{AuditActorType, AuditEntry, AuditQuery, AuditResult};
pub use battery_store::{BatteryEdge, BatteryRecord};
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
    plugin_state: Arc<PluginStateStore>,
}

impl StateStore {
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
            plugin_state,
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
            let db = Arc::new(Database::create(&state_path).context("failed to open state DB")?);
            let devices = DeviceStore::new(Arc::clone(&db))?;
            let rules = RuleStore::new(Arc::clone(&db))?;
            let history = HistoryStore::open(&history_path)?;
            let schemas = SchemaStore::new(Arc::clone(&db))?;
            let users = UserStore::new(Arc::clone(&db))?;
            let api_keys = ApiKeyStore::new(Arc::clone(&db))?;
            let refresh_tokens = RefreshTokenStore::new(Arc::clone(&db))?;
            let audit = AuditStore::open(std::path::Path::new(&audit_path))?;
            let battery = BatteryStore::new(Arc::clone(&db))?;
            let plugin_state = PluginStateStore::new(Arc::clone(&db))?;
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
                plugin_state,
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
            plugin_state: Arc::new(plugin_state),
        })
    }

    // --- Plugin-learned state (D8 split, leg 2) ---

    /// Full learned-state document a plugin has persisted, or `None`.
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
        tokio::task::spawn_blocking(move || store.upsert(&s)).await?
    }

    pub async fn delete_device(&self, device_id: &str) -> Result<bool> {
        let store = Arc::clone(&self.devices);
        let id = device_id.to_string();
        tokio::task::spawn_blocking(move || store.delete(&id)).await?
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
        tokio::task::spawn_blocking(move || store.upsert_scene(&s)).await?
    }

    pub async fn delete_scene(&self, id: Uuid) -> Result<bool> {
        let store = Arc::clone(&self.rules);
        tokio::task::spawn_blocking(move || store.delete_scene(id)).await?
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
