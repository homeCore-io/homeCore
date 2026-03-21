//! redb-backed persistence for Rule and Scene values.

use anyhow::Result;
use hc_types::device::Area;
use hc_types::rule::{Rule, Scene};
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;
use uuid::Uuid;

const RULES: TableDefinition<&str, &str> = TableDefinition::new("rules");
const SCENES: TableDefinition<&str, &str> = TableDefinition::new("scenes");
const AREAS: TableDefinition<&str, &str> = TableDefinition::new("areas");

pub struct RuleStore {
    db: Arc<Database>,
}

impl RuleStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(RULES)?;
        write_txn.open_table(SCENES)?;
        write_txn.open_table(AREAS)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    // --- Rules ---

    pub fn get_rule(&self, id: Uuid) -> Result<Option<Rule>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(RULES)?;
        match table.get(id.to_string().as_str())? {
            Some(g) => Ok(Some(serde_json::from_str(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn upsert_rule(&self, rule: &Rule) -> Result<()> {
        let json = serde_json::to_string(rule)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(RULES)?;
            table.insert(rule.id.to_string().as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn delete_rule(&self, id: Uuid) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(RULES)?;
            let result = table.remove(id.to_string().as_str())?.is_some();
            result
        };
        write_txn.commit()?;
        Ok(removed)
    }

    pub fn list_rules(&self) -> Result<Vec<Rule>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(RULES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v): (_, _) = entry?;
            out.push(serde_json::from_str(v.value())?);
        }
        Ok(out)
    }

    // --- Scenes ---

    pub fn get_scene(&self, id: Uuid) -> Result<Option<Scene>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SCENES)?;
        match table.get(id.to_string().as_str())? {
            Some(g) => Ok(Some(serde_json::from_str(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn upsert_scene(&self, scene: &Scene) -> Result<()> {
        let json = serde_json::to_string(scene)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(SCENES)?;
            table.insert(scene.id.to_string().as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn delete_scene(&self, id: Uuid) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(SCENES)?;
            let result = table.remove(id.to_string().as_str())?.is_some();
            result
        };
        write_txn.commit()?;
        Ok(removed)
    }

    pub fn list_scenes(&self) -> Result<Vec<Scene>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SCENES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v): (_, _) = entry?;
            out.push(serde_json::from_str(v.value())?);
        }
        Ok(out)
    }

    // --- Areas ---

    pub fn upsert_area(&self, area: &Area) -> Result<()> {
        let json = serde_json::to_string(area)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(AREAS)?;
            table.insert(area.id.to_string().as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_area(&self, id: Uuid) -> Result<Option<Area>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(AREAS)?;
        match table.get(id.to_string().as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn delete_area(&self, id: Uuid) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(AREAS)?;
            let result = table.remove(id.to_string().as_str())?.is_some();
            result
        };
        write_txn.commit()?;
        Ok(removed)
    }

    pub fn list_areas(&self) -> Result<Vec<Area>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(AREAS)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v): (_, _) = entry?;
            out.push(serde_json::from_str(v.value())?);
        }
        Ok(out)
    }
}
