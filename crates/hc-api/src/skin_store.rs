//! Persistent store for user-defined skins (`data/skins.json`).
//!
//! Deliberately the same shape as `dashboard_store`: a JSON file, loaded once
//! at boot into an `RwLock` and rewritten whole on change. Skins are a handful
//! of small documents edited by one person occasionally — the same access
//! pattern as dashboards, and a different mechanism would only be a second
//! thing to understand.
//!
//! A missing file is not an error. A house that has never defined a skin has no
//! skins file, and the four built-ins are compiled into the client, so nothing
//! is degraded by its absence.

use anyhow::{Context, Result};
use hc_types::skin::Skin;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkinStoreData {
    #[serde(default)]
    pub skins: Vec<Skin>,
}

#[derive(Clone)]
pub struct SkinStore {
    pub path: PathBuf,
}

impl SkinStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<SkinStoreData> {
        if !self.path.exists() {
            return Ok(SkinStoreData::default());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading skins file {}", self.path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing skins file {}", self.path.display()))
    }

    pub fn save(&self, data: &SkinStoreData) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(data).context("serializing skins")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("writing skins file {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hc_types::skin::{Brightness, Density, Glass, Motion, SkinSeeds};

    fn seeds() -> SkinSeeds {
        SkinSeeds {
            brightness: Brightness::Dark,
            ground: "#0B0E13".into(),
            raised: "#141922".into(),
            sunken: "#0D1116".into(),
            overlay: "#1A202A".into(),
            ink: "#E9EDF2".into(),
            ink_muted: "#8B95A4".into(),
            accent: "#7CC4FF".into(),
            on_accent: "#06131F".into(),
            active: "#FFB661".into(),
            inactive: "#2A313B".into(),
            success: "#6FD1A6".into(),
            warn: "#FFC978".into(),
            danger: "#FF7B72".into(),
            offline: "#AA737A".into(),
            hairline: "#262D38".into(),
            focus: None,
            corners: [4.0, 8.0, 14.0, 22.0],
            space_unit: 8.0,
            type_scale: 1.0,
            glow_strength: 1.0,
            glow_radius: 34.0,
            density: Density::Comfortable,
            motion: Motion::Standard,
            glass: Glass::None,
        }
    }

    fn skin(id: &str) -> Skin {
        Skin {
            id: id.into(),
            name: "Hallway".into(),
            base: "midnight".into(),
            seeds: seeds(),
            overrides: Default::default(),
        }
    }

    #[test]
    fn a_house_with_no_skins_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkinStore::new(dir.path().join("data").join("skins.json"));
        assert!(store.load().unwrap().skins.is_empty());
    }

    #[test]
    fn a_skin_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkinStore::new(dir.path().join("data").join("skins.json"));
        let data = SkinStoreData {
            skins: vec![skin("hallway")],
        };
        store.save(&data).unwrap();
        assert_eq!(store.load().unwrap().skins, data.skins);
    }

    #[test]
    fn saving_creates_the_data_directory() {
        // The first skin on a fresh house arrives before anything else has had
        // reason to make `data/`.
        let dir = tempfile::tempdir().unwrap();
        let store = SkinStore::new(dir.path().join("data").join("skins.json"));
        store.save(&SkinStoreData::default()).unwrap();
        assert!(store.path.exists());
    }

    #[test]
    fn overrides_survive_a_round_trip() {
        // The free-form half. A typed struct would drop a key an older core did
        // not know about; this is the test that says it does not.
        let dir = tempfile::tempdir().unwrap();
        let store = SkinStore::new(dir.path().join("skins.json"));
        let mut s = skin("hallway");
        s.overrides.insert("accent.warn".into(), "#C8761F".into());
        s.overrides
            .insert("metric.temperature".into(), "#FF8A5B".into());
        store
            .save(&SkinStoreData {
                skins: vec![s.clone()],
            })
            .unwrap();
        assert_eq!(store.load().unwrap().skins[0].overrides, s.overrides);
    }
}
