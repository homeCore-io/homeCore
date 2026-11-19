//! Turning an artifact into something that can be launched.
//!
//! One directory per plugin per version, which is how core stores binary plugins
//! too — an upgrade installs alongside rather than over, so a rollback is a
//! placement change rather than a re-download:
//!
//! ```text
//! <data>/plugins/plugin.foo/0.2.1+python-3.13.18/
//! ├── pkg/          the unpacked artifact
//! ├── env/          whatever the adapter's create_env made
//! ├── config.toml   what core sent, verbatim
//! ├── artifact.tar.zst  the package, for an interpreter update to reuse
//! ├── .interpreter  the interpreter env/ was made from, when there is one
//! └── .installed    written last, and only on success
//! <data>/interpreters/python-3.13.18/
//! ├── python/       the unpacked interpreter
//! └── .installed
//! ```
//!
//! A directory is named for the plugin version and the interpreter together,
//! because an environment belongs to the interpreter it was made from. An
//! interpreter update is a new directory beside the old one, made from the
//! wheels the package already carries.
//!
//! The marker is written last on purpose. A venv that was half-built when the
//! container was killed is indistinguishable from a finished one by looking at
//! it, and the failure it causes — an import error at launch — points nowhere
//! near the interrupted install.

use crate::adapter::{self, Adapter};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The extra fields a runtime plugin's `plugin.toml` carries beyond a binary
/// one's. Everything else in the file is core's business, not the host's.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    /// What the adapter's `launch` means by an entrypoint, e.g. `hc_foo.main`.
    pub entrypoint: String,
    /// The distribution name inside the wheelhouse, e.g. `hc-foo`. Optional
    /// because an ecosystem whose install step takes a directory does not need
    /// one; `{package}` and `{plugin_wheel}` are then unavailable, and an
    /// adapter that asks for them fails with the placeholder named.
    #[serde(default)]
    pub package: Option<String>,
    /// The plugin's own screens: a folder in the package, which the plugin
    /// serves itself. Its path is handed to the plugin as `HC_PLUGIN_UI_DIR`.
    #[serde(default)]
    pub ui: Option<String>,
}

/// A plugin installed and ready to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub plugin_id: String,
    pub version: String,
    pub env: PathBuf,
    pub entrypoint: String,
    pub config: PathBuf,
    /// The unpacked UI folder, when the package carries one.
    pub ui_dir: Option<PathBuf>,
}

impl Installed {
    /// Template variables for the adapter's `launch`.
    pub fn launch_vars(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            ("env", self.env.display().to_string()),
            ("entrypoint", self.entrypoint.clone()),
            ("config", self.config.display().to_string()),
        ])
    }

    /// The environment the plugin is started with: where its UI folder is.
    pub fn launch_env(&self) -> Vec<(String, String)> {
        self.ui_dir
            .iter()
            .map(|d| ("HC_PLUGIN_UI_DIR".to_string(), d.display().to_string()))
            .collect()
    }
}

/// The UI folder a manifest names, if it is in the package.
fn ui_dir(pkg: &Path, manifest: &Manifest) -> Option<PathBuf> {
    let ui = manifest.ui.as_deref()?.trim();
    if ui.is_empty() || ui.contains("..") || ui.starts_with('/') {
        return None;
    }
    Some(pkg.join(ui)).filter(|d| d.is_dir())
}

/// Everything belonging to one plugin, across versions.
///
/// The id comes from core, but it becomes a path, so it is checked here rather
/// than trusted: a compromised core is already past this, but a typo in a
/// registry entry should not be able to write outside the data directory.
pub fn plugin_dir(root: &Path, plugin_id: &str) -> Result<PathBuf> {
    check_path_component("plugin id", plugin_id)?;
    Ok(root.join("plugins").join(plugin_id))
}

/// Where one version of one plugin lives.
pub fn version_dir(root: &Path, plugin_id: &str, version: &str) -> Result<PathBuf> {
    check_path_component("version", version)?;
    Ok(plugin_dir(root, plugin_id)?.join(version))
}

