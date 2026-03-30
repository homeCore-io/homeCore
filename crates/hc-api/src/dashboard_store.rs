//! Persistent store for dashboard definitions and per-user default selections.

use anyhow::{Context, Result};
use hc_types::dashboard::DashboardDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardStoreData {
    #[serde(default)]
    pub dashboards: Vec<DashboardDefinition>,
    #[serde(default)]
    pub user_defaults: HashMap<String, String>,
}

#[derive(Clone)]
pub struct DashboardStore {
    pub path: PathBuf,
}

impl DashboardStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<DashboardStoreData> {
        if !self.path.exists() {
            return Ok(DashboardStoreData::default());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading dashboards file {}", self.path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing dashboards file {}", self.path.display()))
    }

    pub fn save(&self, data: &DashboardStoreData) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(data).context("serializing dashboards")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("writing dashboards file {}", self.path.display()))
    }
}
