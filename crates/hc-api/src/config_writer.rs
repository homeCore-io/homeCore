//! Targeted edits to homecore.toml from runtime API handlers.
//!
//! Uses `toml_edit` so the operator's comments, ordering, and
//! formatting are preserved across writes — we only touch the exact
//! field being changed. Writes go through a temp-file + rename to
//! avoid leaving a half-written config on a crash.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// Persist the `enabled` flag for a single `[[plugins]]` entry,
/// matched by `id`. Fails if the file can't be parsed, no matching
/// entry is found, or the write fails. The matching entry's other
/// fields and surrounding TOML structure (other entries, comments,
/// section ordering) are left untouched.
pub fn persist_plugin_enabled(
    config_path: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;

    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .with_context(|| format!("parse {}", config_path.display()))?;

    // [[plugins]] is an array of tables. toml_edit exposes them as
    // ArrayOfTables under the top-level "plugins" key.
    let entries = doc
        .get_mut("plugins")
        .and_then(|v| v.as_array_of_tables_mut())
        .ok_or_else(|| anyhow!("[[plugins]] section not found in homecore.toml"))?;

    let mut matched = false;
    for table in entries.iter_mut() {
        let id_match = table
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == plugin_id)
            .unwrap_or(false);

        if id_match {
            table["enabled"] = toml_edit::value(enabled);
            matched = true;
            break;
        }
    }

    if !matched {
        return Err(anyhow!(
            "plugin id {plugin_id:?} not found in [[plugins]] entries"
        ));
    }

    write_atomic(config_path, doc.to_string().as_bytes())
        .with_context(|| format!("write {}", config_path.display()))
}

/// Write `bytes` to `path` via a sibling tmp file + rename, so a crash
/// mid-write can never leave the destination half-populated.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    // Use a deterministic-ish suffix (process pid + nanos) rather than
    // a random crate dep — this is just for atomic-write, not security.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.tmp.{pid}.{nanos}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("homecore.toml")
    ));

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_config(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn flips_enabled_preserving_other_fields() {
        let f = make_temp_config(
            r#"
[server]
host = "0.0.0.0"

[[plugins]]
id      = "hc-hue"
binary  = "/usr/local/bin/hc-hue"
config  = "config/hc-hue/config.toml"
enabled = false

[[plugins]]
id      = "hc-sonos"
binary  = "/usr/local/bin/hc-sonos"
config  = "config/hc-sonos/config.toml"
enabled = false
"#,
        );

        persist_plugin_enabled(f.path(), "hc-hue", true).unwrap();
        let after = std::fs::read_to_string(f.path()).unwrap();

        assert!(after.contains("id      = \"hc-hue\""));
        assert!(after.contains("enabled = true")); // hue flipped
        assert!(after.contains("id      = \"hc-sonos\""));
        // Sonos still false — only the matching entry was touched.
        assert_eq!(after.matches("enabled = false").count(), 1);
        // Server section still present and untouched.
        assert!(after.contains("[server]"));
        assert!(after.contains("host = \"0.0.0.0\""));
    }

    #[test]
    fn preserves_standalone_comments() {
        let f = make_temp_config(
            r#"
# This is the appliance flavor.

[[plugins]]
# Philips Hue plugin
id      = "hc-hue"
binary  = "/usr/local/bin/hc-hue"
config  = "config/hc-hue/config.toml"
enabled = false
"#,
        );

        persist_plugin_enabled(f.path(), "hc-hue", true).unwrap();
        let after = std::fs::read_to_string(f.path()).unwrap();

        // Standalone comments and other fields survive.
        assert!(after.contains("# This is the appliance flavor."));
        assert!(after.contains("# Philips Hue plugin"));
        assert!(after.contains("binary  = \"/usr/local/bin/hc-hue\""));
        assert!(after.contains("enabled = true"));

        // Note: inline trailing comments on the line being edited are
        // dropped — toml_edit's value replacement resets the
        // suffix decor. We accept that limitation; standalone comments
        // (the more common style in seeded configs) are preserved.
    }

    #[test]
    fn errors_when_plugin_id_missing() {
        let f = make_temp_config(
            r#"
[[plugins]]
id      = "hc-hue"
enabled = false
"#,
        );

        let err = persist_plugin_enabled(f.path(), "hc-mystery", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hc-mystery"));
    }

    #[test]
    fn errors_when_no_plugins_section() {
        let f = make_temp_config(
            r#"
[server]
host = "0.0.0.0"
"#,
        );

        let err = persist_plugin_enabled(f.path(), "hc-hue", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[[plugins]]"));
    }
}
