//! Bridge info refresher — periodically pulls the static + diagnostic
//! info exposed by each WLED instance and merges it onto the device's
//! existing state via `publish_state_partial`.
//!
//! This is the read-only "what is this thing" half of a WLED device:
//! firmware, hardware, LED layout, WiFi signal, peer-mesh size, and
//! counts of effects/palettes/presets the device knows about. The
//! schema-declared writable attributes (on/brightness/preset) keep
//! flowing through the existing bridge loop unchanged — partial
//! publishes don't disturb them.

use anyhow::Result;
use plugin_sdk_rs::DevicePublisher;
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::time::Duration;
use tracing::{debug, warn};

/// Refresh interval. Bridge info changes rarely (firmware upgrades,
/// LED-count rewires, mesh peer churn) — 5 minutes is plenty.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Spawn one refresh task per device. Each task owns its own
/// `WledClient` and runs a slow tick loop, partial-publishing info
/// attributes onto the device. Errors are logged and the loop
/// continues — a transient WLED reboot shouldn't kill the task.
pub fn spawn_per_device(publisher: DevicePublisher, devices: Vec<crate::config::DeviceConfig>) {
    for dev in devices {
        let pub_for_task = publisher.clone();
        tokio::spawn(async move {
            // First refresh fires after a brief grace so we don't race
            // the bridge's own initial state publish — we want our
            // partial merge to land on top of an already-published
            // device, not before it.
            tokio::time::sleep(Duration::from_secs(5)).await;
            let http = match Client::builder().timeout(Duration::from_secs(5)).build() {
                Ok(c) => c,
                Err(e) => {
                    warn!(hc_id = %dev.hc_id, error = %e, "bridge_info reqwest build failed");
                    return;
                }
            };
            let base = format!("http://{}", dev.host.trim_end_matches('/'));
            let mut interval = tokio::time::interval(REFRESH_INTERVAL);
            loop {
                interval.tick().await;
                match build_attrs(&http, &base).await {
                    Ok(attrs) => {
                        if let Err(e) = pub_for_task
                            .publish_state_partial(&dev.hc_id, &Value::Object(attrs))
                            .await
                        {
                            warn!(hc_id = %dev.hc_id, error = %e, "publish bridge info failed");
                        } else {
                            debug!(hc_id = %dev.hc_id, "bridge info refreshed");
                        }
                    }
                    Err(e) => {
                        debug!(hc_id = %dev.hc_id, error = %e, "bridge info fetch failed");
                    }
                }
            }
        });
    }
}

/// Pull `/json/info`, `/json/nodes`, and `/json/presets` and roll the
/// useful fields into a flat attributes map. Anything missing or
/// malformed is silently skipped — a partial picture is still useful.
async fn build_attrs(http: &Client, base: &str) -> Result<Map<String, Value>> {
    // /json/info is the heavy hitter; everything else is supplemental.
    // If it fails we bail — a device that can't answer /json/info
    // isn't going to give us anything useful anyway.
    let info = fetch_json(http, base, "/json/info").await?;
    let mut attrs: Map<String, Value> = Map::new();

    // Top-level identity / firmware
    if let Some(s) = info.get("ver").and_then(Value::as_str) {
        attrs.insert("firmware".into(), json!(s));
    }
    if let Some(s) = info.get("product").and_then(Value::as_str) {
        attrs.insert("product".into(), json!(s));
    }
    if let Some(s) = info.get("brand").and_then(Value::as_str) {
        attrs.insert("brand".into(), json!(s));
    }
    if let Some(s) = info.get("arch").and_then(Value::as_str) {
        attrs.insert("arch".into(), json!(s));
    }
    if let Some(s) = info.get("mac").and_then(Value::as_str) {
        attrs.insert("mac".into(), json!(s));
    }
    if let Some(s) = info.get("ip").and_then(Value::as_str) {
        attrs.insert("ip".into(), json!(s));
    }

    // LED hardware layout
    if let Some(leds) = info.get("leds") {
        if let Some(n) = leds.get("count").and_then(Value::as_u64) {
            attrs.insert("led.count".into(), json!(n));
        }
        if let Some(n) = leds.get("pwr").and_then(Value::as_u64) {
            attrs.insert("led.power_mw".into(), json!(n));
        }
        if let Some(n) = leds.get("maxpwr").and_then(Value::as_u64) {
            if n > 0 {
                attrs.insert("led.max_power_mw".into(), json!(n));
            }
        }
        if let Some(n) = leds.get("maxseg").and_then(Value::as_u64) {
            attrs.insert("led.max_segments".into(), json!(n));
        }
        if let Some(b) = leds.get("rgbw").and_then(Value::as_bool) {
            attrs.insert("led.rgbw".into(), json!(b));
        }
    }

    // Effect / palette catalog sizes
    if let Some(n) = info.get("fxcount").and_then(Value::as_u64) {
        attrs.insert("effects.count".into(), json!(n));
    }
    if let Some(n) = info.get("palcount").and_then(Value::as_u64) {
        attrs.insert("palettes.count".into(), json!(n));
    }

    // WiFi link quality
    if let Some(w) = info.get("wifi") {
        if let Some(n) = w.get("signal").and_then(Value::as_i64) {
            attrs.insert("wifi.signal_pct".into(), json!(n));
        }
        if let Some(n) = w.get("rssi").and_then(Value::as_i64) {
            attrs.insert("wifi.rssi".into(), json!(n));
        }
        if let Some(n) = w.get("channel").and_then(Value::as_i64) {
            attrs.insert("wifi.channel".into(), json!(n));
        }
    }

    // Uptime + on-device clock
    if let Some(n) = info.get("uptime").and_then(Value::as_u64) {
        attrs.insert("uptime_secs".into(), json!(n));
    }
    if let Some(s) = info.get("time").and_then(Value::as_str) {
        attrs.insert("device_time".into(), json!(s));
    }

    // Peer mesh — size only; the full list is too long to be useful
    // as an attribute, but a count tells the operator "yes this is
    // syncing with N other WLEDs" at a glance.
    if let Ok(nodes) = fetch_json(http, base, "/json/nodes").await {
        if let Some(arr) = nodes.get("nodes").and_then(Value::as_array) {
            attrs.insert("peers.count".into(), json!(arr.len()));
        }
    }

    // Preset count — /json/presets returns a {"<id>": {...}, ...} map.
    // The "0" entry is reserved (the "current" preset), so subtract it
    // when present.
    if let Ok(presets) = fetch_json(http, base, "/presets.json").await {
        if let Some(obj) = presets.as_object() {
            let count = obj.len().saturating_sub(usize::from(obj.contains_key("0")));
            attrs.insert("presets.count".into(), json!(count));
        }
    }

    Ok(attrs)
}

async fn fetch_json(http: &Client, base: &str, path: &str) -> Result<Value> {
    let url = format!("{base}{path}");
    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{path}: status {}", resp.status());
    }
    Ok(resp.json().await?)
}
