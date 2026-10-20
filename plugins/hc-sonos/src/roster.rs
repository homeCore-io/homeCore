//! Every speaker this plugin has ever registered, by Sonos UUID, kept on disk.
//!
//! **A speaker's homeCore id is decided once and kept.** It used to be worked
//! out from the speaker's name every time the plugin started — so renaming a
//! speaker in the Sonos app and restarting minted a second device, and the
//! first one (with its room and every automation that named it) was deleted
//! as stale. The roster remembers which id each UUID got, so a rename, a
//! restart, a new IP address or a long sleep never changes it.
//!
//! It also lets the plugin say "offline" for a speaker that is asleep when the
//! plugin starts, instead of saying nothing about it, and gives discovery the
//! last address each speaker was seen at.
//!
//! Nothing here ever removes a speaker on its own. `forget_speaker` is the one
//! way out, and somebody has to ask for it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Known {
    pub hc_id: String,
    pub name: String,
    /// `host:port` it last answered at.
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    #[serde(default)]
    pub speakers: BTreeMap<String, Known>,
}

/// `<config dir>/.sonos-speakers.<plugin_id>.json` — beside the config, and
/// per plugin id so two Sonos plugins sharing a folder never share it.
pub fn path_for(config_path: &str, plugin_id: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".sonos-speakers.{plugin_id}.json"))
}

/// A speaker name as an id: `Office-1` → `sonos_office_1`.
pub fn id_from_name(name: &str) -> String {
    let sanitized: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("sonos_{sanitized}")
}

impl Roster {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "Speaker roster unreadable; starting a new one");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Write it, through a temporary file so a crash never leaves half of it.
    pub fn save(&self, path: &Path) {
        let tmp = path.with_extension("json.tmp");
        let write = serde_json::to_vec_pretty(self)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| std::fs::write(&tmp, bytes).map_err(Into::into))
            .and_then(|()| std::fs::rename(&tmp, path).map_err(Into::into));
        if let Err(e) = write {
            warn!(path = %path.display(), error = %e, "Could not save the speaker roster");
        }
    }

    /// The id for a speaker: the one it was given before; else a pinned one
    /// from `[[devices]]`; else one from its name that no other speaker has.
    ///
    /// A pin wins over the roster, so an operator can still move a speaker to
    /// the id their automations use.
    pub fn id_for(&self, uuid: &str, name: &str, pinned: Option<&str>) -> String {
        if let Some(p) = pinned {
            return p.to_string();
        }
        if let Some(k) = self.speakers.get(uuid) {
            return k.hc_id.clone();
        }
        let taken = |id: &str| {
            self.speakers
                .iter()
                .any(|(u, k)| u != uuid && k.hc_id == id)
        };
        let base = id_from_name(name);
        if !taken(&base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}_{n}"))
            .find(|id| !taken(id))
            .expect("an unbounded range always has a free id")
    }

    /// Remember a speaker. `true` when anything changed.
    pub fn remember(&mut self, uuid: &str, known: Known) -> bool {
        if self.speakers.get(uuid) == Some(&known) {
            return false;
        }
        self.speakers.insert(uuid.to_string(), known);
        true
    }

    /// The UUID behind a homeCore id.
    pub fn uuid_of(&self, hc_id: &str) -> Option<&str> {
        self.speakers
            .iter()
            .find(|(_, k)| k.hc_id == hc_id)
            .map(|(u, _)| u.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(id: &str, name: &str) -> Known {
        Known {
            hc_id: id.into(),
            name: name.into(),
            host: None,
        }
    }

    #[test]
    fn a_renamed_speaker_keeps_its_id() {
        let mut r = Roster::default();
        let id = r.id_for("RINCON_A", "Office-1", None);
        assert_eq!(id, "sonos_office_1");
        r.remember("RINCON_A", known(&id, "Office-1"));
        // Renamed "Study" in the Sonos app: same speaker, same id.
        assert_eq!(r.id_for("RINCON_A", "Study", None), "sonos_office_1");
    }

    #[test]
    fn two_speakers_with_one_name_get_two_ids() {
        let mut r = Roster::default();
        r.remember("RINCON_A", known("sonos_office", "Office"));
        assert_eq!(r.id_for("RINCON_B", "Office", None), "sonos_office_2");
        r.remember("RINCON_B", known("sonos_office_2", "Office"));
        assert_eq!(r.id_for("RINCON_C", "Office", None), "sonos_office_3");
    }

    #[test]
    fn a_pin_wins() {
        let mut r = Roster::default();
        r.remember("RINCON_A", known("sonos_office_1", "Office-1"));
        assert_eq!(
            r.id_for("RINCON_A", "Office-1", Some("den_speaker")),
            "den_speaker"
        );
    }

    #[test]
    fn it_survives_a_restart_and_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("hc-sonos-roster-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".sonos-speakers.plugin.sonos.json");
        let mut r = Roster::default();
        r.remember(
            "RINCON_A",
            Known {
                hc_id: "sonos_bathroom".into(),
                name: "Bathroom".into(),
                host: Some("10.0.0.7:1400".into()),
            },
        );
        r.save(&path);
        assert_eq!(Roster::load(&path), r);
        assert_eq!(
            Roster::load(&path).uuid_of("sonos_bathroom"),
            Some("RINCON_A")
        );
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Roster::load(&path), Roster::default());
        assert_eq!(Roster::load(&dir.join("missing.json")), Roster::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_per_plugin_beside_the_config() {
        assert_eq!(
            path_for("/etc/hc/plugins/plugin.sonos.toml", "plugin.sonos"),
            PathBuf::from("/etc/hc/plugins/.sonos-speakers.plugin.sonos.json")
        );
    }
}
