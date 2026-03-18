//! redb-backed user account store.

use anyhow::Result;
use hc_auth::user::User;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::sync::Arc;
use uuid::Uuid;

const USERS_BY_ID: TableDefinition<&str, &str> = TableDefinition::new("users_by_id");
const USERS_BY_NAME: TableDefinition<&str, &str> = TableDefinition::new("users_by_name");

pub struct UserStore {
    db: Arc<Database>,
}

impl UserStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(USERS_BY_ID)?;
            write_txn.open_table(USERS_BY_NAME)?;
        }
        write_txn.commit()?;
        Ok(Self { db })
    }

    pub fn create_user(&self, user: &User) -> Result<()> {
        let json = serde_json::to_string(user)?;
        let id_str = user.id.to_string();
        let write_txn = self.db.begin_write()?;
        {
            let mut id_table = write_txn.open_table(USERS_BY_ID)?;
            id_table.insert(id_str.as_str(), json.as_str())?;
            let mut name_table = write_txn.open_table(USERS_BY_NAME)?;
            name_table.insert(user.username.as_str(), id_str.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn get_user_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let id_str = id.to_string();
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(USERS_BY_ID)?;
        match table.get(id_str.as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let read_txn = self.db.begin_read()?;
        let name_table = read_txn.open_table(USERS_BY_NAME)?;
        let Some(id_entry) = name_table.get(username)? else { return Ok(None) };
        let id_str = id_entry.value().to_string();
        drop(id_entry);
        drop(name_table);
        let id_table = read_txn.open_table(USERS_BY_ID)?;
        match id_table.get(id_str.as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn update_user(&self, user: &User) -> Result<()> {
        // Update only the id→json entry (username is immutable for now).
        let json = serde_json::to_string(user)?;
        let id_str = user.id.to_string();
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(USERS_BY_ID)?;
            table.insert(id_str.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    pub fn delete_user(&self, id: Uuid) -> Result<bool> {
        let id_str = id.to_string();
        let read_txn = self.db.begin_read()?;
        let id_table = read_txn.open_table(USERS_BY_ID)?;
        let Some(v) = id_table.get(id_str.as_str())? else { return Ok(false) };
        let user: User = serde_json::from_str(v.value())?;
        let username = user.username.clone();
        drop(v);
        drop(id_table);
        drop(read_txn);

        let write_txn = self.db.begin_write()?;
        {
            let mut id_table = write_txn.open_table(USERS_BY_ID)?;
            id_table.remove(id_str.as_str())?;
            let mut name_table = write_txn.open_table(USERS_BY_NAME)?;
            name_table.remove(username.as_str())?;
        }
        write_txn.commit()?;
        Ok(true)
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(USERS_BY_ID)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_str(v.value())?);
        }
        Ok(out)
    }

    pub fn user_count(&self) -> Result<usize> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(USERS_BY_ID)?;
        Ok(table.len()? as usize)
    }
}
