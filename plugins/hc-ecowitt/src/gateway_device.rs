//! Virtual "gateway" device — the Ecowitt console itself, modeled as a
//! single homeCore device whose attributes mirror the gateway's settings
//! and metadata pages.
//!
//! There is one gateway device per plugin instance (and one plugin
//! instance per console). A background task periodically polls the
//! gateway's `/get_*` cgi-bin endpoints and republishes the merged
//! state under a stable, MAC-derived hc_id so the homeCore device
//! page renders gateway info as attributes the same way it does for
//! sensors.

use anyhow::Result;
use plugin_sdk_rs::DevicePublisher;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::{fetch_custom_server, http_get_json, GwDialect};

/// Registration record for the virtual gateway. Cached so the poller
/// doesn't re-register on every cycle, and so the hc_id (which is
/// MAC-derived) survives even if the gateway's IP changes.
#[allow(dead_code)] // fields are diagnostic; reserved for future refresh-trigger wiring
#[derive(Debug, Clone)]
pub struct GatewayDevice {
    pub hc_id: String,
    pub mac: String,
}

pub type GatewayDeviceState = Arc<Mutex<Option<GatewayDevice>>>;

/// Build the empty cell the poller writes its registration into.
pub fn new_state() -> GatewayDeviceState {
    Arc::new(Mutex::new(None))
}

