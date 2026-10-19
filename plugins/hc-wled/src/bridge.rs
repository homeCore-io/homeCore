//! Bridge: each controller's live state, over its WebSocket (or polling when
//! it has none), and every command sent to it.
//!
//! **Light state is published as a *partial*, never a full state.** A full
//! publish replaces the retained document, and `bridge_info` merges the
//! read-only attributes — LED layout, WiFi signal, presets, firmware — onto
//! that same device on its own slow tick. Publishing the light state whole
//! would delete them all until the next refresh. `state::attributes` emits a
//! fixed key set, so a partial says exactly as much and disturbs nothing else.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::command::{self, Strip};
use crate::config::DeviceConfig;
use crate::runtime::Runtime;
use crate::wled::WledClient;
use crate::{controller, firmware, presets, state};

pub async fn run(rt: Runtime, mut cmd_rx: mpsc::Receiver<(String, Value)>) {
    for dev in rt.cfg.devices.clone() {
        let poll_secs = dev
            .poll_interval_secs
            .unwrap_or(rt.cfg.wled.poll_interval_secs);
        let rt = rt.clone();
        tokio::spawn(async move {
            let ws = startup_device(&rt, &dev).await;
            if ws {
                run_websocket(rt, dev, poll_secs).await;
            } else {
                run_poller(rt, dev, poll_secs).await;
            }
        });
    }

    info!("Bridge running ({} devices)", rt.cfg.devices.len());

    while let Some((hc_id, cmd)) = cmd_rx.recv().await {
        let Some(dev) = rt.device(&hc_id).cloned() else {
            warn!(hc_id, "Command for unknown device");
            continue;
        };
        let rt = rt.clone();
        tokio::spawn(async move {
            debug!(hc_id = %dev.hc_id, cmd = ?cmd, "Executing command");
            if let Err(e) = handle_command(&rt, &dev, &cmd).await {
                warn!(hc_id = %dev.hc_id, error = %e, "Command failed");
            }
        });
    }
}

/// Publish the controller's state, as it is now, with where the change came
/// from when a command caused it.
async fn publish_now(rt: &Runtime, dev: &DeviceConfig, cause: Option<&Value>) {
    let client = WledClient::new(&dev.host);
    let Ok((st, info)) = client.get_state_info().await else {
        return;
    };
    let attrs = state::attributes(&st, info.as_ref());
    let r = match cause {
        Some(cmd) => {
            rt.publisher
                .publish_state_partial_for_command(&dev.hc_id, &attrs, cmd, "hc-wled")
                .await
        }
        None => rt.publisher.publish_state_partial(&dev.hc_id, &attrs).await,
    };
    if let Err(e) = r {
        warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state");
    }
}

