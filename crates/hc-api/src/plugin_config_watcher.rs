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
//! simply skipped — there is nothing to restart. Conversely, having a channel
//! is what makes a plugin watchable: the set is resolved per event rather than
//! frozen at startup, so a plugin installed mid-run is covered.

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
    /// Start watching `store.dir()`. On a genuine content change, send
    /// `PluginCommand::Restart` on that plugin's channel in `commands`.
    ///
    /// `plugin_ids` seeds the baseline content hashes and the filename lookup;
    /// it is **not** the limit of what gets watched. A file that matches no
    /// seeded id is resolved against the live `commands` map instead, so a
    /// plugin installed after core booted is covered too — see
    /// [`resolve_plugin_id`].
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
                    let Some(fname) = p.file_name() else { continue };
                    let Some(id) = resolve_plugin_id(fname, &store, &commands, &by_filename).await
                    else {
                        continue;
                    };
                    match store.read(&id) {
                        Ok(content) => {
                            let h = content_hash(&content);
                            if hashes.get(&id) == Some(&h) {
                                continue; // unchanged bytes — ignore
                            }
                            hashes.insert(id.clone(), h);
                            to_reload.insert(id);
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

/// Resolve a changed config filename back to the plugin that owns it.
///
/// The startup map alone is not enough. It is built from the plugin list as it
/// stood when core booted, so a plugin **installed from the registry during the
/// run** never appears in it, and saving its config silently restarts nothing —
/// while the config editor tells the operator that saving applies the change.
/// That is what happened to `plugin.lutron`: core started at 21:43, the plugin
/// was installed at 22:02, and the repeater credentials it was given only took
/// effect because an unrelated crash-backoff restart happened to follow them.
///
/// The live command-channel map is the authority on what core is supervising
/// right now — `spawn_one` registers a runtime install there as it starts it,
/// which is why that plugin's Restart button works. Consulting it at event time
/// costs one read-lock per changed file and needs no extra bookkeeping.
async fn resolve_plugin_id(
    file: &std::ffi::OsStr,
    store: &PluginConfigStore,
    commands: &PluginCommandChannels,
    known: &HashMap<OsString, String>,
) -> Option<String> {
    if let Some(id) = known.get(file) {
        return Some(id.clone());
    }
    commands
        .read()
        .await
        .keys()
        .find(|id| store.path_for(id).file_name() == Some(file))
        .cloned()
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

    #[tokio::test]
    async fn a_plugin_installed_after_startup_is_still_watched() {
        // The live failure: core booted at 21:43 knowing nothing of
        // plugin.lutron, which was installed from the registry at 22:02. Its
        // repeater credentials were saved and silently applied to nothing,
        // because the filename→id map was built once at startup.
        let dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(dir.path());
        let commands: PluginCommandChannels = Arc::new(RwLock::new(HashMap::new()));

        // Started with no plugins at all — as core does on a fresh install.
        let _watcher =
            PluginConfigWatcher::start(store.clone(), commands.clone(), Vec::new()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now install one: the supervisor registers its channel, exactly as
        // `spawn_one` does for a runtime install.
        let (tx, mut rx) = mpsc::channel::<PluginCommand>(8);
        commands
            .write()
            .await
            .insert("plugin.lutron".to_string(), tx);
        store.write("plugin.lutron", "host = \"\"\n").unwrap();

        // Drain the install's own write, which legitimately looks like a change.
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

        // The operator enters the repeater address and saves.
        store
            .write("plugin.lutron", "host = \"10.0.10.24\"\n")
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(
            matches!(got, Ok(Some(PluginCommand::Restart))),
            "a config save must restart a plugin installed mid-run, got {got:?}"
        );
    }

    #[tokio::test]
    async fn a_file_belonging_to_no_known_plugin_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(dir.path());
        let (tx, mut rx) = mpsc::channel::<PluginCommand>(8);
        let commands: PluginCommandChannels = Arc::new(RwLock::new(HashMap::new()));
        commands.write().await.insert("plugin.a".to_string(), tx);

        let _watcher =
            PluginConfigWatcher::start(store.clone(), commands.clone(), vec!["plugin.a".into()])
                .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A stray .toml in the directory must not restart anything — resolving
        // against the live channel map must not become "restart whatever".
        store.write("plugin.not_installed", "x = 1\n").unwrap();
        let none = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(none.is_err(), "unknown file must not restart, got {none:?}");
    }
}