/// Drive one refresh cycle: pull /get_device_info, /get_version,
/// /get_units_info, /get_network_info, and the customserver settings;
/// flatten them into a single attributes-friendly state map; register
/// the virtual device on first sight; publish state.
///
/// Errors from individual endpoints are logged and the cycle continues
/// with whatever data did come back — partial gateways (older firmware
/// missing some endpoints) still get a meaningful device record.
pub async fn refresh(
    publisher: &DevicePublisher,
    prefix: &str,
    plugin_id: &str,
    ip: &str,
    password: &str,
    state: &GatewayDeviceState,
) -> Result<()> {
    // /get_device_info is the cheapest "is this even a gateway" probe.
    // GW2000 firmware embeds `sta_mac` here; GW1100 doesn't, so we
    // fall back to `/get_network_info` which exposes `mac` on every
    // firmware rev we've seen. If both fail we abandon the cycle —
    // the rest of the endpoints would just pile up errors against
    // an unreachable host.
    let device_info_url = format!("http://{ip}/get_device_info");
    let device_info = http_get_json(&device_info_url).await?;

    // Probe network info upfront — both for MAC fallback and to reuse
    // its fields below for the network.* attributes.
    let network_url = format!("http://{ip}/get_network_info");
    let network_info = http_get_json(&network_url).await.ok();

    let mac = device_info
        .get("sta_mac")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            network_info
                .as_ref()
                .and_then(|v| v.get("mac"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            device_info
                .get("mac")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if mac.is_empty() {
        anyhow::bail!("no MAC in /get_device_info or /get_network_info; cannot key gateway device");
    }
    let hc_id = format!("{prefix}_gw_{}", normalize_mac(&mac));

    // First-cycle: register the device. We use device_type "gateway"
    // — same string the wider homeCore catalog uses for hubs/bridges.
    // The friendly name comes from the firmware's curr_msg / version
    // string when available, falling back to "Ecowitt Gateway".
    {
        let mut g = state.lock().await;
        if g.is_none() {
            let name = derive_gateway_name(&device_info);
            if let Err(e) = publisher
                .register_device_full(&hc_id, &name, Some("gateway"), None, None)
                .await
            {
                warn!(hc_id = %hc_id, error = %e, "register gateway device failed");
                return Err(e);
            }
            let _ = publisher.publish_availability(&hc_id, true).await;
            info!(plugin_id = %plugin_id, hc_id = %hc_id, mac = %mac, "Registered virtual gateway device");
            *g = Some(GatewayDevice {
                hc_id: hc_id.clone(),
                mac: mac.clone(),
            });
        }
    }

    // Build the attributes map. Order is fixed so the device page
    // renders predictably across refreshes.
    let mut attrs: Map<String, Value> = Map::new();
    attrs.insert("ip".into(), json!(ip));
    attrs.insert("mac".into(), json!(mac));
    if let Some(v) = device_info.get("date").and_then(Value::as_str) {
        attrs.insert("gateway_time".into(), json!(v));
    }
    if let Some(v) = device_info.get("tz_name").and_then(Value::as_str) {
        attrs.insert("timezone".into(), json!(v));
    }
    if let Some(v) = device_info
        .get("ntp_server")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        attrs.insert("ntp_server".into(), json!(v));
    }

    // /get_version: model + firmware, plus an "update available" flag.
    let version_url = format!("http://{ip}/get_version");
    if let Ok(v) = http_get_json(&version_url).await {
        if let Some(s) = v.get("version").and_then(Value::as_str) {
            attrs.insert("firmware".into(), json!(s.replace("Version: ", "")));
            // "GW1100B_V2.4.5" → model = "GW1100B"
            if let Some((model, _)) = s.replace("Version: ", "").split_once('_') {
                attrs.insert("model".into(), json!(model));
            }
        }
        if let Some(s) = v.get("platform").and_then(Value::as_str) {
            attrs.insert("platform".into(), json!(s));
        }
        if let Some(s) = v.get("newVersion").and_then(Value::as_str) {
            attrs.insert("update_available".into(), json!(s == "1"));
        }
    }

    // /get_units_info: numeric codes for the gateway's display units.
    // The codes are firmware-defined; we publish the raw numbers under
    // `units.*` and let the UI present them as-is. Mapping to friendly
    // labels is a follow-up — guessing wrong would be worse than
    // leaving them numeric.
    let units_url = format!("http://{ip}/get_units_info");
    if let Ok(v) = http_get_json(&units_url).await {
        for k in ["temperature", "pressure", "wind", "rain", "light"] {
            if let Some(s) = v.get(k).and_then(Value::as_str) {
                attrs.insert(format!("units.{k}"), json!(s));
            }
        }
    }

    // /get_network_info: WiFi station info (already fetched above for
    // MAC fallback; reuse it). wifi_pwd is base64-encoded on this
    // firmware and would leak the user's WiFi password into homeCore
    // state — drop it. Everything else is safe to surface.
    if let Some(v) = network_info.as_ref() {
        for (src, dst) in [
            ("ssid", "network.ssid"),
            ("wifi_ip", "network.ip"),
            ("wifi_mask", "network.netmask"),
            ("wifi_gateway", "network.gateway"),
            ("wifi_DNS", "network.dns"),
        ] {
            if let Some(s) = v.get(src).and_then(Value::as_str) {
                attrs.insert(dst.into(), json!(s));
            }
        }
    }

    // Customserver settings — dialect-aware fetch already normalizes
    // GW1100 vs GW2000 schema differences, so we get the same field
    // names regardless of firmware.
    if let Ok((dialect, normalized, _raw)) = fetch_custom_server(ip, password).await {
        attrs.insert(
            "customserver.dialect".into(),
            json!(match dialect {
                GwDialect::Modern => "customserver",
                GwDialect::Ws => "ws_settings",
            }),
        );
        for k in ["protocol", "enable", "server", "path", "port", "interval"] {
            if let Some(val) = normalized.get(k) {
                attrs.insert(format!("customserver.{k}"), val.clone());
            }
        }
    }

    publisher
        .publish_state(&hc_id, &Value::Object(attrs))
        .await?;
    debug!(hc_id = %hc_id, "gateway state refreshed");
    Ok(())
}

fn normalize_mac(mac: &str) -> String {
    mac.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn derive_gateway_name(device_info: &Value) -> String {
    // curr_msg looks like "Current version:V2.4.5\r\n- ...". We don't
    // get the model from /get_device_info directly, so fall back to a
    // generic name; /get_version refines it on the next attribute push.
    if let Some(s) = device_info.get("model").and_then(Value::as_str) {
        return s.to_string();
    }
    "Ecowitt Gateway".to_string()
}
