//! Write-through file store for automation rules.
//!
//! When the REST API creates, updates, or deletes a rule, `RuleFileStore`
//! writes (or removes) the corresponding `.toml` file in the rules directory.
//! The `hc_core::rule_loader::RuleWatcher` detects the change and reloads the
//! live rule set — no manual signalling required.
//!
//! # File naming
//!
//! Filenames are derived from the rule's `name` field via [`slugify`]:
//! `"Morning Lights"` → `morning_lights.toml`.
//!
//! # Import note
//!
//! `RuleFileStore` is deliberately synchronous (blocking filesystem calls) so
//! it can be called directly from async handlers without needing
//! `spawn_blocking`.  Rule files are small; the I/O is negligible.

use anyhow::{Context, Result};
use hc_types::rule::Rule;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Provides create / update / delete operations on rule TOML files.
#[derive(Clone)]
pub struct RuleFileStore {
    pub dir: PathBuf,
}

impl RuleFileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Serialize `rule` and write it to `{dir}/{slug}.toml`.
    ///
    /// Creates the directory if it does not exist.  Returns the path written.
    pub fn write_rule(&self, rule: &Rule) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating rules directory {}", self.dir.display()))?;

        let slug = slugify(&rule.name);
        let path = self.dir.join(format!("{slug}.toml"));

        let content = toml::to_string_pretty(rule)
            .context("serializing rule to TOML")?;

        std::fs::write(&path, content)
            .with_context(|| format!("writing rule file {}", path.display()))?;

        Ok(path)
    }

    /// Delete the `.toml` file whose `id` field matches `id`.
    ///
    /// Returns `true` if a file was found and deleted, `false` if no matching
    /// file was found.
    pub fn delete_rule(&self, id: Uuid) -> Result<bool> {
        match self.find_file(id)? {
            Some(path) => {
                std::fs::remove_file(&path)
                    .with_context(|| format!("deleting rule file {}", path.display()))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Scan the rules directory and return the path of the file whose `id`
    /// field matches `id`.  Returns `None` if not found.
    pub fn find_file(&self, id: Uuid) -> Result<Option<PathBuf>> {
        if !self.dir.exists() {
            return Ok(None);
        }
        let id_str = id.to_string();
        for entry in std::fs::read_dir(&self.dir)
            .with_context(|| format!("scanning {}", self.dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Fast pre-filter: skip files that don't contain the UUID string.
                if content.contains(&id_str) {
                    if let Ok(rule) = toml::from_str::<Rule>(&content) {
                        if rule.id == id {
                            return Ok(Some(path));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Write a rule to a file named after `new_name`, and delete the old file
    /// if the slug changed (i.e. the rule was renamed).
    pub fn write_rule_renamed(&self, rule: &Rule, old_name: &str) -> Result<PathBuf> {
        let old_slug = slugify(old_name);
        let new_slug = slugify(&rule.name);

        let path = self.write_rule(rule)?;

        // Remove the old file if the name changed.
        if old_slug != new_slug {
            let old_path = self.dir.join(format!("{old_slug}.toml"));
            if old_path.exists() {
                std::fs::remove_file(&old_path)
                    .with_context(|| format!("removing old rule file {}", old_path.display()))?;
            }
        }

        Ok(path)
    }
}

/// Convert a display name to a filesystem-safe slug.
///
/// Rules: lowercase, non-alphanumeric characters become underscores,
/// consecutive underscores are collapsed, leading/trailing underscores removed.
///
/// Examples: `"Morning Lights"` → `"morning_lights"`,
///           `"CO₂ Sensor!"` → `"co_2_sensor"`.
pub fn slugify(name: &str) -> String {
    let raw: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();

    // Collapse runs of underscores and strip leading/trailing ones.
    raw.split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// Resolve the path of a rule file given its name, without reading the file.
pub fn rule_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.toml", slugify(name)))
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Morning Lights"), "morning_lights");
        assert_eq!(slugify("front_door_arrival"), "front_door_arrival");
        assert_eq!(slugify("  My Rule!  "), "my_rule");
        assert_eq!(slugify("CO2 Sensor"), "co2_sensor");
    }
}
