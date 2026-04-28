//! redb-backed storage for refresh tokens.
//!
//! Refresh tokens form a chain per session: every successful
//! `/auth/refresh` call marks the presented token as used, mints a new
//! one, and links them via `parent_id`. If a token marked `used_at` is
//! presented again, the entire chain is revoked (reuse indicates likely
//! theft — the original holder has already rotated once).
//!
//! Two tables:
//! - `refresh_tokens_by_prefix`: prefix (12 chars of base64 body) → JSON
//!   of RefreshTokenRecord. Used by the refresh handler for O(1) lookup.
//! - `refresh_tokens_by_id`: uuid id → prefix. Used to revoke the chain
//!   starting from the parent.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const REFRESH_BY_PREFIX: TableDefinition<&str, &str> =
    TableDefinition::new("refresh_tokens_by_prefix");
const REFRESH_BY_ID: TableDefinition<&str, &str> = TableDefinition::new("refresh_tokens_by_id");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    /// Lookup prefix (first 12 chars of base64 body).
    pub prefix: String,
    /// argon2id hash of the full `hc_rt_...` token. Secret is never stored.
    pub hash: String,
    /// The id of the refresh token that this one was issued in exchange
    /// for, if any. Lets us walk the chain on reuse detection.
    pub parent_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// When this token was presented to `/auth/refresh` and retired. Set
    /// once; its presence here means any subsequent presentation is reuse.
    pub used_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    /// Explicit revocation (manual, or chain-revocation triggered by reuse).
    pub revoked_at: Option<DateTime<Utc>>,
    /// Soft fingerprint for the "active sessions" UI. Best-effort.
    #[serde(default)]
    pub user_agent: String,
}

impl RefreshTokenRecord {
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.used_at.is_none() && self.revoked_at.is_none() && self.expires_at > now
    }
}

pub struct RefreshTokenStore {
    db: Arc<Database>,
}

