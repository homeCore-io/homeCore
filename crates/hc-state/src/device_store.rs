//! redb-backed device registry: get, upsert, list.

use anyhow::{Context, Result};
use hc_types::device::DeviceState;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;

const DEVICES: TableDefinition<&str, &str> = TableDefinition::new("devices");

/// What a [`DeviceStore::upsert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceWrite {
    /// The device was not there before.
    Added,
    /// Its name, room, type or other metadata changed.
    MetadataChanged,
    /// Only what it reports changed — attributes, availability, timestamps.
    StateOnly,
}

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

    /// Write a device, and say what kind of write it was — so the caller can
    /// announce a new device or changed metadata without a second read.
    ///
    /// The previous record is read in the same transaction as the write, so
    /// two writers racing on one device each compare against what the other
    /// actually left.
    pub fn upsert(&self, state: &DeviceState) -> Result<DeviceWrite> {
        let json = serde_json::to_string(state).context("device state serialisation failed")?;
        let write_txn = self.db.begin_write()?;
        let kind = {
            let mut table = write_txn.open_table(DEVICES)?;
            let kind = match table.get(state.device_id.as_str())? {
                None => DeviceWrite::Added,
                Some(previous) => match serde_json::from_str::<DeviceState>(previous.value()) {
                    Ok(previous) if previous.same_metadata(state) => DeviceWrite::StateOnly,
                    // A record that no longer parses is being replaced by one
                    // that does: that is a change worth telling clients about.
                    _ => DeviceWrite::MetadataChanged,
                },
            };
            table.insert(state.device_id.as_str(), json.as_str())?;
            kind
        };
        write_txn.commit()?;
        Ok(kind)
    }

    pub fn list(&self) -> Result<Vec<DeviceState>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(DEVICES)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v): (_, _) = entry?;
            let state: DeviceState =
                serde_json::from_str(v.value()).context("device state deserialisation failed")?;
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