fn check_path_component(what: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.starts_with('.')
    {
        bail!("refusing to use {what} `{value}` as a directory name");
    }
    Ok(())
}

/// True when this exact version finished installing.
pub fn is_installed(root: &Path, plugin_id: &str, key: &str) -> bool {
    version_dir(root, plugin_id, key).is_ok_and(|d| d.join(".installed").exists())
}

// ── Interpreters ─────────────────────────────────────────────────────────────

/// Where one interpreter is unpacked.
pub fn interpreter_dir(root: &Path, kind: &str, version: &str) -> Result<PathBuf> {
    check_path_component("interpreter kind", kind)?;
    check_path_component("interpreter version", version)?;
    Ok(root.join("interpreters").join(format!("{kind}-{version}")))
}

/// True when this interpreter finished unpacking.
pub fn is_interpreter_installed(root: &Path, kind: &str, version: &str) -> bool {
    interpreter_dir(root, kind, version).is_ok_and(|d| d.join(".installed").exists())
}

/// Unpack an interpreter archive, marking it installed last.
///
/// Unpacked into a scratch directory and renamed into place, so a host killed
/// halfway leaves something the next attempt clears rather than a directory
/// that looks finished.
pub fn install_interpreter(
    root: &Path,
    kind: &str,
    version: &str,
    archive: &[u8],
) -> Result<PathBuf> {
    let dir = interpreter_dir(root, kind, version)?;
    let scratch = root
        .join("interpreters")
        .join(format!("{kind}-{version}.partial"));
    for stale in [&dir, &scratch] {
        if stale.exists() {
            std::fs::remove_dir_all(stale)
                .with_context(|| format!("clearing {}", stale.display()))?;
        }
    }
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let unpacked = unpack_tar_zst(archive, &scratch).and_then(|()| {
        std::fs::write(scratch.join(".installed"), version)
            .with_context(|| format!("marking {} installed", scratch.display()))?;
        std::fs::rename(&scratch, &dir)
            .with_context(|| format!("moving {} into place", dir.display()))
    });
    if let Err(e) = unpacked {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }
    Ok(dir)
}

/// Remove interpreters no installed plugin directory was made from.
///
/// Returns the names removed. An interpreter is kept while any version
/// directory of any plugin still records it, so a rollback target keeps its
/// interpreter.
pub fn prune_interpreters(root: &Path) -> Vec<String> {
    let mut in_use = std::collections::HashSet::new();
    for plugin in read_dirs(&root.join("plugins")) {
        for version in read_dirs(&plugin) {
            if let Ok(name) = std::fs::read_to_string(version.join(".interpreter")) {
                in_use.insert(name.trim().to_string());
            }
        }
    }
    let mut removed = Vec::new();
    for dir in read_dirs(&root.join("interpreters")) {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if in_use.contains(&name) {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed.push(name),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not remove an unused interpreter")
            }
        }
    }
    removed
}

/// Keep the running install of a plugin and the newest other one, for
/// rollback, and remove the rest. Returns the directory names removed.
///
/// Only finished installs are candidates: a plugin keeps its own files beside
/// them (the Assistant's `data/`), and those are not this function's to touch.
pub fn prune_versions(root: &Path, plugin_id: &str, current: &str) -> Vec<String> {
    let Ok(plugin) = plugin_dir(root, plugin_id) else {
        return Vec::new();
    };
    let mut others: Vec<(std::time::SystemTime, PathBuf)> = read_dirs(&plugin)
        .into_iter()
        .filter(|d| d.join(".installed").is_file())
        .filter(|d| d.file_name().is_some_and(|n| n != current))
        .map(|d| {
            let when = std::fs::metadata(d.join(".installed"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (when, d)
        })
        .collect();
    others.sort_by_key(|o| std::cmp::Reverse(o.0));
    let mut removed = Vec::new();
    for (_, dir) in others.into_iter().skip(1) {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed.push(name),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not remove an old install")
            }
        }
    }
    removed
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// What to install, and where.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub plugin_id: &'a str,
    pub version: &'a str,
    /// The directory name under the plugin's: the version, plus the
    /// interpreter when there is one (see [`install_key`]).
    pub key: &'a str,
    /// The unpacked interpreter, handed to the adapter as `{interpreter}`.
    pub interpreter: Option<&'a Path>,
}

