//! Hot-reload watcher for the core-owned plugin config store.
//!
//! Watches `{base}/config/plugins/` and, when a plugin's config file changes on
//! disk (an operator editing it headlessly, or an API `PUT` writing it), asks
//! the supervisor to **restart** that plugin so it re-reads its config.  Restart
//! is the Phase 0 reload mechanism: the plugin still reads its config once at
//! startup (`argv[1]`), and core owns its process, so a restart is the
//! no-plugin-change way to apply a config edit.  (In-place SIGHUP-style reload
//! needs plugin cooperation and is deferred to a later phase.)
//!
//! Modeled on [`hc_core::rule_loader::RuleWatcher`]: a `notify` watcher feeds
//! paths to a debounced async loop.  Two guards keep it quiet:
//!   - **content hashing** — a change whose bytes match the last-seen content is
//!     ignored (dedups `notify`'s multiple events per save, and skips no-op
//!     writes).
//!   - **removal is ignored** — a transient unlink/rename never kills a running
//!     plugin; only a readable, genuinely-changed file triggers a restart.
//!
//! A plugin with no local supervisor channel (disabled, or a remote plugin) is
//! simply skipped — there is nothing to restart.

use crate::{PluginCommand, PluginCommandChannels, PluginConfigStore};
use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Live filesystem watcher for plugin config files. Keep the returned value
/// alive for as long as reloads should happen — dropping it stops watching.
pub struct PluginConfigWatcher {
    _watcher: RecommendedWatcher,
}

impl PluginConfigWatcher {
    /// Start watching `store.dir()` for changes to the config files of
    /// `plugin_ids`. On a genuine content change, send `PluginCommand::Restart`
    /// on that plugin's channel in `commands`.
    pub fn start(
        store: PluginConfigStore,
        commands: PluginCommandChannels,
        plugin_ids: Vec<String>,
    ) -> Result<Self> {
        let dir = store.dir().to_path_buf();

        // filename → plugin_id, so a changed path resolves back to the plugin
        // even though the on-disk name is a (lossy) slug of the id.
        let by_filename: HashMap<OsString, String> = plugin_ids
            .iter()
            .filter_map(|id| {
                store
                    .path_for(id)
                    .file_name()
                    .map(|f| (f.to_os_string(), id.clone()))
            })
            .collect();

        // Baseline hashes from current content so the first stray event (e.g. a
        // metadata touch) doesn't cause a spurious restart.
        let mut hashes: HashMap<String, u64> = HashMap::new();
        for id in &plugin_ids {
            if let Ok(content) = store.read(id) {
                hashes.insert(id.clone(), content_hash(&content));
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(64);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for p in event
                .paths
                .iter()
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
            {
                // From the notify callback thread → the async loop.
                let _ = tx.blocking_send(p.clone());
            }
        })?;

        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        info!(dir = %dir.display(), "Plugin config hot-reload watcher active");

        tokio::spawn(async move {
            let mut hashes = hashes;
            loop {
                let first = match rx.recv().await {
                    Some(p) => p,
                    None => break,
                };
                let mut changed: HashSet<PathBuf> = HashSet::new();
                changed.insert(first);

                // Debounce: collect further events within 300 ms (an editor save
                // often emits several).
                let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
                // Exits on timeout (Err) or a closed channel (Ok(None)).
                while let Ok(Some(p)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                    changed.insert(p);
                }

                // Resolve changed files → plugin ids with a genuine content change.
                let mut to_reload: HashSet<String> = HashSet::new();
                for p in &changed {
                    let Some(id) = p.file_name().and_then(|f| by_filename.get(f)) else {
                        continue;
                    };
                    match store.read(id) {
                        Ok(content) => {
                            let h = content_hash(&content);
                            if hashes.get(id) == Some(&h) {
                                continue; // unchanged bytes — ignore
                            }
                            hashes.insert(id.clone(), h);
                            to_reload.insert(id.clone());
                        }
                        // Unreadable (mid-rename/removed) — don't restart on a
                        // transient state; wait for the settled write.
                        Err(_) => continue,
                    }
                }

                for id in to_reload {
                    let cmds = commands.read().await;
                    match cmds.get(&id) {
                        Some(tx) => match tx.send(PluginCommand::Restart).await {
                            Ok(()) => {
                                info!(id = %id, "Plugin config changed on disk — restarting to apply")
                            }
                            Err(_) => {
                                warn!(id = %id, "Plugin config changed but supervisor not responding")
                            }
                        },
                        None => debug!(
                            id = %id,
                            "Plugin config changed but no local supervisor channel; skipping reload"
                        ),
                    }
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}

/// Non-cryptographic content hash for change detection within a single run.
fn content_hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{mpsc, RwLock};

    #[test]
    fn content_hash_detects_change() {
        assert_eq!(content_hash("v = 1\n"), content_hash("v = 1\n"));
        assert_ne!(content_hash("v = 1\n"), content_hash("v = 2\n"));
    }

    #[tokio::test]
    async fn edit_restarts_plugin_but_identical_write_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(dir.path());
        store.write("plugin.a", "v = 1\n").unwrap();

        let (tx, mut rx) = mpsc::channel::<PluginCommand>(8);
        let commands: PluginCommandChannels = Arc::new(RwLock::new(HashMap::new()));
        commands.write().await.insert("plugin.a".to_string(), tx);

        let _watcher =
            PluginConfigWatcher::start(store.clone(), commands.clone(), vec!["plugin.a".into()])
                .unwrap();

        // Let the inotify watch establish before the first edit.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Genuine content change → one Restart.
        store.write("plugin.a", "v = 2\n").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(
            matches!(got, Ok(Some(PluginCommand::Restart))),
            "expected restart on edit, got {got:?}"
        );

        // Re-writing identical bytes must NOT restart (hash unchanged).
        store.write("plugin.a", "v = 2\n").unwrap();
        let none = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(
            none.is_err(),
            "identical content must not restart, got {none:?}"
        );
    }
}
