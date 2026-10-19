//! Firmware: what each controller runs, what its project has released, and
//! installing one.
//!
//! A controller says what it is: `ver` (16.0.1), `release` (the board build,
//! `ESP32`) and `repo` (where its firmware comes from, `wled/WLED`). The
//! file to install is that repository's release asset `WLED_<version>_<release>.bin`
//! — so a controller running a build nobody publishes (a custom compile, a
//! fork without releases) is never offered one.
//!
//! Installing is WLED's own update: the file posted to `/update`. Before it,
//! the presets and settings are backed up; after it, the controller must come
//! back running the new version, or the install has failed and says so.
//!
//! The alerts raised here — declared in `main.rs`:
//! - `firmware_update` (info, off unless turned on): a newer one is out, keyed
//!   by its version, with an "Update…" button;
//! - `firmware_updating` (info): one is being installed now, so an automation
//!   can hold off;
//! - `firmware_update_failed` (error): the last install didn't take.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Which releases count as an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    #[default]
    Stable,
    /// Pre-releases too, but not nightly builds.
    Beta,
    Nightly,
}

impl Channel {
    pub fn parse(s: &str) -> Self {
        match s {
            "beta" => Channel::Beta,
            "nightly" => Channel::Nightly,
            _ => Channel::Stable,
        }
    }
}

/// One release that has a file for a given board.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub version: String,
    pub tag: String,
    pub prerelease: bool,
    pub published: String,
    pub asset: String,
    pub url: String,
    pub notes_url: String,
}

/// `16.0.1` against `17.0.0-dev`: the numbers, part by part; a version with
/// a suffix comes before the same numbers without one.
pub fn compare(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let v = v.trim_start_matches(['v', 'V']);
        let (nums, suffix) = match v.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
            Some(i) => (&v[..i], &v[i..]),
            None => (v, ""),
        };
        let parts: Vec<u64> = nums
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        (parts, suffix.to_string())
    };
    let (pa, sa) = split(a);
    let (pb, sb) = split(b);
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    match (sa.is_empty(), sb.is_empty()) {
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => sa.cmp(&sb),
    }
}

/// Every release in a GitHub releases list that has a file for `release`
/// (the board build), newest first, in the channel.
pub fn offers(releases: &Value, release: &str, channel: Channel) -> Vec<Offer> {
    let suffix = format!("_{release}.bin");
    let mut out: Vec<Offer> = releases
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|r| !r.get("draft").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|r| {
            let tag = r.get("tag_name")?.as_str()?.to_string();
            let pre = r
                .get("prerelease")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let nightly = tag.eq_ignore_ascii_case("nightly");
            let wanted = match channel {
                Channel::Stable => !pre,
                Channel::Beta => !nightly,
                Channel::Nightly => true,
            };
            if !wanted {
                return None;
            }
            let asset = r.get("assets")?.as_array()?.iter().find(|a| {
                a.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.starts_with("WLED_") && n.ends_with(&suffix))
            })?;
            let name = asset.get("name")?.as_str()?.to_string();
            let version = name
                .strip_prefix("WLED_")?
                .strip_suffix(&suffix)?
                .to_string();
            Some(Offer {
                version,
                tag,
                prerelease: pre,
                published: r
                    .get("published_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: asset
                    .get("browser_download_url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                notes_url: r
                    .get("html_url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                asset: name,
            })
        })
        .collect();
    out.sort_by(|a, b| compare(&b.version, &a.version));
    out
}

/// What one controller's firmware is, and what is on offer for it.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub installed: Option<String>,
    pub release: Option<String>,
    pub repo: Option<String>,
    /// Newest first, in the channel.
    pub offers: Vec<Offer>,
    /// `installing` while it runs; `failed` after one that didn't take.
    pub state: Option<String>,
    pub installing: Option<String>,
    pub failure: Option<String>,
    pub checked_at: Option<String>,
}

impl Status {
    pub fn newer(&self) -> Option<&Offer> {
        let installed = self.installed.as_deref()?;
        self.offers
            .first()
            .filter(|o| compare(&o.version, installed) == Ordering::Greater)
    }