/// The directory name for a plugin version on an interpreter.
pub fn install_key(version: &str, interpreter: Option<(&str, &str)>) -> String {
    match interpreter {
        Some((kind, iv)) => format!("{version}+{kind}-{iv}"),
        None => version.to_string(),
    }
}

/// Unpack, provision an environment, and write the config, with no
/// interpreter: the adapter brings its own. What the tests that are not about
/// interpreters install with.
#[cfg(test)]
pub async fn install(
    root: &Path,
    adapter: &Adapter,
    plugin_id: &str,
    version: &str,
    artifact: &[u8],
    config: &str,
) -> Result<Installed> {
    let target = Target {
        plugin_id,
        version,
        key: version,
        interpreter: None,
    };
    install_target(root, adapter, &target, artifact, config).await
}

/// Unpack, provision an environment, and write the config.
///
/// Any failure removes the version directory. Leaving a partial install behind
/// would make the next reconcile skip it as already present, and a plugin that
/// is missing half its dependencies fails at import time with nothing pointing
/// back to the install that was interrupted.
pub async fn install_target(
    root: &Path,
    adapter: &Adapter,
    target: &Target<'_>,
    artifact: &[u8],
    config: &str,
) -> Result<Installed> {
    let dir = version_dir(root, target.plugin_id, target.key)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing a previous attempt at {}", dir.display()))?;
    }
    match install_into(&dir, adapter, target, artifact, config).await {
        Ok(installed) => Ok(installed),
        Err(e) => {
            if let Err(cleanup) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(dir = %dir.display(), error = %cleanup,
                    "could not remove a failed install; the next attempt will replace it");
            }
            Err(e)
        }
    }
}

async fn install_into(
    dir: &Path,
    adapter: &Adapter,
    target: &Target<'_>,
    artifact: &[u8],
    config: &str,
) -> Result<Installed> {
    let (plugin_id, version) = (target.plugin_id, target.version);
    let pkg = dir.join("pkg");
    std::fs::create_dir_all(&pkg).with_context(|| format!("creating {}", pkg.display()))?;
    unpack_tar_zst(artifact, &pkg)?;
    // Kept so an interpreter update can make a new environment from the same
    // package without downloading it again.
    std::fs::write(dir.join("artifact.tar.zst"), artifact)
        .with_context(|| format!("keeping the artifact in {}", dir.display()))?;

    let manifest = read_manifest(&pkg)?;
    // The artifact says what it is; core said what it asked for. A mismatch
    // means the registry served something other than what was placed, and
    // running it anyway would leave core's records describing a version that is
    // not the one on disk.
    if manifest.id != plugin_id || manifest.version != version {
        bail!(
            "core placed {plugin_id} {version} but the artifact contains {} {}",
            manifest.id,
            manifest.version
        );
    }

    let env = dir.join("env");
    let mut vars: HashMap<&str, String> = HashMap::from([
        ("env", env.display().to_string()),
        ("pkg", pkg.display().to_string()),
        ("wheelhouse", pkg.join("wheelhouse").display().to_string()),
        ("entrypoint", manifest.entrypoint.clone()),
    ]);
    if let Some(interp) = target.interpreter {
        vars.insert("interpreter", interp.display().to_string());
    }
    if let Some(package) = &manifest.package {
        vars.insert("package", package.clone());
        vars.insert(
            "plugin_wheel",
            find_wheel(&pkg.join("wheelhouse"), package)?
                .display()
                .to_string(),
        );
    }

    run(&adapter::render(&adapter.create_env, &vars)?, "create_env").await?;
    run(&adapter::render(&adapter.install, &vars)?, "install").await?;

    let config_path = dir.join("config.toml");
    write_config(&config_path, config)?;

    if let Some(interp) = target.interpreter.and_then(Path::file_name) {
        std::fs::write(
            dir.join(".interpreter"),
            interp.to_string_lossy().as_bytes(),
        )
        .with_context(|| format!("recording the interpreter of {}", dir.display()))?;
    }

    // Last, and only now.
    std::fs::write(dir.join(".installed"), version)
        .with_context(|| format!("marking {} installed", dir.display()))?;

    Ok(Installed {
        plugin_id: plugin_id.to_string(),
        version: version.to_string(),
        env,
        ui_dir: ui_dir(&pkg, &manifest),
        entrypoint: manifest.entrypoint,
        config: config_path,
    })
}

