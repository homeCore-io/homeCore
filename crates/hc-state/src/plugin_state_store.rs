//! redb-backed persistence for **plugin-learned state** (the D8 data-model
//! split, leg 2).
//!
//! This is state the *plugin* authors at runtime — Hue bridge `app_key`s, OAuth
//! tokens, discovered/published device ids — as distinct from *operator config*
//! (the file store) and *device inventory* (the device registry). It must
//! survive a plugin container being recreated, so it lives in core's redb keyed
//! by `plugin_id` and is handed back to the plugin at startup. It is **never**
//! surfaced as editable config, and (unlike the operator config file) the plugin
//! is its single writer — so there is no operator/plugin merge conflict.
//!
//! The document is stored opaquely as a JSON value; core does not interpret its
//! shape (that's between a plugin and its future self).

use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use serde_json::Value;
use std::sync::Arc;

const PLUGIN_STATE: TableDefinition<&str, &str> = TableDefinition::new("plugin_state");

pub struct PluginStateStore {
    db: Arc<Database>,
}

impl PluginStateStore {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let write_txn = db.begin_write()?;
        write_txn.open_table(PLUGIN_STATE)?;
        write_txn.commit()?;
        Ok(Self { db })
    }

    /// The full learned-state document for a plugin, or `None` if it has never
    /// persisted any.
    pub fn get(&self, plugin_id: &str) -> Result<Option<Value>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(PLUGIN_STATE)?;
        match table.get(plugin_id)? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    /// Replace a plugin's entire learned-state document.
    pub fn put(&self, plugin_id: &str, state: &Value) -> Result<()> {
        let json = serde_json::to_string(state)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(PLUGIN_STATE)?;
            table.insert(plugin_id, json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Shallow-merge the top-level keys of `delta` into the stored document and
    /// return the result. A `null` value in `delta` deletes that key. This lets
    /// a plugin persist just what it learned (e.g. one newly-paired bridge's
    /// `app_key`) without read-modify-writing the whole document itself. If
    /// either side isn't a JSON object, `delta` replaces the document wholesale.
    pub fn merge(&self, plugin_id: &str, delta: &Value) -> Result<Value> {
        let write_txn = self.db.begin_write()?;
        let merged = {
            let mut table = write_txn.open_table(PLUGIN_STATE)?;
            let mut current = match table.get(plugin_id)? {
                Some(v) => serde_json::from_str::<Value>(v.value())?,
                None => Value::Object(Default::default()),
            };
            merge_shallow(&mut current, delta);
            table.insert(plugin_id, serde_json::to_string(&current)?.as_str())?;
            current
        };
        write_txn.commit()?;
        Ok(merged)
    }

    /// All plugin ids that have persisted learned state. Used at startup to
    /// re-publish each doc as a retained MQTT message (the broker's retained
    /// store is in-memory; redb is the durable source).
    pub fn list_ids(&self) -> Result<Vec<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(PLUGIN_STATE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, _) = entry?;
            out.push(k.value().to_string());
        }
        Ok(out)
    }

    /// Drop a plugin's learned state entirely (e.g. on deregistration). Returns
    /// whether an entry existed.
    pub fn delete(&self, plugin_id: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(PLUGIN_STATE)?;
            let result = table.remove(plugin_id)?.is_some();
            result
        };
        write_txn.commit()?;
        Ok(removed)
    }
}

/// Top-level object merge: keys in `delta` overwrite `base`; a `null` value
/// removes the key. If either side is not an object, `delta` wins wholesale.
fn merge_shallow(base: &mut Value, delta: &Value) {
    match (base.as_object_mut(), delta.as_object()) {
        (Some(base_map), Some(delta_map)) => {
            for (k, v) in delta_map {
                if v.is_null() {
                    base_map.remove(k);
                } else {
                    base_map.insert(k.clone(), v.clone());
                }
            }
        }
        _ => *base = delta.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> PluginStateStore {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("state.redb")).unwrap());
        // Keep the tempdir alive for the process by leaking it — tests are short
        // and the OS reclaims it; avoids threading a guard through every call.
        std::mem::forget(dir);
        PluginStateStore::new(db).unwrap()
    }

    #[test]
    fn get_absent_is_none_then_put_get_round_trips() {
        let s = store();
        assert!(s.get("plugin.hue").unwrap().is_none());
        let doc = json!({ "bridges": { "abc": { "app_key": "k1" } } });
        s.put("plugin.hue", &doc).unwrap();
        assert_eq!(s.get("plugin.hue").unwrap().unwrap(), doc);
    }

    #[test]
    fn merge_adds_overwrites_and_deletes_top_level_keys() {
        let s = store();
        s.put(
            "plugin.a",
            &json!({ "keep": 1, "change": "old", "drop": true }),
        )
        .unwrap();
        let merged = s
            .merge(
                "plugin.a",
                &json!({ "change": "new", "add": 2, "drop": null }),
            )
            .unwrap();
        assert_eq!(merged, json!({ "keep": 1, "change": "new", "add": 2 }));
        // Persisted, not just returned.
        assert_eq!(s.get("plugin.a").unwrap().unwrap(), merged);
    }

    #[test]
    fn merge_into_absent_creates_the_document() {
        let s = store();
        let merged = s.merge("plugin.new", &json!({ "token": "t" })).unwrap();
        assert_eq!(merged, json!({ "token": "t" }));
    }

    #[test]
    fn list_ids_returns_all_stored_plugins() {
        let s = store();
        assert!(s.list_ids().unwrap().is_empty());
        s.put("plugin.hue", &json!({ "a": 1 })).unwrap();
        s.put("plugin.yolink", &json!({ "b": 2 })).unwrap();
        let mut ids = s.list_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["plugin.hue", "plugin.yolink"]);
    }

    #[test]
    fn delete_reports_presence_and_clears() {
        let s = store();
        s.put("plugin.a", &json!({ "x": 1 })).unwrap();
        assert!(s.delete("plugin.a").unwrap());
        assert!(!s.delete("plugin.a").unwrap());
        assert!(s.get("plugin.a").unwrap().is_none());
    }
}
