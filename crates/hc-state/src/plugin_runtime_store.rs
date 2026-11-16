//! redb-backed storage for plugin runtimes.
//!
//! A runtime is a container the operator runs, which hosts plugins written in
//! something other than Rust. An admin adds one in homeCore, which makes its
//! login on the spot and shows a connection key once; the operator pastes the
//! key into the runtime. This store holds what was added, the hash of its key,
//! and what the runtime last reported about itself.
//!
//! One table, keyed by `runtime_id`, plus the placements each runtime should be
//! running. A runtime's broker login is found by its plugin id, which is a scan
//! of a set measured in single digits.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// `_v2`: runtimes used to ask to join and wait for approval, and those records
/// have a different shape. None were in use; they are left unread.
const RUNTIMES: TableDefinition<&str, &str> = TableDefinition::new("plugin_runtimes_v2");
/// Placements, keyed `<runtime_id>\u{1f}<plugin_id>`. Core is the authority on
/// what should be running where; a runtime reports what it actually has and core
/// replays the difference, so this table is the desired state rather than a
/// record of what happened.
const PLACEMENTS: TableDefinition<&str, &str> = TableDefinition::new("plugin_runtime_placements");

/// Separator for the composite key. A unit separator cannot appear in a plugin
/// id (they are dotted identifiers) or a runtime id (`rt-` plus hex), so no pair
/// can collide with another by concatenation.
const KEY_SEP: char = '\u{1f}';

/// One runtime an admin added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub runtime_id: String,
    /// What the admin called it.
    pub name: String,
    /// The language it hosts, e.g. `python`, and the ABI its artifacts must
    /// match, e.g. `cp312-manylinux_2_28`. Chosen when it is added; a runtime
    /// whose image disagrees refuses the key rather than installing plugins that
    /// cannot import.
    pub kind: String,
    pub abi: String,
    /// The plugin id it registers under, and its broker username.
    pub plugin_id: String,
    /// argon2id hash of the runtime's secret: the one password it uses for the
    /// broker and for homeCore's API. The secret itself is never stored;
    /// Reset login replaces this hash, which cuts the old key off at once.
    pub key_hash: String,
    /// The address the admin said the runtime reaches homeCore at, written into
    /// the key. Kept so Show details can say it again.
    pub homecore_url: String,
    pub created_at: DateTime<Utc>,
    /// The most plugins it may hold, set by the admin for the machine it runs on.
    /// None means no limit.
    #[serde(default)]
    pub max_plugins: Option<u32>,
    /// What the runtime said about itself the last time it checked in.
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub host_version: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub network_mode: Option<String>,
    #[serde(default)]
    pub source_ip: Option<String>,
    #[serde(default)]
    pub page_port: Option<u16>,
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// One plugin, placed on one runtime.
///
/// Carries everything needed to re-provision it from nothing, because that is
/// the point: a runtime that loses its volume reports an empty set, and gets
/// all of this replayed at it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub runtime_id: String,
    pub plugin_id: String,
    pub version: String,
    /// Where core fetched the artifact from, kept for diagnosis rather than for
    /// the runtime — the runtime pulls verified bytes from core, never from the
    /// registry.
    pub artifact_url: String,
    /// Hex sha256 of the artifact. The runtime checks the bytes it receives
    /// against this, which catches transport corruption.
    pub sha256: String,
    /// The plugin's operator config, which core owns exactly as it does for a
    /// binary plugin. Includes the minted MQTT credential.
    pub config: String,
    pub placed_at: DateTime<Utc>,
}

pub struct PluginRuntimeStore {
    db: Arc<Database>,
}

impl PluginRuntimeStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(RUNTIMES)?;
            write_txn.open_table(PLACEMENTS)?;
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

    /// The runtime that logs in to the broker as `plugin_id`, if one does.
    pub fn by_plugin_id(&self, plugin_id: &str) -> Result<Option<RuntimeRecord>> {
        Ok(self.list()?.into_iter().find(|r| r.plugin_id == plugin_id))
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
}

// ── Placements ───────────────────────────────────────────────────────────────

fn placement_key(runtime_id: &str, plugin_id: &str) -> String {
    format!("{runtime_id}{KEY_SEP}{plugin_id}")
}

