//! redb-backed device schema registry: upsert and get.

use anyhow::{Context, Result};
use hc_types::DeviceSchema;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

pub const DEVICE_SCHEMAS: TableDefinition<&str, &[u8]> = TableDefinition::new("device_schemas");

pub struct SchemaStore {
    db: Arc<Database>,
}

impl SchemaStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(DEVICE_SCHEMAS)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn upsert(&self, device_id: &str, schema: &DeviceSchema) -> Result<()> {
        let bytes = serde_json::to_vec(schema).context("device schema serialisation failed")?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(DEVICE_SCHEMAS)?;
            table.insert(device_id, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get(&self, device_id: &str) -> Result<Option<DeviceSchema>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEVICE_SCHEMAS)?;
        match table.get(device_id)? {
            Some(guard) => {
                let schema: DeviceSchema = serde_json::from_slice(guard.value())
                    .context("device schema deserialisation failed")?;
                Ok(Some(schema))
            }
            None => Ok(None),
        }
    }

    pub fn list(&self) -> Result<Vec<(String, DeviceSchema)>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEVICE_SCHEMAS)?;
        let mut results = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let device_id = key.value().to_string();
            let schema: DeviceSchema = serde_json::from_slice(value.value())
                .context("device schema deserialisation failed")?;
            results.push((device_id, schema));
        }
        Ok(results)
    }
}
