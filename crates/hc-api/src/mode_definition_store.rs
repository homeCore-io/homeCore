//! Persistent store for criteria-driven mode definitions.
//!
//! Definitions live beside `config/modes.toml` as `config/mode_definitions.json`.
//! Each definition owns a set of generated rule IDs that reconcile the mode's
//! boolean state from native HomeCore rule conditions.

use anyhow::{Context, Result};
use hc_types::rule::Condition;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CriteriaOffBehavior {
    #[default]
    Inverse,
    Explicit,
}

fn default_reevaluate_minutes() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriteriaModeConfig {
    pub on_condition: Condition,
    #[serde(default)]
    pub off_behavior: CriteriaOffBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_condition: Option<Condition>,
    #[serde(default = "default_reevaluate_minutes")]
    pub reevaluate_every_n_minutes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeDefinition {
    pub mode_id: String,
    pub criteria: CriteriaModeConfig,
    #[serde(default)]
    pub generated_rule_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ModeDefinitionsFile {
    #[serde(default)]
    definitions: Vec<ModeDefinition>,
}

#[derive(Clone)]
pub struct ModeDefinitionStore {
    pub path: PathBuf,
}

impl ModeDefinitionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Vec<ModeDefinition>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading mode definitions {}", self.path.display()))?;
        let file: ModeDefinitionsFile = serde_json::from_str(&raw)
            .with_context(|| format!("parsing mode definitions {}", self.path.display()))?;
        Ok(file.definitions)
    }

    pub fn save(&self, definitions: &[ModeDefinition]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&ModeDefinitionsFile {
            definitions: definitions.to_vec(),
        })
        .context("serialising mode definitions")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("writing mode definitions {}", self.path.display()))
    }
}

pub fn mode_definitions_path(modes_path: &Path) -> PathBuf {
    modes_path
        .parent()
        .unwrap_or(modes_path)
        .join("mode_definitions.json")
}
