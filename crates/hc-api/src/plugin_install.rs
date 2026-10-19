//! Plugin **install pipeline** (Phase B). Unpacks a `.tar.zst` artifact, installs
//! the binary under `HOMECORE_HOME/plugins/<id>/<version>/`, mints an MQTT
//! credential + seeds operator config, and produces a managed-plugin record for
//! the caller to persist. Activation is on the next core start (dynamic spawn is
//! a follow-up).
//!
//! In Phase B the artifact is a local path; the remote registry (Phase C) hands
//! this the same on-disk `.tar.zst`, so the pipeline is unchanged.
//!
//! Artifact layout (`.tar.zst`):
//! ```text
//!   plugin.toml        # the manifest below
//!   hc-<name>          # the plugin binary (name from manifest.binary)
//!   config.default.toml  # optional seed operator config (manifest.default_config)
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::managed_plugins::ManagedRecord;
use crate::plugin_config_store::PluginConfigStore;

/// A request to spawn a just-installed plugin into the running supervisor
/// (dynamic activation — no restart). The install handler sends this to a
/// listener in `main.rs` that owns the supervisor handles (which live in the
/// binary crate, out of reach of hc-api).
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub id: String,
    pub binary: String,
    pub config: String,
    pub enabled: bool,
}

/// The `plugin.toml` manifest inside an artifact.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Binary filename within the archive.
    pub binary: String,
    /// Optional seed operator-config filename within the archive.
    #[serde(default)]
    pub default_config: Option<String>,
    /// The plugin's own screens: a folder within the archive, installed
    /// beside the binary as `ui/`, where the SDK serves them from.
    #[serde(default)]
    pub ui: Option<String>,
}

/// Where + how core installs plugins. Built once from config in `main.rs` and
/// held on `AppState`.
#[derive(Clone)]
pub struct InstallContext {
    /// `HOMECORE_HOME/plugins` — install root for binaries.
    pub plugins_dir: PathBuf,
    /// `HOMECORE_HOME/config/plugins` — the central operator-config store.
    pub config_plugins_dir: PathBuf,
    /// Broker coordinates written into the generated `[homecore]` bootstrap.
    pub broker_host: String,
    pub broker_port: u16,
}

/// The result of an install — the record to persist + whether existing operator
/// config was preserved (reinstall).
pub struct InstallOutcome {
    pub record: ManagedRecord,
    pub reinstall: bool,
}

