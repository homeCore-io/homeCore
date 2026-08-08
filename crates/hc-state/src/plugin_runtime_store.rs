//! redb-backed storage for plugin runtimes.
//!
//! A runtime is a container the operator runs, which enrolls with core and — once
//! approved — hosts plugins written in something other than Rust. This store holds
//! the enrollment record: who asked, whether an admin said yes, and the hash of the
//! secret it polls with.
//!
//! One table, keyed by `runtime_id`. There is no second index: a runtime is looked
//! up by the id it supplied, and the admin list is a full scan of a set that is
//! measured in single digits.
//!
//! What is deliberately *not* here: the runtime's MQTT credential and API key. Those
//! are minted on approval and handed over once, exactly as a binary plugin's
//! credential is — this store records that approval happened, not what was granted.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const RUNTIMES: TableDefinition<&str, &str> = TableDefinition::new("plugin_runtimes");
/// Enrollment tokens, keyed by lookup prefix. Same table file as the runtimes
/// themselves: both answer "who may join", and splitting them would mean two
/// stores that always change together.
const ENROLL_TOKENS: TableDefinition<&str, &str> = TableDefinition::new("plugin_runtime_tokens");

/// Where an enrollment stands. Mirrors `hc_api_types::plugin_runtimes::RuntimeStatus`;
/// duplicated rather than shared so the storage layer does not depend on the wire crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeStatus {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub runtime_id: String,
    /// base64 ed25519 public key. Proof of possession on re-enrollment: the
    /// `runtime_id` appears in logs, so without this anyone who read one could
    /// re-enroll as a known-good runtime and be handed its credentials.
    pub public_key: String,
    pub kind: String,
    pub abi: String,
    pub arch: String,
    pub host_version: String,
    pub sdk_version: String,
    pub hostname: String,
    pub network_mode: String,
    pub status: RuntimeStatus,
    /// Short human-comparable code, shown to the admin while pending and cleared
    /// once resolved. Never an authentication credential.
    #[serde(default)]
    pub code: Option<String>,
    /// argon2id hash of the enrollment secret. The secret itself is never stored.
    #[serde(default)]
    pub secret_hash: Option<String>,
    /// Address the enrollment arrived from, for the admin's judgement at approval.
    #[serde(default)]
    pub source_ip: Option<String>,
    /// The plugin id it registers under once approved.
    #[serde(default)]
    pub plugin_id: Option<String>,
    /// How many times this identity has been denied. Bounds casual spraying; the
    /// real defences are the whitelist, the code match and the admin.
    #[serde(default)]
    pub denial_count: u32,
    /// Set when `denial_count` crosses the configured limit.
    #[serde(default)]
    pub cooldown_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// When the pending record stops being answerable.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl RuntimeRecord {
    /// Whether this record can still be approved or polled.
    ///
    /// An expired pending record is not merely stale: answering it would let a
    /// request the admin never saw sit around indefinitely waiting for a
    /// mis-click.
    pub fn is_pending_open(&self, now: DateTime<Utc>) -> bool {
        self.status == RuntimeStatus::Pending && self.expires_at.is_none_or(|e| e > now)
    }

    /// Whether a denied identity may enroll again yet.
    pub fn may_retry(&self, now: DateTime<Utc>) -> bool {
        self.cooldown_until.is_none_or(|c| c <= now)
    }
}

/// An admin-issued, single-use enrollment token.
///
/// Token mode's whole point is that the operator expresses intent *before* a
/// container asks, so nothing is ever left pending. The token is that intent,
/// and it is spent the moment it works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollTokenRecord {
    /// Indexed lookup prefix of the token body.
    pub prefix: String,
    /// argon2id hash of the full token. The token itself is never stored.
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set when redeemed. Present means spent — a second use is not an error to
    /// be lenient about, it means the token leaked or is being replayed.
    #[serde(default)]
    pub used_at: Option<DateTime<Utc>>,
    /// Which runtime redeemed it, for the audit trail.
    #[serde(default)]
    pub used_by: Option<String>,
}

impl EnrollTokenRecord {
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.used_at.is_none() && self.expires_at > now
    }
}

pub struct PluginRuntimeStore {
    db: Arc<Database>,
}

