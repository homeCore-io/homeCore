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
use hc_types::device::DeviceState;
use hc_types::rule::{Action, Condition, Rule, Trigger};
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

    /// Serialize `rule` and write it to the appropriate `.toml` file.
    ///
    /// If a file already exists in the rules directory whose `id` matches
    /// `rule.id`, that file is overwritten in-place (preserving the original
    /// filename).  Otherwise a new file is created at `{dir}/{slug}.toml`
    /// where `slug` is derived from `rule.name`.
    ///
    /// Creates the directory if it does not exist.  Returns the path written.
    pub fn write_rule(&self, rule: &Rule) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating rules directory {}", self.dir.display()))?;

        // Prefer overwriting the existing file (if any) so a rule whose file
        // was manually named never produces a duplicate.
        let path = match self.find_file(rule.id)? {
            Some(existing) => existing,
            None => {
                let slug = slugify(&rule.name);
                self.dir.join(format!("{slug}.toml"))
            }
        };

        let content = toml::to_string_pretty(rule).context("serializing rule to TOML")?;

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

    /// Write a rule to its slug-derived filename, deleting the old file when
    /// the rule is renamed.
    ///
    /// Finds the current file on disk by rule ID.  If the file's path no
    /// longer matches the slug of `rule.name` (i.e. the rule was renamed or
    /// the original file had a custom name), the old file is deleted after the
    /// new one is written.
    pub fn write_rule_renamed(&self, rule: &Rule, old_name: &str) -> Result<PathBuf> {
        // Find the existing file (by ID) before writing the new one.
        let existing_path = self.find_file(rule.id)?;

        let new_slug = slugify(&rule.name);
        let new_path = self.dir.join(format!("{new_slug}.toml"));

        let content = toml::to_string_pretty(rule).context("serializing rule to TOML")?;

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating rules directory {}", self.dir.display()))?;

        std::fs::write(&new_path, &content)
            .with_context(|| format!("writing rule file {}", new_path.display()))?;

        // Delete the old file if it exists at a different path than where we
        // just wrote (covers slug-mismatch as well as renamed files).
        if let Some(old) = existing_path {
            if old != new_path && old.exists() {
                std::fs::remove_file(&old)
                    .with_context(|| format!("removing old rule file {}", old.display()))?;
            }
        } else {
            // No existing file found by ID — fall back to deleting by old slug.
            let old_slug = slugify(old_name);
            if old_slug != new_slug {
                let old_path = self.dir.join(format!("{old_slug}.toml"));
                if old_path.exists() {
                    std::fs::remove_file(&old_path).with_context(|| {
                        format!("removing old rule file {}", old_path.display())
                    })?;
                }
            }
        }

        Ok(new_path)
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
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
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

// ── Device-deletion cascading ─────────────────────────────────────────────────

/// Scan every rule file and replace all references to `device_id` with a
/// `"DELETED:{device_id}"` placeholder.  Affected rules are disabled and
/// annotated with `error`.
///
/// Returns the names of rules that were modified and written back to disk.
pub fn nullify_device_refs(
    dir: &Path,
    device_id: &str,
    devices: &[DeviceState],
) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(vec![]);
    }

    let placeholder = format!("DELETED:{device_id}");
    let mut affected = Vec::new();

    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("scanning rules directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut rule) = toml::from_str::<Rule>(&content) else {
            continue;
        };

        if rule_references_device(&rule, device_id, devices) {
            replace_device_refs(&mut rule, device_id, &placeholder, devices);
            rule.enabled = false;
            rule.error = Some(format!("references deleted device: {device_id}"));

            let updated = toml::to_string_pretty(&rule)
                .with_context(|| format!("serialising rule {}", rule.name))?;
            std::fs::write(&path, updated)
                .with_context(|| format!("writing {}", path.display()))?;

            tracing::warn!(
                rule_name = %rule.name,
                rule_id   = %rule.id,
                device_id = %device_id,
                "Rule disabled: references deleted device"
            );
            affected.push(rule.name.clone());
        }
    }

    Ok(affected)
}

/// Returns `true` if any trigger, condition, or action in `rule` directly
/// references `device_id` (does not search inside Rhai script strings).
fn rule_references_device(rule: &Rule, device_id: &str, devices: &[DeviceState]) -> bool {
    trigger_references_device(&rule.trigger, device_id, devices)
        || rule
            .conditions
            .iter()
            .any(|c| condition_references_device(c, device_id, devices))
        || rule
            .actions
            .iter()
            .any(|ra| action_references_device(&ra.action, device_id, devices))
}

fn trigger_references_device(trigger: &Trigger, device_id: &str, devices: &[DeviceState]) -> bool {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id: id,
            device_ids,
            ..
        } => {
            hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)
                || device_ids.iter().any(|id| {
                    hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)
                })
        }
        Trigger::DeviceAvailabilityChanged { device_id: id, .. }
        | Trigger::ButtonEvent { device_id: id, .. }
        | Trigger::NumericThreshold { device_id: id, .. } => {
            hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)
        }
        _ => false,
    }
}

fn condition_references_device(cond: &Condition, device_id: &str, devices: &[DeviceState]) -> bool {
    match cond {
        Condition::DeviceState { device_id: id, .. }
        | Condition::TimeElapsed { device_id: id, .. } => {
            hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)
        }
        Condition::Not { condition } => condition_references_device(condition, device_id, devices),
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => conditions
            .iter()
            .any(|cond| condition_references_device(cond, device_id, devices)),
        _ => false,
    }
}

