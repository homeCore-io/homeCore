//! redb-backed storage for API keys.
//!
//! Layout (two tables):
//! - `api_keys_by_prefix`: indexed lookup for middleware bearer-token
//!   verification. Key = lookup prefix (12 chars of base64 body).
//!   Value = JSON-serialised `ApiKeyRecord`.
//! - `api_keys_by_id`: id → prefix index for revoke-by-id and listings
//!   ordered by creation time.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const API_KEYS_BY_PREFIX: TableDefinition<&str, &str> = TableDefinition::new("api_keys_by_prefix");
const API_KEYS_BY_ID: TableDefinition<&str, &str> = TableDefinition::new("api_keys_by_id");

/// The persisted form of an API key. The plaintext token is NEVER stored —
/// only its argon2id hash. `prefix` is the first 12 characters of the base64
/// body (after `hc_sk_`), used for O(1) lookup during auth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: Uuid,
    pub prefix: String,
    pub hash: String,
    pub owner_uid: Uuid,
    pub scopes: Vec<String>,
    pub label: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub allowed_cidrs: Vec<String>,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    /// A display's key (H3): a wall tablet's identity, not a person's. It is
    /// listed and managed under `/displays`, and `/auth/me` answers for it as
    /// the display rather than as the admin who made it.
    #[serde(default)]
    pub display: bool,
    /// The plugin this key was minted for, when core made it to give a plugin
    /// REST access (its manifest's `api_scopes`). Core replaces it each time
    /// the plugin starts and deletes it when the plugin is uninstalled; no
    /// person owns it, so `owner_uid` is the nil UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

impl ApiKeyRecord {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.map(|exp| exp <= now).unwrap_or(false)
    }
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        !self.is_revoked() && !self.is_expired(now)
    }
}

pub struct ApiKeyStore {
    db: Arc<Database>,
}

