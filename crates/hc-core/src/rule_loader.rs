//! File-based rule loader and hot-reload watcher.
//!
//! # Directory layout
//!
//! Each automation rule lives in its own RON file under the configured rules
//! directory (default: `{base_dir}/rules/`).  The filename stem is the rule's
//! slug; the `name` field inside the file provides a human-readable display
//! name (defaults to the slug if omitted).
//!
//! Legacy `.toml` files are still loaded for backwards compatibility but new
//! rules are always written as `.ron`.
//!
//! # Hot reload
//!
//! `RuleWatcher` uses the `notify` crate to monitor the directory.  Any
//! `.ron` or `.toml` create / modify / delete event triggers a debounced
//! reload (200 ms).  All files are re-parsed and validated before the live
//! rule set is atomically replaced.

use anyhow::{Context, Result};
use hc_state::StateStore;
use hc_types::rule::{Rule, RuleAction};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Recognised rule file extensions.
fn is_rule_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ron") | Some("toml")
    )
}

// ── Public load function ─────────────────────────────────────────────────────

/// Parse every `*.ron` and `*.toml` file in `dir` into a `Vec<Rule>`.
///
/// Never returns `Err` due to individual file parse failures or duplicate IDs.
/// Instead, broken files produce a disabled stub rule with `error` set so the
/// problem is visible in the API and logs without preventing startup or reload.
///
/// The only failure modes are I/O errors reading the directory itself.
pub fn load_all(dir: &Path) -> Result<Vec<Rule>> {
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut rules = Vec::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading rules directory {}", dir.display()))?;

    for entry in entries {
        let path = entry?.path();
        if !is_rule_file(&path) {
            continue;
        }
        match load_file(&path) {
            Ok(rule) => rules.push(rule),
            Err(e) => {
                warn!(file = %path.display(), error = %e, "Rule file parse error — inserting disabled stub");
                rules.push(broken_stub(&path, &e));
            }
        }
    }

    // Duplicate UUID check — keep the first occurrence, stub the rest.
    let mut seen: HashSet<uuid::Uuid> = HashSet::new();
    for rule in rules.iter_mut() {
        if !seen.insert(rule.id) {
            let msg = format!(
                "duplicate rule ID {} — rule disabled until ID is fixed",
                rule.id
            );
            warn!(rule_name = %rule.name, rule_id = %rule.id, "Duplicate rule ID found");
            rule.enabled = false;
            rule.error = Some(msg);
            // Assign a fresh ID so the stub doesn't conflict in the set.
            rule.id = uuid::Uuid::new_v4();
        }
    }

    // Sort by priority descending so the engine evaluates high-priority rules first.
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));

    Ok(rules)
}

/// Build a disabled placeholder `Rule` for a file that could not be parsed.
///
/// Uses a UUID v5 derived from the file path so the stub has a stable ID
/// across reloads (the broken file doesn't change between reloads).
fn broken_stub(path: &Path, err: &anyhow::Error) -> Rule {
    use hc_types::rule::{Condition, Trigger};

    // Stable UUID from the absolute path so repeated reloads yield the same ID.
    let path_bytes = path.to_string_lossy();
    let stub_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, path_bytes.as_bytes());

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    Rule {
        id: stub_id,
        name: format!("{name} [BROKEN]"),
        enabled: false,
        priority: 0,
        tags: Vec::new(),
        trigger: Trigger::ManualTrigger,
        conditions: Vec::<Condition>::new(),
        actions: Vec::<RuleAction>::new(),
        error: Some(format!("parse error: {err}")),
        cooldown_secs: None,
        log_events: false,
        log_triggers: false,
        log_actions: false,
        required_expression: None,
        cancel_on_false: false,
        trigger_condition: None,
        variables: std::collections::HashMap::new(),
        trigger_label: None,
        run_mode: hc_types::rule::RunMode::Parallel,
    }
}

