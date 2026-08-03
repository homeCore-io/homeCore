//! Bridge: manages per-device state polling, WebSocket subscriptions,
//! and command execution.

use std::collections::HashMap;

use anyhow::Result;
use futures_util::StreamExt;
use plugin_sdk_rs::DevicePublisher;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::{DeviceConfig, WledConfig};
use crate::wled::{WledClient, WledState};

pub struct Bridge {
    cfg: WledConfig,
    publisher: DevicePublisher,
}

impl Bridge {
    pub fn new(cfg: WledConfig, publisher: DevicePublisher) -> Self {
        Self { cfg, publisher }
    }

    pub async fn run(self, mut cmd_rx: mpsc::Receiver<(String, Value)>) {
        for dev in &self.cfg.devices {
            let client = WledClient::new(&dev.host);
            let publisher = self.publisher.clone();
            let poll_secs = dev
                .poll_interval_secs
                .unwrap_or(self.cfg.wled.poll_interval_secs);

            // Fetch initial state + determine transport
            let ws_supported = startup_device(&client, dev, &publisher).await;

            // Spawn real-time listener: WebSocket if supported, else HTTP poll
            let dev_clone = dev.clone();
            if ws_supported {
                let ws_url = client.ws_url();
                tokio::spawn(run_websocket(dev_clone, ws_url, publisher, poll_secs));
            } else {
                tokio::spawn(run_poller(dev_clone, publisher, poll_secs));
            }
        }

        // Command routing map: hc_id → host
        let host_map: HashMap<String, String> = self
            .cfg
            .devices
            .iter()
            .map(|d| (d.hc_id.clone(), d.host.clone()))
            .collect();

        info!("Bridge running ({} devices)", self.cfg.devices.len());

        while let Some((hc_id, cmd)) = cmd_rx.recv().await {
            let host = match host_map.get(&hc_id) {
                Some(h) => h.clone(),
                None => {
                    warn!(hc_id, "Command for unknown device");
                    continue;
                }
            };
            let client = WledClient::new(&host);
            let publisher = self.publisher.clone();
            let hc_id2 = hc_id.clone();
            tokio::spawn(async move {
                debug!(hc_id = %hc_id2, cmd = ?cmd, "Executing command");
                match execute_command(&client, &cmd).await {
                    Ok(()) => {
                        // Re-fetch and publish updated state
                        if let Ok(state) = client.get_state().await {
                            let j = state_to_json(&state);
                            if let Err(e) = publisher
                                .publish_state_for_command(&hc_id2, &j, &cmd, "hc-wled")
                                .await
                            {
                                warn!(hc_id = %hc_id2, error = %e, "Failed to publish state");
                            }
                        }
                    }
                    Err(e) => warn!(hc_id = %hc_id2, error = %e, "Command failed"),
                }
            });
        }
    }
}

/// Fetch initial state and publish availability.
/// Registration is handled in main.rs before the bridge starts.
/// Returns true if the device reports WebSocket support.
async fn startup_device(
    client: &WledClient,
    dev: &DeviceConfig,
    publisher: &DevicePublisher,
) -> bool {
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
            let _ = publisher.set_available(&dev.hc_id, true).await;
            if let Ok(state) = client.get_state().await {
                let _ = publisher
                    .publish_state(&dev.hc_id, &state_to_json(&state))
                    .await;
            }
            info.ws >= 0
        }
        Err(e) => {
            warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable at startup");
            let _ = publisher.set_available(&dev.hc_id, false).await;
            false
        }
    }
}

