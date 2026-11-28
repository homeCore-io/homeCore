//! redb-backed persistence for rotations: each rotation as written, and
//! separately where it is (what plays now, its period, its shuffled round),
//! so a restart neither loses one nor moves it on.

use anyhow::Result;
use hc_types::rotation::{Rotation, RotationState};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::Arc;
use uuid::Uuid;

const ROTATIONS: TableDefinition<&str, &str> = TableDefinition::new("rotations");
const ROTATION_STATE: TableDefinition<&str, &str> = TableDefinition::new("rotation_state");

pub struct RotationStore {
    db: Arc<Database>,
}

impl RotationStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(ROTATIONS)?;
        write_txn.open_table(ROTATION_STATE)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn list(&self) -> Result<Vec<Rotation>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ROTATIONS)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, v) = row?;
            match serde_json::from_str::<Rotation>(v.value()) {
                Ok(r) => out.push(r),
                Err(e) => tracing::warn!(error = %e, "a stored rotation doesn't read; left out"),
            }
        }
        out.sort_by_key(|r| r.name.to_lowercase());
        Ok(out)
    }

    pub fn get(&self, id: Uuid) -> Result<Option<Rotation>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ROTATIONS)?;
        let key = id.to_string();
        match table.get(key.as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    /// Save a rotation. When its choices changed, where it was means nothing
    /// for the new list, so its state starts again.
    pub fn put(&self, rotation: &Rotation) -> Result<()> {
        let key = rotation.id.to_string();
        let json = serde_json::to_string(rotation)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROTATIONS)?;
            let changed_choices = match table.get(key.as_str())? {
                Some(old) => serde_json::from_str::<Rotation>(old.value())
                    .map(|old| {
                        old.choices != rotation.choices
                            || old.order != rotation.order
                            // Another device: what it lacks is another matter.
                            || old.device_id != rotation.device_id
                    })
                    .unwrap_or(true),
                None => true,
            };
            table.insert(key.as_str(), json.as_str())?;
            if changed_choices {
                let mut state = write_txn.open_table(ROTATION_STATE)?;
                state.remove(key.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Forget a rotation and where it was. Returns whether there was one.
    pub fn delete(&self, id: Uuid) -> Result<bool> {
        let key = id.to_string();
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(ROTATIONS)?;
            let removed = table.remove(key.as_str())?.is_some();
            let mut state = write_txn.open_table(ROTATION_STATE)?;
            state.remove(key.as_str())?;
            removed
        };
        write_txn.commit()?;
        Ok(removed)
    }

    /// Where a rotation is; a fresh state when it has never played.
    pub fn state(&self, id: Uuid) -> Result<RotationState> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ROTATION_STATE)?;
        let key = id.to_string();
        Ok(match table.get(key.as_str())? {
            Some(v) => serde_json::from_str(v.value()).unwrap_or_default(),
            None => RotationState::default(),
        })
    }

    pub fn put_state(&self, id: Uuid, state: &RotationState) -> Result<()> {
        let key = id.to_string();
        let json = serde_json::to_string(state)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ROTATION_STATE)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::rotation::{RotationAdvance, RotationChoice, RotationOrder};
    use serde_json::json;

    fn store() -> (tempfile::TempDir, RotationStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("s.redb")).unwrap());
        (dir, RotationStore::new(db).unwrap())
    }

    fn rotation(choices: &[&str]) -> Rotation {
        Rotation {
            id: Uuid::from_u128(7),
            name: "Bathroom music".into(),
            device_id: "sonos_bathroom".into(),
            choices: choices
                .iter()
                .map(|p| RotationChoice {
                    label: None,
                    command: json!({ "action": "play_playlist", "playlist": p }),
                    only_when: None,
                })
                .collect(),
            order: RotationOrder::InTurn,
            advance: RotationAdvance::EachUse,
        }
    }

    #[test]
    fn a_rename_keeps_its_place_and_new_choices_start_again() {
        let (_d, s) = store();
        let mut r = rotation(&["a", "b"]);
        s.put(&r).unwrap();
        let mut st = s.state(r.id).unwrap();
        st.current = Some(1);
        s.put_state(r.id, &st).unwrap();

        r.name = "Bath".into();
        s.put(&r).unwrap();
        assert_eq!(s.state(r.id).unwrap().current, Some(1));

        r.choices = rotation(&["a", "c"]).choices;
        s.put(&r).unwrap();
        assert_eq!(s.state(r.id).unwrap(), RotationState::default());
    }

    #[test]
    fn delete_takes_its_state_too() {
        let (_d, s) = store();
        let r = rotation(&["a"]);
        s.put(&r).unwrap();
        s.put_state(
            r.id,
            &RotationState {
                current: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.delete(r.id).unwrap());
        assert!(s.get(r.id).unwrap().is_none());
        assert_eq!(s.state(r.id).unwrap(), RotationState::default());
        assert_eq!(s.list().unwrap().len(), 0);
    }
}
