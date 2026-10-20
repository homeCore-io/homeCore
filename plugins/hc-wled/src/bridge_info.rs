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
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::time::Duration;
use tracing::{debug, warn};

/// Refresh interval. Bridge info changes rarely (firmware upgrades,
/// LED-count rewires, mesh peer churn) — 5 minutes is plenty.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Spawn one refresh task per device. Each runs on a slow tick — or sooner
/// when woken (`Runtime::wake`, after its presets change) — and
/// partial-publishes what the controller is and holds. Errors are logged and
/// the loop continues — a transient WLED reboot shouldn't kill the task.
pub fn spawn_per_device(rt: crate::runtime::Runtime) {
    for dev in rt.cfg.devices.clone() {
        let rt = rt.clone();
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
            let wake = rt.refresh.get(&dev.hc_id).cloned();
            let mut last_uptime: Option<u64> = None;
            let mut last_check: Option<std::time::Instant> = None;
            let mut last_schema: Option<Value> = None;
            loop {
                match build_attrs(&http, &base).await {
                    Ok((mut attrs, hardware, info)) => {
                        // Re-register with what the controller says it is.
                        // Startup registration happens from config alone,
                        // before anything has been asked — this is the first
                        // moment the identity exists. Absent fields leave what
                        // core holds alone, so doing it on every refresh is
                        // idempotent rather than chatty.
                        if let Err(e) = rt
                            .publisher
                            .register_device_detailed(
                                &dev.hc_id,
                                &dev.name,
                                None,
                                None,
                                None,
                                Some(&hardware),
                            )
                            .await
                        {
                            debug!(hc_id = %dev.hc_id, error = %e, "identity re-register failed");
                        }
                        // Where this plugin reaches it — the config's address, which
                        // the plugin page edits; not always the controller's own IP.
                        attrs.insert("address".into(), json!(dev.host));
                        attrs.insert(
                            "catalog_storage".into(),
                            json!(dev.catalog.clone().unwrap_or_else(|| rt
                                .cfg
                                .wled
                                .catalog
                                .clone())),
                        );
                        if let Some(v) = settings_attrs(&http, &base).await {
                            attrs.extend(v);
                        }
                        // Its own names in its schema, when they changed.
                        let named = crate::schema::schema_named(&attrs);
                        if last_schema.as_ref() != Some(&named) {
                            match rt
                                .publisher
                                .register_device_schema_json(&dev.hc_id, &named)
                                .await
                            {
                                Ok(()) => {
                                    rt.schemas
                                        .lock()
                                        .unwrap()
                                        .insert(dev.hc_id.clone(), named.clone());
                                    last_schema = Some(named);
                                }
                                Err(e) => {
                                    debug!(hc_id = %dev.hc_id, error = %e, "schema publish failed")
                                }
                            }
                        }
                        if let Err(e) = rt
                            .publisher
                            .publish_state_partial(&dev.hc_id, &Value::Object(attrs))
                            .await
                        {
                            warn!(hc_id = %dev.hc_id, error = %e, "publish bridge info failed");
                        } else {
                            debug!(hc_id = %dev.hc_id, "bridge info refreshed");
                        }

                        // A restart nobody asked for: its uptime went back.
                        let uptime = info.get("uptime").and_then(Value::as_u64);
                        if let (Some(was), Some(now)) = (last_uptime, uptime) {
                            if now < was && !rt.restart_was_expected(&dev.hc_id) {
                                warn!(hc_id = %dev.hc_id, "Controller restarted by itself");
                                rt.restarted.lock().unwrap().insert(
                                    dev.hc_id.clone(),
                                    (std::time::Instant::now(), chrono::Utc::now().to_rfc3339()),
                                );
                            }
                        }
                        last_uptime = uptime;

                        // Firmware: what it runs now, and — daily, or as
                        // configured — what its project has released.
                        let every = rt.cfg.wled.firmware_check_hours;
                        let due = every > 0
                            && last_check
                                .is_none_or(|t| t.elapsed() >= Duration::from_secs(every * 3600));
                        crate::firmware_check::refresh(&rt, &dev.hc_id, &info, due).await;
                        if due {
                            last_check = Some(std::time::Instant::now());
                        }
                        rt.publish_firmware(&dev.hc_id).await;
                    }
                    Err(e) => {
                        debug!(hc_id = %dev.hc_id, error = %e, "bridge info fetch failed");
                    }
                }
                match &wake {
                    Some(n) => {
                        let _ = tokio::time::timeout(REFRESH_INTERVAL, n.notified()).await;
                    }
                    None => tokio::time::sleep(REFRESH_INTERVAL).await,
                }
            }
        });
    }
}