/// One command, of any kind.
async fn handle_command(rt: &Runtime, dev: &DeviceConfig, cmd: &Value) -> Result<()> {
    let client = WledClient::new(&dev.host);
    let action = cmd.get("action").and_then(Value::as_str);
    let slot = |k: &str| cmd.get(k).and_then(Value::as_u64);

    let body = match action {
        // Long: runs on its own, saying how it goes as alerts.
        Some("install_firmware") => {
            let version = cmd
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string);
            let rt = rt.clone();
            let dev = dev.clone();
            tokio::spawn(async move { install(&rt, &dev, version).await });
            return Ok(());
        }
        // Both rewrite presets.json rather than "save" through the state:
        // WLED applies a request's `seg` before it saves the preset, so a
        // save would also change what the strip is showing (`json.cpp`) —
        // and saving a playlist that way starts it. The file is what WLED's
        // own file editor writes. When the controller's settings are locked
        // with a PIN it can't be written, and the save is used instead.
        Some("save_playlist") => {
            let mut file = client.get_presets().await.unwrap_or(json!({}));
            let at = slot("slot")
                .or_else(|| presets::free_slot(&file))
                .context("every preset slot is taken")?;
            let name = cmd
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Playlist");
            let view = cmd.get("playlist").unwrap_or(&Value::Null);
            let wire =
                presets::playlist_wire(view).context("a playlist needs at least one step")?;
            let start = cmd.get("start").and_then(Value::as_bool) == Some(true);
            file[at.to_string()] = json!({ "n": name, "on": true, "playlist": wire });
            match rewrite_presets(dev, &file).await {
                Ok(()) => {
                    if start {
                        client.post_state(&json!({ "ps": at })).await?;
                    }
                    publish_now(rt, dev, Some(cmd)).await;
                    rt.wake(&dev.hc_id);
                    return Ok(());
                }
                Err(e) => {
                    warn!(hc_id = %dev.hc_id, error = %e, "presets.json can't be written; saving through the state, which starts it");
                    presets::save_playlist_body(at, name, view)
                        .context("a playlist needs at least one step")?
                }
            }
        }
        Some("rename_preset") => {
            let at = slot("preset_id").context("which preset: `preset_id`")?;
            let mut file = client.get_presets().await?;
            let existing = file
                .get(at.to_string())
                .filter(|p| p.as_object().is_some_and(|o| !o.is_empty()))
                .cloned()
                .context("no such preset")?;
            let name = cmd
                .get("name")
                .and_then(Value::as_str)
                .context("a new `name`")?;
            let ql = cmd.get("quick_label").and_then(Value::as_str);
            let mut renamed = existing.clone();
            renamed["n"] = json!(name);
            match ql {
                Some(q) if !q.is_empty() => renamed["ql"] = json!(q),
                Some(_) => {
                    if let Some(o) = renamed.as_object_mut() {
                        o.remove("ql");
                    }
                }
                None => {}
            }
            file[at.to_string()] = renamed;
            match rewrite_presets(dev, &file).await {
                Ok(()) => {
                    rt.wake(&dev.hc_id);
                    return Ok(());
                }
                Err(e) => {
                    warn!(hc_id = %dev.hc_id, error = %e, "presets.json can't be written; renaming through the state, which applies it");
                    presets::rename_body(at, &existing, name, ql)
                }
            }
        }
        Some("set_power_on_preset") => {
            // In the settings, not the state: what it starts with.
            let ps = cmd.get("preset_id").and_then(Value::as_i64).unwrap_or(0);
            let mut def = json!({ "ps": ps.max(0) });
            if let Some(on) = cmd.get("on").and_then(Value::as_bool) {
                def["on"] = json!(on);
            }
            controller::apply_settings(&dev.host, &json!({ "def": def })).await?;
            rt.wake(&dev.hc_id);
            return Ok(());
        }
        Some("restart") => {
            rt.expect_restart(&dev.hc_id);
            json!({ "rb": true })
        }
        _ => {
            let strip = if command::needs_state(cmd) || is_identify(cmd) {
                client
                    .get_state()
                    .await
                    .map(|s| Strip::from_state(&s))
                    .unwrap_or_default()
            } else {
                Strip::default()
            };
            if cmd.get("reboot").and_then(Value::as_bool) == Some(true) {
                rt.expect_restart(&dev.hc_id);
            }
            if is_identify(cmd) {
                return identify(rt, dev, &client, &strip, cmd).await;
            }
            match command::build(cmd, &strip) {
                Some(b) => b,
                None => {
                    warn!(cmd = ?cmd, "No recognized fields in WLED command");
                    return Ok(());
                }
            }
        }
    };

    let presets_changed = body.get("psave").is_some() || body.get("pdel").is_some();
    client.post_state(&body).await?;
    if body.get("rb").is_some() {
        return Ok(());
    }
    publish_now(rt, dev, Some(cmd)).await;
    if presets_changed {
        // WLED writes the file after answering; give it a moment.
        tokio::time::sleep(Duration::from_millis(800)).await;
        rt.wake(&dev.hc_id);
    }
    Ok(())
}

/// Replace the controller's presets file.
async fn rewrite_presets(dev: &DeviceConfig, file: &Value) -> Result<()> {
    controller::upload_file(&dev.host, "presets.json", serde_json::to_vec(file)?).await
}

fn is_identify(cmd: &Value) -> bool {
    cmd.get("identify").and_then(Value::as_bool) == Some(true)
        || cmd.get("action").and_then(Value::as_str) == Some("identify")
}