/// Rebuild an `Installed` for a version already on disk, without reinstalling.
///
/// The manifest is re-read rather than remembered: the host restarts far more
/// often than a plugin is installed, and a cached entrypoint would be one more
/// thing that can disagree with what is actually there.
pub fn existing(root: &Path, plugin_id: &str, key: &str) -> Result<Installed> {
    let dir = version_dir(root, plugin_id, key)?;
    let pkg = dir.join("pkg");
    let manifest = read_manifest(&pkg)?;
    Ok(Installed {
        plugin_id: plugin_id.to_string(),
        version: manifest.version.clone(),
        env: dir.join("env"),
        ui_dir: ui_dir(&pkg, &manifest),
        entrypoint: manifest.entrypoint,
        config: dir.join("config.toml"),
    })
}

/// Write the config core sent, if it is not already exactly that.
///
/// Core owns a plugin's config and may change it without the version changing,
/// so this runs on every reconcile. Returns true when the file changed, which is
/// what tells the caller a running plugin needs restarting — plugins read their
/// config at startup.
pub fn write_config(path: &Path, config: &str) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == config) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, config).with_context(|| format!("writing {}", path.display()))?;
    // It holds a minted broker password.
    restrict(path)?;
    Ok(true)
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}
#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

fn read_manifest(pkg: &Path) -> Result<Manifest> {
    let path = pkg.join("plugin.toml");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} — a runtime artifact must contain plugin.toml at its root",
            path.display()
        )
    })?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Find the plugin's own wheel among its dependencies.
///
/// pip normalises distribution names into wheel filenames: `hc-foo` becomes
/// `hc_foo-0.2.1-py3-none-any.whl`. Matching on the normalised prefix is what
/// makes a `plugin.toml` that names the package the way PyPI does work anyway.
fn find_wheel(wheelhouse: &Path, package: &str) -> Result<PathBuf> {
    let normalised = package.replace(['-', '.'], "_").to_lowercase();
    let entries = std::fs::read_dir(wheelhouse).with_context(|| {
        format!(
            "reading {} — the artifact ships its dependencies as wheels",
            wheelhouse.display()
        )
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(dist) = name.split('-').next() {
            if dist.to_lowercase() == normalised && name.ends_with(".whl") {
                return Ok(entry.path());
            }
        }
        names.push(name);
    }
    names.sort();
    bail!(
        "no wheel for `{package}` in {} — it holds {}",
        wheelhouse.display(),
        if names.is_empty() {
            "nothing".to_string()
        } else {
            names.join(", ")
        }
    )
}

fn unpack_tar_zst(bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder =
        zstd::stream::read::Decoder::new(bytes).context("initialising the zstd decoder")?;
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("reading the artifact's entries")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Defence in depth; tar's unpack_in already refuses these.
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("artifact entry escapes its root: {}", path.display());
        }
        entry.unpack_in(dest)?;
    }
    Ok(())
}

/// Run one of the adapter's commands, failing with what it printed.
///
/// Output is captured rather than inherited so a pip resolution failure reaches
/// the operator as the reason this plugin did not install, rather than as a
/// wall of text interleaved with every other plugin installing at the same time.
async fn run(argv: &[String], what: &str) -> Result<()> {
    let (program, args) = argv.split_first().expect("adapter commands are non-empty");
    tracing::info!(command = %argv.join(" "), "running the adapter's {what}");

    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running `{program}` for {what}"))?;
    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    bail!(
        "{what} failed ({}): {}",
        out.status,
        tail(detail.trim(), 2000)
    )
}