fn action_references_device(action: &Action, device_id: &str, devices: &[DeviceState]) -> bool {
    match action {
        Action::SetDeviceState { device_id: id, .. }
        | Action::SetDeviceStatePerMode { device_id: id, .. }
        | Action::FadeDevice { device_id: id, .. } => {
            hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)
        }
        Action::WaitForEvent {
            device_id: Some(id),
            ..
        } => hc_core::rule_resolver::reference_points_to_device(id, device_id, devices),
        Action::CaptureDeviceState { device_ids, .. } => device_ids
            .iter()
            .any(|id| hc_core::rule_resolver::reference_points_to_device(id, device_id, devices)),
        Action::Parallel { actions } => actions
            .iter()
            .any(|a| action_references_device(a, device_id, devices)),
        Action::RepeatUntil { actions, .. } => actions
            .iter()
            .any(|a| action_references_device(a, device_id, devices)),
        Action::RepeatWhile { actions, .. } => actions
            .iter()
            .any(|a| action_references_device(a, device_id, devices)),
        Action::RepeatCount { actions, .. } => actions
            .iter()
            .any(|a| action_references_device(a, device_id, devices)),
        Action::Conditional {
            then_actions,
            else_if,
            else_actions,
            ..
        } => {
            then_actions
                .iter()
                .any(|a| action_references_device(a, device_id, devices))
                || else_if.iter().any(|branch| {
                    branch
                        .actions
                        .iter()
                        .any(|a| action_references_device(a, device_id, devices))
                })
                || else_actions
                    .iter()
                    .any(|a| action_references_device(a, device_id, devices))
        }
        Action::PingHost {
            then_actions,
            else_actions,
            ..
        } => {
            then_actions
                .iter()
                .any(|a| action_references_device(a, device_id, devices))
                || else_actions
                    .iter()
                    .any(|a| action_references_device(a, device_id, devices))
        }
        _ => false,
    }
}

/// Mutably replace all occurrences of `device_id` with `placeholder` in a rule.
fn replace_device_refs(
    rule: &mut Rule,
    device_id: &str,
    placeholder: &str,
    devices: &[DeviceState],
) {
    replace_in_trigger(&mut rule.trigger, device_id, placeholder, devices);
    for cond in &mut rule.conditions {
        replace_in_condition(cond, device_id, placeholder, devices);
    }
    for ra in &mut rule.actions {
        replace_in_action(&mut ra.action, device_id, placeholder, devices);
    }
}

fn replace_in_trigger(
    trigger: &mut Trigger,
    device_id: &str,
    placeholder: &str,
    devices: &[DeviceState],
) {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id: id,
            device_ids,
            ..
        } => {
            if hc_core::rule_resolver::reference_points_to_device(id, device_id, devices) {
                *id = placeholder.to_string();
            }
            for ref_id in device_ids {
                if hc_core::rule_resolver::reference_points_to_device(ref_id, device_id, devices) {
                    *ref_id = placeholder.to_string();
                }
            }
        }
        Trigger::DeviceAvailabilityChanged { device_id: id, .. }
        | Trigger::ButtonEvent { device_id: id, .. }
        | Trigger::NumericThreshold { device_id: id, .. } => {
            if hc_core::rule_resolver::reference_points_to_device(id, device_id, devices) {
                *id = placeholder.to_string();
            }
        }
        _ => {}
    }
}

fn replace_in_condition(
    cond: &mut Condition,
    device_id: &str,
    placeholder: &str,
    devices: &[DeviceState],
) {
    match cond {
        Condition::DeviceState { device_id: id, .. }
        | Condition::TimeElapsed { device_id: id, .. } => {
            if hc_core::rule_resolver::reference_points_to_device(id, device_id, devices) {
                *id = placeholder.to_string();
            }
        }
        Condition::Not { condition } => {
            replace_in_condition(condition, device_id, placeholder, devices)
        }
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => {
            for cond in conditions {
                replace_in_condition(cond, device_id, placeholder, devices);
            }
        }
        _ => {}
    }
}

fn replace_in_action(
    action: &mut Action,
    device_id: &str,
    placeholder: &str,
    devices: &[DeviceState],
) {
    match action {
        Action::SetDeviceState { device_id: id, .. }
        | Action::SetDeviceStatePerMode { device_id: id, .. }
        | Action::FadeDevice { device_id: id, .. } => {
            if hc_core::rule_resolver::reference_points_to_device(id, device_id, devices) {
                *id = placeholder.to_string();
            }
        }
        Action::WaitForEvent {
            device_id: Some(id),
            ..
        } => {
            if hc_core::rule_resolver::reference_points_to_device(id, device_id, devices) {
                *id = placeholder.to_string();
            }
        }
        Action::CaptureDeviceState { device_ids, .. } => {
            for ref_id in device_ids {
                if hc_core::rule_resolver::reference_points_to_device(ref_id, device_id, devices) {
                    *ref_id = placeholder.to_string();
                }
            }
        }
        Action::Parallel { actions } => {
            for a in actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
        }
        Action::RepeatUntil { actions, .. }
        | Action::RepeatWhile { actions, .. }
        | Action::RepeatCount { actions, .. } => {
            for a in actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
        }
        Action::Conditional {
            then_actions,
            else_if,
            else_actions,
            ..
        } => {
            for a in then_actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
            for branch in else_if {
                for a in &mut branch.actions {
                    replace_in_action(a, device_id, placeholder, devices);
                }
            }
            for a in else_actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
        }
        Action::PingHost {
            then_actions,
            else_actions,
            ..
        } => {
            for a in then_actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
            for a in else_actions {
                replace_in_action(a, device_id, placeholder, devices);
            }
        }
        _ => {}
    }
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
