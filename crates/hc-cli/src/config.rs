//! `hc-cli` config file — lives at `~/.config/hc-cli/config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the admin UDS. Defaults to `/run/homecore/admin.sock`.
    #[serde(default = "default_uds_path")]
    pub uds_path: PathBuf,
    /// Base URL for TCP fallback. Defaults to `http://127.0.0.1:8080`.
    #[serde(default = "default_tcp_url")]
    pub tcp_url: String,
    /// Saved credentials for TCP remote use.
    #[serde(default)]
    pub credentials: Option<StoredCredentials>,
    /// Output format — `human` (default), `json`.
    #[serde(default = "default_output")]
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub host: String,
    /// API key (`hc_sk_...`) preferred; JWT tokens are shorter-lived and
    /// will invalidate across core restarts.
    pub token: String,
}

fn default_uds_path() -> PathBuf {
    PathBuf::from("/run/homecore/admin.sock")
}
fn default_tcp_url() -> String {
    "http://127.0.0.1:8080".into()
}
fn default_output() -> String {
    "human".into()
}

impl Config {
    /// Default path: `~/.config/hc-cli/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("hc-cli").join("config.toml"))
    }

    /// Load from the given path; returns default config if the file does not
    /// exist (a fresh install has no config yet).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str::<Config>(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Atomically write the config to `path`, creating parent dirs with 0700
    /// and the file with 0600.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }

        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("opening {}", tmp.display()))?;
            let text = toml::to_string_pretty(self).context("serialising config")?;
            f.write_all(text.as_bytes())
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

impl Default for Config {
    // Manual default so the serde default fns get applied.
    fn default() -> Self {
        Self {
            uds_path: default_uds_path(),
            tcp_url: default_tcp_url(),
            credentials: None,
            output: default_output(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn loads_default_when_missing() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("missing.toml");
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.uds_path, PathBuf::from("/run/homecore/admin.sock"));
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        let cfg = Config {
            output: "json".into(),
            ..Config::default()
        };
        cfg.save(&p).unwrap();

        let got = Config::load(&p).unwrap();
        assert_eq!(got.output, "json");
    }

    #[test]
    fn saved_config_is_0600() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("inner").join("config.toml");
        Config::default().save(&p).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
