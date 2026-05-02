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
pub fn persist_plugin_enabled(config_path: &Path, plugin_id: &str, enabled: bool) -> Result<()> {
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

/// Apply a structured per-section patch to a TOML document and
/// return the resulting text. The patch shape matches what the
/// admin UI's per-section Save buttons emit:
///
/// ```json
/// {
///   "server":   { "port": 8090 },
///   "battery":  { "threshold_pct": 25.0 },
///   "auth.admin_uds": { "enabled": true, "path": "/run/foo.sock" }
/// }
/// ```
///
/// Section names are top-level TOML table keys, with dotted keys
/// for nested tables (e.g. `auth.admin_uds`). For each section in
/// the patch, every field/value pair surgically replaces the
/// matching entry in the section, leaving comments + ordering +
/// other sections untouched. New fields are appended at the end of
/// the section.
///
/// Returns the new TOML string ready to write atomically.
pub fn apply_section_patch(current: &str, patch: &serde_json::Value) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = current
        .parse()
        .with_context(|| "parse current homecore.toml")?;

    let patch_obj = patch
        .as_object()
        .ok_or_else(|| anyhow!("patch must be a JSON object keyed by section name"))?;

    for (section_path, section_patch) in patch_obj {
        let fields = section_patch.as_object().ok_or_else(|| {
            anyhow!("patch[{section_path:?}] must be an object of field=value pairs")
        })?;

        // Walk the dotted section path (e.g. "auth.admin_uds") to
        // get a mutable Table handle.
        let table = walk_to_table_mut(&mut doc, section_path)?;

        for (key, value) in fields {
            let item = json_to_toml_item(value).with_context(|| {
                format!("section [{section_path}] field {key:?}: unsupported value type")
            })?;
            table[key.as_str()] = item;
        }
    }

    Ok(doc.to_string())
}

/// Replace an entire `[[section.name]]` array-of-tables with the
/// provided items.  `section_path` is a dotted parent path plus a
/// trailing leaf key (e.g. `"notify.channels"` → parent `notify`,
/// leaf `channels`).  Each item must be a JSON object whose values
/// are primitives or arrays of primitives; nested objects are
/// rejected so callers don't accidentally smuggle an inline table
/// where an array-of-tables row is expected.
///
/// Comments and ordering of *other* sections are preserved; the
/// existing array-of-tables block is replaced wholesale.  Use this
/// for editors over `[[notify.channels]]` and similar list-shaped
/// config that doesn't fit `apply_section_patch`'s field-merge
/// model.
pub fn replace_array_of_tables(
    current: &str,
    section_path: &str,
    items: &[serde_json::Value],
) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = current
        .parse()
        .with_context(|| "parse current homecore.toml")?;

    let (parent_path, leaf) = section_path
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("section path must be dotted (e.g. notify.channels)"))?;

    let parent = walk_to_table_mut(&mut doc, parent_path)?;

    let mut aot = toml_edit::ArrayOfTables::new();
    for (idx, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| anyhow!("items[{idx}] must be a JSON object (TOML table)"))?;
        let mut tbl = toml_edit::Table::new();
        for (k, v) in obj {
            let item = json_to_toml_item(v)
                .with_context(|| format!("items[{idx}] field {k:?}: unsupported value type"))?;
            tbl[k.as_str()] = item;
        }
        aot.push(tbl);
    }

    parent.remove(leaf);
    parent.insert(leaf, toml_edit::Item::ArrayOfTables(aot));

    Ok(doc.to_string())
}

/// Walk a dotted section path (e.g. "auth.admin_uds") and return a
/// mutable handle to the table at the leaf. Creates intermediate
/// tables if missing.
fn walk_to_table_mut<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    section_path: &str,
) -> Result<&'a mut toml_edit::Table> {
    let segments: Vec<&str> = section_path.split('.').collect();

    let mut current: &mut toml_edit::Table = doc.as_table_mut();
    for seg in segments {
        let item = current
            .entry(seg)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        match item {
            toml_edit::Item::Table(t) => current = t,
            other => {
                return Err(anyhow!(
                    "section path {section_path:?} crosses non-table at {seg:?}: {other:?}"
                ))
            }
        }
    }
    Ok(current)
}

/// Translate a JSON value into a toml_edit Item suitable for direct
/// assignment. Supports strings, booleans, numbers (integer +
/// floating), and arrays-of-the-same. Objects (nested tables in
/// the patch) aren't supported here — use a dotted section path
/// instead.
fn json_to_toml_item(v: &serde_json::Value) -> Result<toml_edit::Item> {
    use serde_json::Value as J;
    let value: toml_edit::Value = match v {
        J::Null => return Err(anyhow!("null is not a valid TOML value")),
        J::Bool(b) => (*b).into(),
        J::String(s) => s.as_str().into(),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                return Err(anyhow!("unrepresentable JSON number: {n}"));
            }
        }
        J::Array(arr) => {
            let mut a = toml_edit::Array::new();
            for elem in arr {
                let item = json_to_toml_item(elem)?;
                if let toml_edit::Item::Value(v) = item {
                    a.push(v);
                } else {
                    return Err(anyhow!("array elements must be primitives"));
                }
            }
            a.into()
        }
        J::Object(_) => {
            return Err(anyhow!(
                "nested objects in patch values not supported — use dotted section path"
            ))
        }
    };
    Ok(toml_edit::Item::Value(value))
}