/// Flash it white for three seconds, then put back what it was showing.
async fn identify(
    rt: &Runtime,
    dev: &DeviceConfig,
    client: &WledClient,
    strip: &Strip,
    cmd: &Value,
) -> Result<()> {
    let before = client.get_state().await.ok();
    let flash = command::build(&json!({ "identify": true }), strip).context("identify")?;
    client.post_state(&flash).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Some(b) = before {
        let segs: Vec<Value> = b
            .get("seg")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|s| json!({ "id": s["id"], "col": s["col"], "fx": s["fx"] }))
                    .collect()
            })
            .unwrap_or_default();
        let back = json!({ "on": b["on"], "bri": b["bri"], "seg": segs });
        client.post_state(&back).await?;
    }
    publish_now(rt, dev, Some(cmd)).await;
    Ok(())
}

/// Install firmware `version` (the newest on offer when left out): back up,
/// download, send, wait for it to come back, and check what it runs.
async fn install(rt: &Runtime, dev: &DeviceConfig, version: Option<String>) {
    let hc_id = dev.hc_id.clone();
    // One at a time: the update alert stays up through an install, so its
    // button can be pressed again meanwhile.
    if let Some(v) = rt
        .firmware
        .lock()
        .unwrap()
        .get(&hc_id)
        .and_then(|s| s.installing.clone())
    {
        warn!(hc_id = %hc_id, installing = %v, "Firmware is already being installed; not starting another");
        return;
    }
    let set = |f: &dyn Fn(&mut firmware::Status)| {
        let mut all = rt.firmware.lock().unwrap();
        f(all.entry(hc_id.clone()).or_default());
    };
    let result: Result<String> = async {
        let (installed, release, repo) = firmware::identify(&dev.host).await?;
        let release = release.context("the controller doesn't say which build it is")?;
        let repo = repo.unwrap_or_else(|| "wled/WLED".into());
        let releases = firmware::fetch_releases(&repo).await?;
        let all = firmware::offers(&releases, &release, firmware::Channel::Nightly);
        let offer = match &version {
            Some(v) => all.iter().find(|o| &o.version == v),
            None => firmware::offers(&releases, &release, rt.cfg.wled.firmware_channel())
                .first()
                .and_then(|best| all.iter().find(|o| o.version == best.version)),
        }
        .cloned()
        .with_context(|| match &version {
            Some(v) => format!("{repo} has no {v} for {release}"),
            None => format!("{repo} has nothing for {release}"),
        })?;
        if offer.version == installed {
            anyhow::bail!("it already runs {installed}");
        }
        set(&|s| {
            s.installing = Some(offer.version.clone());
            s.state = Some("installing".into());
            s.failure = None;
        });
        rt.publish_firmware(&hc_id).await;
        info!(hc_id = %hc_id, from = %installed, to = %offer.version, "Installing firmware");

        // A backup first, kept beside the plugin's config.
        let saved = save_backup(rt, dev, &format!("before-{}", offer.version)).await;
        if let Err(e) = &saved {
            anyhow::bail!("couldn't back up first, so didn't install: {e}");
        }
        let image = firmware::download(&offer.url).await?;
        rt.expect_restart(&hc_id);
        firmware::upload(&dev.host, &offer.asset, image).await?;
        let now = firmware::wait_back(&dev.host, Duration::from_secs(180)).await?;
        if firmware::compare(&now, &offer.version) != std::cmp::Ordering::Equal {
            anyhow::bail!("it came back running {now}, not {}", offer.version);
        }
        Ok(now)
    }
    .await;

    match result {
        Ok(now) => {
            info!(hc_id = %hc_id, version = %now, "Firmware installed");
            set(&|s| {
                s.installed = Some(now.clone());
                s.installing = None;
                s.state = None;
                s.failure = None;
            });
        }
        Err(e) => {
            warn!(hc_id = %hc_id, error = %e, "Firmware install failed");
            let why = e.to_string();
            set(&|s| {
                s.installing = None;
                s.state = Some("failed".into());
                s.failure = Some(why.clone());
            });
        }
    }
    rt.publish_firmware(&hc_id).await;
    rt.wake(&hc_id);
}

/// Take a backup of the controller and keep it in the plugin's backups
/// folder. Its file name, or why not.
pub async fn save_backup(rt: &Runtime, dev: &DeviceConfig, why: &str) -> Result<String> {
    let backup = controller::backup(&dev.host).await?;
    let dir = rt.cfg.wled.backups_path(&rt.config_path).join(&dev.hc_id);
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let name = format!("{stamp}-{why}.json");
    std::fs::write(dir.join(&name), serde_json::to_vec_pretty(&backup)?)?;
    Ok(name)
}

