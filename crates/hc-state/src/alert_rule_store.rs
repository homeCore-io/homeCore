//! Alert time rules (H4): the admin-written half of alerts, one row per rule,
//! keyed by id. The built-in alerts need no storage — they are worked out
//! from device state — and active alerts are recomputed on start rather than
//! stored, so a restart can never leave a stale one behind.

use anyhow::{Context, Result};
use hc_types::alert::AlertRule;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;

const ALERT_RULES: TableDefinition<&str, &str> = TableDefinition::new("alert_rules");

pub struct AlertRuleStore {
    db: Arc<Database>,
}

impl AlertRuleStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(ALERT_RULES)?;
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
}