/// Parse a single rule file (RON or legacy TOML, detected by extension).
///
/// For RON files: if the rule has a nil UUID (all zeros), a fresh UUID v4 is
/// generated and the file is rewritten with the new ID.
///
/// For TOML files (legacy): the old `id = ""` / missing-id logic still works.
pub fn load_file(path: &Path) -> Result<Rule> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let mut rule: Rule = match ext {
        "ron" => {
            ron::from_str(&content).with_context(|| format!("parsing RON in {}", path.display()))?
        }
        "toml" => toml::from_str(&content)
            .with_context(|| format!("parsing TOML in {}", path.display()))?,
        _ => anyhow::bail!("unsupported rule file extension: {ext}"),
    };

    // Auto-generate UUID if nil (RON) or missing/empty (TOML).
    if rule.id.is_nil() {
        let new_id = uuid::Uuid::new_v4();
        rule.id = new_id;
        // Rewrite the file with the generated ID.
        if ext == "ron" {
            let cfg = ron::ser::PrettyConfig::default().struct_names(true);
            let updated =
                ron::ser::to_string_pretty(&rule, cfg).context("serializing rule to RON")?;
            std::fs::write(path, &updated)
                .with_context(|| format!("writing generated id back to {}", path.display()))?;
        } else {
            // Legacy TOML rewrite
            let updated = if has_empty_id(&content) {
                replace_empty_id(&content, &new_id.to_string())
            } else if is_missing_id(&content) {
                format!("id = \"{new_id}\"\n{content}")
            } else {
                // ID was present but parsed as nil — rewrite whole file
                toml::to_string_pretty(&rule).context("serializing rule to TOML")?
            };
            std::fs::write(path, &updated)
                .with_context(|| format!("writing generated id back to {}", path.display()))?;
        }
        info!(file = %path.display(), id = %new_id, "Generated missing rule ID and wrote to file");
    }

    // Derive display name from filename if the `name` field is empty.
    if rule.name.is_empty() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            rule.name = stem.to_string();
        }
    }

    Ok(rule)
}

// ── Legacy TOML ID helpers ──────────────────────────────────────────────────

/// Regex-free check: does the file's `id` field contain an empty string?
fn has_empty_id(content: &str) -> bool {
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("id") {
            let after_key = t["id".len()..].trim_start();
            if let Some(after_eq) = after_key.strip_prefix('=') {
                return after_eq.trim() == r#""""#;
            }
        }
    }
    false
}

/// Returns `true` if the file has no `id = ...` key at all (top-level only).
fn is_missing_id(content: &str) -> bool {
    !content.lines().any(|line| {
        let t = line.trim();
        if !t.starts_with("id") {
            return false;
        }
        let after_key = t["id".len()..].trim_start();
        after_key.starts_with('=')
    })
}

