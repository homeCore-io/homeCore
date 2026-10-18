//! Alert time rules (H4): the admin-written half of alerts, one row per rule,
//! keyed by id. The built-in alerts need no storage — they are worked out
//! from device state — and active alerts are recomputed on start rather than
//! stored, so a restart can never leave a stale one behind.
//!
//! Ignores live here too, one row each: what a person chose not to see.

use anyhow::{Context, Result};
use hc_types::alert::{AlertIgnore, AlertRule};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;

const ALERT_RULES: TableDefinition<&str, &str> = TableDefinition::new("alert_rules");
const ALERT_IGNORES: TableDefinition<&str, &str> = TableDefinition::new("alert_ignores");

pub struct AlertRuleStore {
    db: Arc<Database>,
}

impl AlertRuleStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(ALERT_RULES)?;
            write_txn.open_table(ALERT_IGNORES)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn list(&self) -> Result<Vec<AlertRule>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALERT_RULES)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            match serde_json::from_str::<AlertRule>(value.value()) {
                Ok(rule) => out.push(rule),
                // One unreadable row must not hide every other rule.
                Err(e) => tracing::warn!(error = %e, "skipping an unreadable alert rule"),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<AlertRule>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALERT_RULES)?;
        let Some(value) = table.get(id)? else {
            return Ok(None);
        };
        Ok(Some(
            serde_json::from_str(value.value()).context("decoding alert rule")?,
        ))
    }

    pub fn upsert(&self, rule: &AlertRule) -> Result<()> {
        let json = serde_json::to_string(rule)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ALERT_RULES)?;
            table.insert(rule.id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Whether there was such a rule.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(ALERT_RULES)?;
            let removed = table.remove(id)?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(existed)
    }

    // ---- ignores ----

    pub fn list_ignores(&self) -> Result<Vec<AlertIgnore>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ALERT_IGNORES)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            match serde_json::from_str::<AlertIgnore>(value.value()) {
                Ok(i) => out.push(i),
                Err(e) => tracing::warn!(error = %e, "skipping an unreadable alert ignore"),
            }
        }
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub fn upsert_ignore(&self, ignore: &AlertIgnore) -> Result<()> {
        let json = serde_json::to_string(ignore)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ALERT_IGNORES)?;
            table.insert(ignore.id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Whether there was such an ignore.
    pub fn delete_ignore(&self, id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(ALERT_IGNORES)?;
            let removed = table.remove(id)?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::alert::AlertKind;

    #[test]
    fn ignores_are_kept_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.redb")).unwrap());
        let store = AlertRuleStore::new(db).unwrap();
        let i = AlertIgnore {
            id: "a".into(),
            kind: Some(AlertKind::LowBattery),
            device_id: Some("lock".into()),
            rule_id: None,
            until: None,
            note: Some("spare batteries on order".into()),
            by: Some("sam".into()),
            created_at: chrono::Utc::now(),
        };
        store.upsert_ignore(&i).unwrap();
        assert_eq!(store.list_ignores().unwrap(), vec![i]);
        assert!(store.delete_ignore("a").unwrap());
        assert!(!store.delete_ignore("a").unwrap());
        assert!(store.list_ignores().unwrap().is_empty());
    }
}