impl PluginRuntimeStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(RUNTIMES)?;
            write_txn.open_table(ENROLL_TOKENS)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn upsert(&self, record: &RuntimeRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut t = write_txn.open_table(RUNTIMES)?;
            t.insert(record.runtime_id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get(&self, runtime_id: &str) -> Result<Option<RuntimeRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(RUNTIMES)?;
        match t.get(runtime_id)? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("RuntimeRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    pub fn list(&self) -> Result<Vec<RuntimeRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(RUNTIMES)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<RuntimeRecord>(v.value()) {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    pub fn delete(&self, runtime_id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed;
        {
            let mut t = write_txn.open_table(RUNTIMES)?;
            existed = t.remove(runtime_id)?.is_some();
        }
        write_txn.commit()?;
        Ok(existed)
    }

    /// How many records are currently pending and unexpired.
    ///
    /// The cap this feeds is what stops an open enrollment endpoint being used to
    /// fill the admin's screen with plausible-looking requests until one is
    /// approved by fatigue.
    pub fn pending_count(&self, now: DateTime<Utc>) -> Result<usize> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|r| r.is_pending_open(now))
            .count())
    }

    /// Drop pending records that have expired. Denied records are kept — their
    /// `denial_count` is the memory that makes retry limits mean anything.
    pub fn purge_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        let stale: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|r| {
                r.status == RuntimeStatus::Pending && r.expires_at.is_some_and(|e| e <= now)
            })
            .map(|r| r.runtime_id)
            .collect();
        for id in &stale {
            self.delete(id)?;
        }
        Ok(stale.len())
    }
}

// ── Enrollment tokens ────────────────────────────────────────────────────────