/// Fetch initial state and publish availability. Registration is handled in
/// main.rs before the bridge starts. True when the controller has a
/// WebSocket.
async fn startup_device(rt: &Runtime, dev: &DeviceConfig) -> bool {
    let client = WledClient::new(&dev.host);
    match client.get_info().await {
        Ok(info) => {
            info!(
                hc_id    = %dev.hc_id,
                host     = %dev.host,
                ver      = %info.ver,
                leds     = info.leds.count,
                effects  = info.fxcount,
                palettes = info.palcount,
                ws       = info.ws,
                "WLED device online"
            );
            let _ = rt.publisher.set_available(&dev.hc_id, true).await;
            publish_now(rt, dev, None).await;
            info.ws >= 0
        }
        Err(e) => {
            warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable at startup");
            let _ = rt.publisher.set_available(&dev.hc_id, false).await;
            // Try its WebSocket anyway: a controller that was off at startup
            // still has one when it comes up, and the loop below falls back
            // to polling while it can't connect.
            true
        }
    }
}

/// Drive state updates via WLED's WebSocket (`ws://{host}/ws`). Falls back to
/// polling while it can't connect, and reconnects.
async fn run_websocket(rt: Runtime, dev: DeviceConfig, poll_secs: u64) {
    let client = WledClient::new(&dev.host);
    let url = client.ws_url();
    loop {
        debug!(hc_id = %dev.hc_id, url = %url, "Connecting WebSocket");
        match connect_async(&url).await {
            Ok((mut ws, _)) => {
                info!(hc_id = %dev.hc_id, "WebSocket connected");
                let _ = rt.publisher.set_available(&dev.hc_id, true).await;
                let _ = rt
                    .publisher
                    .publish_state_partial(&dev.hc_id, &json!({ "connection": "push" }))
                    .await;
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Some((st, info)) = state::from_ws(&text) {
                                let attrs = state::attributes(&st, info.as_ref());
                                if let Err(e) =
                                    rt.publisher.publish_state_partial(&dev.hc_id, &attrs).await
                                {
                                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish WS state");
                                }
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                warn!(hc_id = %dev.hc_id, "WebSocket closed; reconnecting");
                // Not unavailable yet: a poll says whether it is really gone.
                if client.get_state().await.is_err() {
                    let _ = rt.publisher.set_available(&dev.hc_id, false).await;
                }
            }
            Err(e) => {
                debug!(hc_id = %dev.hc_id, error = %e, "WebSocket connect failed; polling");
                let _ = rt
                    .publisher
                    .publish_state_partial(&dev.hc_id, &json!({ "connection": "poll" }))
                    .await;
                match client.get_state_info().await {
                    Ok((st, info)) => {
                        let _ = rt
                            .publisher
                            .publish_state_partial(
                                &dev.hc_id,
                                &state::attributes(&st, info.as_ref()),
                            )
                            .await;
                        let _ = rt.publisher.set_available(&dev.hc_id, true).await;
                    }
                    Err(_) => {
                        let _ = rt.publisher.set_available(&dev.hc_id, false).await;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

/// Drive state updates via periodic HTTP polling.
async fn run_poller(rt: Runtime, dev: DeviceConfig, poll_secs: u64) {
    let client = WledClient::new(&dev.host);
    let mut ticker = interval(Duration::from_secs(poll_secs));
    // Not assumed: a device startup couldn't reach is marked unavailable, and
    // the first good poll is what puts it back.
    let mut online = false;

    loop {
        ticker.tick().await;
        match client.get_state_info().await {
            Ok((st, info)) => {
                if !online {
                    info!(hc_id = %dev.hc_id, "WLED device online");
                    let _ = rt.publisher.set_available(&dev.hc_id, true).await;
                    online = true;
                }
                let mut attrs = state::attributes(&st, info.as_ref());
                attrs["connection"] = json!("poll");
                if let Err(e) = rt.publisher.publish_state_partial(&dev.hc_id, &attrs).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state");
                }
            }
            Err(e) => {
                if online {
                    warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable");
                    let _ = rt.publisher.set_available(&dev.hc_id, false).await;
                    online = false;
                }
            }
        }
    }
}
