//! What this runtime is supposed to be running.
//!
//! In the finished system core holds the desired state and the runtime
//! reconciles against it. Until placement over MQTT exists, a `placements.toml`
//! in the data directory says the same thing locally, so the supervision path
//! can be exercised without the registry.
//!
//! Deliberately the same shape the reconcile will take — a list of plugin ids
//! with an environment, an entrypoint and a config path — so replacing the
//! source later changes where the list comes from and nothing else.
//!
//! ```toml
//! [[plugin]]
//! id         = "plugin.example"
//! env        = "/var/lib/hc-runtime/envs/example"
//! entrypoint = "hc_example.main"
//! config     = "/var/lib/hc-runtime/config/example.toml"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub const FILE: &str = "placements.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    /// The plugin id homeCore knows it by. Also what a crash-loop notice names.
    pub id: String,
    /// Environment the adapter created for it.
    pub env: String,
    /// Whatever the adapter's `launch` means by an entrypoint.
    pub entrypoint: String,
    /// Config file, handed to the plugin as `argv[1]`.
    pub config: String,
}

#[derive(Debug, Default, Deserialize)]
struct PlacementFile {
    #[serde(default, rename = "plugin")]
    plugins: Vec<Placement>,
}

/// Read the placements, or an empty list when the file is absent.
///
/// Absent is the normal state for a freshly approved runtime, not an error — it
/// hosts nothing until something is placed on it.
pub fn load(dir: &Path) -> Result<Vec<Placement>> {
    let path = dir.join(FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading placements at {}", path.display()))?;
    let parsed: PlacementFile = toml::from_str(&text)
        .with_context(|| format!("parsing placements at {}", path.display()))?;
    Ok(parsed.plugins)
}

impl Placement {
    /// Template variables for the adapter's command templates.
    pub fn vars(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            ("env", self.env.clone()),
            ("entrypoint", self.entrypoint.clone()),
            ("config", self.config.clone()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime with nothing placed on it is the normal state right after
    /// approval, so a missing file must not read as a failure to start.
    #[test]
    fn no_file_means_nothing_hosted() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn placements_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE),
            r#"
[[plugin]]
id         = "plugin.example"
env        = "/envs/example"
entrypoint = "hc_example.main"
config     = "/cfg/example.toml"
"#,
        )
        .unwrap();

        let list = load(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "plugin.example");

        let vars = list[0].vars();
        assert_eq!(vars["config"], "/cfg/example.toml");
        assert_eq!(vars["entrypoint"], "hc_example.main");
    }

    /// A malformed file is an error rather than an empty list: silently hosting
    /// nothing because of a typo looks identical to a runtime that was never
    /// given anything.
    #[test]
    fn a_broken_file_is_loud() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "this is not toml {{{").unwrap();
        assert!(load(dir.path()).is_err());
    }
}