/// Write `bytes` to `path` via a sibling tmp file + rename, so a crash
/// mid-write can never leave the destination half-populated.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
        let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
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
    fn apply_patch_replaces_field_in_existing_section() {
        let input = r#"
[server]
host = "0.0.0.0"
port = 8080

[battery]
threshold_pct    = 20.0
recover_band_pct = 5.0
"#;
        let patch = serde_json::json!({ "battery": { "threshold_pct": 25.5 } });
        let after = apply_section_patch(input, &patch).unwrap();

        // toml_edit may preserve the original key-padding from the source
        // document, so don't assert exact spacing. Match the value plus
        // the surrounding section context.
        assert!(after.contains("threshold_pct"));
        assert!(after.contains("25.5"));
        assert!(!after.contains("20.0")); // old value gone
        assert!(after.contains("recover_band_pct"));
        assert!(after.contains("5.0"));
        assert!(after.contains("[server]"));
        assert!(after.contains("port = 8080"));
    }

    #[test]
    fn apply_patch_dotted_section_path() {
        let input = r#"
[auth]
token_expiry_hours = 24

[auth.admin_uds]
enabled = false
path    = "/run/homecore/admin.sock"
"#;
        let patch = serde_json::json!({
            "auth.admin_uds": { "enabled": true }
        });
        let after = apply_section_patch(input, &patch).unwrap();

        assert!(after.contains("enabled = true"));
        assert!(after.contains(r#"path    = "/run/homecore/admin.sock""#));
        assert!(after.contains("token_expiry_hours = 24"));
    }

    #[test]
    fn apply_patch_creates_missing_section() {
        let input = r#"
[server]
port = 8080
"#;
        let patch = serde_json::json!({
            "battery": { "threshold_pct": 20.0 }
        });
        let after = apply_section_patch(input, &patch).unwrap();
        assert!(after.contains("[battery]"));
        assert!(after.contains("threshold_pct = 20.0"));
    }

    #[test]
    fn apply_patch_handles_arrays() {
        let input = r#"
[auth]
whitelist = []
"#;
        let patch = serde_json::json!({
            "auth": { "whitelist": ["127.0.0.1/32", "::1/128"] }
        });
        let after = apply_section_patch(input, &patch).unwrap();
        assert!(after.contains(r#""127.0.0.1/32""#));
        assert!(after.contains(r#""::1/128""#));
    }

    #[test]
    fn apply_patch_rejects_nested_objects() {
        let input = "";
        let patch = serde_json::json!({
            "server": { "nested": { "no": "good" } }
        });
        assert!(apply_section_patch(input, &patch).is_err());
    }

    #[test]
    fn replace_aot_writes_proper_array_of_tables() {
        let input = r#"
[server]
port = 8080

[notify]
default_channel = "phone"

[[notify.channels]]
name = "old"
type = "pushover"
api_token = "obsolete"
user_key = "obsolete"
"#;
        let items = vec![
            serde_json::json!({
                "name": "phone",
                "type": "pushover",
                "api_token": "atok",
                "user_key": "ukey"
            }),
            serde_json::json!({
                "name": "ops",
                "type": "telegram",
                "bot_token": "bot",
                "chat_id": "12345"
            }),
        ];
        let after = replace_array_of_tables(input, "notify.channels", &items).unwrap();

        // Old entry replaced.
        assert!(!after.contains("obsolete"));
        // Two new entries present as proper [[notify.channels]] tables.
        assert_eq!(after.matches("[[notify.channels]]").count(), 2);
        assert!(after.contains(r#"name = "phone""#));
        assert!(after.contains(r#"type = "pushover""#));
        assert!(after.contains(r#"api_token = "atok""#));
        assert!(after.contains(r#"name = "ops""#));
        assert!(after.contains(r#"chat_id = "12345""#));
        // Sibling section untouched.
        assert!(after.contains(r#"default_channel = "phone""#));
        assert!(after.contains("[server]"));
        assert!(after.contains("port = 8080"));
    }

    #[test]
    fn replace_aot_handles_empty_items_list() {
        let input = r#"
[notify]

[[notify.channels]]
name = "doomed"
type = "pushover"
api_token = "x"
user_key = "y"
"#;
        let after = replace_array_of_tables(input, "notify.channels", &[]).unwrap();
        assert!(!after.contains("doomed"));
        assert!(!after.contains("[[notify.channels]]"));
        assert!(after.contains("[notify]"));
    }

    #[test]
    fn replace_aot_creates_section_when_missing() {
        let input = r#"
[server]
port = 8080
"#;
        let items = vec![serde_json::json!({
            "name": "phone",
            "type": "pushover",
            "api_token": "t",
            "user_key": "u"
        })];
        let after = replace_array_of_tables(input, "notify.channels", &items).unwrap();
        assert!(after.contains("[[notify.channels]]"));
        assert!(after.contains(r#"name = "phone""#));
    }

    #[test]
    fn replace_aot_rejects_nested_objects() {
        let input = "";
        let items = vec![serde_json::json!({
            "name": "x",
            "nested": { "no": "good" }
        })];
        assert!(replace_array_of_tables(input, "notify.channels", &items).is_err());
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
