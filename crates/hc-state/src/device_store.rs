//! redb-backed device registry: get, upsert, list.

use anyhow::{Context, Result};
use hc_types::device::DeviceState;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;

const DEVICES: TableDefinition<&str, &str> = TableDefinition::new("devices");
/// Plugin devices a person removed: kept out when their plugin reports them
/// again, until brought back.
const FORGOTTEN: TableDefinition<&str, &str> = TableDefinition::new("forgotten_devices");

/// A plugin device a person removed, and when.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForgottenDevice {
    pub device_id: String,
    pub plugin_id: String,
    /// What it was called, to recognise it in the list.
    pub name: String,
    pub removed_at: chrono::DateTime<chrono::Utc>,
}

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
        write_txn.open_table(FORGOTTEN)?;
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

    // ---- removed plugin devices ----

    pub fn forget(&self, f: &ForgottenDevice) -> Result<()> {
        let json = serde_json::to_string(f)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(FORGOTTEN)?;
            table.insert(f.device_id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn is_forgotten(&self, device_id: &str) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORGOTTEN)?;
        Ok(table.get(device_id)?.is_some())
    }

    pub fn list_forgotten(&self) -> Result<Vec<ForgottenDevice>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(FORGOTTEN)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, v) = row?;
            match serde_json::from_str::<ForgottenDevice>(v.value()) {
                Ok(f) => out.push(f),
                Err(e) => tracing::warn!(error = %e, "skipping an unreadable removed device"),
            }
        }
        out.sort_by_key(|f| std::cmp::Reverse(f.removed_at));
        Ok(out)
    }

    /// Whether there was such a record.
    pub fn unforget(&self, device_id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(FORGOTTEN)?;
            let r = table.remove(device_id)?.is_some();
            r
        };
        write_txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod forgotten_tests {
    use super::*;

    #[test]
    fn a_removed_device_is_remembered_until_brought_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("d.redb")).unwrap());
        let store = DeviceStore::new(db).unwrap();
        let f = ForgottenDevice {
            device_id: "hue_lamp".into(),
            plugin_id: "plugin.hue".into(),
            name: "Lamp".into(),
            removed_at: chrono::Utc::now(),
        };
        assert!(!store.is_forgotten("hue_lamp").unwrap());
        store.forget(&f).unwrap();
        assert!(store.is_forgotten("hue_lamp").unwrap());
        assert_eq!(store.list_forgotten().unwrap(), vec![f]);
        assert!(store.unforget("hue_lamp").unwrap());
        assert!(!store.is_forgotten("hue_lamp").unwrap());
    }
}