/// Keep the end of a long output — the reason a command failed is at the
/// bottom, under everything it did successfully first.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let start = s
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= start)
        .unwrap_or(s.len());
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn adapter(create_env: &[&str], install: &[&str]) -> Adapter {
        Adapter {
            kind: "python".into(),
            abi: "cp312".into(),
            create_env: create_env.iter().map(|s| s.to_string()).collect(),
            install: install.iter().map(|s| s.to_string()).collect(),
            launch: vec!["true".into(), "{config}".into()],
        }
    }

    /// Build a `.tar.zst` the way the pipeline will.
    fn artifact(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        {
            let mut builder = tar::Builder::new(&mut enc);
            for (name, body) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut out = enc.finish().unwrap();
        out.flush().unwrap();
        out
    }

    fn plugin_toml(id: &str, version: &str) -> Vec<u8> {
        format!(
            r#"
id         = "{id}"
name       = "Foo"
version    = "{version}"
runtime    = "python"
abi        = "cp312-manylinux_2_28"
arch       = "x86_64"
entrypoint = "hc_foo.main"
package    = "hc-foo"
"#
        )
        .into_bytes()
    }

    fn foo_artifact() -> Vec<u8> {
        artifact(&[
            ("plugin.toml", plugin_toml("plugin.foo", "0.2.1").as_slice()),
            ("wheelhouse/hc_foo-0.2.1-py3-none-any.whl", b"wheel"),
            ("wheelhouse/aiohttp-3.9.5-py3-none-any.whl", b"dep"),
        ])
    }

    #[tokio::test]
    async fn a_plugin_installs_and_is_launchable() {
        let root = tempfile::tempdir().unwrap();
        let a = adapter(&["true", "{env}"], &["true", "{plugin_wheel}"]);

        let installed = install(
            root.path(),
            &a,
            "plugin.foo",
            "0.2.1",
            &foo_artifact(),
            "id = \"plugin.foo\"\n",
        )
        .await
        .expect("install");

        assert_eq!(installed.entrypoint, "hc_foo.main");
        assert!(is_installed(root.path(), "plugin.foo", "0.2.1"));
        assert_eq!(
            std::fs::read_to_string(&installed.config).unwrap(),
            "id = \"plugin.foo\"\n"
        );

        // And it can be rebuilt on the next boot without reinstalling.
        let again = existing(root.path(), "plugin.foo", "0.2.1").unwrap();
        assert_eq!(again, installed);
    }

    /// The whole reason the marker is written last: a half-built environment
    /// that reads as installed produces an import error at launch, a long way
    /// from the install that was interrupted.
    #[tokio::test]
    async fn a_failed_install_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let a = adapter(&["true", "{env}"], &["false"]);

        let err = install(
            root.path(),
            &a,
            "plugin.foo",
            "0.2.1",
            &foo_artifact(),
            "x = 1",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("install failed"), "{err}");
        assert!(!is_installed(root.path(), "plugin.foo", "0.2.1"));
        assert!(
            !version_dir(root.path(), "plugin.foo", "0.2.1")
                .unwrap()
                .exists(),
            "a failed install must not leave a directory the next reconcile skips"
        );
    }

    /// What a command printed is the only useful thing about a failure, and pip
    /// puts the reason at the end.
    #[tokio::test]
    async fn a_failing_command_reports_its_output() {
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                "echo 'no matching distribution' >&2; exit 1".into(),
            ],
            "install",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no matching distribution"),
            "{err}"
        );
    }

    /// Serving a different version than the one placed would leave core's
    /// records describing something other than what is on disk.
    #[tokio::test]
    async fn an_artifact_that_is_not_what_was_placed_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let a = adapter(&["true"], &["true"]);
        let err = install(
            root.path(),
            &a,
            "plugin.foo",
            "0.3.0",
            &foo_artifact(),
            "x = 1",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("0.2.1"), "{err}");
        assert!(err.to_string().contains("0.3.0"), "{err}");
    }

    #[test]
    fn a_wheel_is_found_by_its_distribution_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hc_foo-0.2.1-py3-none-any.whl"), b"w").unwrap();
        // Named with dashes in plugin.toml, with underscores on disk.
        let found = find_wheel(dir.path(), "hc-foo").unwrap();
        assert!(found.ends_with("hc_foo-0.2.1-py3-none-any.whl"));
    }

    /// A wheelhouse missing the plugin's own wheel is a build mistake, and the
    /// message has to show what *was* shipped or there is nothing to go on.
    #[test]
    fn a_missing_wheel_lists_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aiohttp-3.9.5-py3-none-any.whl"), b"w").unwrap();
        let err = find_wheel(dir.path(), "hc-foo").unwrap_err();
        assert!(err.to_string().contains("aiohttp"), "{err}");
    }

    #[test]
    fn a_plugin_is_told_where_the_ui_folder_in_its_package_is() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path();
        std::fs::create_dir_all(pkg.join("ui")).unwrap();
        let m = |ui: Option<&str>| Manifest {
            id: "plugin.foo".into(),
            version: "1.0.0".into(),
            entrypoint: "hc_foo.main".into(),
            package: None,
            ui: ui.map(str::to_string),
        };
        assert_eq!(ui_dir(pkg, &m(Some("ui"))), Some(pkg.join("ui")));
        assert_eq!(ui_dir(pkg, &m(None)), None);
        assert_eq!(ui_dir(pkg, &m(Some("missing"))), None);
        assert_eq!(ui_dir(pkg, &m(Some("../ui"))), None);
        let installed = Installed {
            plugin_id: "plugin.foo".into(),
            version: "1.0.0".into(),
            env: pkg.join("env"),
            entrypoint: "hc_foo.main".into(),
            config: pkg.join("config.toml"),
            ui_dir: Some(pkg.join("ui")),
        };
        assert_eq!(
            installed.launch_env(),
            vec![(
                "HC_PLUGIN_UI_DIR".to_string(),
                pkg.join("ui").display().to_string()
            )]
        );
    }

    #[test]
    fn pruning_keeps_the_current_install_the_one_before_and_the_plugins_own_files() {
        let root = tempfile::tempdir().unwrap();
        let plugin = plugin_dir(root.path(), "plugin.foo").unwrap();
        for (i, key) in ["0.1.0", "0.2.0", "0.3.0", "0.4.0"].iter().enumerate() {
            let d = plugin.join(key);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".installed"), key).unwrap();
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::File::options()
                .write(true)
                .open(d.join(".installed"))
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        std::fs::create_dir_all(plugin.join("data")).unwrap();
        std::fs::write(plugin.join("data/history.json"), "[]").unwrap();

        let mut removed = prune_versions(root.path(), "plugin.foo", "0.2.0");
        removed.sort();
        assert_eq!(removed, vec!["0.1.0", "0.3.0"]);
        assert!(plugin.join("0.2.0").exists(), "the running one stays");
        assert!(
            plugin.join("0.4.0").exists(),
            "the newest other one stays for rollback"
        );
        assert!(
            plugin.join("data/history.json").exists(),
            "the plugin's own files stay"
        );
    }

    #[test]
    fn a_plugin_id_that_is_a_path_is_refused() {
        let root = tempfile::tempdir().unwrap();
        for bad in ["../escape", "a/b", ".hidden"] {
            assert!(
                version_dir(root.path(), bad, "1.0.0").is_err(),
                "{bad} must not become a directory"
            );
        }
    }

    /// Core may change a plugin's config without its version changing, and a
    /// plugin only reads it at startup — so the caller needs to know.
    #[test]
    fn a_changed_config_is_reported_and_an_identical_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert!(write_config(&p, "a = 1").unwrap());
        assert!(!write_config(&p, "a = 1").unwrap(), "no change, no restart");
        assert!(write_config(&p, "a = 2").unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a = 2");
    }

    /// It holds a minted broker password.
    #[cfg(unix)]
    #[test]
    fn the_config_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        write_config(&p, "password = \"s\"").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        let s = format!("{}the actual error", "noise ".repeat(1000));
        let t = tail(&s, 100);
        assert!(t.ends_with("the actual error"));
        assert!(t.starts_with('…'));
    }
}