    /// The firmware attributes published on the device.
    pub fn attributes(&self) -> Value {
        let newer = self.newer();
        let status = match (self.state.as_deref(), self.release.as_ref(), newer) {
            (Some("installing"), _, _) => "installing",
            (Some("failed"), _, _) => "failed",
            (_, None, _) => "unknown",
            (_, Some(_), _) if self.offers.is_empty() && self.checked_at.is_some() => "no_releases",
            (_, _, Some(_)) => "available",
            _ if self.checked_at.is_some() => "up_to_date",
            _ => "unknown",
        };
        json!({
            "firmware_build": self.release,
            "firmware_source": self.repo,
            "firmware_latest": self.offers.first().map(|o| o.version.clone()),
            "firmware_update_available": newer.is_some(),
            "firmware_status": status,
            "firmware_checked_at": self.checked_at,
        })
    }

    /// The firmware alerts true of the controller now.
    pub fn alerts(&self) -> Vec<plugin_sdk_rs::types::PluginAlert> {
        use plugin_sdk_rs::types::{AlertAction, AlertActionTarget, PluginAlert};
        let mut out = Vec::new();
        if let Some(v) = &self.installing {
            out.push(
                PluginAlert::new("firmware_updating", format!("Installing firmware {v}"))
                    .key(v.clone()),
            );
        }
        // The update stays an alert until it is running — through the install
        // too — so "wait until it clears" waits for the install to finish,
        // and one that fails is still on offer.
        if let Some(o) = self.newer() {
            out.push(
                PluginAlert::new("firmware_update", format!("Firmware {} is out", o.version))
                    .key(o.version.clone())
                    .action(AlertAction {
                        label: "Update…".into(),
                        id: "install_firmware".into(),
                        target: AlertActionTarget::Device,
                        params: json!({ "version": o.version }),
                        confirm: true,
                    }),
            );
        }
        if let (Some("failed"), Some(why)) = (self.state.as_deref(), &self.failure) {
            out.push(PluginAlert::new(
                "firmware_update_failed",
                format!("Firmware update failed: {why}"),
            ));
        }
        out
    }

