//! Persistent store for rule groups.
//!
//! Rule groups are named bundles of rule IDs that can be enabled or disabled
//! as a unit via `POST /automations/groups/{id}/enable|disable`.  They are
//! stored as a JSON array in `{rules_dir}/groups.json` and loaded at startup.
//!
//! Groups are purely organisational — they do not affect rule evaluation order
//! or priorities.  A rule can belong to multiple groups.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A named collection of rule IDs that can be toggled together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleGroup {
    pub id: Uuid,
    pub name: String,
    /// Optional human-readable description (e.g. "Rules to pause while on vacation").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered list of rule UUIDs that belong to this group.
    #[serde(default)]
    pub rule_ids: Vec<Uuid>,
}

/// Synchronous JSON file store for rule groups.
///
/// All operations are blocking filesystem calls and are safe to call
/// directly from async handlers (files are small; I/O is negligible).
#[derive(Clone)]
pub struct GroupStore {
    pub path: PathBuf,
}

impl GroupStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load all groups from the JSON file.  Returns an empty vec if the file
    /// does not exist (first run).
    pub fn load(&self) -> Result<Vec<RuleGroup>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading groups file {}", self.path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing groups file {}", self.path.display()))
    }

    /// Persist the full group list to the JSON file (pretty-printed).
    pub fn save(&self, groups: &[RuleGroup]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(groups).context("serialising groups")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("writing groups file {}", self.path.display()))
    }
}

/// Derive the groups file path from the rules directory.
pub fn groups_path(rules_dir: &Path) -> PathBuf {
    rules_dir.join("groups.json")
}
