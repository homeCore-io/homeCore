//! What differs per language, expressed as data.
//!
//! The host knows how to enroll, register, supervise and report. It knows nothing
//! about Python. Everything language-specific is four command templates baked
//! into the image as `adapter.toml`, so a Node or .NET runtime is a new base
//! image rather than a new program:
//!
//! ```toml
//! kind = "python"
//! abi  = "cp312-manylinux_2_28"
//!
//! create_env = ["python3", "-m", "venv", "{env}"]
//! install    = ["{env}/bin/pip", "install", "--no-index",
//!               "--find-links", "{wheelhouse}", "{plugin_wheel}"]
//! launch     = ["{env}/bin/python", "-m", "{entrypoint}", "{config}"]
//! ```
//!
//! Placeholders are substituted verbatim into argv. They never go near a shell,
//! so a path with a space is a path with a space rather than two arguments.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Adapter {
    /// Runtime kind, e.g. `python`. Advertised at enrollment.
    pub kind: String,
    /// ABI tag artifacts must match, e.g. `cp312-manylinux_2_28`.
    pub abi: String,
    /// Create an isolated environment for one plugin.
    pub create_env: Vec<String>,
    /// Install an artifact into that environment.
    pub install: Vec<String>,
    /// Run the plugin. Must end with the config path, which every plugin
    /// receives as `argv[1]` regardless of language.
    pub launch: Vec<String>,
}

impl Adapter {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading adapter at {}", path.display()))?;
        let adapter: Adapter = toml::from_str(&text)
            .with_context(|| format!("parsing adapter at {}", path.display()))?;
        adapter.validate()?;
        Ok(adapter)
    }

    fn validate(&self) -> Result<()> {
        if self.kind.trim().is_empty() {
            bail!("adapter `kind` must not be empty");
        }
        if self.abi.trim().is_empty() {
            bail!("adapter `abi` must not be empty");
        }
        for (name, argv) in [
            ("create_env", &self.create_env),
            ("install", &self.install),
            ("launch", &self.launch),
        ] {
            if argv.is_empty() {
                bail!("adapter `{name}` must name a program to run");
            }
        }
        // A plugin that is never handed its config path cannot read its
        // credentials, and the failure would look like a broker auth problem
        // rather than a missing argument.
        if !self.launch.iter().any(|a| a.contains("{config}")) {
            bail!("adapter `launch` must pass {{config}} — every plugin reads its config from argv[1]");
        }
        Ok(())
    }
}

/// Substitute `{placeholder}` values into an argv template.
///
/// Unknown placeholders are an error rather than being passed through: a literal
/// `{wheelhouse}` reaching a shell-less `exec` becomes a file named
/// `{wheelhouse}`, and the resulting "no such file" is a long way from the typo
/// that caused it.
pub fn render(argv: &[String], vars: &HashMap<&str, String>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(argv.len());
    for arg in argv {
        let mut rendered = arg.clone();
        for (key, value) in vars {
            rendered = rendered.replace(&format!("{{{key}}}"), value);
        }
        if let Some(start) = rendered.find('{') {
            if let Some(end) = rendered[start..].find('}') {
                let unknown = &rendered[start..=start + end];
                bail!("adapter used an unknown placeholder {unknown} in `{arg}`");
            }
        }
        out.push(rendered);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("adapter.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    const PYTHON: &str = r#"
kind = "python"
abi  = "cp312-manylinux_2_28"
create_env = ["python3", "-m", "venv", "{env}"]
install    = ["{env}/bin/pip", "install", "--no-index", "--find-links", "{wheelhouse}", "{plugin_wheel}"]
launch     = ["{env}/bin/python", "-m", "{entrypoint}", "{config}"]
"#;

    #[test]
    fn a_python_adapter_loads() {
        let dir = tempfile::tempdir().unwrap();
        let a = Adapter::load(&write(dir.path(), PYTHON)).unwrap();
        assert_eq!(a.kind, "python");
        assert_eq!(a.abi, "cp312-manylinux_2_28");
    }

    /// The point of the design: a different language is a different file, not a
    /// different program.
    #[test]
    fn a_node_adapter_loads_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let a = Adapter::load(&write(
            dir.path(),
            r#"
kind = "node"
abi  = "node22"
create_env = ["mkdir", "-p", "{env}"]
install    = ["npm", "ci", "--offline", "--prefix", "{env}"]
launch     = ["node", "{env}/index.js", "{config}"]
"#,
        ))
        .unwrap();
        assert_eq!(a.kind, "node");
    }

    /// A launch template that forgets the config path produces a plugin with no
    /// credentials, which surfaces as a broker auth failure — nowhere near the
    /// cause.
    #[test]
    fn a_launch_without_the_config_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = Adapter::load(&write(
            dir.path(),
            r#"
kind = "python"
abi  = "cp312"
create_env = ["python3", "-m", "venv", "{env}"]
install    = ["true"]
launch     = ["{env}/bin/python", "-m", "{entrypoint}"]
"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("{config}"), "{err}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = Adapter::load(&write(
            dir.path(),
            r#"
kind = "python"
abi  = "cp312"
create_env = []
install    = ["true"]
launch     = ["python", "{config}"]
"#,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("create_env"), "{err}");
    }

    #[test]
    fn placeholders_are_substituted() {
        let vars = HashMap::from([
            ("env", "/data/envs/foo".to_string()),
            ("entrypoint", "hc_foo.main".to_string()),
            ("config", "/data/cfg/foo.toml".to_string()),
        ]);
        let launch = vec![
            "{env}/bin/python".to_string(),
            "-m".to_string(),
            "{entrypoint}".to_string(),
            "{config}".to_string(),
        ];
        assert_eq!(
            render(&launch, &vars).unwrap(),
            vec![
                "/data/envs/foo/bin/python",
                "-m",
                "hc_foo.main",
                "/data/cfg/foo.toml"
            ]
        );
    }

    /// A typo must fail loudly. Passed through, `{wheelhouse}` becomes a file of
    /// that literal name and the error arrives as "no such file" from a program
    /// that never mentions the adapter.
    #[test]
    fn an_unknown_placeholder_is_an_error() {
        let vars = HashMap::from([("env", "/e".to_string())]);
        let err = render(&["{whelhouse}/x".to_string()], &vars).unwrap_err();
        assert!(err.to_string().contains("{whelhouse}"), "{err}");
    }

    /// Paths go into argv, never through a shell, so a space stays one argument.
    #[test]
    fn a_path_with_a_space_stays_one_argument() {
        let vars = HashMap::from([("config", "/data/my plugins/foo.toml".to_string())]);
        let out = render(&["{config}".to_string()], &vars).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "/data/my plugins/foo.toml");
    }
}