/// What the controller's settings say that a screen shows beside its state:
/// the preset it starts with.
async fn settings_attrs(http: &Client, base: &str) -> Option<Map<String, Value>> {
    let cfg = fetch_json(http, base, "/json/cfg").await.ok()?;
    let mut m = Map::new();
    let def = cfg.get("def");
    m.insert(
        "power_on_preset_id".into(),
        match def.and_then(|d| d.get("ps")).and_then(Value::as_u64) {
            Some(n) if n > 0 => json!(n),
            _ => Value::Null,
        },
    );
    m.insert(
        "power_on".into(),
        def.and_then(|d| d.get("on"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    Some(m)
}

/// What the controller *is*, from the same `/json/info` the attributes come
/// from.
///
/// Separate from [`build_attrs`] because it goes somewhere else: registration
/// rather than state. WLED reports `brand` and `product` on every controller
/// and `ver` once it is running, so this is usually complete on the first poll
/// — and where it is not, absent means "not said" and core keeps what it has.
fn identity(info: &Value) -> plugin_sdk_rs::DeviceHardware {
    let field = |key: &str| {
        info.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let mut hw = plugin_sdk_rs::DeviceHardware::new();
    if let Some(v) = field("brand") {
        hw = hw.manufacturer(v);
    }
    if let Some(v) = field("product") {
        hw = hw.model(v);
    }
    if let Some(v) = field("ver") {
        hw = hw.sw_version(v);
    }
    hw
}

/// Pull `/json/info`, `/json/nodes`, and `/json/presets` and roll the
/// useful fields into a flat attributes map. Anything missing or
/// malformed is silently skipped — a partial picture is still useful.
async fn build_attrs(
    http: &Client,
    base: &str,
) -> Result<(Map<String, Value>, plugin_sdk_rs::DeviceHardware, Value)> {
    // /json/info is the heavy hitter; everything else is supplemental.
    // If it fails we bail — a device that can't answer /json/info
    // isn't going to give us anything useful anyway.
    let info = fetch_json(http, base, "/json/info").await?;
    let hardware = identity(&info);
    let mut attrs: Map<String, Value> = Map::new();

    // `brand`, `product` and `ver` used to be published here as attributes,
    // which is where they went when a device's identity had nowhere else to
    // go. They are not readings — they do not change while you watch, and a
    // dashboard listing them beside brightness is listing three facts about
    // the hardware among the things it is doing. They now ride the
    // registration as manufacturer / model / sw_version; see `identity()`.
    //
    // `arch` stays an attribute: it is the ESP variant, which is closer to a
    // diagnostic than to a model name.
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
            attrs.insert("led_count".into(), json!(n));
        }
        // WLED's `pwr` and `maxpwr` are current in milliamps — the LEDs' draw
        // as its brightness limiter estimates it, and that limiter's budget —
        // not power. Both are 0 with the limiter off.
        if let Some(n) = leds.get("pwr").and_then(Value::as_u64) {
            attrs.insert("led_current_ma".into(), json!(n));
        }
        if let Some(n) = leds.get("maxpwr").and_then(Value::as_u64) {
            if n > 0 {
                attrs.insert("led_max_current_ma".into(), json!(n));
            }
        }
        if let Some(n) = leds.get("maxseg").and_then(Value::as_u64) {
            attrs.insert("led_max_segments".into(), json!(n));
        }
        if let Some(b) = leds.get("rgbw").and_then(Value::as_bool) {
            attrs.insert("led_rgbw".into(), json!(b));
        }
    }

    // The effects and palettes by name, as `{id, name}` — what the
    // `set_effect` / `set_palette` actions offer, so a screen shows names
    // rather than numbers. Supplemental: a controller that won't say keeps
    // what it said before.
    for (path, key) in [("/json/effects", "effects"), ("/json/palettes", "palettes")] {
        if let Ok(Value::Array(names)) = fetch_json(http, base, path).await {
            let list: Vec<Value> = names
                .iter()
                .enumerate()
                .filter_map(|(id, n)| n.as_str().map(|name| json!({ "id": id, "name": name })))
                // WLED marks its reserved slots "RSVD" / "-".
                .filter(|v| !matches!(v["name"].as_str(), Some("RSVD") | Some("-")))
                .collect();
            if !list.is_empty() {
                attrs.insert(key.into(), Value::Array(list));
            }
        }
    }

    // Effect / palette catalog sizes
    if let Some(n) = info.get("fxcount").and_then(Value::as_u64) {
        attrs.insert("effects_count".into(), json!(n));
    }
    if let Some(n) = info.get("palcount").and_then(Value::as_u64) {
        attrs.insert("palettes_count".into(), json!(n));
    }

    // WiFi link quality
    if let Some(w) = info.get("wifi") {
        if let Some(n) = w.get("signal").and_then(Value::as_i64) {
            attrs.insert("wifi_signal_pct".into(), json!(n));
        }
        if let Some(n) = w.get("rssi").and_then(Value::as_i64) {
            attrs.insert("wifi_rssi".into(), json!(n));
        }
        if let Some(n) = w.get("channel").and_then(Value::as_i64) {
            attrs.insert("wifi_channel".into(), json!(n));
        }
    }
    // **No uptime, and no device clock.** Both change on their own between
    // refreshes, so publishing either makes every refresh a
    // `device_state_changed` for a controller that did nothing — the same
    // churn hc-roku's `uptime` caused, on a five-minute tick instead of a
    // ten-second one. A WLED reboot already shows as availability dropping,
    // and core records `last_seen`.

    // Peer mesh — size only; the full list is too long to be useful
    // as an attribute, but a count tells the operator "yes this is
    // syncing with N other WLEDs" at a glance.
    if let Ok(nodes) = fetch_json(http, base, "/json/nodes").await {
        if let Some(arr) = nodes.get("nodes").and_then(Value::as_array) {
            attrs.insert("peers_count".into(), json!(arr.len()));
        }
    }

    // Preset count — /json/presets returns a {"<id>": {...}, ...} map.
    // The "0" entry is reserved (the "current" preset), so subtract it
    // when present.
    if let Ok(file) = fetch_json(http, base, "/presets.json").await {
        let all = crate::presets::list(&file);
        attrs.insert("presets_count".into(), json!(all.len()));
        // `presets` is every slot (a playlist is applied like a preset);
        // `playlists` only the playlists, for "start a playlist".
        let playlists: Vec<Value> = all
            .iter()
            .filter(|e| e["kind"] == "playlist")
            .map(|e| json!({ "id": e["id"], "name": e["name"] }))
            .collect();
        attrs.insert("presets".into(), Value::Array(all));
        attrs.insert("playlists".into(), Value::Array(playlists));
    }
    // Capabilities a screen needs before offering a control: white
    // temperature, a white channel.
    if let Some(leds) = info.get("leds") {
        // `cct` is a bitmask on newer firmware (bit 2: the strip has CCT), a
        // bool on older.
        let cct = match leds.get("cct") {
            Some(Value::Bool(b)) => *b,
            Some(v) => v.as_u64().is_some_and(|n| n & 4 != 0 || n == 1),
            None => false,
        };
        attrs.insert("led_cct".into(), json!(cct));
        if let Some(fps) = leds.get("fps").and_then(Value::as_u64) {
            attrs.insert("led_fps".into(), json!(fps));
        }
    }
    // Whether its firmware has the AudioReactive usermod — the sound effects
    // and settings exist only then.
    attrs.insert(
        "has_audio".into(),
        json!(info
            .get("u")
            .is_some_and(|u| u.get("AudioReactive").is_some())),
    );

    Ok((attrs, hardware, info))
}

async fn fetch_json(http: &Client, base: &str, path: &str) -> Result<Value> {
    let url = format!("{base}{path}");
    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{path}: status {}", resp.status());
    }
    Ok(resp.json().await?)
}