impl PluginRuntimeStore {
    pub fn place(&self, record: &PlacementRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let key = placement_key(&record.runtime_id, &record.plugin_id);
        let write_txn = self.db.begin_write()?;
        {
            let mut t = write_txn.open_table(PLACEMENTS)?;
            t.insert(key.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Everything placed on one runtime — the desired state it reconciles to.
    /// Every placement, whichever runtime it belongs to.
    ///
    /// For the admin surface, which asks both halves of the question at once:
    /// what does this runtime host, and where does this plugin run. One read
    /// answers both, rather than a request per runtime and another per plugin.
    pub fn all_placements(&self) -> Result<Vec<PlacementRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(PLACEMENTS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<PlacementRecord>(v.value()) {
                out.push(rec);
            }
        }
        out.sort_by(|a, b| {
            a.runtime_id
                .cmp(&b.runtime_id)
                .then_with(|| a.plugin_id.cmp(&b.plugin_id))
        });
        Ok(out)
    }

    pub fn placements_for(&self, runtime_id: &str) -> Result<Vec<PlacementRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(PLACEMENTS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<PlacementRecord>(v.value()) {
                if rec.runtime_id == runtime_id {
                    out.push(rec);
                }
            }
        }
        out.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
        Ok(out)
    }

    /// Which runtime hosts this plugin, if any.
    ///
    /// A plugin lives in one place at a time, so this also answers "is it
    /// already placed" before a second install tries to put it somewhere else.
    pub fn placement_of(&self, plugin_id: &str) -> Result<Option<PlacementRecord>> {
        let read_txn = self.db.begin_read()?;
        let t = read_txn.open_table(PLACEMENTS)?;
        for row in t.iter()? {
            let (_, v) = row?;
            if let Ok(rec) = serde_json::from_str::<PlacementRecord>(v.value()) {
                if rec.plugin_id == plugin_id {
                    return Ok(Some(rec));
                }
            }
        }
        Ok(None)
    }

    pub fn unplace(&self, runtime_id: &str, plugin_id: &str) -> Result<bool> {
        let key = placement_key(runtime_id, plugin_id);
        let write_txn = self.db.begin_write()?;
        let existed;
        {
            let mut t = write_txn.open_table(PLACEMENTS)?;
            existed = t.remove(key.as_str())?.is_some();
        }
        write_txn.commit()?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (PluginRuntimeStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.redb")).unwrap());
        (PluginRuntimeStore::new(db).unwrap(), dir)
    }

    fn rec(id: &str, plugin_id: &str, minutes_ago: i64) -> RuntimeRecord {
        RuntimeRecord {
            runtime_id: id.into(),
            name: format!("Runtime {id}"),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            plugin_id: plugin_id.into(),
            key_hash: "hash".into(),
            homecore_url: "http://homecore.local:8080".into(),
            max_plugins: None,
            created_at: Utc::now() - chrono::Duration::minutes(minutes_ago),
            arch: None,
            host_version: None,
            hostname: None,
            network_mode: None,
            source_ip: None,
            page_port: None,
            page_url: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn round_trips_and_lists_in_creation_order() {
        let (s, _d) = store();
        s.upsert(&rec("rt-b", "plugin.python-rtb", 1)).unwrap();
        s.upsert(&rec("rt-a", "plugin.python-rta", 5)).unwrap();
        let ids: Vec<_> = s
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.runtime_id)
            .collect();
        assert_eq!(ids, vec!["rt-a", "rt-b"]);
        assert_eq!(s.get("rt-b").unwrap().unwrap().name, "Runtime rt-b");
    }

    #[test]
    fn a_runtime_is_found_by_its_broker_login() {
        let (s, _d) = store();
        s.upsert(&rec("rt-a", "plugin.python-rta", 1)).unwrap();
        assert_eq!(
            s.by_plugin_id("plugin.python-rta")
                .unwrap()
                .unwrap()
                .runtime_id,
            "rt-a"
        );
        assert!(s.by_plugin_id("plugin.other").unwrap().is_none());
    }

    #[test]
    fn deleting_reports_whether_it_existed() {
        let (s, _d) = store();
        s.upsert(&rec("rt-a", "plugin.python-rta", 1)).unwrap();
        assert!(s.delete("rt-a").unwrap());
        assert!(!s.delete("rt-a").unwrap());
        assert!(s.get("rt-a").unwrap().is_none());
    }

    fn placement(rt: &str, plugin: &str, version: &str) -> PlacementRecord {
        PlacementRecord {
            runtime_id: rt.into(),
            plugin_id: plugin.into(),
            version: version.into(),
            artifact_url: "https://example/a.tar.zst".into(),
            sha256: "abc".into(),
            config: "[homecore]\n".into(),
            placed_at: Utc::now(),
        }
    }

    #[test]
    fn placements_are_scoped_to_their_runtime() {
        let (s, _d) = store();
        s.place(&placement("rt-a", "plugin.one", "1.0.0")).unwrap();
        s.place(&placement("rt-a", "plugin.two", "1.0.0")).unwrap();
        s.place(&placement("rt-b", "plugin.three", "1.0.0"))
            .unwrap();

        let ids: Vec<String> = s
            .placements_for("rt-a")
            .unwrap()
            .into_iter()
            .map(|p| p.plugin_id)
            .collect();
        assert_eq!(
            ids,
            vec!["plugin.one", "plugin.two"],
            "sorted, and rt-b excluded"
        );
        assert_eq!(s.placements_for("rt-none").unwrap().len(), 0);
    }

    /// Re-placing is an upgrade, not a duplicate. Core holds desired state, so
    /// the same plugin on the same runtime is one row whatever version it is on.
    #[test]
    fn re_placing_replaces_rather_than_duplicating() {
        let (s, _d) = store();
        s.place(&placement("rt-a", "plugin.one", "1.0.0")).unwrap();
        s.place(&placement("rt-a", "plugin.one", "1.1.0")).unwrap();

        let all = s.placements_for("rt-a").unwrap();
        assert_eq!(all.len(), 1, "one row per (runtime, plugin)");
        assert_eq!(all[0].version, "1.1.0");
    }

    /// A plugin lives in one place. Asking where it is must not depend on
    /// knowing the runtime already — that is the question being asked.
    #[test]
    fn a_plugin_can_be_found_without_knowing_its_runtime() {
        let (s, _d) = store();
        s.place(&placement("rt-b", "plugin.somewhere", "2.0.0"))
            .unwrap();

        let found = s.placement_of("plugin.somewhere").unwrap().unwrap();
        assert_eq!(found.runtime_id, "rt-b");
        assert!(s.placement_of("plugin.elsewhere").unwrap().is_none());
    }

    /// The composite key must not let one pair collide with another by
    /// concatenation — `rt-a` + `x.y` and `rt-a\u{1f}x` + `y` would be the same
    /// string under a naive join.
    #[test]
    fn keys_cannot_collide_by_concatenation() {
        let (s, _d) = store();
        s.place(&placement("rt-a", "plugin.one", "1.0.0")).unwrap();
        s.place(&placement("rt-aplugin", "one", "1.0.0")).unwrap();

        assert_eq!(s.placements_for("rt-a").unwrap().len(), 1);
        assert_eq!(s.placements_for("rt-aplugin").unwrap().len(), 1);
    }

    #[test]
    fn unplacing_removes_only_that_pair() {
        let (s, _d) = store();
        s.place(&placement("rt-a", "plugin.one", "1.0.0")).unwrap();
        s.place(&placement("rt-a", "plugin.two", "1.0.0")).unwrap();

        assert!(s.unplace("rt-a", "plugin.one").unwrap());
        assert!(!s.unplace("rt-a", "plugin.one").unwrap(), "idempotent");
        let left: Vec<String> = s
            .placements_for("rt-a")
            .unwrap()
            .into_iter()
            .map(|p| p.plugin_id)
            .collect();
        assert_eq!(left, vec!["plugin.two"]);
    }

    /// The admin view reads every placement at once. Sorted by runtime then
    /// plugin, so a page rendering "what does this host" does not have to sort
    /// per section and two runs never disagree about the order.
    #[test]
    fn all_placements_spans_runtimes_and_is_ordered() {
        let (store, _dir) = store();
        store
            .place(&placement("rt-b", "plugin.two", "1.0.0"))
            .unwrap();
        store
            .place(&placement("rt-a", "plugin.two", "1.0.0"))
            .unwrap();
        store
            .place(&placement("rt-a", "plugin.one", "1.0.0"))
            .unwrap();

        let all = store.all_placements().unwrap();
        assert_eq!(
            all.iter()
                .map(|p| (p.runtime_id.as_str(), p.plugin_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("rt-a", "plugin.one"),
                ("rt-a", "plugin.two"),
                ("rt-b", "plugin.two"),
            ]
        );
    }
}
