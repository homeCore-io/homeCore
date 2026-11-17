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

    /// Turn an installed plugin on or off for good. `false` when the id isn't
    /// a managed record (a static `[[plugins]]` entry is persisted in
    /// homecore.toml instead).
    pub fn set_enabled(&self, id: &str, enabled: bool) -> std::io::Result<bool> {
        {
            let mut doc = self.doc.write().unwrap();
            let Some(rec) = doc.records.iter_mut().find(|r| r.id == id) else {
                return Ok(false);
            };
            rec.enabled = enabled;
        }
        self.persist().map(|()| true)
    }

    /// Whether an installed plugin is on, if it is a managed record.
    pub fn enabled_of(&self, id: &str) -> Option<bool> {
        self.doc
            .read()
            .unwrap()
            .records
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.enabled)
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

    /// The version installed for a plugin, from what core keeps on disk —
    /// known whether or not the plugin is running, since only a running one
    /// says its own version.
    ///
    /// The install record's version first. Failing that, the version folder
    /// an install put the binary in (`plugins/<id>/<version>/<binary>`), which
    /// also covers a `[[plugins]]` entry pointed at an installed binary by
    /// hand. A plugin configured by hand anywhere else has no installed
    /// version core could know: `None`.
    pub fn installed_version(&self, id: &str, binary: &str) -> Option<String> {
        let recorded = self
            .doc
            .read()
            .unwrap()
            .records
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.version.trim().to_string(), r.binary.clone()));
        match recorded {
            Some((version, _)) if !version.is_empty() => Some(version),
            Some((_, rec_binary)) => version_from_install_path(id, &rec_binary)
                .or_else(|| version_from_install_path(id, binary)),
            None => version_from_install_path(id, binary),
        }
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

/// The version folder of an installed binary, `…/<id>/<version>/<binary>` as
/// `plugin_install` lays it out. Only a folder named for this plugin holding
/// one that reads as a version counts: `target/debug/hc-hue` is no install.
fn version_from_install_path(id: &str, binary: &str) -> Option<String> {
    let path = std::path::Path::new(binary);
    let version_dir = path.parent()?;
    let id_dir = version_dir.parent()?;
    if id_dir.file_name()?.to_str()? != id {
        return None;
    }
    let version = version_dir.file_name()?.to_str()?;
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();
    let looks_like_version = parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    looks_like_version.then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_plugin_still_has_its_installed_version() {
        let dir = tmp_dir();
        let store = ManagedPluginStore::load(dir.clone());
        store
            .install(ManagedRecord {
                id: "plugin.hue".into(),
                version: "0.2.7".into(),
                binary: "/home/hc/plugins/plugin.hue/0.2.7/hc-hue".into(),
                enabled: false,
                ..Default::default()
            })
            .unwrap();
        // From the record, which survives a restart.
        let reloaded = ManagedPluginStore::load(dir);
        assert_eq!(
            reloaded.installed_version("plugin.hue", "/anything"),
            Some("0.2.7".into())
        );
    }

    #[test]
    fn the_install_folder_says_the_version_when_the_record_does_not() {
        let store = ManagedPluginStore::load(tmp_dir());
        store
            .install(ManagedRecord {
                id: "plugin.roku".into(),
                binary: "/home/hc/plugins/plugin.roku/0.1.4/hc-roku".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            store.installed_version("plugin.roku", ""),
            Some("0.1.4".into())
        );
        // A `[[plugins]]` entry pointed at an installed binary, no record.
        assert_eq!(
            store.installed_version(
                "plugin.caseta",
                "plugins/plugin.caseta/0.3.0-rc.1/hc-caseta"
            ),
            Some("0.3.0-rc.1".into())
        );
    }

    #[test]
    fn a_plugin_configured_by_hand_has_no_installed_version() {
        let store = ManagedPluginStore::load(tmp_dir());
        for binary in [
            "/home/dev/homeCore/core/target/debug/hc-hue",
            "plugins/hc-hue/bin/hc-hue",
            // Another plugin's install folder is not this one's.
            "/home/hc/plugins/plugin.lutron/0.1.24/hc-hue",
            "/home/hc/plugins/plugin.hue/latest/hc-hue",
            "hc-hue",
        ] {
            assert_eq!(
                store.installed_version("plugin.hue", binary),
                None,
                "{binary}"
            );
        }
    }

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
    fn turning_an_installed_plugin_off_survives_a_reload() {
        let dir = tmp_dir();
        let store = ManagedPluginStore::load(dir.clone());
        store
            .install(ManagedRecord {
                id: "plugin.wled".into(),
                enabled: true,
                ..Default::default()
            })
            .unwrap();
        assert!(store.set_enabled("plugin.wled", false).unwrap());
        assert!(
            !store.set_enabled("plugin.static", false).unwrap(),
            "not a managed record"
        );
        let reloaded = ManagedPluginStore::load(dir);
        assert_eq!(reloaded.enabled_of("plugin.wled"), Some(false));
        assert_eq!(reloaded.enabled_of("plugin.static"), None);
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

        assert!(
            store.removed_ids().is_empty(),
            "install clears the tombstone"
        );
        let recs = store.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "plugin.hue");
    }
}
