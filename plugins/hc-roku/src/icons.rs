//! Channel icons, fetched once from a Roku and kept on disk.
//!
//! A Roku serves each installed channel's icon at `query/icon/{id}`. They
//! change only when the channel updates, so each is fetched once, kept as
//! `icons/{id}-{version}.{ext}` in the plugin's files folder, and served to
//! the screens from there (`ManagementHandle::with_files_dir`; the app gets
//! them from `GET /plugins/plugin.roku/files/icons/…`). The name carries the
//! version, so an updated channel gets a new file — and a new address the
//! browser has never cached. Two Rokus with the same channel share one icon.
//!
//! A Roku that is off still answers (in standby), and one whose "Control by
//! mobile apps" is disabled refuses icons with a 403; either way, icons
//! already kept keep showing, after a restart too.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing::debug;

use crate::ecp::{App, EcpClient};

pub struct Icons {
    dir: PathBuf,
    /// `{id}-{version}` → file name, for the icons on disk.
    index: Mutex<HashMap<String, String>>,
    /// Being fetched now, so two Rokus don't fetch the same one at once.
    busy: Mutex<HashSet<String>>,
}

/// A path segment made safe: letters, digits, `.` and `_`; the rest `_`.
fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What an icon is kept under: its channel and version.
pub fn stem(app: &App) -> String {
    format!(
        "{}-{}",
        safe(&app.id),
        safe(app.version.as_deref().unwrap_or("0"))
    )
}

fn extension(content_type: &str) -> &'static str {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

impl Icons {
    /// Icons under `files_dir/icons`, read from disk now.
    pub fn new(files_dir: &Path) -> Self {
        let dir = files_dir.join("icons");
        let _ = std::fs::create_dir_all(&dir);
        let mut index = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some((s, ext)) = name.rsplit_once('.') {
                    if matches!(ext, "png" | "jpg" | "gif" | "webp") {
                        index.insert(s.to_string(), name.clone());
                    }
                }
            }
        }
        debug!(count = index.len(), dir = %dir.display(), "Channel icons on disk");
        Self {
            dir,
            index: Mutex::new(index),
            busy: Mutex::new(HashSet::new()),
        }
    }

    /// Where a channel's icon is, as a path in the plugin's files — `None`
    /// until it has been fetched.
    pub fn path_for(&self, app: &App) -> Option<String> {
        self.index
            .lock()
            .ok()?
            .get(&stem(app))
            .map(|f| format!("icons/{f}"))
    }

    /// The channels (not inputs) whose icon isn't kept yet.
    pub fn missing(&self, apps: &[App]) -> Vec<App> {
        let index = self.index.lock().map(|i| i.clone()).unwrap_or_default();
        apps.iter()
            .filter(|a| !a.is_input() && !index.contains_key(&stem(a)))
            .cloned()
            .collect()
    }

    /// Fetch one icon and keep it. `Ok(false)` when another task is already
    /// fetching it.
    pub async fn fetch(&self, client: &EcpClient, app: &App) -> Result<bool> {
        let key = stem(app);
        if !self
            .busy
            .lock()
            .map(|mut b| b.insert(key.clone()))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let result = self.fetch_inner(client, app, &key).await;
        if let Ok(mut b) = self.busy.lock() {
            b.remove(&key);
        }
        result.map(|()| true)
    }

    async fn fetch_inner(&self, client: &EcpClient, app: &App, key: &str) -> Result<()> {
        let (content_type, bytes) = client.icon(&app.id).await?;
        let name = format!("{key}.{}", extension(&content_type));
        // Written aside and moved into place, so a half-written icon is never
        // served.
        let tmp = self.dir.join(format!(".{name}.part"));
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, self.dir.join(&name))?;
        if let Ok(mut i) = self.index.lock() {
            i.insert(key.to_string(), name);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scripted;

    fn app(id: &str, version: &str) -> App {
        App {
            id: id.into(),
            name: "x".into(),
            app_type: Some("appl".into()),
            version: Some(version.into()),
        }
    }

    fn folder() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hc-roku-icons-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Fetched once, kept by channel and version, and found again on disk by
    /// the next run — with the Roku no longer answering for icons.
    #[tokio::test]
    async fn an_icon_is_fetched_once_and_kept_across_restarts() {
        let dir = folder();
        let (client, log) = scripted(|_, _| (200, "PNG".into())).await;
        let icons = Icons::new(&dir);
        let yt = app("837", "2.23.150000189");
        assert_eq!(icons.missing(std::slice::from_ref(&yt)).len(), 1);
        assert!(icons.fetch(&client, &yt).await.unwrap());
        assert_eq!(
            icons.path_for(&yt).as_deref(),
            Some("icons/837-2.23.150000189.png")
        );
        assert_eq!(log.lock().unwrap().len(), 1);

        let again = Icons::new(&dir);
        assert!(again.missing(std::slice::from_ref(&yt)).is_empty());
        assert_eq!(again.path_for(&yt), icons.path_for(&yt));
        // A new version is a new icon.
        assert!(again.path_for(&app("837", "2.24")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Inputs have no channel icon; ids that are not path-safe stay inside
    /// the folder.
    #[test]
    fn inputs_are_skipped_and_names_stay_in_the_folder() {
        let dir = folder();
        let icons = Icons::new(&dir);
        let hdmi = App {
            id: "tvinput.hdmi1".into(),
            name: "HDMI 1".into(),
            app_type: Some("tvin".into()),
            version: None,
        };
        assert!(icons.missing(&[hdmi]).is_empty());
        assert_eq!(stem(&app("../../x", "1/2")), ".._.._x-1_2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