/// Replace `id = ""` (any whitespace variant) with `id = "{new_id}"` in content.
fn replace_empty_id(content: &str, new_id: &str) -> String {
    let mut result = String::with_capacity(content.len() + 40);
    let mut replaced = false;
    for line in content.lines() {
        if !replaced {
            let t = line.trim();
            if t.starts_with("id") {
                let after_key = t["id".len()..].trim_start();
                if let Some(after_eq) = after_key.strip_prefix('=') {
                    if after_eq.trim() == r#""""# {
                        result.push_str(&format!("id = \"{new_id}\""));
                        result.push('\n');
                        replaced = true;
                        continue;
                    }
                }
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

/// Write a single rule to a RON file in `dir`.
///
/// The file is named after the rule's `name` field (slugified).
pub fn write_rule(dir: &Path, rule: &Rule) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating rules directory {}", dir.display()))?;

    let slug = slugify(&rule.name);
    let path = dir.join(format!("{slug}.ron"));

    let cfg = ron::ser::PrettyConfig::default().struct_names(true);
    let content = ron::ser::to_string_pretty(rule, cfg).context("serializing rule to RON")?;

    std::fs::write(&path, &content)
        .with_context(|| format!("writing rule file {}", path.display()))?;

    Ok(path)
}

/// Convert a display name to a filesystem-safe slug.
fn slugify(name: &str) -> String {
    let raw: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    raw.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

// ── RuleWatcher ──────────────────────────────────────────────────────────────

/// Watches a rules directory for filesystem changes and hot-reloads the live
/// rule set atomically.
pub struct RuleWatcher {
    _watcher: RecommendedWatcher,
}

impl RuleWatcher {
    /// Start watching `dir`.
    ///
    /// On any `.ron` or `.toml` create / modify / delete event the watcher:
    /// 1. Debounces 200 ms to coalesce rapid edits.
    /// 2. Calls `load_all` on a blocking thread.
    /// 3. Validates the full set (parse + duplicate ID check).
    /// 4. On success: atomically swaps `handle` and logs at INFO.
    /// 5. On failure: logs a warning and leaves `handle` unchanged.
    pub fn start(
        dir: PathBuf,
        store: StateStore,
        source_handle: Arc<RwLock<Vec<Rule>>>,
        handle: Arc<RwLock<Vec<Rule>>>,
        on_reload: Option<Arc<dyn Fn(&[Rule]) + Send + Sync>>,
    ) -> Result<Self> {
        // Watcher sends `WatchSignal` messages — either targeted path changes
        // for a surgical reload, or `FullReload` when the OS can't tell us
        // which files changed.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WatchSignal>(64);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for p in event.paths.iter().filter(|p| is_rule_file(p)) {
                let _ = tx.blocking_send(WatchSignal::Path(p.clone()));
            }
        })?;

        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        info!(dir = %dir.display(), "Rule hot-reload watcher active");

        // Track the canonical filename that each rule was loaded from, so
        // path-based reloads can locate the right entry in the live set.
        let path_index: Arc<RwLock<HashMap<PathBuf, uuid::Uuid>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Seed the path index from the current rules directory so the first
        // reload after startup doesn't have to fall back to a full rescan.
        {
            let dir_seed = dir.clone();
            let path_index_seed = Arc::clone(&path_index);
            tokio::spawn(async move {
                let scan = tokio::task::spawn_blocking(move || scan_paths(&dir_seed)).await;
                if let Ok(map) = scan {
                    *path_index_seed.write().await = map;
                }
            });
        }

        let dir_clone = dir.clone();
        tokio::spawn(async move {
            loop {
                let first = match rx.recv().await {
                    Some(s) => s,
                    None => break,
                };
                let mut changed_paths: HashSet<PathBuf> = HashSet::new();
                let mut need_full = matches!(first, WatchSignal::FullReload);
                if let WatchSignal::Path(p) = first {
                    changed_paths.insert(p);
                }
                // Debounce: drain additional events within 200 ms, collecting
                // all changed paths.
                let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                loop {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(WatchSignal::Path(p))) => {
                            changed_paths.insert(p);
                        }
                        Ok(Some(WatchSignal::FullReload)) => need_full = true,
                        _ => break,
                    }
                }

                let did_surgical = if !need_full && !changed_paths.is_empty() {
                    surgical_reload(
                        &changed_paths,
                        &store,
                        &source_handle,
                        &handle,
                        &path_index,
                        &on_reload,
                    )
                    .await
                } else {
                    false
                };
                if did_surgical {
                    continue;
                }

                // Full reload fallback.
                let dir2 = dir_clone.clone();
                match tokio::task::spawn_blocking(move || load_all(&dir2)).await {
                    Ok(Ok(new_rules)) => {
                        *source_handle.write().await = new_rules.clone();
                        let rules_for_purge = new_rules.clone();
                        let compiled = match crate::rule_resolver::compile_rules_for_store(
                            &store, new_rules,
                        )
                        .await
                        {
                            Ok(rules) => rules,
                            Err(e) => {
                                warn!(error = %e, "Rule reload compilation failed — existing rules unchanged");
                                continue;
                            }
                        };
                        let count = compiled.len();
                        *handle.write().await = compiled;
                        let dir3 = dir_clone.clone();
                        if let Ok(map) =
                            tokio::task::spawn_blocking(move || scan_paths(&dir3)).await
                        {
                            *path_index.write().await = map;
                        }
                        if let Some(ref cb) = on_reload {
                            cb(&rules_for_purge);
                        }
                        info!(count, "Rules hot-reloaded (full)");
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

/// Signal from the filesystem watcher to the debounce/reload loop.
enum WatchSignal {
    /// A single rule file changed (created, modified, or removed).
    Path(PathBuf),
    /// The watcher couldn't attribute the change to a specific file — force a
    /// full directory rescan. Reserved for future use.
    #[allow(dead_code)]
    FullReload,
}

/// Scan the rules directory and return a `path → rule_id` map for all
/// successfully parsed rule files.
fn scan_paths(dir: &Path) -> HashMap<PathBuf, uuid::Uuid> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_rule_file(&path) {
            continue;
        }
        if let Ok(rule) = load_file(&path) {
            out.insert(path, rule.id);
        }
    }
    out
}

/// Reload only the rule files that changed during the debounce window.
///
/// Returns `true` if the surgical path completed successfully. On any
/// unrecoverable inconsistency returns `false` so the caller falls back to a
/// full rescan.
async fn surgical_reload(
    changed: &HashSet<PathBuf>,
    store: &StateStore,
    source_handle: &Arc<RwLock<Vec<Rule>>>,
    handle: &Arc<RwLock<Vec<Rule>>>,
    path_index: &Arc<RwLock<HashMap<PathBuf, uuid::Uuid>>>,
    on_reload: &Option<Arc<dyn Fn(&[Rule]) + Send + Sync>>,
) -> bool {
    // Partition changes into (path, rule) pairs for files that still exist,
    // and a separate list for deleted paths.
    let mut reloaded: Vec<(PathBuf, Rule)> = Vec::new();
    let mut removed_paths: Vec<PathBuf> = Vec::new();
    for path in changed {
        if path.exists() {
            match load_file(path) {
                Ok(rule) => reloaded.push((path.clone(), rule)),
                Err(e) => {
                    warn!(file = %path.display(), error = %e,
                        "Surgical reload: parse error — inserting disabled stub");
                    reloaded.push((path.clone(), broken_stub(path, &e)));
                }
            }
        } else {
            removed_paths.push(path.clone());
        }
    }

    // Resolve deleted paths to the rule IDs they used to map to, and include
    // any rule IDs whose file now points to a *different* rule (unlikely but
    // possible if a user renames+replaces). We treat those as "replaced".
    let idx_read = path_index.read().await;
    let mut removed_ids: HashSet<uuid::Uuid> = removed_paths
        .iter()
        .filter_map(|p| idx_read.get(p).copied())
        .collect();
    // If a reloaded path now maps to a different rule ID than before, the
    // previous rule is effectively removed.
    for (path, rule) in &reloaded {
        if let Some(prev_id) = idx_read.get(path) {
            if *prev_id != rule.id {
                removed_ids.insert(*prev_id);
            }
        }
    }
    drop(idx_read);

    // Compile only the rules we actually loaded.
    let rules_only: Vec<Rule> = reloaded.iter().map(|(_, r)| r.clone()).collect();
    let compiled = match crate::rule_resolver::compile_rules_for_store(store, rules_only.clone())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Surgical reload: compilation failed — falling back to full reload");
            return false;
        }
    };

    // Apply the diff to both handles.
    {
        let mut src = source_handle.write().await;
        apply_rule_diff(&mut src, &rules_only, &removed_ids);
    }
    {
        let mut live = handle.write().await;
        apply_rule_diff(&mut live, &compiled, &removed_ids);
    }

    // Refresh the path index for this batch.
    {
        let mut idx = path_index.write().await;
        for path in &removed_paths {
            idx.remove(path);
        }
        for (path, rule) in &reloaded {
            idx.insert(path.clone(), rule.id);
        }
    }

    // Purge stale per-rule DashMap state for removed rule IDs.
    if let Some(cb) = on_reload {
        let live = handle.read().await;
        cb(&live);
    }

    let added_or_updated = compiled.len();
    let removed = removed_ids.len();
    info!(
        added_or_updated,
        removed, "Rules hot-reloaded (surgical)"
    );
    true
}

/// Merge `new_rules` and remove `removed_ids` into the live `rules` slice.
///
/// Rules in `new_rules` with a matching ID replace the existing entry;
/// otherwise they're appended. Rules whose ID appears in `removed_ids` are
/// dropped.
fn apply_rule_diff(rules: &mut Vec<Rule>, new_rules: &[Rule], removed_ids: &HashSet<uuid::Uuid>) {
    // Drop removed IDs and IDs that are being replaced.
    let replacing: HashSet<uuid::Uuid> = new_rules.iter().map(|r| r.id).collect();
    rules.retain(|r| !removed_ids.contains(&r.id) && !replacing.contains(&r.id));
    rules.extend(new_rules.iter().cloned());
    // Keep priority-desc ordering consistent with `load_all`.
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));
}