impl PluginRuntimeStore {
    pub fn create_token(&self, record: &EnrollTokenRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut t = write_txn.open_table(ENROLL_TOKENS)?;
            t.insert(record.prefix.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_token(&self, prefix: &str) -> Result<Option<EnrollTokenRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(ENROLL_TOKENS)?;
        match t.get(prefix)? {
            Some(v) => Ok(Some(
                serde_json::from_str(v.value()).context("EnrollTokenRecord deserialize")?,
            )),
            None => Ok(None),
        }
    }

    /// Mark a token spent. Returns false when it was already used or expired, so
    /// the caller cannot accidentally honour a replay by ignoring the result.
    pub fn redeem_token(&self, prefix: &str, runtime_id: &str, now: DateTime<Utc>) -> Result<bool> {
        let Some(mut rec) = self.get_token(prefix)? else {
            return Ok(false);
        };
        if !rec.is_usable(now) {
            return Ok(false);
        }
        rec.used_at = Some(now);
        rec.used_by = Some(runtime_id.to_string());
        self.create_token(&rec)?;
        Ok(true)
    }

    pub fn list_tokens(&self) -> Result<Vec<EnrollTokenRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(ENROLL_TOKENS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<EnrollTokenRecord>(v.value()) {
                out.push(rec);
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn store() -> (PluginRuntimeStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.redb")).unwrap());
        (PluginRuntimeStore::new(db).unwrap(), dir)
    }

    fn rec(id: &str, status: RuntimeStatus) -> RuntimeRecord {
        RuntimeRecord {
            runtime_id: id.into(),
            public_key: "PUB".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
            host_version: "0.1.0".into(),
            sdk_version: "0.2.0".into(),
            hostname: "pyhost".into(),
            network_mode: "host".into(),
            status,
            code: None,
            secret_hash: None,
            source_ip: None,
            plugin_id: None,
            denial_count: 0,
            cooldown_until: None,
            created_at: Utc::now(),
            expires_at: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn round_trips_and_lists_in_creation_order() {
        let (s, _d) = store();
        let mut a = rec("rt-a", RuntimeStatus::Pending);
        a.created_at = Utc::now() - Duration::minutes(5);
        let b = rec("rt-b", RuntimeStatus::Approved);
        s.upsert(&b).unwrap();
        s.upsert(&a).unwrap();

        assert_eq!(s.get("rt-a").unwrap().unwrap().kind, "python");
        let ids: Vec<String> = s
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.runtime_id)
            .collect();
        assert_eq!(ids, vec!["rt-a", "rt-b"], "oldest first");
    }

    /// An expired pending record must not be approvable. Otherwise a request the
    /// admin never got round to looking at stays live indefinitely, waiting for a
    /// mis-click long after the operator gave up and moved on.
    #[test]
    fn an_expired_pending_record_is_closed() {
        let now = Utc::now();
        let mut r = rec("rt-a", RuntimeStatus::Pending);
        r.expires_at = Some(now - Duration::minutes(1));
        assert!(!r.is_pending_open(now));

        r.expires_at = Some(now + Duration::minutes(1));
        assert!(r.is_pending_open(now));
    }

    /// A record with no expiry is open — token-mode enrollments never go pending,
    /// so `None` must not read as "already expired".
    #[test]
    fn no_expiry_means_open_not_expired() {
        let r = rec("rt-a", RuntimeStatus::Pending);
        assert!(r.is_pending_open(Utc::now()));
    }

    #[test]
    fn pending_count_ignores_expired_and_resolved() {
        let (s, _d) = store();
        let now = Utc::now();

        s.upsert(&rec("open", RuntimeStatus::Pending)).unwrap();

        let mut expired = rec("expired", RuntimeStatus::Pending);
        expired.expires_at = Some(now - Duration::minutes(1));
        s.upsert(&expired).unwrap();

        s.upsert(&rec("approved", RuntimeStatus::Approved)).unwrap();
        s.upsert(&rec("denied", RuntimeStatus::Denied)).unwrap();

        assert_eq!(s.pending_count(now).unwrap(), 1);
    }

    /// Purging must not forget a denial. `denial_count` is the whole memory behind
    /// the retry limit — drop it and an identity gets unlimited attempts by simply
    /// waiting for its pending record to age out.
    #[test]
    fn purge_removes_expired_pending_but_keeps_denials() {
        let (s, _d) = store();
        let now = Utc::now();

        let mut expired = rec("expired", RuntimeStatus::Pending);
        expired.expires_at = Some(now - Duration::minutes(1));
        s.upsert(&expired).unwrap();

        let mut denied = rec("denied", RuntimeStatus::Denied);
        denied.denial_count = 2;
        denied.expires_at = Some(now - Duration::hours(1));
        s.upsert(&denied).unwrap();

        assert_eq!(s.purge_expired(now).unwrap(), 1);
        assert!(s.get("expired").unwrap().is_none());
        assert_eq!(s.get("denied").unwrap().unwrap().denial_count, 2);
    }

    #[test]
    fn cooldown_gates_retry() {
        let now = Utc::now();
        let mut r = rec("rt-a", RuntimeStatus::Denied);
        assert!(r.may_retry(now), "no cooldown set");

        r.cooldown_until = Some(now + Duration::minutes(30));
        assert!(!r.may_retry(now));

        r.cooldown_until = Some(now - Duration::minutes(1));
        assert!(r.may_retry(now), "cooldown elapsed");
    }

    fn token(prefix: &str, expires_in: Duration) -> EnrollTokenRecord {
        EnrollTokenRecord {
            prefix: prefix.into(),
            hash: "HASH".into(),
            created_at: Utc::now(),
            expires_at: Utc::now() + expires_in,
            used_at: None,
            used_by: None,
        }
    }

    #[test]
    fn a_token_redeems_once() {
        let (s, _d) = store();
        let now = Utc::now();
        s.create_token(&token("abc", Duration::hours(1))).unwrap();

        assert!(s.redeem_token("abc", "rt-a", now).unwrap(), "first use");
        assert!(
            !s.redeem_token("abc", "rt-b", now).unwrap(),
            "a spent token must not work again — a second use means it leaked"
        );

        let rec = s.get_token("abc").unwrap().unwrap();
        assert_eq!(
            rec.used_by.as_deref(),
            Some("rt-a"),
            "first redeemer recorded"
        );
    }

    #[test]
    fn an_expired_token_cannot_be_redeemed() {
        let (s, _d) = store();
        let now = Utc::now();
        s.create_token(&token("old", Duration::minutes(-1)))
            .unwrap();
        assert!(!s.redeem_token("old", "rt-a", now).unwrap());
    }

    #[test]
    fn redeeming_an_unknown_token_is_false_not_an_error() {
        let (s, _d) = store();
        assert!(!s.redeem_token("nope", "rt-a", Utc::now()).unwrap());
    }
}
