//! redb-backed persistence for what Room off saved: how each room's lights
//! were, so Room on can put them back after a restart as well as before one.
//!
//! One record per room, keyed by the room's id. Written by Room off, read by
//! Room on and the saved-lights endpoint, and removed with the room.

use anyhow::Result;
use hc_types::device::RoomLightsSnapshot;
use redb::{Database, ReadableDatabase, TableDefinition};
use std::sync::Arc;
use uuid::Uuid;

const ROOM_LIGHTS: TableDefinition<&str, &str> = TableDefinition::new("room_lights");

pub struct RoomLightsStore {
    db: Arc<Database>,
}

impl RoomLightsStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(ROOM_LIGHTS)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    /// What Room off last saved for this room, if anything.
    pub fn get(&self, room_id: Uuid) -> Result<Option<RoomLightsSnapshot>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ROOM_LIGHTS)?;
        let key = room_id.to_string();
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    /// Replace the room's saved lights.
    pub fn put(&self, snapshot: &RoomLightsSnapshot) -> Result<()> {
        let json = serde_json::to_string(snapshot)?;
        let key = snapshot.room_id.to_string();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROOM_LIGHTS)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Forget the room's saved lights. Returns whether there were any.
    pub fn delete(&self, room_id: Uuid) -> Result<bool> {
        let key = room_id.to_string();
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(ROOM_LIGHTS)?;
            let removed = table.remove(key.as_str())?.is_some();
            removed
        };
        write_txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::device::SavedLight;

    fn snapshot(room_id: Uuid) -> RoomLightsSnapshot {
        let mut settings = serde_json::Map::new();
        settings.insert("brightness_pct".into(), serde_json::json!(40));
        RoomLightsSnapshot {
            room_id,
            saved_at: chrono::Utc::now(),
            lights: vec![
                SavedLight {
                    device_id: "lamp".into(),
                    on: true,
                    settings,
                },
                SavedLight {
                    device_id: "ceiling".into(),
                    on: false,
                    settings: Default::default(),
                },
            ],
            scenes: vec!["hue_scene_read".into()],
        }
    }

    /// Saved lights outlive the process: a fresh store over the same file
    /// reads back what the last one wrote.
    #[test]
    fn saved_lights_survive_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let room = Uuid::new_v4();
        let saved = snapshot(room);
        {
            let db = Arc::new(Database::create(&path).unwrap());
            let store = RoomLightsStore::new(db).unwrap();
            assert!(store.get(room).unwrap().is_none());
            store.put(&saved).unwrap();
        }
        let db = Arc::new(Database::create(&path).unwrap());
        let store = RoomLightsStore::new(db).unwrap();
        assert_eq!(store.get(room).unwrap(), Some(saved));
        assert!(store.delete(room).unwrap());
        assert!(!store.delete(room).unwrap());
        assert!(store.get(room).unwrap().is_none());
    }
}
