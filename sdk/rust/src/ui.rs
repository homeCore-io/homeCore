//! A plugin's own screens, carried and served by the plugin.
//!
//! The UI is a built folder — web components in one ES module, plus a
//! `manifest.json` the build writes:
//!
//! ```json
//! { "host_api": 1, "entry": "index.js",
//!   "slots": [{ "slot": "plugin.page", "element": "hc-x-page" }] }
//! ```
//!
//! It travels in whatever package installs the plugin, beside the program
//! (`ui/`). [`ManagementHandle::with_ui_dir`](crate::ManagementHandle::with_ui_dir)
//! reads it, declares it on the manifest with a hash of its files as its
//! `build`, and answers homeCore's `ui_file` requests from it — homeCore
//! passes the files through to the app and keeps none.

use std::path::{Path, PathBuf};

use hc_types::plugin_ui::{relative_path, PluginUi};
use sha2::{Digest, Sha256};

#[derive(serde::Deserialize)]
struct Manifest {
    host_api: u32,
    #[serde(default = "index_js")]
    entry: String,
    #[serde(default)]
    slots: Vec<hc_types::plugin_ui::UiSlot>,
}
fn index_js() -> String {
    "index.js".into()
}

/// Where a plugin's UI folder is: `HC_PLUGIN_UI_DIR` when set; else `ui/`
/// beside the program (how a package installs it); else, for a plugin run
/// from its source tree, `<crate>/ui/dist` — pass `env!("CARGO_MANIFEST_DIR")`.
pub fn find_dir(crate_dir: &str) -> Option<PathBuf> {
    if let Ok(d) = std::env::var("HC_PLUGIN_UI_DIR") {
        return Some(PathBuf::from(d));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ui")));
    let source = Path::new(crate_dir).join("ui").join("dist");
    [beside, Some(source)]
        .into_iter()
        .flatten()
        .find(|d| d.join("manifest.json").is_file())
}

/// The UI in `dir`, as the manifest declares it: `None` (with the reason)
/// when there is none to offer.
pub fn load(dir: &Path, version: &str) -> Result<PluginUi, String> {
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .map_err(|e| format!("{}: {e}", dir.join("manifest.json").display()))?;
    let m: Manifest = serde_json::from_str(&text).map_err(|e| format!("manifest.json: {e}"))?;
    if !dir.join(&m.entry).is_file() {
        return Err(format!("its entry {} isn't there", m.entry));
    }
    PluginUi {
        host_api: m.host_api,
        version: version.to_string(),
        build: build_of(dir)?,
        entry: m.entry,
        slots: m.slots,
    }
    .validated()
    .ok_or_else(|| "it declares nowhere the app can put it".into())
}

/// A hash of every file's name and contents: what changes when the UI does.
fn build_of(dir: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    walk(dir, dir, &mut files).map_err(|e| e.to_string())?;
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        h.update(f.as_bytes());
        h.update([0]);
        h.update(std::fs::read(dir.join(&f)).map_err(|e| e.to_string())?);
    }
    Ok(h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(root, &p, out)?;
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Up to `max` bytes of `file` from `offset`, and whether that is the end.
pub fn piece(dir: &Path, file: &str, offset: u64, max: usize) -> Result<(Vec<u8>, bool), String> {
    use std::io::{Read, Seek, SeekFrom};
    if !relative_path(file) {
        return Err("no such file".into());
    }
    let mut f = std::fs::File::open(dir.join(file)).map_err(|_| "no such file".to_string())?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    f.seek(SeekFrom::Start(offset.min(size)))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(max.min((size - offset.min(size)) as usize));
    f.take(max as u64)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok((buf, offset + max as u64 >= size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempdir::Dir {
        tempdir::Dir::new()
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "hc-ui-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_ui_folder_is_declared_with_a_build_that_changes_with_its_files() {
        let d = folder();
        std::fs::write(
            d.0.join("manifest.json"),
            r#"{"host_api":1,"slots":[{"slot":"plugin.page","element":"hc-x-page"}]}"#,
        )
        .unwrap();
        std::fs::write(d.0.join("index.js"), "export {}").unwrap();
        let a = load(&d.0, "1.2.3").unwrap();
        assert_eq!(
            (a.version.as_str(), a.entry.as_str()),
            ("1.2.3", "index.js")
        );
        assert_eq!(a.build.len(), 16);
        std::fs::write(d.0.join("index.js"), "export const x = 1").unwrap();
        assert_ne!(load(&d.0, "1.2.3").unwrap().build, a.build);
    }

    #[test]
    fn a_folder_without_its_entry_or_a_manifest_offers_nothing() {
        let d = folder();
        assert!(load(&d.0, "1").is_err());
        std::fs::write(d.0.join("manifest.json"), r#"{"host_api":1,"slots":[]}"#).unwrap();
        assert!(load(&d.0, "1").is_err());
    }

    #[test]
    fn files_go_in_pieces_and_never_from_outside_the_folder() {
        let d = folder();
        std::fs::write(d.0.join("index.js"), "abcdefghij").unwrap();
        assert_eq!(
            piece(&d.0, "index.js", 0, 4).unwrap(),
            (b"abcd".to_vec(), false)
        );
        assert_eq!(
            piece(&d.0, "index.js", 8, 4).unwrap(),
            (b"ij".to_vec(), true)
        );
        assert!(piece(&d.0, "../etc/passwd", 0, 4).is_err());
        assert!(piece(&d.0, "missing.js", 0, 4).is_err());
    }
}
