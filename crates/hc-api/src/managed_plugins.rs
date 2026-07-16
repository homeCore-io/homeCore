//! The **managed-plugin store** — the runtime-mutable plugin set that layers
//! over the static `[[plugins]]` blocks in `homecore.toml`. Persisted as
//! `config/plugins/managed.toml`. Backs plugin install / uninstall (Phase A+).
//!
//! Two parts:
//! - `[[managed]]` **records** — plugins added at runtime (installed from the
//!   registry, or added manually). Empty until the install pipeline lands.
//! - `removed` **tombstones** — ids that were uninstalled. Because a plugin can
//!   also be declared statically in `homecore.toml` (which we do NOT rewrite —
//!   it holds operator comments + is the dev source), a tombstone is how an
//!   uninstall of a *static* plugin sticks: boot subtracts tombstoned ids, so it
//!   doesn't respawn. Re-installing / re-adding clears the tombstone.
//!
//! The effective plugin set at boot is: `static ∪ records − removed`, with
//! records winning on id collision.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// One installed/added plugin. Mirrors a static `[[plugins]]` entry plus
/// provenance (where it came from + which version).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManagedRecord {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `registry` | `manual` | `imported`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub version: String,
    /// Absolute path to the plugin binary.
    #[serde(default)]
    pub binary: String,
    /// Absolute path to the plugin's operator config file.
    #[serde(default)]
    pub config: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub installed_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ManagedDoc {
    /// Uninstalled ids — suppress a still-declared static plugin on boot.
    #[serde(default)]
    removed: Vec<String>,
    /// Runtime-added plugin records.
    #[serde(default, rename = "managed")]
    records: Vec<ManagedRecord>,
}

/// File-backed store; all mutations persist immediately.
pub struct ManagedPluginStore {
    path: PathBuf,
    doc: RwLock<ManagedDoc>,
}

impl ManagedPluginStore {
    /// Load `<config_plugins_dir>/managed.toml`. A missing or unparseable file
    /// yields an empty store (logged by the caller if it matters).
    pub fn load(config_plugins_dir: PathBuf) -> Self {
        let path = config_plugins_dir.join("managed.toml");
        let doc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<ManagedDoc>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            doc: RwLock::new(doc),
        }
    }

    /// Tombstoned (uninstalled) ids.
    pub fn removed_ids(&self) -> Vec<String> {
        self.doc.read().unwrap().removed.clone()
    }

    /// Runtime-added plugin records (installed / manually added).
    pub fn records(&self) -> Vec<ManagedRecord> {
        self.doc.read().unwrap().records.clone()
    }

    /// Mark a plugin uninstalled: drop any managed record + tombstone the id so a
    /// still-declared static `[[plugins]]` entry is suppressed on the next boot.
    pub fn uninstall(&self, id: &str) -> std::io::Result<()> {
        {
            let mut doc = self.doc.write().unwrap();
            doc.records.retain(|r| r.id != id);
            if !doc.removed.iter().any(|r| r == id) {
                doc.removed.push(id.to_string());
            }
        }
        self.persist()
    }

    /// Add or replace a managed record and clear any tombstone (install / add).
    pub fn install(&self, rec: ManagedRecord) -> std::io::Result<()> {
        {
            let mut doc = self.doc.write().unwrap();
            doc.removed.retain(|r| r != &rec.id);
            doc.records.retain(|r| r.id != rec.id);
            doc.records.push(rec);
        }
        self.persist()
    }

    fn persist(&self) -> std::io::Result<()> {
        let doc = self.doc.read().unwrap();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(&*doc).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        // Unique-ish without Date/rand (unavailable in some harnesses): use the
        // store's own address via a counter isn't possible here, so use PID +
        // an atomic counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        p.push(format!(
            "hc-managed-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn uninstall_tombstones_and_persists_across_reload() {
        let dir = tmp_dir();
        let store = ManagedPluginStore::load(dir.clone());
        assert!(store.removed_ids().is_empty());

        store.uninstall("plugin.hue").unwrap();
        assert_eq!(store.removed_ids(), vec!["plugin.hue".to_string()]);

        // Reload from disk — tombstone survives.
        let reloaded = ManagedPluginStore::load(dir);
        assert_eq!(reloaded.removed_ids(), vec!["plugin.hue".to_string()]);
    }

    #[test]
    fn install_clears_tombstone_and_records_the_plugin() {
        let dir = tmp_dir();
        let store = ManagedPluginStore::load(dir);
        store.uninstall("plugin.hue").unwrap();

        store
            .install(ManagedRecord {
                id: "plugin.hue".into(),
                name: "Hue".into(),
                source: "registry".into(),
                version: "1.0.0".into(),
                binary: "/opt/hue".into(),
                config: "/cfg/hue.toml".into(),
                enabled: true,
                installed_at: "2026-07-16".into(),
            })
            .unwrap();

        assert!(store.removed_ids().is_empty(), "install clears the tombstone");
        let recs = store.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "plugin.hue");
    }
}