    pub fn view(&self) -> Value {
        json!({
            "installed": self.installed,
            "build": self.release,
            "source": self.repo,
            "state": self.state,
            "installing": self.installing,
            "failure": self.failure,
            "checked_at": self.checked_at,
            "offers": self.offers.iter().map(|o| json!({
                "version": o.version,
                "tag": o.tag,
                "prerelease": o.prerelease,
                "published": o.published,
                "asset": o.asset,
                "notes_url": o.notes_url,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Every controller's firmware status, by homeCore id, shared between the
/// daily check, installs and the actions that report it.
pub type Statuses = Arc<Mutex<HashMap<String, Status>>>;

fn http(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("hc-wled/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

/// A repository's releases, from GitHub.
pub async fn fetch_releases(repo: &str) -> Result<Value> {
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        || repo.matches('/').count() != 1
    {
        bail!("not a GitHub repository: {repo}");
    }
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let resp = http(Duration::from_secs(15))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("GitHub releases")?;
    if !resp.status().is_success() {
        bail!("GitHub said {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// What the controller at `host` says it runs: `(ver, release, repo)`.
pub async fn identify(host: &str) -> Result<(String, Option<String>, Option<String>)> {
    let info: Value = http(Duration::from_secs(5))
        .get(format!("http://{host}/json/info"))
        .send()
        .await?
        .json()
        .await?;
    let s = |k: &str| {
        info.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok((s("ver").unwrap_or_default(), s("release"), s("repo")))
}

/// Download a release file. A board image is a few megabytes; anything
/// outside that is not one.
pub async fn download(url: &str) -> Result<Vec<u8>> {
    if !url.starts_with("https://github.com/")
        && !url.starts_with("https://objects.githubusercontent.com/")
    {
        bail!("not a GitHub download: {url}");
    }
    let bytes = http(Duration::from_secs(120))
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if bytes.len() < 100_000 || bytes.len() > 16 * 1024 * 1024 {
        bail!("{} bytes isn't a firmware image", bytes.len());
    }
    Ok(bytes.to_vec())
}

/// Post an image to the controller's `/update`, as its own update page does.
/// Whether it could be a firmware image: an ESP application image (0xE9) or
/// a gzipped one (an ESP8266 `.bin.gz`), of a plausible size. A stray file
/// sent by mistake is turned away before the controller is backed up and
/// darkened for it.
pub fn looks_like_image(image: &[u8]) -> std::result::Result<(), String> {
    if image.len() < 64 * 1024 || image.len() > 16 * 1024 * 1024 {
        return Err(format!(
            "that's {} KB — not the size of a WLED firmware file",
            image.len() / 1024
        ));
    }
    match image {
        [0xE9, ..] | [0x1F, 0x8B, ..] => Ok(()),
        _ => Err("that isn't a firmware file (.bin) for an ESP32 or ESP8266".into()),
    }
}

pub async fn upload(host: &str, name: &str, image: Vec<u8>) -> Result<()> {
    let part = reqwest::multipart::Part::bytes(image)
        .file_name(name.to_string())
        .mime_str("application/octet-stream")?;
    let form = reqwest::multipart::Form::new().part("update", part);
    let resp = http(Duration::from_secs(180))
        .post(format!("http://{host}/update"))
        .multipart(form)
        .send()
        .await
        .context("sending the image")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // WLED explains in its page: subnet, PIN, OTA lock.
        let why = if text.contains("subnet") {
            "the controller only takes updates from its own subnet".to_string()
        } else if text.contains("PIN") || text.contains("pin") {
            "the controller's settings are locked with a PIN".to_string()
        } else if text.to_ascii_lowercase().contains("lock") {
            "updates are locked on the controller (OTA lock)".to_string()
        } else {
            format!("it said {status}")
        };
        bail!("{why}");
    }
    Ok(())
}

/// Wait for the controller to come back after an update, and say what it
/// runs then.
pub async fn wait_back(host: &str, within: Duration) -> Result<String> {
    let start = std::time::Instant::now();
    // It restarts after answering; give it a moment to go away first.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if let Ok((ver, _, _)) = identify(host).await {
            if !ver.is_empty() {
                return Ok(ver);
            }
        }
        if start.elapsed() > within {
            bail!("it didn't come back within {} s", within.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn releases() -> Value {
        // The shape of api.github.com/repos/wled/WLED/releases, 2026-10-18.
        json!([
            {"tag_name": "nightly", "prerelease": true, "published_at": "2026-07-17T00:00:00Z", "html_url": "n",
             "assets": [{"name": "WLED_17.0.0-dev_ESP32.bin", "browser_download_url": "https://github.com/x/n.bin"},
                        {"name": "WLED_17.0.0-dev_ESP32-C3.bin", "browser_download_url": "u"}]},
            {"tag_name": "v17.0.0-b1", "prerelease": true, "published_at": "2026-08-01T00:00:00Z", "html_url": "b",
             "assets": [{"name": "WLED_17.0.0-b1_ESP32.bin", "browser_download_url": "https://github.com/x/b.bin"}]},
            {"tag_name": "v16.0.1", "prerelease": false, "published_at": "2026-07-07T00:00:00Z", "html_url": "s",
             "assets": [{"name": "WLED_16.0.1_ESP32.bin", "browser_download_url": "https://github.com/x/s.bin"},
                        {"name": "WLED_16.0.1_ESP32-S3_16MB_opi.bin", "browser_download_url": "u"}]},
            {"tag_name": "v16.0.0", "prerelease": false, "published_at": "2026-05-03T00:00:00Z", "html_url": "o",
             "assets": [{"name": "WLED_16.0.0_ESP32.bin", "browser_download_url": "https://github.com/x/o.bin"}]},
            {"tag_name": "v9", "draft": true, "assets": [{"name": "WLED_99.0.0_ESP32.bin"}]}
        ])
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert_eq!(compare("16.0.1", "16.0.0"), Ordering::Greater);
        assert_eq!(compare("v16.0.1", "16.0.1"), Ordering::Equal);
        assert_eq!(compare("17.0.0-dev", "16.0.1"), Ordering::Greater);
        assert_eq!(compare("17.0.0-b1", "17.0.0"), Ordering::Less);
        assert_eq!(compare("0.15.10", "0.15.9"), Ordering::Greater);
    }

    #[test]
    fn only_the_board_s_own_file_in_the_channel_is_offered() {
        let stable = offers(&releases(), "ESP32", Channel::Stable);
        let v: Vec<&str> = stable.iter().map(|o| o.version.as_str()).collect();
        assert_eq!(v, ["16.0.1", "16.0.0"]);
        assert_eq!(stable[0].asset, "WLED_16.0.1_ESP32.bin");
        let beta = offers(&releases(), "ESP32", Channel::Beta);
        assert_eq!(beta[0].version, "17.0.0-b1");
        let nightly = offers(&releases(), "ESP32", Channel::Nightly);
        assert_eq!(nightly[0].version, "17.0.0-dev");
        // An S3 isn't offered the ESP32's file, and a board nobody builds
        // for is offered nothing.
        assert_eq!(
            offers(&releases(), "ESP32-S3_16MB_opi", Channel::Stable)[0].version,
            "16.0.1"
        );
        assert!(offers(&releases(), "MyCustomBuild", Channel::Stable).is_empty());
    }

    #[test]
    fn the_house_controller_is_up_to_date_and_says_so() {
        let mut s = Status {
            installed: Some("16.0.1".into()),
            release: Some("ESP32".into()),
            repo: Some("wled/WLED".into()),
            offers: offers(&releases(), "ESP32", Channel::Stable),
            checked_at: Some("2026-10-18T22:00:00Z".into()),
            ..Default::default()
        };
        assert!(s.newer().is_none());
        assert_eq!(s.attributes()["firmware_status"], "up_to_date");
        assert!(s.alerts().is_empty());
        // On the beta channel, 17.0.0-b1 is newer, with its button.
        s.offers = offers(&releases(), "ESP32", Channel::Beta);
        let a = s.alerts();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].topic, "firmware_update");
        assert_eq!(a[0].key.as_deref(), Some("17.0.0-b1"));
        assert_eq!(a[0].action.as_ref().unwrap().params["version"], "17.0.0-b1");
        assert_eq!(s.attributes()["firmware_status"], "available");
        // While installing, that as well; after a failure, the failure too.
        s.installing = Some("17.0.0-b1".into());
        s.state = Some("installing".into());
        let topics: Vec<String> = s.alerts().into_iter().map(|a| a.topic).collect();
        assert_eq!(topics, ["firmware_updating", "firmware_update"]);
        s.installing = None;
        s.state = Some("failed".into());
        s.failure = Some("it didn't come back within 120 s".into());
        let topics: Vec<String> = s.alerts().into_iter().map(|a| a.topic).collect();
        assert_eq!(topics, ["firmware_update", "firmware_update_failed"]);
    }

    #[test]
    fn a_build_nobody_publishes_is_never_offered_one() {
        let s = Status {
            installed: Some("0.15.0".into()),
            release: Some("MyCustomBuild".into()),
            offers: vec![],
            checked_at: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(s.attributes()["firmware_status"], "no_releases");
        assert!(s.alerts().is_empty());
    }

    #[test]
    fn only_what_could_be_a_firmware_image_is_sent() {
        let mut esp = vec![0u8; 900 * 1024];
        esp[0] = 0xE9;
        assert!(looks_like_image(&esp).is_ok());
        let mut gz = vec![0u8; 600 * 1024];
        gz[0] = 0x1F;
        gz[1] = 0x8B;
        assert!(looks_like_image(&gz).is_ok());
        assert!(looks_like_image(&vec![0u8; 900 * 1024]).is_err());
        assert!(looks_like_image(&[0xE9, 0, 0]).is_err(), "far too small");
    }
}
