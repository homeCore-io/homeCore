//! Keeping each controller's firmware status current: what it runs (every
//! refresh), and what its project has released (when a check is due).
//!
//! Releases are fetched once per firmware source and kept for an hour, so a
//! house of twelve WLEDs on one source asks GitHub once, not twelve times —
//! its unauthenticated limit is sixty an hour.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, info};

use crate::firmware;
use crate::runtime::Runtime;

static RELEASES: Mutex<Option<HashMap<String, (Instant, Value)>>> = Mutex::new(None);
const KEEP: Duration = Duration::from_secs(3600);

async fn releases(repo: &str, fresh: bool) -> Option<Value> {
    if !fresh {
        if let Some((at, v)) = RELEASES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .get(repo)
        {
            if at.elapsed() < KEEP {
                return Some(v.clone());
            }
        }
    }
    match firmware::fetch_releases(repo).await {
        Ok(v) => {
            RELEASES
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(repo.to_string(), (Instant::now(), v.clone()));
            Some(v)
        }
        Err(e) => {
            debug!(repo, error = %e, "firmware releases unavailable");
            None
        }
    }
}

/// Update one controller's status from its `/json/info`, and — when `check`
/// — from its project's releases.
pub async fn refresh(rt: &Runtime, hc_id: &str, info: &Value, check: bool) {
    let s = |k: &str| {
        info.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (ver, release, repo) = (s("ver"), s("release"), s("repo"));
    {
        let mut all = rt.firmware.lock().unwrap();
        let st = all.entry(hc_id.to_string()).or_default();
        if st.installing.is_none() {
            st.installed = ver;
        }
        st.release = release.clone();
        st.repo = repo.clone();
    }
    if !check {
        return;
    }
    let (Some(release), Some(repo)) = (release, repo) else {
        return;
    };
    let Some(list) = releases(&repo, false).await else {
        return;
    };
    let offers = firmware::offers(&list, &release, rt.cfg.wled.firmware_channel());
    let mut all = rt.firmware.lock().unwrap();
    let st = all.entry(hc_id.to_string()).or_default();
    st.offers = offers;
    st.checked_at = Some(chrono::Utc::now().to_rfc3339());
    if let Some(o) = st.newer() {
        info!(hc_id, installed = ?st.installed, available = %o.version, "Firmware update available");
    }
}

/// Check now, whatever the schedule — the "Check now" button.
pub async fn check_now(rt: &Runtime, hc_id: &str, host: &str) -> Result<(), String> {
    let info: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("http://{host}/json/info"))
        .send()
        .await
        .map_err(|e| format!("the controller isn't answering: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    // A person asking wants an answer from GitHub, not from the hour-old copy.
    if let Some(repo) = info.get("repo").and_then(Value::as_str) {
        let _ = releases(repo, true).await;
    }
    refresh(rt, hc_id, &info, true).await;
    rt.publish_firmware(hc_id).await;
    Ok(())
}
