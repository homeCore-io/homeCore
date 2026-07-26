//! Core-owned store for plugin configuration files.
//!
//! Historically each plugin's config lived *inside* the plugin's own directory
//! (`plugins/hc-<name>/config/config.toml`) and was passed to the plugin as
//! `argv[1]`.  That location is fragile: a fetch+uncompress upgrade of a plugin
//! can clobber it, and there was no single place the API, the editor, and an
//! operator editing a file all agreed on.
//!
//! `PluginConfigStore` gives each plugin one authoritative config file under a
//! core-owned directory (`{base}/config/plugins/<plugin_id>.toml`).  The
//! supervisor passes that path as `argv[1]`, so plugins keep reading a file
//! exactly as before — only the *location* moved out of the plugin's tree.
//!
//! Like [`crate::rule_file_store::RuleFileStore`], operations are synchronous
//! blocking I/O — config files are tiny, so this is safe to call directly from
//! async contexts.  Writes go through [`crate::config_writer::write_atomic`]
//! (temp file + rename) so a reader (or a future file watcher) never observes a
//! half-written file.

use crate::config_writer::write_atomic;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The canonical store for plugin config files.
#[derive(Clone)]
pub struct PluginConfigStore {
    dir: PathBuf,
}

impl PluginConfigStore {
    /// Create a store rooted at `dir` (typically `{base}/config/plugins`).
    /// Does not touch the filesystem; the directory is created lazily on first
    /// write/import.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory holding the config files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Canonical path for a plugin's config file: `{dir}/<slug(id)>.toml`.
    ///
    /// The id is slugged defensively so it can never escape `dir` (path
    /// separators and other unusual characters collapse to `_`).  Ordinary
    /// dotted ids like `plugin.yolink` map to `plugin.yolink.toml` unchanged.
    pub fn path_for(&self, plugin_id: &str) -> PathBuf {
        self.dir.join(format!("{}.toml", slug(plugin_id)))
    }

    /// Whether a central config file already exists for this plugin.
    pub fn exists(&self, plugin_id: &str) -> bool {
        self.path_for(plugin_id).exists()
    }

    /// Read the raw TOML text for a plugin.
    pub fn read(&self, plugin_id: &str) -> Result<String> {
        let path = self.path_for(plugin_id);
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    }

    /// Atomically write raw TOML text for a plugin, creating the directory if
    /// needed.
    pub fn write(&self, plugin_id: &str, raw: &str) -> Result<()> {
        let path = self.path_for(plugin_id);
        write_atomic(&path, raw.as_bytes()).with_context(|| format!("write {}", path.display()))
    }

    /// One-time import: byte-copy `legacy` into the canonical location **iff**
    /// no central file exists yet.  Returns `Ok(true)` if a copy was made,
    /// `Ok(false)` if the central file already existed (idempotent — the central
    /// copy is always authoritative once present).
    ///
    /// A raw byte copy preserves the operator's comments, formatting, and
    /// secret values exactly.
    pub fn import_legacy(&self, plugin_id: &str, legacy: &Path) -> Result<bool> {
        if self.exists(plugin_id) {
            return Ok(false);
        }
        let bytes = std::fs::read(legacy)
            .with_context(|| format!("read legacy config {}", legacy.display()))?;
        let dest = self.path_for(plugin_id);
        write_atomic(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
        Ok(true)
    }
}

/// Collapse anything that isn't a safe filename character to `_`, guaranteeing
/// the result stays a single path component inside the store directory.
fn slug(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_maps_dotted_id_and_sanitizes_separators() {
        let store = PluginConfigStore::new("/tmp/hc-cfg");
        assert_eq!(
            store.path_for("plugin.yolink"),
            PathBuf::from("/tmp/hc-cfg/plugin.yolink.toml")
        );
        // A separator or traversal attempt can never leave the store dir.
        let evil = store.path_for("../../etc/passwd");
        assert_eq!(evil.parent().unwrap(), Path::new("/tmp/hc-cfg"));
        assert!(!evil.to_string_lossy().contains('/') || evil.starts_with("/tmp/hc-cfg"));
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(dir.path());
        let body = "[homecore]\npassword = \"s3cr3t\"\n# keep me\n";
        store.write("plugin.hue", body).unwrap();
        assert_eq!(store.read("plugin.hue").unwrap(), body);
        // No partial/tmp file left behind by the atomic write.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "atomic write left a tmp file");
    }

    #[test]
    fn import_legacy_copies_when_absent_then_is_idempotent() {
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("config.toml");
        let original = "[yolink]\nmode = \"local\"\n# operator comment\n";
        std::fs::write(&legacy, original).unwrap();

        let store_dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(store_dir.path());

        // First import copies byte-for-byte.
        assert!(store.import_legacy("plugin.yolink", &legacy).unwrap());
        assert_eq!(store.read("plugin.yolink").unwrap(), original);

        // A subsequent legacy edit must NOT overwrite the now-authoritative
        // central copy; import is a no-op once the central file exists.
        std::fs::write(&legacy, "changed = true\n").unwrap();
        assert!(!store.import_legacy("plugin.yolink", &legacy).unwrap());
        assert_eq!(store.read("plugin.yolink").unwrap(), original);
    }

    #[test]
    fn import_legacy_reports_missing_source() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(store_dir.path());
        let err = store.import_legacy("plugin.ghost", Path::new("/no/such/file.toml"));
        assert!(err.is_err());
    }
}
