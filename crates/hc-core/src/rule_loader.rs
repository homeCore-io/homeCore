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
use std::collections::HashSet;
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            let is_relevant = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) && event.paths.iter().any(|p| is_rule_file(p));
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
                        // Purge stale rule state (DashMap entries for deleted rule IDs).
                        if let Some(ref cb) = on_reload {
                            cb(&rules_for_purge);
                        }
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
