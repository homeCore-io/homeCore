//! File-based rule loader and hot-reload watcher.
//!
//! # Directory layout
//!
//! Each automation rule lives in its own TOML file under the configured rules
//! directory (default: `{base_dir}/rules/`).  The filename stem is the rule's
//! slug; the `name` field inside the file provides a human-readable display
//! name (defaults to the slug if omitted).
//!
//! # Hot reload
//!
//! `RuleWatcher` uses the `notify` crate to monitor the directory.  Any
//! `.toml` create / modify / delete event triggers a debounced reload (200 ms).
//! All files are re-parsed and validated before the live rule set is atomically
//! replaced.  If validation fails the existing rules remain unchanged and an
//! error is logged — the running system is never affected by a bad file.

use anyhow::{Context, Result};
use hc_types::rule::Rule;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};

// ── Public load function ─────────────────────────────────────────────────────

/// Parse every `*.toml` file in `dir` into a `Vec<Rule>`.
///
/// Returns an error (without modifying any state) when:
/// - Any file fails to parse (all errors are logged as warnings first)
/// - Duplicate rule IDs are found across files
///
/// The caller should keep the existing in-memory rules on error.
pub fn load_all(dir: &Path) -> Result<Vec<Rule>> {
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut rules = Vec::new();
    let mut errors: Vec<(PathBuf, anyhow::Error)> = Vec::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading rules directory {}", dir.display()))?;

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match load_file(&path) {
            Ok(rule) => rules.push(rule),
            Err(e) => {
                warn!(file = %path.display(), error = %e, "Rule file parse error");
                errors.push((path, e));
            }
        }
    }

    if !errors.is_empty() {
        return Err(anyhow::anyhow!(
            "{} rule file(s) failed to parse — keeping existing rules unchanged",
            errors.len()
        ));
    }

    // Duplicate UUID check.
    let mut seen: HashSet<uuid::Uuid> = HashSet::new();
    for rule in &rules {
        if !seen.insert(rule.id) {
            return Err(anyhow::anyhow!(
                "Duplicate rule ID {} found across rule files — keeping existing rules unchanged",
                rule.id
            ));
        }
    }

    // Sort by priority descending so the engine evaluates high-priority rules first.
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

    Ok(rules)
}

/// Parse a single rule TOML file.
pub fn load_file(path: &Path) -> Result<Rule> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut rule: Rule = toml::from_str(&content)
        .with_context(|| format!("parsing TOML in {}", path.display()))?;

    // Derive display name from filename if the `name` field is empty.
    if rule.name.is_empty() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            rule.name = stem.to_string();
        }
    }

    Ok(rule)
}

// ── RuleWatcher ──────────────────────────────────────────────────────────────

/// Watches a rules directory for filesystem changes and hot-reloads the live
/// rule set atomically.
///
/// Keep the returned value alive for the duration of the process (typically
/// until the end of `main`).  Dropping it stops the watcher.
pub struct RuleWatcher {
    _watcher: RecommendedWatcher,
}

impl RuleWatcher {
    /// Start watching `dir`.
    ///
    /// On any `.toml` create / modify / delete event the watcher:
    /// 1. Debounces 200 ms to coalesce rapid edits.
    /// 2. Calls `load_all` on a blocking thread.
    /// 3. Validates the full set (parse + duplicate ID check).
    /// 4. On success: atomically swaps `handle` and logs at INFO.
    /// 5. On failure: logs a warning and leaves `handle` unchanged.
    pub fn start(dir: PathBuf, handle: Arc<RwLock<Vec<Rule>>>) -> Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            let is_relevant = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) && event.paths.iter().any(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("toml")
            });
            if is_relevant {
                let _ = tx.blocking_send(());
            }
        })?;

        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        info!(dir = %dir.display(), "Rule hot-reload watcher active");

        let dir_clone = dir.clone();
        tokio::spawn(async move {
            loop {
                // Wait for first notification.
                if rx.recv().await.is_none() {
                    break;
                }
                // Debounce: drain additional events within 200 ms.
                let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                loop {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(())) => {} // more events, keep draining
                        _ => break,
                    }
                }
                // Reload on a blocking thread (filesystem I/O).
                let dir2 = dir_clone.clone();
                match tokio::task::spawn_blocking(move || load_all(&dir2)).await {
                    Ok(Ok(new_rules)) => {
                        let count = new_rules.len();
                        *handle.write().await = new_rules;
                        info!(count, "Rules hot-reloaded successfully");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "Rule reload failed — existing rules unchanged");
                    }
                    Err(e) => {
                        warn!(error = %e, "Rule reload task panicked");
                    }
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}