/// Install a plugin from a local `.tar.zst`. Pure of the managed store (caller
/// persists `record` via `ManagedPluginStore::install`) so it stays testable.
pub fn install_from_archive(archive: &Path, ctx: &InstallContext) -> Result<InstallOutcome> {
    // 1. Decompress + unpack to a temp dir.
    let tmp = tempfile::tempdir().context("creating temp dir for install")?;
    unpack_tar_zst(archive, tmp.path())
        .with_context(|| format!("unpacking {}", archive.display()))?;

    // 2. Read + validate the manifest.
    let manifest: PluginManifest = toml::from_str(
        &std::fs::read_to_string(tmp.path().join("plugin.toml"))
            .context("reading plugin.toml from artifact")?,
    )
    .context("parsing plugin.toml")?;
    if manifest.id.trim().is_empty() {
        bail!("plugin.toml: `id` is required");
    }
    let version = if manifest.version.trim().is_empty() {
        "0.0.0".to_string()
    } else {
        manifest.version.clone()
    };

    // 3. Install the binary under plugins/<id>/<version>/.
    let src_bin = tmp.path().join(&manifest.binary);
    if !src_bin.is_file() {
        bail!("artifact is missing its binary `{}`", manifest.binary);
    }
    let install_dir = ctx.plugins_dir.join(&manifest.id).join(&version);
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let dst_bin = install_dir.join(&manifest.binary);
    // Stage to a sibling temp file and atomically rename over the destination.
    // A plain copy over dst_bin fails with ETXTBSY ("Text file busy") when that
    // binary is the currently-running plugin — i.e. an in-place upgrade of a
    // live plugin. rename() swaps the directory entry instead: the running
    // process keeps the old, now-unlinked inode and the next launch picks up
    // the new binary. Same directory, so the rename stays on one filesystem.
    let tmp_bin = install_dir.join(format!(".{}.new", manifest.binary));
    std::fs::copy(&src_bin, &tmp_bin)
        .with_context(|| format!("staging plugin binary at {}", tmp_bin.display()))?;
    make_executable(&tmp_bin)?;
    std::fs::rename(&tmp_bin, &dst_bin).context("installing plugin binary")?;

    // 3b. Its screens, beside it — staged, then swapped in whole, so a
    //     reinstall never leaves a mix of two builds' files.
    if let Some(ui) = manifest.ui.as_deref().filter(|u| !u.trim().is_empty()) {
        let src_ui = tmp.path().join(ui);
        if !src_ui.is_dir() {
            bail!("artifact is missing its UI folder `{ui}`");
        }
        let staged = install_dir.join(".ui.new");
        let _ = std::fs::remove_dir_all(&staged);
        copy_dir(&src_ui, &staged).context("staging the plugin's UI")?;
        let dst_ui = install_dir.join("ui");
        let _ = std::fs::remove_dir_all(&dst_ui);
        std::fs::rename(&staged, &dst_ui).context("installing the plugin's UI")?;
    }

    // 4. Seed operator config — only when none exists, so a reinstall never
    //    clobbers operator edits. Injects the [homecore] bootstrap + a freshly
    //    minted MQTT password ("generate the API key on install").
    let config_store = PluginConfigStore::new(ctx.config_plugins_dir.clone());
    let reinstall = config_store.exists(&manifest.id);
    if !reinstall {
        let seed = match &manifest.default_config {
            Some(f) => std::fs::read_to_string(tmp.path().join(f)).unwrap_or_default(),
            None => String::new(),
        };
        let password = uuid::Uuid::new_v4().simple().to_string();
        let bootstrap = format!(
            "[homecore]\nbroker_host = \"{}\"\nbroker_port = {}\nplugin_id = \"{}\"\npassword = \"{}\"\n\n",
            ctx.broker_host, ctx.broker_port, manifest.id, password,
        );
        config_store
            .write(&manifest.id, &format!("{bootstrap}{seed}"))
            .context("writing seeded operator config")?;
    }

    let record = ManagedRecord {
        id: manifest.id.clone(),
        name: if manifest.name.trim().is_empty() {
            manifest.id.clone()
        } else {
            manifest.name.clone()
        },
        source: "local".into(),
        version,
        binary: dst_bin.to_string_lossy().into_owned(),
        config: config_store
            .path_for(&manifest.id)
            .to_string_lossy()
            .into_owned(),
        enabled: true,
        installed_at: String::new(), // stamped by the caller (no clock here)
    };

    Ok(InstallOutcome { record, reinstall })
}

fn unpack_tar_zst(archive: &Path, dest: &Path) -> Result<()> {
    let f =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let decoder = zstd::stream::read::Decoder::new(f).context("initialising zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("reading tar entries")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Defence in depth (tar's unpack_in already refuses these).
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!(
                "artifact entry escapes the archive root: {}",
                path.display()
            );
        }
        entry.unpack_in(dest)?;
    }
    Ok(())
}