impl RefreshTokenStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(REFRESH_BY_PREFIX)?;
            write_txn.open_table(REFRESH_BY_ID)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn create(&self, record: &RefreshTokenRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(REFRESH_BY_PREFIX)?;
            by_prefix.insert(record.prefix.as_str(), json.as_str())?;
            let mut by_id = write_txn.open_table(REFRESH_BY_ID)?;
            by_id.insert(record.id.to_string().as_str(), record.prefix.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn prefix_exists(&self, prefix: &str) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(REFRESH_BY_PREFIX)?;
        Ok(table.get(prefix)?.is_some())
    }

    pub fn get_by_prefix(&self, prefix: &str) -> Result<Option<RefreshTokenRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(REFRESH_BY_PREFIX)?;
        match table.get(prefix)? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("RefreshTokenRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    pub fn get_by_id(&self, id: Uuid) -> Result<Option<RefreshTokenRecord>> {
        let read_txn = self.db.begin_read()?;
        let by_id = read_txn.open_table(REFRESH_BY_ID)?;
        let prefix = match by_id.get(id.to_string().as_str())? {
            Some(v) => v.value().to_string(),
            None => return Ok(None),
        };
        drop(by_id);
        let by_prefix = read_txn.open_table(REFRESH_BY_PREFIX)?;
        match by_prefix.get(prefix.as_str())? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("RefreshTokenRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    pub fn list_by_user(&self, user_id: Uuid) -> Result<Vec<RefreshTokenRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(REFRESH_BY_PREFIX)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<RefreshTokenRecord>(v.value()) {
                if rec.user_id == user_id {
                    out.push(rec);
                }
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    pub fn update(&self, record: &RefreshTokenRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(REFRESH_BY_PREFIX)?;
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

    /// Mark a token as used (retired by a successful refresh).
    pub fn mark_used(&self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        let Some(mut rec) = self.get_by_id(id)? else {
            return Ok(());
        };
        rec.used_at = Some(now);
        self.update(&rec)
    }

    /// Revoke the full refresh chain rooted at `any_id`. Walks `parent_id`
    /// up the tree and also revokes every descendant. Called when reuse of
    /// a retired token is detected — indicates potential token theft.
    pub fn revoke_chain(&self, any_id: Uuid, now: DateTime<Utc>) -> Result<usize> {
        // Collect ancestors: walk parents.
        let mut to_revoke: Vec<Uuid> = Vec::new();
        let mut cursor = Some(any_id);
        while let Some(id) = cursor {
            if let Some(rec) = self.get_by_id(id)? {
                to_revoke.push(rec.id);
                cursor = rec.parent_id;
            } else {
                break;
            }
        }
        // Descendants: linear scan over by_user then filter on parent chain.
        // Cheaper than a per-node scan since chains are short in practice.
        // We find the user from the first hit, then scan their records for
        // any whose parent_id chain lands on one of `to_revoke`.
        let root_user = match self.get_by_id(any_id)? {
            Some(r) => r.user_id,
            None => return Ok(0),
        };
        let all = self.list_by_user(root_user)?;
        // Multi-pass to catch grandchildren etc.
        let mut changed = true;
        while changed {
            changed = false;
            for rec in &all {
                if to_revoke.contains(&rec.id) {
                    continue;
                }
                if let Some(parent) = rec.parent_id {
                    if to_revoke.contains(&parent) {
                        to_revoke.push(rec.id);
                        changed = true;
                    }
                }
            }
        }

        let mut count = 0usize;
        for id in to_revoke {
            if self.revoke(id, now)? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Delete tokens that are both expired and used/revoked. Safe to call
    /// periodically — active tokens (usable) are never touched.
    pub fn prune(&self, now: DateTime<Utc>) -> Result<usize> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(REFRESH_BY_PREFIX)?;
        let mut to_delete: Vec<(String, String)> = Vec::new();
        for row in table.iter()? {
            let (k, v) = row?;
            if let Ok(rec) = serde_json::from_str::<RefreshTokenRecord>(v.value()) {
                let retired = rec.used_at.is_some() || rec.revoked_at.is_some();
                let expired = rec.expires_at <= now;
                if retired && expired {
                    to_delete.push((k.value().to_string(), rec.id.to_string()));
                }
            }
        }
        drop(table);
        drop(read_txn);

        let mut count = 0usize;
        let write_txn = self.db.begin_write()?;
        {
            let mut by_prefix = write_txn.open_table(REFRESH_BY_PREFIX)?;
            let mut by_id = write_txn.open_table(REFRESH_BY_ID)?;
            for (prefix, id) in to_delete {
                by_prefix.remove(prefix.as_str())?;
                by_id.remove(id.as_str())?;
                count += 1;
            }
        }
        write_txn.commit()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, RefreshTokenStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.redb");
        let db = Arc::new(Database::create(path).unwrap());
        let store = RefreshTokenStore::new(db).unwrap();
        (dir, store)
    }

    fn sample(user: Uuid, prefix: &str, parent: Option<Uuid>) -> RefreshTokenRecord {
        RefreshTokenRecord {
            id: Uuid::new_v4(),
            user_id: user,
            prefix: prefix.into(),
            hash: "$argon2id$...".into(),
            parent_id: parent,
            created_at: Utc::now(),
            used_at: None,
            expires_at: Utc::now() + chrono::Duration::days(30),
            revoked_at: None,
            user_agent: String::new(),
        }
    }

    #[test]
    fn create_and_lookup() {
        let (_d, s) = fresh();
        let u = Uuid::new_v4();
        let r = sample(u, "abcdefghijkl", None);
        s.create(&r).unwrap();
        let got = s.get_by_prefix("abcdefghijkl").unwrap().unwrap();
        assert_eq!(got.id, r.id);
        let got2 = s.get_by_id(r.id).unwrap().unwrap();
        assert_eq!(got2.prefix, "abcdefghijkl");
    }

    #[test]
    fn mark_used_then_is_no_longer_usable() {
        let (_d, s) = fresh();
        let r = sample(Uuid::new_v4(), "aaaaaaaaaaaa", None);
        s.create(&r).unwrap();
        s.mark_used(r.id, Utc::now()).unwrap();
        let got = s.get_by_id(r.id).unwrap().unwrap();
        assert!(!got.is_usable(Utc::now()));
    }

    #[test]
    fn revoke_chain_propagates() {
        let (_d, s) = fresh();
        let u = Uuid::new_v4();
        let a = sample(u, "aaaaaaaaaaaa", None);
        s.create(&a).unwrap();
        let b = sample(u, "bbbbbbbbbbbb", Some(a.id));
        s.create(&b).unwrap();
        let c = sample(u, "cccccccccccc", Some(b.id));
        s.create(&c).unwrap();

        // Revoking the middle should take out the whole chain.
        let n = s.revoke_chain(b.id, Utc::now()).unwrap();
        assert!(n >= 3, "expected full chain revocation, got {n}");
        assert!(s.get_by_id(a.id).unwrap().unwrap().revoked_at.is_some());
        assert!(s.get_by_id(b.id).unwrap().unwrap().revoked_at.is_some());
        assert!(s.get_by_id(c.id).unwrap().unwrap().revoked_at.is_some());
    }

    #[test]
    fn prune_removes_only_expired_and_retired() {
        let (_d, s) = fresh();
        let u = Uuid::new_v4();

        let active = sample(u, "active1aaaaa", None);
        s.create(&active).unwrap();

        let mut retired_fresh = sample(u, "retired2aaaa", None);
        retired_fresh.used_at = Some(Utc::now());
        s.create(&retired_fresh).unwrap();

        let mut retired_expired = sample(u, "retiredexpir", None);
        retired_expired.used_at = Some(Utc::now() - chrono::Duration::days(40));
        retired_expired.expires_at = Utc::now() - chrono::Duration::days(10);
        s.create(&retired_expired).unwrap();

        let n = s.prune(Utc::now()).unwrap();
        assert_eq!(n, 1);
        assert!(s.get_by_id(active.id).unwrap().is_some());
        assert!(s.get_by_id(retired_fresh.id).unwrap().is_some());
        assert!(s.get_by_id(retired_expired.id).unwrap().is_none());
    }
}