/// Drive state updates via WLED's WebSocket (`ws://{host}/ws`).
/// Falls back to polling on connection error and reconnects automatically.
async fn run_websocket(
    dev: DeviceConfig,
    ws_url: String,
    publisher: DevicePublisher,
    poll_secs: u64,
) {
    let client = WledClient::new(&dev.host);
    loop {
        info!(hc_id = %dev.hc_id, url = %ws_url, "Connecting WebSocket");
        match connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                let _ = publisher.set_available(&dev.hc_id, true).await;
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Ok(state) = serde_json::from_str::<WledState>(&text) {
                                let j = state_to_json(&state);
                                if let Err(e) = publisher.publish_state(&dev.hc_id, &j).await {
                                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish WS state");
                                }
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
                warn!(hc_id = %dev.hc_id, "WebSocket closed; reconnecting");
                let _ = publisher.set_available(&dev.hc_id, false).await;
            }
            Err(e) => {
                warn!(hc_id = %dev.hc_id, error = %e, "WebSocket connect failed; falling back to poll");
                if let Ok(state) = client.get_state().await {
                    let _ = publisher
                        .publish_state(&dev.hc_id, &state_to_json(&state))
                        .await;
                    let _ = publisher.set_available(&dev.hc_id, true).await;
                } else {
                    let _ = publisher.set_available(&dev.hc_id, false).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

/// Drive state updates via periodic HTTP polling.
async fn run_poller(dev: DeviceConfig, publisher: DevicePublisher, poll_secs: u64) {
    let client = WledClient::new(&dev.host);
    let mut ticker = interval(Duration::from_secs(poll_secs));
    let mut online = true;

    loop {
        ticker.tick().await;
        match client.get_state().await {
            Ok(state) => {
                if !online {
                    info!(hc_id = %dev.hc_id, "WLED device back online");
                    let _ = publisher.set_available(&dev.hc_id, true).await;
                    online = true;
                }
                let j = state_to_json(&state);
                if let Err(e) = publisher.publish_state(&dev.hc_id, &j).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish state");
                }
            }
            Err(e) => {
                if online {
                    warn!(hc_id = %dev.hc_id, host = %dev.host, error = %e, "WLED unreachable");
                    let _ = publisher.set_available(&dev.hc_id, false).await;
                    online = false;
                }
            }
        }
    }
}

async fn execute_command(client: &WledClient, cmd: &Value) -> Result<()> {
    let mut body = serde_json::Map::new();

    if let Some(on) = cmd.get("on").and_then(Value::as_bool) {
        body.insert("on".into(), json!(on));
    }
    if let Some(bri) = cmd.get("brightness").and_then(Value::as_u64) {
        body.insert("bri".into(), json!((bri.min(255)) as u8));
    }
    if let Some(pct) = cmd.get("brightness_pct").and_then(Value::as_f64) {
        let bri = ((pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
        body.insert("bri".into(), json!(bri));
    }
    if let Some(ms) = cmd.get("transition").and_then(Value::as_u64) {
        body.insert("tt".into(), json!((ms / 100).min(65535)));
    }
    if let Some(ps) = cmd.get("preset").and_then(Value::as_i64) {
        body.insert("ps".into(), json!(ps));
    }
    // `apply_preset` is an explicit alias for the convenience of the
    // capability-action surface: the action sends `{apply_preset: N}`,
    // which here becomes `{ps: N}` on WLED.
    if let Some(ps) = cmd.get("apply_preset").and_then(Value::as_i64) {
        body.insert("ps".into(), json!(ps));
    }
    // `save_preset`: capture the device's current state into preset
    // slot N, optionally with a name. WLED's `psave` field is what
    // triggers the save; `n` (name) and `ql` (quick label) are
    // metadata.
    if let Some(slot) = cmd.get("save_preset").and_then(Value::as_i64) {
        body.insert("psave".into(), json!(slot));
        if let Some(name) = cmd.get("preset_name").and_then(Value::as_str) {
            body.insert("n".into(), json!(name));
        }
    }
    // `identify`: flash the strip to make it visually distinct. WLED
    // doesn't ship a real identify primitive, so we send a single
    // bright-white snapshot. Restoring prior state is left to the
    // user — the act of identifying typically precedes a name change
    // anyway, so the next state push from the user happens
    // immediately after.
    if cmd.get("identify").and_then(Value::as_bool) == Some(true) {
        body.insert("on".into(), json!(true));
        body.insert("bri".into(), json!(255u8));
        let mut id_seg = serde_json::Map::new();
        id_seg.insert("id".into(), json!(0));
        id_seg.insert("col".into(), json!([[255, 255, 255]]));
        id_seg.insert("fx".into(), json!(0));
        body.insert("seg".into(), json!([id_seg]));
    }

    let mut seg = serde_json::Map::new();
    seg.insert("id".into(), json!(0));

    if let Some(color) = cmd.get("color").and_then(Value::as_array) {
        seg.insert("col".into(), json!([color]));
    }
    if let Some(fx) = cmd.get("effect").and_then(Value::as_u64) {
        seg.insert("fx".into(), json!(fx));
    }
    if let Some(sx) = cmd.get("effect_speed").and_then(Value::as_u64) {
        seg.insert("sx".into(), json!((sx.min(255)) as u8));
    }
    if let Some(ix) = cmd.get("effect_intensity").and_then(Value::as_u64) {
        seg.insert("ix".into(), json!((ix.min(255)) as u8));
    }
    if let Some(pal) = cmd.get("palette").and_then(Value::as_u64) {
        seg.insert("pal".into(), json!(pal));
    }

    if seg.len() > 1 {
        body.insert("seg".into(), json!([seg]));
    }

    if body.is_empty() {
        warn!(cmd = ?cmd, "No recognized fields in WLED command");
        return Ok(());
    }

    client.post_state(&Value::Object(body)).await
}

pub fn state_to_json(state: &WledState) -> Value {
    let bri_pct = (state.bri as f64 / 255.0 * 1000.0).round() / 10.0;

    let mut j = json!({
        "on":             state.on,
        "brightness":     state.bri,
        "brightness_pct": bri_pct,
        "preset_id":      state.ps,
    });

    if let Some(seg) = state.seg.first() {
        if let Some(primary) = seg.col.first() {
            j["color"] = primary.clone();
        }
        j["effect_id"] = json!(seg.fx);
        j["effect_speed"] = json!(seg.sx);
        j["effect_intensity"] = json!(seg.ix);
        j["palette_id"] = json!(seg.pal);
    }

    j
}