impl ApiKeyStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(API_KEYS_BY_PREFIX)?;
            write_txn.open_table(API_KEYS_BY_ID)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    /// Persist a new API key record. Caller is responsible for ensuring
    /// the prefix doesn't collide with an existing row (generate a fresh
    /// token and retry on collision).
    pub fn create(&self, record: &ApiKeyRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
            by_prefix.insert(record.prefix.as_str(), json.as_str())?;
            let mut by_id = write_txn.open_table(API_KEYS_BY_ID)?;
            by_id.insert(record.id.to_string().as_str(), record.prefix.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Does a record with this lookup prefix already exist? Used by the
    /// issuer to probe for collisions before committing.
    pub fn prefix_exists(&self, prefix: &str) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(API_KEYS_BY_PREFIX)?;
        Ok(table.get(prefix)?.is_some())
    }

    pub fn get_by_prefix(&self, prefix: &str) -> Result<Option<ApiKeyRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(API_KEYS_BY_PREFIX)?;
        match table.get(prefix)? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("ApiKeyRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    pub fn get_by_id(&self, id: Uuid) -> Result<Option<ApiKeyRecord>> {
        let read_txn = self.db.begin_read()?;
        let by_id = read_txn.open_table(API_KEYS_BY_ID)?;
        let prefix = match by_id.get(id.to_string().as_str())? {
            Some(v) => v.value().to_string(),
            None => return Ok(None),
        };
        drop(by_id);
        let by_prefix = read_txn.open_table(API_KEYS_BY_PREFIX)?;
        match by_prefix.get(prefix.as_str())? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("ApiKeyRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    pub fn list(&self) -> Result<Vec<ApiKeyRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(API_KEYS_BY_PREFIX)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<ApiKeyRecord>(v.value()) {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    pub fn list_by_owner(&self, owner_uid: Uuid) -> Result<Vec<ApiKeyRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.owner_uid == owner_uid)
            .collect())
    }

    /// Persist a mutation to an existing record (identified by its id).
    /// Write back a record a caller read earlier and changed.
    ///
    /// Read and written in one transaction, against what is stored *now*: a
    /// key revoked or deleted since the caller read it stays revoked or
    /// deleted. Revocation only ever goes one way, so an edit (or a
    /// `last_used_at` stamp) that started before it cannot undo it, and a
    /// deleted key is never written back into existence.
    pub fn update(&self, record: &ApiKeyRecord) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
            let stored: Option<ApiKeyRecord> = match by_prefix.get(record.prefix.as_str())? {
                Some(v) => {
                    Some(serde_json::from_str(v.value()).context("ApiKeyRecord deserialize")?)
                }
                None => None,
            };
            if let Some(stored) = stored {
                let mut record = record.clone();
                if record.revoked_at.is_none() {
                    record.revoked_at = stored.revoked_at;
                }
                let json = serde_json::to_string(&record)?;
                by_prefix.insert(record.prefix.as_str(), json.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Change one key in place: read, `change`, write, in one transaction, so
    /// no other write can land in between. `false` when there is no such key.
    fn modify(&self, id: Uuid, change: impl FnOnce(&mut ApiKeyRecord)) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let found = {
            let by_id = write_txn.open_table(API_KEYS_BY_ID)?;
            let prefix = by_id
                .get(id.to_string().as_str())?
                .map(|v| v.value().to_string());
            drop(by_id);
            match prefix {
                None => false,
                Some(prefix) => {
                    let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
                    let stored: Option<ApiKeyRecord> = match by_prefix.get(prefix.as_str())? {
                        Some(v) => Some(
                            serde_json::from_str(v.value()).context("ApiKeyRecord deserialize")?,
                        ),
                        None => None,
                    };
                    match stored {
                        None => false,
                        Some(mut rec) => {
                            change(&mut rec);
                            let json = serde_json::to_string(&rec)?;
                            by_prefix.insert(prefix.as_str(), json.as_str())?;
                            true
                        }
                    }
                }
            }
        };
        write_txn.commit()?;
        Ok(found)
    }

    pub fn revoke(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        self.modify(id, |rec| {
            if rec.revoked_at.is_none() {
                rec.revoked_at = Some(now);
            }
        })
    }

    /// Remove a key outright, both rows. For keys core made itself and
    /// replaces (a plugin's), where a revoked record kept forever would only
    /// pile up: one per plugin start. `false` when there was no such key.
    pub fn delete(&self, id: Uuid) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let found = {
            let mut by_id = write_txn.open_table(API_KEYS_BY_ID)?;
            let prefix = by_id
                .remove(id.to_string().as_str())?
                .map(|v| v.value().to_string());
            match prefix {
                Some(prefix) => {
                    let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
                    by_prefix.remove(prefix.as_str())?;
                    true
                }
                None => false,
            }
        };
        write_txn.commit()?;
        Ok(found)
    }

    /// The keys core minted for `plugin_id`.
    pub fn list_by_plugin(&self, plugin_id: &str) -> Result<Vec<ApiKeyRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.plugin.as_deref() == Some(plugin_id))
            .collect())
    }

    pub fn touch_last_used(&self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        self.modify(id, |rec| rec.last_used_at = Some(now))?;
        Ok(())
    }

    /// Replace the secret material on an existing key while keeping its id,
    /// owner, scopes, label, CIDRs, and expiry. Used by the `rotate` flow.
    ///
    /// The prefix moves, so this is not a simple in-place update: the old
    /// prefix row is deleted and a new one inserted, and the `by_id` index
    /// points at the new prefix. All of it in one transaction, against the key
    /// as stored now, so a key deleted meanwhile is not brought back and a
    /// revocation is kept: rotating never makes a revoked key work again.
    pub fn replace_secret(
        &self,
        id: Uuid,
        new_prefix: String,
        new_hash: String,
        now: DateTime<Utc>,
    ) -> Result<Option<ApiKeyRecord>> {
        let write_txn = self.db.begin_write()?;
        let rotated = {
            let mut by_id = write_txn.open_table(API_KEYS_BY_ID)?;
            let old_prefix = by_id
                .get(id.to_string().as_str())?
                .map(|v| v.value().to_string());
            let Some(old_prefix) = old_prefix else {
                return Ok(None);
            };
            let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
            let stored: Option<ApiKeyRecord> = match by_prefix.get(old_prefix.as_str())? {
                Some(v) => {
                    Some(serde_json::from_str(v.value()).context("ApiKeyRecord deserialize")?)
                }
                None => None,
            };
            let Some(mut rec) = stored else {
                return Ok(None);
            };
            rec.prefix = new_prefix.clone();
            rec.hash = new_hash;
            rec.last_used_at = None;
            rec.created_at = now;
            let json = serde_json::to_string(&rec)?;
            by_prefix.remove(old_prefix.as_str())?;
            by_prefix.insert(new_prefix.as_str(), json.as_str())?;
            by_id.insert(id.to_string().as_str(), new_prefix.as_str())?;
            rec
        };
        write_txn.commit()?;
        Ok(Some(rotated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, ApiKeyStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.redb");
        let db = Arc::new(Database::create(path).unwrap());
        let store = ApiKeyStore::new(db).unwrap();
        (dir, store)
    }

    fn sample(prefix: &str, label: &str) -> ApiKeyRecord {
        ApiKeyRecord {
            id: Uuid::new_v4(),
            prefix: prefix.into(),
            hash: "$argon2id$...".into(),
            owner_uid: Uuid::new_v4(),
            scopes: vec!["devices:read".into()],
            label: label.into(),
            created_at: Utc::now(),
            last_used_at: None,
            expires_at: None,
            allowed_cidrs: vec![],
            revoked_at: None,
            display: false,
            plugin: None,
        }
    }

    #[test]
    fn delete_removes_both_rows_and_list_by_plugin_finds_a_plugins_keys() {
        let (_d, s) = fresh_store();
        let mut a = sample("aaaaaaaaaaaa", "plugin plugin.mcp");
        a.plugin = Some("plugin.mcp".into());
        let b = sample("bbbbbbbbbbbb", "bot");
        s.create(&a).unwrap();
        s.create(&b).unwrap();
        let mine = s.list_by_plugin("plugin.mcp").unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, a.id);
        assert!(s.delete(a.id).unwrap());
        assert!(s.get_by_id(a.id).unwrap().is_none());
        assert!(s.get_by_prefix("aaaaaaaaaaaa").unwrap().is_none());
        assert!(!s.delete(a.id).unwrap());
        assert!(s.get_by_id(b.id).unwrap().is_some());
    }

    #[test]
    fn a_record_from_before_plugin_keys_reads_as_nobodys() {
        let rec = sample("cccccccccccc", "old");
        let mut v = serde_json::to_value(&rec).unwrap();
        assert!(v.get("plugin").is_none());
        v.as_object_mut().unwrap().remove("plugin");
        let back: ApiKeyRecord = serde_json::from_value(v).unwrap();
        assert!(back.plugin.is_none());
    }

    #[test]
    fn create_and_get_by_prefix() {
        let (_d, s) = fresh_store();
        let rec = sample("abc123def456", "mcp");
        s.create(&rec).unwrap();
        let got = s.get_by_prefix("abc123def456").unwrap().unwrap();
        assert_eq!(got.id, rec.id);
        assert_eq!(got.label, "mcp");
    }

    #[test]
    fn create_and_get_by_id() {
        let (_d, s) = fresh_store();
        let rec = sample("xyz123456789", "bot");
        s.create(&rec).unwrap();
        let got = s.get_by_id(rec.id).unwrap().unwrap();
        assert_eq!(got.label, "bot");
    }

    #[test]
    fn prefix_exists_reflects_writes() {
        let (_d, s) = fresh_store();
        assert!(!s.prefix_exists("abcdefghijkl").unwrap());
        s.create(&sample("abcdefghijkl", "x")).unwrap();
        assert!(s.prefix_exists("abcdefghijkl").unwrap());
    }

    #[test]
    fn list_returns_all_sorted_by_created_at() {
        let (_d, s) = fresh_store();
        let mut a = sample("aaaaaaaaaaaa", "a");
        let mut b = sample("bbbbbbbbbbbb", "b");
        a.created_at = Utc::now() - chrono::Duration::seconds(10);
        b.created_at = Utc::now();
        s.create(&b).unwrap();
        s.create(&a).unwrap();
        let list = s.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label, "a"); // older first
        assert_eq!(list[1].label, "b");
    }

    #[test]
    fn list_by_owner_filters() {
        let (_d, s) = fresh_store();
        let owner1 = Uuid::new_v4();
        let owner2 = Uuid::new_v4();
        let mut a = sample("aaaaaaaaaaaa", "a");
        a.owner_uid = owner1;
        let mut b = sample("bbbbbbbbbbbb", "b");
        b.owner_uid = owner2;
        s.create(&a).unwrap();
        s.create(&b).unwrap();
        assert_eq!(s.list_by_owner(owner1).unwrap().len(), 1);
        assert_eq!(s.list_by_owner(owner2).unwrap().len(), 1);
    }

    /// A stamp or an edit that read the key before it was revoked must not
    /// write it back unrevoked: the race CI caught as a deleted display's key
    /// still signing in.
    #[test]
    fn a_stale_write_never_undoes_a_revocation() {
        let (_d, s) = fresh_store();
        let rec = sample("abcdefghijkl", "x");
        s.create(&rec).unwrap();
        let read_before = s.get_by_id(rec.id).unwrap().unwrap();
        assert!(s.revoke(rec.id, Utc::now()).unwrap());

        let mut edited = read_before.clone();
        edited.last_used_at = Some(Utc::now());
        s.update(&edited).unwrap();
        assert!(s.get_by_id(rec.id).unwrap().unwrap().is_revoked());

        s.touch_last_used(rec.id, Utc::now()).unwrap();
        let got = s.get_by_id(rec.id).unwrap().unwrap();
        assert!(got.is_revoked());
        assert!(got.last_used_at.is_some());
    }

    #[test]
    fn a_deleted_key_is_never_written_back() {
        let (_d, s) = fresh_store();
        let rec = sample("abcdefghijkl", "x");
        s.create(&rec).unwrap();
        let read_before = s.get_by_id(rec.id).unwrap().unwrap();
        assert!(s.delete(rec.id).unwrap());
        s.update(&read_before).unwrap();
        s.touch_last_used(rec.id, Utc::now()).unwrap();
        assert!(s.get_by_id(rec.id).unwrap().is_none());
        assert!(s.get_by_prefix("abcdefghijkl").unwrap().is_none());
    }

    #[test]
    fn rotating_keeps_a_revocation_and_never_revives_a_deleted_key() {
        let (_d, s) = fresh_store();
        let rec = sample("abcdefghijkl", "x");
        s.create(&rec).unwrap();
        assert!(s.revoke(rec.id, Utc::now()).unwrap());
        let got = s
            .replace_secret(rec.id, "mnopqrstuvwx".into(), "h2".into(), Utc::now())
            .unwrap()
            .unwrap();
        assert!(got.is_revoked());
        assert!(s
            .get_by_prefix("mnopqrstuvwx")
            .unwrap()
            .unwrap()
            .is_revoked());
        assert!(s.get_by_prefix("abcdefghijkl").unwrap().is_none());

        assert!(s.delete(rec.id).unwrap());
        assert!(s
            .replace_secret(rec.id, "yzabcdefghij".into(), "h3".into(), Utc::now())
            .unwrap()
            .is_none());
        assert!(s.get_by_id(rec.id).unwrap().is_none());
    }

    #[test]
    fn revoke_sets_timestamp() {
        let (_d, s) = fresh_store();
        let rec = sample("abcdefghijkl", "x");
        s.create(&rec).unwrap();
        let now = Utc::now();
        assert!(s.revoke(rec.id, now).unwrap());
        let got = s.get_by_id(rec.id).unwrap().unwrap();
        assert_eq!(got.revoked_at, Some(now));
        assert!(got.is_revoked());
    }

    #[test]
    fn revoke_missing_returns_false() {
        let (_d, s) = fresh_store();
        assert!(!s.revoke(Uuid::new_v4(), Utc::now()).unwrap());
    }

    #[test]
    fn is_usable_reflects_expiry_and_revocation() {
        let mut rec = sample("abcdefghijkl", "x");
        let now = Utc::now();
        assert!(rec.is_usable(now));
        rec.expires_at = Some(now - chrono::Duration::seconds(1));
        assert!(!rec.is_usable(now));
        rec.expires_at = None;
        rec.revoked_at = Some(now);
        assert!(!rec.is_usable(now));
    }
}
