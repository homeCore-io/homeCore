//! Where the runtime's connection key comes from, and where it is kept.
//!
//! `HC_RUNTIME_KEY` wins when it is set: a compose file or script that sets it
//! has said what this runtime connects to, and the page shows it without
//! offering to change it. Otherwise the key pasted on the page is saved in the
//! data volume, readable only by the runtime's own user, since it holds the
//! runtime's password.

use anyhow::{Context, Result};
use hc_api_types::plugin_runtimes::ConnectionKey;
use std::path::{Path, PathBuf};

pub const ENV: &str = "HC_RUNTIME_KEY";

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("connection.key")
}

/// The key set in the environment, if any. A malformed one is an error rather
/// than ignored: whoever set it meant it, and should hear why it is wrong.
pub fn from_env() -> Result<Option<ConnectionKey>> {
    match std::env::var(ENV) {
        Ok(v) if !v.trim().is_empty() => ConnectionKey::decode(&v)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{ENV}: {e}")),
        _ => Ok(None),
    }
}

/// The key saved from the page, if one was.
pub fn load(data_dir: &Path) -> Result<Option<ConnectionKey>> {
    let p = path(data_dir);
    match std::fs::read_to_string(&p) {
        Ok(text) => ConnectionKey::decode(&text)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(data_dir: &Path, key: &ConnectionKey) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let p = path(data_dir);
    let tmp = p.with_extension("key.new");
    std::fs::write(&tmp, key.encode()).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &p).with_context(|| format!("saving {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ConnectionKey {
        ConnectionKey {
            url: "http://homecore.local:8080".into(),
            broker_host: "homecore.local".into(),
            broker_port: 1883,
            runtime_id: "rt-1".into(),
            username: "plugin.python-rt1".into(),
            secret: "hc_sk_x".into(),
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
        }
    }

    #[test]
    fn a_saved_key_loads_and_only_its_owner_can_read_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        save(dir.path(), &key()).unwrap();
        assert_eq!(load(dir.path()).unwrap(), Some(key()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
