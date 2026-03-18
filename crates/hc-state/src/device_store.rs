//! redb-backed device registry: get, upsert, list.

use anyhow::{Context, Result};
use hc_types::device::DeviceState;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

const DEVICES: TableDefinition<&str, &str> = TableDefinition::new("devices");

pub struct DeviceStore {
    db: Arc<Database>,
}

impl DeviceStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(DEVICES)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, device_id: &str) -> Result<Option<DeviceState>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEVICES)?;
        match table.get(device_id)? {
            Some(guard) => {
                let state: DeviceState = serde_json::from_str(guard.value())
                    .context("device state deserialisation failed")?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    pub fn upsert(&self, state: &DeviceState) -> Result<()> {
        let json = serde_json::to_string(state).context("device state serialisation failed")?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(DEVICES)?;
            table.insert(state.device_id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<DeviceState>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEVICES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v): (_, _) = entry?;
            let state: DeviceState = serde_json::from_str(v.value())
                .context("device state deserialisation failed")?;
            out.push(state);
        }
        Ok(out)
    }

    pub fn delete(&self, device_id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(DEVICES)?;
            let result = table.remove(device_id)?.is_some();
            result
        };
        write_txn.commit()?;
        Ok(removed)
    }
}
