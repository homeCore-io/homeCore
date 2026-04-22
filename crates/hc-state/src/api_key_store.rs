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
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const API_KEYS_BY_PREFIX: TableDefinition<&str, &str> =
    TableDefinition::new("api_keys_by_prefix");
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
    pub fn update(&self, record: &ApiKeyRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
            by_prefix.insert(record.prefix.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn revoke(&self, id: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let Some(mut rec) = self.get_by_id(id)? else {
            return Ok(false);
        };
        if rec.revoked_at.is_some() {
            return Ok(true);
        }
        rec.revoked_at = Some(now);
        self.update(&rec)?;
        Ok(true)
    }

    pub fn touch_last_used(&self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        let Some(mut rec) = self.get_by_id(id)? else {
            return Ok(());
        };
        rec.last_used_at = Some(now);
        self.update(&rec)?;
        Ok(())
    }

    /// Replace the secret material on an existing key while keeping its id,
    /// owner, scopes, label, CIDRs, and expiry. Used by the `rotate` flow.
    ///
    /// The prefix moves, so this is not a simple in-place update — the old
    /// prefix row is deleted and a new one inserted. The `by_id` index is
    /// updated to point at the new prefix.
    pub fn replace_secret(
        &self,
        id: Uuid,
        new_prefix: String,
        new_hash: String,
        now: DateTime<Utc>,
    ) -> Result<Option<ApiKeyRecord>> {
        let Some(mut rec) = self.get_by_id(id)? else {
            return Ok(None);
        };
        let old_prefix = std::mem::replace(&mut rec.prefix, new_prefix.clone());
        rec.hash = new_hash;
        rec.last_used_at = None;
        rec.revoked_at = None;
        rec.created_at = now;

        let json = serde_json::to_string(&rec)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(API_KEYS_BY_PREFIX)?;
            by_prefix.remove(old_prefix.as_str())?;
            by_prefix.insert(new_prefix.as_str(), json.as_str())?;
            let mut by_id = write_txn.open_table(API_KEYS_BY_ID)?;
            by_id.insert(id.to_string().as_str(), new_prefix.as_str())?;
        }
        write_txn.commit()?;
        Ok(Some(rec))
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
        }
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