/// Copy a folder's regular files and folders (never a link) into `to`.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let kind = e.file_type()?;
        let dst = to.join(e.file_name());
        if kind.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else if kind.is_file() {
            std::fs::copy(e.path(), &dst)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack a minimal artifact (`plugin.toml` + binary + default config) into a
    /// real `.tar.zst`, returning its path.
    fn make_fixture(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("plugin.toml"),
            "id = \"plugin.demo\"\nname = \"Demo\"\nversion = \"1.2.3\"\n\
             binary = \"hc-demo\"\ndefault_config = \"config.default.toml\"\n",
        )
        .unwrap();
        std::fs::write(src.join("hc-demo"), b"#!/bin/sh\necho demo\n").unwrap();
        std::fs::write(src.join("config.default.toml"), "[demo]\nfoo = 1\n").unwrap();

        let archive = dir.join("demo.tar.zst");
        let f = std::fs::File::create(&archive).unwrap();
        let enc = zstd::stream::write::Encoder::new(f, 0).unwrap();
        let mut tar = tar::Builder::new(enc);
        for name in ["plugin.toml", "hc-demo", "config.default.toml"] {
            tar.append_path_with_name(src.join(name), name).unwrap();
        }
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();
        archive
    }

    fn ctx(root: &Path) -> InstallContext {
        InstallContext {
            plugins_dir: root.join("plugins"),
            config_plugins_dir: root.join("config").join("plugins"),
            broker_host: "127.0.0.1".into(),
            broker_port: 1883,
        }
    }

    #[test]
    fn a_plugin_s_ui_folder_is_installed_beside_it_and_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("ui/assets")).unwrap();
        std::fs::write(
            src.join("plugin.toml"),
            "id = \"plugin.demo\"\nversion = \"1.2.3\"\nbinary = \"hc-demo\"\nui = \"ui\"\n",
        )
        .unwrap();
        std::fs::write(src.join("hc-demo"), b"#!/bin/sh\n").unwrap();
        std::fs::write(src.join("ui/manifest.json"), "{}").unwrap();
        std::fs::write(src.join("ui/assets/a.css"), "a{}").unwrap();
        let archive = tmp.path().join("demo.tar.zst");
        let f = std::fs::File::create(&archive).unwrap();
        let mut tar = tar::Builder::new(zstd::stream::write::Encoder::new(f, 0).unwrap());
        tar.append_dir_all(".", &src).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let ctx = ctx(tmp.path());
        let dir = ctx.plugins_dir.join("plugin.demo").join("1.2.3");
        // Something left from before: gone after the install.
        std::fs::create_dir_all(dir.join("ui")).unwrap();
        std::fs::write(dir.join("ui/old.js"), "x").unwrap();

        install_from_archive(&archive, &ctx).unwrap();
        assert!(dir.join("ui/manifest.json").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join("ui/assets/a.css")).unwrap(),
            "a{}"
        );
        assert!(!dir.join("ui/old.js").exists());
        assert!(!dir.join(".ui.new").exists());
    }

    #[test]
    fn install_unpacks_binary_seeds_config_and_builds_record() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make_fixture(tmp.path());
        let ctx = ctx(tmp.path());

        let out = install_from_archive(&archive, &ctx).unwrap();
        assert!(!out.reinstall);
        assert_eq!(out.record.id, "plugin.demo");
        assert_eq!(out.record.name, "Demo");
        assert_eq!(out.record.version, "1.2.3");
        assert_eq!(out.record.source, "local");

        // Binary installed under plugins/<id>/<version>/.
        let bin = tmp.path().join("plugins/plugin.demo/1.2.3/hc-demo");
        assert!(bin.is_file(), "binary should be installed");

        // Config seeded: bootstrap block + minted password + the default config.
        let cfg =
            std::fs::read_to_string(tmp.path().join("config/plugins/plugin.demo.toml")).unwrap();
        assert!(cfg.contains("[homecore]"));
        assert!(cfg.contains("plugin_id = \"plugin.demo\""));
        assert!(cfg.contains("password = \""), "a credential is minted");
        assert!(cfg.contains("[demo]"), "default_config is merged in");
    }

    #[test]
    fn reinstall_preserves_operator_config() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make_fixture(tmp.path());
        let ctx = ctx(tmp.path());

        install_from_archive(&archive, &ctx).unwrap();
        // Simulate an operator edit, then reinstall.
        let cfg_path = tmp.path().join("config/plugins/plugin.demo.toml");
        std::fs::write(&cfg_path, "edited = true\n").unwrap();

        let out = install_from_archive(&archive, &ctx).unwrap();
        assert!(out.reinstall);
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            "edited = true\n",
            "reinstall must not clobber operator config"
        );
    }
}
