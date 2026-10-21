//! What the plugin remembers across restarts that its settings do not say:
//! each scene's device id (so a rename keeps it), and Pico layouts learned
//! from presses. A small JSON file beside the config, like the published
//! device ids; losing it costs nothing worse than a re-learn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// `bridge_id:button` → the scene's device id.
    #[serde(default)]
    pub scene_ids: BTreeMap<String, String>,
    /// Pico device id → the buttons a press showed it has.
    #[serde(default)]
    pub learned_buttons: BTreeMap<String, Learned>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub buttons: Vec<u32>,
    /// The configured `buttons` it was learned over. When the operator sets
    /// them to something else, theirs wins and this is forgotten.
    #[serde(default)]
    pub over: Vec<u32>,
}

impl Memory {
    pub fn path_for(config_path: &str) -> PathBuf {
        Path::new(config_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".caseta-memory.json")
    }

    /// The file's contents, or nothing remembered when it is missing or
    /// unreadable.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Ignoring an unreadable memory file");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) {
        let text = match serde_json::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => return warn!(error = %e, "Could not serialise the memory file"),
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path)) {
            warn!(path = %path.display(), error = %e, "Could not write the memory file");
        }
    }

    /// The learned layout for a Pico configured with `configured`, if one
    /// still applies.
    pub fn learned_for(&self, hc_id: &str, configured: &[u32]) -> Option<Vec<u32>> {
        let l = self.learned_buttons.get(hc_id)?;
        let mut a = l.over.clone();
        let mut b = configured.to_vec();
        a.sort_unstable();
        b.sort_unstable();
        (a == b).then(|| l.buttons.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_round_trips_and_a_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("caseta-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".caseta-memory.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Memory::load(&path), Memory::default());

        let mut m = Memory::default();
        m.scene_ids
            .insert("1:3".into(), "caseta_scene_sonos_play".into());
        m.learned_buttons.insert(
            "caseta_9".into(),
            Learned {
                buttons: vec![8, 9, 10, 11],
                over: vec![],
            },
        );
        m.save(&path);
        assert_eq!(Memory::load(&path), m);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Memory::load(&path), Memory::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Learned over nothing configured; once the operator lists buttons,
    /// theirs win.
    #[test]
    fn settings_beat_what_was_learned() {
        let mut m = Memory::default();
        m.learned_buttons.insert(
            "caseta_9".into(),
            Learned {
                buttons: vec![8, 9, 10, 11],
                over: vec![],
            },
        );
        assert_eq!(m.learned_for("caseta_9", &[]), Some(vec![8, 9, 10, 11]));
        assert_eq!(m.learned_for("caseta_9", &[2, 4]), None);
        assert_eq!(m.learned_for("caseta_6", &[]), None);
    }
}
