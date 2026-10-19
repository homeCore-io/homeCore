use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Method};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use super::models::{
    AccessoryCommand, BridgeTarget, HueAuxDevice, HueGroupedLight, HueLight, HueScene,
    LightCommand, SceneRecall,
};
use super::resources::{
    id_of, kind_of, name_of, owner_rid, product_of, white_swatch, xy_to_hex, Resources,
};

#[derive(Debug, Clone)]
pub enum EventstreamSignal {
    Refresh {
        bridge_id: String,
        reason: String,
    },
    Data {
        bridge_id: String,
        payload: serde_json::Value,
    },
    /// The stream connected (`true`) or dropped (`false`).
    Live {
        bridge_id: String,
        live: bool,
    },
}

#[derive(Debug, Clone)]
pub struct HueApiClient {
    target: BridgeTarget,
    app_key: Arc<RwLock<Option<String>>>,
    client: Client,
}

impl HueApiClient {
    pub fn new(target: BridgeTarget) -> Self {
        let app_key = Arc::new(RwLock::new(target.app_key.clone()));
        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .danger_accept_invalid_certs(target.allow_self_signed)
            .build()
            .expect("failed to build Hue HTTP client");
        debug!(bridge_id = %target.bridge_id, "Hue HTTP client initialized");
        Self {
            target,
            app_key,
            client,
        }
    }

    pub fn target(&self) -> &BridgeTarget {
        &self.target
    }

    pub fn has_app_key(&self) -> bool {
        self.current_app_key().is_some()
    }

    fn current_app_key(&self) -> Option<String> {
        self.app_key.read().ok().and_then(|v| v.clone())
    }

    /// Adopt a newly-learned app key. The key lives behind a lock precisely so
    /// a running client can be authorized in place — pairing a bridge that was
    /// already discovered (unpaired) must take effect now, not on next restart.
    pub(crate) fn set_app_key(&self, app_key: String) {
        if let Ok(mut slot) = self.app_key.write() {
            *slot = Some(app_key);
        }
    }

    pub async fn fetch_bridge_summary(&self) -> Result<serde_json::Value> {
        let client = self.http_client();
        let mut summary = json!({
            "bridge_id": self.target.bridge_id,
            "host": self.target.host,
            "auth_configured": self.has_app_key(),
            "verify_tls": self.target.verify_tls,
            "allow_self_signed": self.target.allow_self_signed,
        });

        if let Some(app_key) = self.current_app_key() {
            let url = format!("https://{}/clip/v2/resource/bridge", self.target.host);
            let response = client
                .get(url)
                .header("hue-application-key", &app_key)
                .send()
                .await
                .context("request to Hue bridge v2 endpoint failed")?;

            if response.status().is_success() {
                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .context("failed to parse Hue bridge v2 response")?;
                summary["integration_state"] = json!("connected");
                summary["v2_bridge"] = body;
                return Ok(summary);
            }

            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue bridge v2 request failed: status={status} body={text}");
        }

        // No app key configured yet: query legacy public config as a pairing helper.
        let config_url_https = format!("https://{}/api/config", self.target.host);
        let config_https = client.get(config_url_https).send().await;

        let config_resp = match config_https {
            Ok(resp) if resp.status().is_success() => Some(resp),
            _ => {
                let config_url_http = format!("http://{}/api/config", self.target.host);
                match client.get(config_url_http).send().await {
                    Ok(resp) if resp.status().is_success() => Some(resp),
                    _ => None,
                }
            }
        };

        if let Some(resp) = config_resp {
            let body = resp
                .json::<serde_json::Value>()
                .await
                .context("failed to parse Hue bridge config response")?;
            summary["integration_state"] = json!("auth_required");
            summary["bridge_config"] = body;
            return Ok(summary);
        }

        summary["integration_state"] = json!("unreachable");
        Ok(summary)
    }

    pub async fn pair_bridge(&self, device_type: &str) -> Result<String> {
        let client = self.http_client();
        let payload = json!({
            "devicetype": device_type,
            "generateclientkey": true,
        });

        let urls = [
            format!("https://{}/api", self.target.host),
            format!("http://{}/api", self.target.host),
        ];

        let mut last_err: Option<String> = None;

        for url in urls {
            let response = match client.post(&url).json(&payload).send().await {
                Ok(resp) => resp,
                Err(err) => {
                    last_err = Some(format!("request failed: {err}"));
                    continue;
                }
            };

            let status = response.status();
            let body = response
                .json::<serde_json::Value>()
                .await
                .context("failed to parse Hue pairing response")?;

            if !status.is_success() {
                last_err = Some(format!("status={status} body={body}"));
                continue;
            }

            let Some(results) = body.as_array() else {
                last_err = Some(format!("unexpected pairing response: {body}"));
                continue;
            };

            for result in results {
                if let Some(username) = result
                    .get("success")
                    .and_then(|v| v.get("username"))
                    .and_then(|v| v.as_str())
                {
                    let app_key = username.to_string();
                    self.set_app_key(app_key.clone());
                    return Ok(app_key);
                }

                if let Some(description) = result
                    .get("error")
                    .and_then(|v| v.get("description"))
                    .and_then(|v| v.as_str())
                {
                    bail!("Hue pairing failed: {description}");
                }
            }

            last_err = Some(format!("pairing did not return success username: {body}"));
        }

        bail!(
            "Hue pairing request failed: {}",
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )
    }

    pub async fn execute_raw_command(&self, payload: &serde_json::Value) -> Result<()> {
        if payload.is_null() {
            bail!("null payload is not a valid Hue command");
        }

        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("PUT")
            .to_uppercase();
        let method =
            Method::from_bytes(method.as_bytes()).context("invalid HTTP method in raw command")?;

        let path = payload
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("raw command requires string field 'path'"))?;

        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            if !path.starts_with('/') {
                bail!("raw command path must start with '/' or be a full URL");
            }
            format!("https://{}{}", self.target.host, path)
        };

        let client = self.http_client();
        let mut req = client.request(method, &url);

        if let Some(app_key) = self.current_app_key() {
            req = req.header("hue-application-key", app_key);
        }

        if let Some(body) = payload.get("body") {
            req = req.json(body);
        }

        let response = req.send().await.context("raw command request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("raw command failed: status={status} body={text}");
        }

        Ok(())
    }

    /// Everything the bridge knows, in one request — see [`Resources`].
    pub async fn fetch_all(&self) -> Result<Resources> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot read the Hue bridge without app_key configured");
        };
        let url = format!("https://{}/clip/v2/resource", self.target.host);
        let body = self
            .http_client()
            .get(url)
            .header("hue-application-key", &app_key)
            .send()
            .await
            .context("request to the Hue bridge failed")?
            .error_for_status()
            .context("the Hue bridge refused the request")?
            .json::<serde_json::Value>()
            .await
            .context("failed parsing the Hue bridge's resources")?;
        Ok(Resources::from_body(&body))
    }

    /// The bridge's summary, as `fetch_bridge_summary` gives it, from a
    /// snapshot already read.
    pub fn summary_from(&self, r: &Resources) -> serde_json::Value {
        json!({
            "bridge_id": self.target.bridge_id,
            "host": self.target.host,
            "auth_configured": true,
            "verify_tls": self.target.verify_tls,
            "allow_self_signed": self.target.allow_self_signed,
            "integration_state": "connected",
            "v2_bridge": { "errors": [], "data": r.of("bridge") },
        })
    }

    pub fn lights(&self, r: &Resources) -> Vec<HueLight> {
        let rooms = r.rooms_by_device();
        let mut lights = Vec::new();
        for item in r.of("light") {
            let Some(rid) = id_of(item) else { continue };
            let owner_rid = owner_rid(item).unwrap_or(rid);
            let device = r.owner_device(item);
            let name = device
                .and_then(name_of)
                .or_else(|| name_of(item))
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("Hue Light {}", rid.chars().take(8).collect::<String>())
                });
            let product = device.map(product_of);
            let reachable = device
                .and_then(|d| r.service_of(d, "zigbee_connectivity"))
                .and_then(|z| z.get("status"))
                .and_then(|v| v.as_str())
                .map(|s| s == "connected");

            let ct = item.get("color_temperature");
            let mirek_bound = |k: &str| {
                ct.and_then(|c| c.get("mirek_schema"))
                    .and_then(|s| s.get(k))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16)
            };
            let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
                v.and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            // `effects_v2` is the newer list (and the only one with the newest
            // effects); `effects` is what older firmware has. Either will do.
            let mut effect_values = strings(
                item.get("effects_v2")
                    .and_then(|e| e.get("action"))
                    .and_then(|a| a.get("effect_values")),
            );
            if effect_values.is_empty() {
                effect_values = strings(item.get("effects").and_then(|e| e.get("effect_values")));
            }
            let effect = item
                .get("effects_v2")
                .and_then(|e| e.get("status"))
                .and_then(|s| s.get("effect"))
                .or_else(|| item.get("effects").and_then(|e| e.get("status")))
                .or_else(|| item.get("effects").and_then(|e| e.get("effect")))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let xy = |v: &serde_json::Value| -> Option<(f64, f64)> {
                Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?))
            };
            let gradient = item.get("gradient");

            lights.push(HueLight {
                bridge_id: self.target.bridge_id.clone(),
                owner_rid: owner_rid.to_string(),
                area: rooms
                    .get(owner_rid)
                    .and_then(|room| name_of(room))
                    .map(Self::slugify)
                    .filter(|a| !a.is_empty()),
                resource_id: rid.to_string(),
                device_id: self.target.light_device_id(rid),
                name,
                on: item
                    .get("on")
                    .and_then(|o| o.get("on"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                brightness_pct: item
                    .get("dimming")
                    .and_then(|d| d.get("brightness"))
                    .and_then(|v| v.as_f64()),
                supports_dimming: item.get("dimming").is_some(),
                color_temp_mirek: ct
                    .filter(|c| c.get("mirek_valid").and_then(|v| v.as_bool()) != Some(false))
                    .and_then(|c| c.get("mirek"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16),
                mirek_min: mirek_bound("mirek_minimum"),
                mirek_max: mirek_bound("mirek_maximum"),
                supports_color_temp: ct.is_some(),
                color_xy: item.get("color").and_then(|c| c.get("xy")).and_then(xy),
                supports_color_xy: item.get("color").is_some(),
                effect,
                effect_values,
                dynamic_status: item
                    .get("dynamics")
                    .and_then(|d| d.get("status"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                dynamic_speed: item
                    .get("dynamics")
                    .and_then(|d| d.get("speed"))
                    .and_then(|v| v.as_f64()),
                gradient_points: gradient
                    .and_then(|g| g.get("points"))
                    .and_then(|v| v.as_array())
                    .map(|ps| {
                        ps.iter()
                            .filter_map(|p| xy(p.get("color")?.get("xy")?))
                            .collect()
                    })
                    .unwrap_or_default(),
                supports_gradient: gradient.is_some(),
                gradient_points_capable: gradient
                    .and_then(|g| g.get("points_capable"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u8,
                supports_identify: item.get("identify").is_some(),
                signal_values: strings(item.get("signaling").and_then(|s| s.get("signal_values"))),
                model_id: product
                    .as_ref()
                    .and_then(|p| p.model_id)
                    .map(str::to_string),
                product_name: product
                    .as_ref()
                    .and_then(|p| p.product_name)
                    .map(str::to_string),
                archetype: product
                    .as_ref()
                    .and_then(|p| p.archetype)
                    .map(str::to_string),
                software_version: product
                    .as_ref()
                    .and_then(|p| p.software_version)
                    .map(str::to_string),
                update_state: device.and_then(|d| r.update_state(d)).map(str::to_string),
                reachable,
                room_rid: rooms
                    .get(owner_rid)
                    .and_then(|r| id_of(r))
                    .map(str::to_string),
                room_name: rooms
                    .get(owner_rid)
                    .and_then(|r| name_of(r))
                    .map(str::to_string),
            });
        }
        lights
    }

    /// Rooms and zones as groups of lights: one switch, one dimmer, one
    /// colour for all of them, and the place their scenes belong.
    pub fn grouped_lights(&self, r: &Resources) -> Vec<HueGroupedLight> {
        let mut groups = Vec::new();
        for item in r.of("grouped_light") {
            let Some(rid) = id_of(item) else { continue };
            let owner = owner_rid(item).and_then(|o| r.get(o));
            let kind = owner
                .and_then(kind_of)
                .filter(|k| *k == "room" || *k == "zone");
            // The bridge's own "all lights" group belongs to bridge_home; it
            // is published (or not) as bridge_home, never as a room.
            let Some(kind) = kind else { continue };
            let owner = owner.expect("kind came from the owner");
            let name = name_of(owner).map(str::to_string).unwrap_or_else(|| {
                format!("Hue Group {}", rid.chars().take(8).collect::<String>())
            });
            let light_rids = r.lights_in(owner);
            let member = |rid: &&str| r.get(rid);
            groups.push(HueGroupedLight {
                bridge_id: self.target.bridge_id.clone(),
                resource_id: rid.to_string(),
                device_id: self.target.group_device_id(rid),
                area: Some(Self::slugify(&name)).filter(|a| !a.is_empty()),
                name,
                group_kind: Some(kind.to_string()),
                on: item
                    .get("on")
                    .and_then(|o| o.get("on"))
                    .and_then(|v| v.as_bool()),
                brightness_pct: item
                    .get("dimming")
                    .and_then(|d| d.get("brightness"))
                    .and_then(|v| v.as_f64()),
                owner_rid: id_of(owner).map(str::to_string),
                supports_color: light_rids
                    .iter()
                    .filter_map(member)
                    .any(|l| l.get("color").is_some()),
                supports_color_temp: light_rids
                    .iter()
                    .filter_map(member)
                    .any(|l| l.get("color_temperature").is_some()),
                light_ids: light_rids
                    .iter()
                    .map(|l| self.target.light_device_id(l))
                    .collect(),
            });
        }
        groups
    }

    pub fn scenes(&self, r: &Resources) -> Vec<HueScene> {
        let mut scenes = Vec::new();
        for item in r.of("scene") {
            let Some(rid) = id_of(item) else { continue };
            let group_rid = item
                .get("group")
                .and_then(|g| g.get("rid"))
                .and_then(|v| v.as_str());
            let group = group_rid.and_then(|g| r.get(g));
            let group_name = group.and_then(name_of).map(str::to_string);
            // Hue reports scene `status.active` as a string enum —
            // "inactive" | "static" | "dynamic_palette" — not a bool. A scene
            // is applied when it's anything other than "inactive".
            let status = item
                .get("status")
                .and_then(|s| s.get("active"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            scenes.push(HueScene {
                bridge_id: self.target.bridge_id.clone(),
                resource_id: rid.to_string(),
                device_id: self.target.scene_device_id(rid),
                name: name_of(item).unwrap_or("Unnamed Scene").to_string(),
                area: group_name.as_deref().map(Self::slugify),
                group_kind: group.and_then(kind_of).map(str::to_string),
                active: status.as_deref().map(|s| s != "inactive"),
                group_rid: group_rid.map(str::to_string),
                group_name,
                palette: scene_palette(item),
                status,
                dynamic_capable: item
                    .get("palette")
                    .and_then(|p| p.get("color"))
                    .and_then(|c| c.as_array())
                    .is_some_and(|c| c.len() > 1),
                smart: false,
            });
        }
        // Smart scenes: a scene per time of day. Their colours are the first
        // of each scene they move through.
        for item in r.of("smart_scene") {
            let Some(rid) = id_of(item) else { continue };
            let group_rid = item
                .get("group")
                .and_then(|g| g.get("rid"))
                .and_then(|v| v.as_str());
            let group = group_rid.and_then(|g| r.get(g));
            let group_name = group.and_then(name_of).map(str::to_string);
            let mut palette: Vec<String> = Vec::new();
            for slot in item
                .get("week_timeslots")
                .and_then(|w| w.as_array())
                .and_then(|w| w.first())
                .and_then(|w| w.get("timeslots"))
                .and_then(|t| t.as_array())
                .into_iter()
                .flatten()
            {
                let target = slot
                    .get("target")
                    .and_then(|t| t.get("rid"))
                    .and_then(|v| v.as_str())
                    .and_then(|t| r.get(t));
                if let Some(first) = target.map(scene_palette).and_then(|p| p.into_iter().next()) {
                    if !palette.contains(&first) {
                        palette.push(first);
                    }
                }
            }
            let state = item
                .get("state")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            scenes.push(HueScene {
                bridge_id: self.target.bridge_id.clone(),
                resource_id: rid.to_string(),
                device_id: self.target.scene_device_id(rid),
                name: name_of(item).unwrap_or("Smart scene").to_string(),
                area: group_name.as_deref().map(Self::slugify),
                group_kind: group.and_then(kind_of).map(str::to_string),
                active: state.as_deref().map(|s| s == "active"),
                group_rid: group_rid.map(str::to_string),
                group_name,
                palette,
                status: state,
                dynamic_capable: false,
                smart: true,
            });
        }
        scenes
    }

    pub async fn activate_smart_scene(&self, rid: &str, on: bool) -> Result<()> {
        let action = if on { "activate" } else { "deactivate" };
        self.put(
            &format!("smart_scene/{rid}"),
            json!({ "recall": { "action": action } }),
        )
        .await
        .context("Hue smart scene failed")
    }

    pub async fn rename_scene(&self, rid: &str, name: &str) -> Result<()> {
        self.put(
            &format!("scene/{rid}"),
            json!({ "metadata": { "name": name } }),
        )
        .await
        .context("renaming the Hue scene failed")
    }

    pub async fn delete_scene(&self, rid: &str) -> Result<()> {
        self.send_clip(reqwest::Method::DELETE, &format!("scene/{rid}"), None)
            .await
            .context("deleting the Hue scene failed")
    }

    /// Save how a room's or zone's lights are now as a new scene in it.
    pub async fn save_scene(&self, group_rid: &str, name: &str) -> Result<()> {
        let r = self.fetch_all().await?;
        let group = r
            .get(group_rid)
            .ok_or_else(|| anyhow::anyhow!("that room or zone is no longer on the bridge"))?;
        let kind = kind_of(group).unwrap_or("room").to_string();
        let actions: Vec<_> = r
            .lights_in(group)
            .iter()
            .filter_map(|rid| r.get(rid))
            .map(|l| {
                let mut a = serde_json::Map::new();
                let on = l
                    .get("on")
                    .and_then(|o| o.get("on"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                a.insert("on".into(), json!({ "on": on }));
                if on {
                    if let Some(b) = l.get("dimming").and_then(|d| d.get("brightness")) {
                        a.insert("dimming".into(), json!({ "brightness": b }));
                    }
                    let ct_valid = l
                        .get("color_temperature")
                        .and_then(|c| c.get("mirek_valid"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if let Some(points) = l
                        .get("gradient")
                        .and_then(|g| g.get("points"))
                        .filter(|p| p.as_array().is_some_and(|a| a.len() > 1))
                    {
                        a.insert("gradient".into(), json!({ "points": points }));
                    } else if ct_valid {
                        if let Some(m) = l.get("color_temperature").and_then(|c| c.get("mirek")) {
                            a.insert("color_temperature".into(), json!({ "mirek": m }));
                        }
                    } else if let Some(xy) = l.get("color").and_then(|c| c.get("xy")) {
                        a.insert("color".into(), json!({ "xy": xy }));
                    }
                }
                json!({ "target": { "rid": id_of(l), "rtype": "light" }, "action": a })
            })
            .collect();
        if actions.is_empty() {
            bail!("that room or zone has no lights to save");
        }
        let body = json!({
            "type": "scene",
            "metadata": { "name": name },
            "group": { "rid": group_rid, "rtype": kind },
            "actions": actions,
        });
        self.send_clip(reqwest::Method::POST, "scene", Some(body))
            .await
            .context("saving the Hue scene failed")
    }

    async fn send_clip(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control the Hue bridge without app_key configured");
        };
        let url = format!("https://{}/clip/v2/resource/{path}", self.target.host);
        let mut req = self
            .http_client()
            .request(method, url)
            .header("hue-application-key", &app_key);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let response = req.send().await.context("request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("status={status} body={text}");
        }
        Ok(())
    }

    pub fn aux_devices(&self, r: &Resources) -> Vec<HueAuxDevice> {
        let rooms = r.rooms_by_device();
        let resource_types = [
            "motion",
            "grouped_motion",
            "temperature",
            "light_level",
            "grouped_light_level",
            "contact",
            "device_power",
            // Keep as an aux feed but compact onto owner devices in sync.rs
            // so connectivity is rule-addressable without extra standalone devices.
            "zigbee_connectivity",
            "button",
            "relative_rotary",
            "entertainment_configuration",
            "bridge_home",
        ];
        let buttons = button_presses(r);

        let mut out = Vec::new();
        for resource_type in resource_types {
            for item in r.of(resource_type) {
                let Some(rid) = id_of(item) else { continue };
                let owner_rid = owner_rid(item).unwrap_or(rid);
                let owner = r.get(owner_rid);
                // An entertainment area is owned by the bridge's entertainment
                // service, not a device, so its own name is the one to use.
                let name = if resource_type == "entertainment_configuration" {
                    name_of(item).or_else(|| item.get("name").and_then(|v| v.as_str()))
                } else {
                    owner
                        .filter(|o| kind_of(o) == Some("device"))
                        .and_then(name_of)
                }
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Hue {} {}",
                        resource_type,
                        rid.chars().take(8).collect::<String>()
                    )
                });

                let mut attributes = self.extract_aux_attributes(resource_type, item);
                if resource_type == "button" {
                    if let (Some(owner), Some(obj)) =
                        (buttons.get(owner_rid), attributes.as_object_mut())
                    {
                        obj.extend(owner.clone());
                    }
                }
                if let Some(state) = owner.and_then(|o| r.update_state(o)) {
                    if let Some(obj) = attributes.as_object_mut() {
                        obj.insert("update_state".to_string(), json!(state));
                    }
                }
                out.push(HueAuxDevice {
                    bridge_id: self.target.bridge_id.clone(),
                    area: rooms
                        .get(owner_rid)
                        .and_then(|room| name_of(room))
                        .map(Self::slugify)
                        .filter(|a| !a.is_empty()),
                    owner_rid: owner_rid.to_string(),
                    resource_type: resource_type.to_string(),
                    resource_id: rid.to_string(),
                    device_id: self.target.aux_device_id(resource_type, rid),
                    name,
                    attributes,
                });
            }
        }
        out
    }

    /// Rooms and zones, for the plugin page: what each holds and what it is
    /// called, whether or not it is published as a device.
    pub fn rooms(&self, r: &Resources) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for kind in ["room", "zone"] {
            for g in r.of(kind) {
                let Some(rid) = id_of(g) else { continue };
                let grouped = g
                    .get("services")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
                    .find(|s| s.get("rtype").and_then(|v| v.as_str()) == Some("grouped_light"))
                    .and_then(|s| s.get("rid"))
                    .and_then(|v| v.as_str());
                out.push(json!({
                    "id": rid,
                    "kind": kind,
                    "name": name_of(g).unwrap_or(rid),
                    "bridge_id": self.target.bridge_id,
                    "lights": r.lights_in(g).len(),
                    "light_ids": r.lights_in(g).iter().map(|l| self.target.light_device_id(l)).collect::<Vec<_>>(),
                    "scenes": r.of("scene").iter().filter(|s| {
                        s.get("group").and_then(|g| g.get("rid")).and_then(|v| v.as_str()) == Some(rid)
                    }).count(),
                    "device_id": grouped.map(|g| self.target.group_device_id(g)),
                }));
            }
        }
        out
    }

    pub async fn execute_light_command(
        &self,
        light_rid: &str,
        command: &LightCommand,
    ) -> Result<()> {
        let body = light_body(command, true);
        if body.is_empty() {
            bail!("no writable fields for light command");
        }
        self.put(
            &format!("light/{light_rid}"),
            serde_json::Value::Object(body),
        )
        .await
        .context("Hue light command failed")
    }

    pub async fn execute_grouped_light_command(
        &self,
        group_rid: &str,
        command: &LightCommand,
    ) -> Result<()> {
        let body = light_body(command, false);
        if body.is_empty() {
            bail!("no writable fields for grouped_light command");
        }
        self.put(
            &format!("grouped_light/{group_rid}"),
            serde_json::Value::Object(body),
        )
        .await
        .context("Hue grouped_light command failed")
    }

    /// `PUT /clip/v2/resource/{path}`; anything but 2xx is an error carrying
    /// what the bridge said.
    async fn put(&self, path: &str, body: serde_json::Value) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control the Hue bridge without app_key configured");
        };
        let url = format!("https://{}/clip/v2/resource/{path}", self.target.host);
        let response = self
            .http_client()
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&body)
            .send()
            .await
            .context("request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("status={status} body={text}");
        }
        Ok(())
    }

    pub async fn execute_accessory_command(
        &self,
        resource_type: &str,
        resource_rid: &str,
        command: &AccessoryCommand,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control Hue accessory without app_key configured");
        };

        if command.motion_sensitivity.is_some() && resource_type != "motion" {
            bail!(
                "motion_sensitivity is only supported for motion resources (target type: {resource_type})"
            );
        }

        let mut body = serde_json::Map::new();

        match resource_type {
            "motion" => {
                if let Some(enabled) = command.enabled {
                    body.insert("enabled".to_string(), json!(enabled));
                }
                if let Some(sensitivity) = command.motion_sensitivity {
                    body.insert(
                        "sensitivity".to_string(),
                        json!({ "sensitivity": sensitivity }),
                    );
                }
            }
            "temperature" | "light_level" | "contact" => {
                if let Some(enabled) = command.enabled {
                    body.insert("enabled".to_string(), json!(enabled));
                }
            }
            _ => {
                bail!("unsupported accessory resource type for write command: {resource_type}");
            }
        }

        if body.is_empty() {
            bail!("no writable fields in accessory command for resource type: {resource_type}");
        }

        let client = self.http_client();
        let url = format!(
            "https://{}/clip/v2/resource/{resource_type}/{resource_rid}",
            self.target.host
        );
        let response = client
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .context("Hue accessory command request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue accessory command failed: status={status} body={text}");
        }

        Ok(())
    }

    pub async fn execute_entertainment_command(
        &self,
        config_rid: &str,
        active: bool,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control Hue entertainment without app_key configured");
        };

        let action = if active { "start" } else { "stop" };
        let client = self.http_client();
        let url = format!(
            "https://{}/clip/v2/resource/entertainment_configuration/{config_rid}",
            self.target.host
        );
        let response = client
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&json!({ "action": action }))
            .send()
            .await
            .context("Hue entertainment command request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue entertainment command failed: status={status} body={text}");
        }

        Ok(())
    }

    pub fn slugify(input: &str) -> String {
        input
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string()
    }

    pub async fn activate_scene(&self, scene_rid: &str, recall: &SceneRecall) -> Result<()> {
        let mut r = json!({ "action": if recall.dynamic { "dynamic_palette" } else { "active" } });
        if let Some(ms) = recall.duration_ms {
            r["duration"] = json!(ms);
        }
        if let Some(b) = recall.brightness_pct {
            r["dimming"] = json!({ "brightness": b.clamp(0.0, 100.0) });
        }
        self.put(&format!("scene/{scene_rid}"), json!({ "recall": r }))
            .await
            .context("Hue scene activation failed")
    }

    /// Follow the bridge's event stream for as long as the runtime listens,
    /// reconnecting after `reconnect_secs` whenever it drops. Says when the
    /// stream is up and when it goes, so the runtime knows whether it can
    /// trust what it hears or has to ask.
    ///
    /// Every `data:` line is passed on. They used to be dropped when one came
    /// within half a second of the last, which is exactly when a scene recall
    /// reports its lights — the changes were lost until the next resync.
    pub async fn run_eventstream(
        &self,
        notify_tx: mpsc::Sender<EventstreamSignal>,
        reconnect_secs: u64,
    ) -> Result<()> {
        let sleep_dur = Duration::from_secs(reconnect_secs.max(1));
        let bridge_id = self.target.bridge_id.clone();
        let live = |up: bool| EventstreamSignal::Live {
            bridge_id: bridge_id.clone(),
            live: up,
        };

        loop {
            let Some(app_key) = self.current_app_key() else {
                bail!("cannot start eventstream without app_key configured");
            };
            // The stream is meant to stay open; only the connection may time out.
            let client = Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .danger_accept_invalid_certs(self.target.allow_self_signed)
                .build()
                .unwrap_or_else(|_| self.http_client());
            let url = format!("https://{}/eventstream/clip/v2", self.target.host);
            let response = client
                .get(&url)
                .header("hue-application-key", &app_key)
                .header("Accept", "text/event-stream")
                .send()
                .await
                .ok()
                .and_then(|r| r.error_for_status().ok());

            let Some(response) = response else {
                tokio::time::sleep(sleep_dur).await;
                continue;
            };
            if notify_tx.send(live(true)).await.is_err() {
                return Ok(());
            }

            let mut stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    break;
                };
                buf.extend_from_slice(&chunk);
                // Split on whole lines only, so a UTF-8 character cut across
                // two chunks is put back together before it is read.
                while let Some(idx) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=idx).collect();
                    let line = String::from_utf8_lossy(&line);
                    let Some(payload) = line.trim().strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    if payload.is_empty() {
                        continue;
                    }
                    let signal = match serde_json::from_str::<serde_json::Value>(payload) {
                        Ok(value) => EventstreamSignal::Data {
                            bridge_id: bridge_id.clone(),
                            payload: value,
                        },
                        Err(_) => EventstreamSignal::Refresh {
                            bridge_id: bridge_id.clone(),
                            reason: "eventstream_parse_error".to_string(),
                        },
                    };
                    if notify_tx.send(signal).await.is_err() {
                        return Ok(());
                    }
                }
            }

            if notify_tx.send(live(false)).await.is_err() {
                return Ok(());
            }
            tokio::time::sleep(sleep_dur).await;
        }
    }

    fn extract_aux_attributes(
        &self,
        resource_type: &str,
        item: &serde_json::Value,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();

        match resource_type {
            "motion" | "grouped_motion" => {
                if let Some(v) = item
                    .get("motion")
                    .and_then(|m| m.get("motion"))
                    .and_then(|v| v.as_bool())
                {
                    out.insert("motion".to_string(), json!(v));
                }
                if let Some(v) = item.get("motion_valid").and_then(|v| v.as_bool()) {
                    out.insert("motion_valid".to_string(), json!(v));
                }
                if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
                    out.insert("enabled".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("sensitivity")
                    .and_then(|s| s.get("sensitivity"))
                    .and_then(|v| v.as_u64())
                {
                    out.insert("motion_sensitivity".to_string(), json!(v));
                }
                if resource_type == "grouped_motion" {
                    Self::insert_temperature_attributes(&mut out, item);
                    Self::insert_light_level_attributes(&mut out, item);
                }
            }
            "temperature" => {
                Self::insert_temperature_attributes(&mut out, item);
            }
            "light_level" | "grouped_light_level" => {
                Self::insert_light_level_attributes(&mut out, item);
            }
            "contact" => {
                if let Some(v) = item
                    .get("contact_report")
                    .and_then(|c| c.get("state"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("contact_state".to_string(), json!(v));
                }
                if let Some(v) = item.get("tampered").and_then(|v| v.as_bool()) {
                    out.insert("tampered".to_string(), json!(v));
                }
                if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
                    out.insert("enabled".to_string(), json!(v));
                }
            }
            "device_power" => {
                if let Some(v) = item
                    .get("battery_level")
                    .and_then(|v| v.as_f64())
                    .or_else(|| {
                        item.get("power_state")
                            .and_then(|p| p.get("battery_level"))
                            .and_then(|v| v.as_f64())
                    })
                {
                    out.insert("battery_pct".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("battery_state")
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        item.get("power_state")
                            .and_then(|p| p.get("battery_state"))
                            .and_then(|v| v.as_str())
                    })
                {
                    out.insert("battery_state".to_string(), json!(v));
                }
            }
            "zigbee_connectivity" => {
                if let Some(v) = item.get("status").and_then(|v| v.as_str()) {
                    out.insert("connectivity_status".to_string(), json!(v));
                }
            }
            "button" => {
                // CLIP v2 nests the report under `button`; older firmware put
                // it at the top level.
                let report = item
                    .get("button")
                    .and_then(|b| b.get("button_report"))
                    .or_else(|| item.get("button_report"));
                if let Some(v) = report.and_then(|r| r.get("event")).and_then(|v| v.as_str()) {
                    out.insert("button_event".to_string(), json!(v));
                }
                if let Some(v) = report
                    .and_then(|r| r.get("updated"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("button_updated".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("button")
                    .and_then(|b| b.get("repeat_interval"))
                    .or_else(|| item.get("repeat_interval"))
                    .and_then(|v| v.as_u64())
                {
                    out.insert("button_repeat_interval_ms".to_string(), json!(v));
                }
            }
            "relative_rotary" => {
                let report = item
                    .get("relative_rotary")
                    .and_then(|r| r.get("rotary_report"))
                    .or_else(|| {
                        item.get("relative_rotary")
                            .and_then(|r| r.get("last_event"))
                    })
                    .or_else(|| item.get("rotary_report"));
                if let Some(v) = report
                    .and_then(|r| r.get("action"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_action".to_string(), json!(v));
                }
                let rotation = report.and_then(|r| r.get("rotation"));
                if let Some(v) = rotation
                    .and_then(|r| r.get("direction"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_direction".to_string(), json!(v));
                }
                if let Some(v) = rotation
                    .and_then(|r| r.get("steps"))
                    .and_then(|v| v.as_i64())
                {
                    out.insert("rotary_steps".to_string(), json!(v));
                }
                if let Some(v) = report
                    .and_then(|r| r.get("updated"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_updated".to_string(), json!(v));
                }
            }
            "entertainment_configuration" => {
                if let Some(v) = item
                    .get("status")
                    .and_then(|s| s.as_str().map(|s| s == "active"))
                    .or_else(|| {
                        item.get("status")
                            .and_then(|s| s.get("active"))
                            .and_then(|v| v.as_bool())
                    })
                {
                    out.insert("entertainment_active".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("status")
                    .and_then(|s| s.get("status"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("entertainment_status".to_string(), json!(v));
                }
                if let Some(v) = item.get("configuration_type").and_then(|v| v.as_str()) {
                    out.insert("entertainment_type".to_string(), json!(v));
                }
                if let Some(name) =
                    name_of(item).or_else(|| item.get("name").and_then(|v| v.as_str()))
                {
                    out.insert("entertainment_name".to_string(), json!(name));
                }
                if let Some(owner) = item
                    .get("owner")
                    .and_then(|v| v.get("rid"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("entertainment_owner".to_string(), json!(owner));
                }
                if let Some(channels) = item.get("channels").and_then(|v| v.as_array()) {
                    out.insert(
                        "entertainment_channel_count".to_string(),
                        json!(channels.len() as u32),
                    );
                }
                if let Some(segments) = item.get("segments").and_then(|v| v.as_array()) {
                    out.insert(
                        "entertainment_segment_count".to_string(),
                        json!(segments.len() as u32),
                    );
                }
                if let Some(proxy) = item.get("stream_proxy").and_then(|v| v.get("node")) {
                    out.insert("entertainment_proxy_type".to_string(), json!(proxy));
                }
            }
            "bridge_home" => {
                if let Some(v) = item
                    .get("children")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                {
                    out.insert("child_count".to_string(), json!(v));
                }
                if let Some(v) = item.get("id_v1").and_then(|v| v.as_str()) {
                    out.insert("id_v1".to_string(), json!(v));
                }
            }
            _ => {}
        }

        if out.is_empty() {
            // Fallback to raw payload for forward compatibility if the shape differs.
            if let Some(obj) = item.as_object() {
                return serde_json::Value::Object(obj.clone());
            }
        }

        serde_json::Value::Object(out)
    }

    fn insert_temperature_attributes(
        out: &mut serde_json::Map<String, serde_json::Value>,
        item: &serde_json::Value,
    ) {
        if let Some(v) = extract_temperature_c(item) {
            out.insert("temperature_c".to_string(), json!(v));
            out.insert("temperature_f".to_string(), json!((v * 9.0 / 5.0) + 32.0));
        }
        if let Some(v) = extract_temperature_valid(item) {
            out.insert("temperature_valid".to_string(), json!(v));
        }
        if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
            out.insert("enabled".to_string(), json!(v));
        }
        out.insert("temperature_unit".to_string(), json!("C"));
    }

    fn insert_light_level_attributes(
        out: &mut serde_json::Map<String, serde_json::Value>,
        item: &serde_json::Value,
    ) {
        if let Some(v) = extract_light_level_raw(item) {
            out.insert("illuminance_raw".to_string(), json!(v));
            if let Some(lux) = Self::light_level_to_lux(v) {
                out.insert("illuminance_lux".to_string(), json!(lux));
            }
        }
        if let Some(v) = extract_light_level_valid(item) {
            out.insert("illuminance_valid".to_string(), json!(v));
        }
        if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
            out.insert("enabled".to_string(), json!(v));
        }
        out.insert("illuminance_unit".to_string(), json!("lux"));
    }

    fn light_level_to_lux(raw: f64) -> Option<f64> {
        if raw <= 0.0 {
            return None;
        }
        // Hue light level is log-scaled in many APIs; this approximation is useful for automation thresholds.
        let lux = 10f64.powf((raw - 1.0) / 10000.0);
        if lux.is_finite() {
            Some(lux)
        } else {
            None
        }
    }

    fn http_client(&self) -> Client {
        self.client.clone()
    }
}

fn extract_temperature_c(item: &serde_json::Value) -> Option<f64> {
    let raw = item
        .get("temperature")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            item.get("temperature")
                .and_then(|obj| obj.get("temperature"))
                .and_then(|v| v.as_f64())
        })?;

    // Hue payloads may report centi-degrees (e.g. 2150 == 21.50 C).
    let normalized = if raw.abs() > 120.0 { raw / 100.0 } else { raw };
    Some(normalized)
}

fn extract_temperature_valid(item: &serde_json::Value) -> Option<bool> {
    item.get("temperature_valid")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            item.get("temperature")
                .and_then(|obj| obj.get("temperature_valid"))
                .and_then(|v| v.as_bool())
        })
}

fn extract_light_level_raw(item: &serde_json::Value) -> Option<f64> {
    item.get("light_level")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            item.get("light")
                .and_then(|obj| obj.get("light_level"))
                .and_then(|v| v.as_f64())
        })
}

fn extract_light_level_valid(item: &serde_json::Value) -> Option<bool> {
    item.get("light_level_valid")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            item.get("light")
                .and_then(|obj| obj.get("light_level_valid"))
                .and_then(|v| v.as_bool())
        })
}

/// The body of a light or grouped_light `PUT` for a command. A grouped light
/// takes the same fields bar the ones only a single lamp has.
fn light_body(command: &LightCommand, single: bool) -> serde_json::Map<String, serde_json::Value> {
    let mut body = serde_json::Map::new();
    let xy = |(x, y): (f64, f64)| json!({ "xy": { "x": x, "y": y } });
    if let Some(on) = command.on {
        body.insert("on".into(), json!({ "on": on }));
    }
    if let Some(brightness) = command.brightness_pct {
        body.insert(
            "dimming".into(),
            json!({ "brightness": brightness.clamp(0.0, 100.0) }),
        );
    }
    if let Some(step) = command.brightness_step.filter(|s| *s != 0.0) {
        body.insert(
            "dimming_delta".into(),
            json!({ "action": if step > 0.0 { "up" } else { "down" }, "brightness_delta": step.abs().min(100.0) }),
        );
    }
    if let Some(mirek) = command.color_temp_mirek {
        body.insert("color_temperature".into(), json!({ "mirek": mirek }));
    }
    if let Some(c) = command.color_xy {
        body.insert("color".into(), xy(c));
    }
    let mut dynamics = serde_json::Map::new();
    if let Some(ms) = command.transition_ms {
        dynamics.insert("duration".into(), json!(ms));
    }
    if single {
        if let Some(speed) = command.dynamic_speed {
            dynamics.insert("speed".into(), json!(speed.clamp(0.0, 1.0)));
        }
        if let Some(effect) = &command.effect {
            body.insert("effects".into(), json!({ "effect": effect }));
        }
        if let Some(points) = command.gradient_points.as_ref().filter(|p| !p.is_empty()) {
            let points: Vec<_> = points.iter().map(|p| json!({ "color": xy(*p) })).collect();
            body.insert("gradient".into(), json!({ "points": points }));
        }
        if command.identify == Some(true) {
            body.insert("identify".into(), json!({ "action": "identify" }));
        }
    }
    if !dynamics.is_empty() {
        body.insert("dynamics".into(), serde_json::Value::Object(dynamics));
    }
    if command.breathe {
        body.insert("alert".into(), json!({ "action": "breathe" }));
    }
    if let Some(signal) = &command.signal {
        let mut s = json!({ "signal": signal.kind, "duration": signal.duration_ms });
        if !signal.colors.is_empty() {
            s["colors"] = json!(signal.colors.iter().map(|c| xy(*c)).collect::<Vec<_>>());
        }
        body.insert("signaling".into(), s);
    }
    body
}

/// A scene's colours as hex: its palette when it has one, else what its
/// actions set each light to. Brightness is folded in only a little — a dim
/// scene should still read as its colour.
fn scene_palette(scene: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn add(out: &mut Vec<String>, hex: String) {
        if !out.contains(&hex) && out.len() < 5 {
            out.push(hex);
        }
    }
    let bri = |v: &serde_json::Value| {
        v.get("dimming")
            .and_then(|d| d.get("brightness"))
            .and_then(|b| b.as_f64())
            .unwrap_or(100.0)
    };
    let palette = scene.get("palette");
    for c in palette
        .and_then(|p| p.get("color"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(xy) = c.get("color").and_then(|c| c.get("xy")) {
            if let (Some(x), Some(y)) = (
                xy.get("x").and_then(|v| v.as_f64()),
                xy.get("y").and_then(|v| v.as_f64()),
            ) {
                add(&mut out, xy_to_hex(x, y, bri(c)));
            }
        }
    }
    for c in palette
        .and_then(|p| p.get("color_temperature"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(m) = c
            .get("color_temperature")
            .and_then(|c| c.get("mirek"))
            .and_then(|v| v.as_f64())
        {
            add(&mut out, white_swatch(m));
        }
    }
    if !out.is_empty() {
        return out;
    }
    for a in scene
        .get("actions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("action"))
    {
        if a.get("on")
            .and_then(|o| o.get("on"))
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            continue;
        }
        let xy = a
            .get("color")
            .and_then(|c| c.get("xy"))
            .and_then(|xy| Some((xy.get("x")?.as_f64()?, xy.get("y")?.as_f64()?)));
        if let Some((x, y)) = xy {
            add(&mut out, xy_to_hex(x, y, bri(a)));
        } else if let Some(m) = a
            .get("color_temperature")
            .and_then(|c| c.get("mirek"))
            .and_then(|v| v.as_f64())
        {
            add(&mut out, white_swatch(m));
        } else if a.get("dimming").is_some() {
            add(&mut out, white_swatch(366.0));
        }
    }
    out
}

/// What each button of a remote is called. A Hue dimmer switch's four buttons
/// are On, Brighter, Dimmer and Off (Hue on the newest one); anything else is
/// numbered, as the Hue app numbers it.
pub fn button_label(model_id: Option<&str>, control_id: u64) -> String {
    let named: &[&str] = match model_id {
        Some("RWL020" | "RWL021") => &["On", "Brighter", "Dimmer", "Off"],
        Some("RWL022") => &["On", "Brighter", "Dimmer", "Hue"],
        Some("ROM001" | "RDM003") => &["Button"],
        _ => &[],
    };
    named
        .get(control_id.saturating_sub(1) as usize)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Button {control_id}"))
}

/// What a press is called, from Hue's `button_report.event`. An automation
/// starts on a change, so a press runs "down" → "pressed" and a hold runs
/// "down" → "held" → "released": each press passes through "down", and a
/// second identical press still changes something.
pub fn press_word(event: &str) -> Option<&'static str> {
    Some(match event {
        "initial_press" => "down",
        "short_release" => "pressed",
        "long_press" | "repeat" => "held",
        "long_release" => "released",
        "double_short_release" => "double-pressed",
        _ => return None,
    })
}

/// Button rid → its label, for every button the bridge has.
pub fn button_labels(r: &Resources) -> HashMap<String, String> {
    r.of("button")
        .iter()
        .filter_map(|b| {
            let model = r.owner_device(b).map(product_of).and_then(|p| p.model_id);
            let control = b
                .get("metadata")
                .and_then(|m| m.get("control_id"))
                .and_then(|v| v.as_u64())?;
            Some((id_of(b)?.to_string(), button_label(model, control)))
        })
        .collect()
}

/// Per remote (device rid): which button was pressed last, and how — from the
/// newest `button_report` among its buttons — and what its buttons are called.
fn button_presses(r: &Resources) -> HashMap<String, serde_json::Map<String, serde_json::Value>> {
    let labels = button_labels(r);
    let mut by_device: HashMap<String, Vec<&serde_json::Value>> = HashMap::new();
    for b in r.of("button") {
        if let Some(owner) = owner_rid(b) {
            by_device.entry(owner.to_string()).or_default().push(b);
        }
    }
    let mut out = HashMap::new();
    for (device, mut buttons) in by_device {
        buttons.sort_by_key(|b| {
            b.get("metadata")
                .and_then(|m| m.get("control_id"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        });
        let mut m = serde_json::Map::new();
        let names: Vec<String> = buttons
            .iter()
            .filter_map(|b| labels.get(id_of(b)?).cloned())
            .collect();
        m.insert("buttons".into(), json!(names));
        let newest = buttons
            .iter()
            .filter_map(|b| {
                let report = b
                    .get("button")
                    .and_then(|x| x.get("button_report"))
                    .or_else(|| b.get("button_report"))?;
                Some((
                    report.get("updated")?.as_str()?,
                    report.get("event")?.as_str()?,
                    *b,
                ))
            })
            .max_by(|a, b| a.0.cmp(b.0));
        if let Some((updated, event, b)) = newest {
            if let (Some(label), Some(word)) =
                (id_of(b).and_then(|i| labels.get(i)), press_word(event))
            {
                m.insert("last_button".into(), json!(label));
                m.insert("last_press".into(), json!(format!("{label} {word}")));
                m.insert("last_pressed_at".into(), json!(updated));
            }
        }
        out.insert(device, m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use serde_json::Value;

    fn test_client() -> HueApiClient {
        HueApiClient::new(BridgeTarget {
            name: "test".to_string(),
            bridge_id: "bridge-1".to_string(),
            host: "127.0.0.1".to_string(),
            app_key: None,
            verify_tls: false,
            allow_self_signed: true,
        })
    }

    #[test]
    fn extracts_button_attributes() {
        let client = test_client();
        let item = json!({
            "button_report": { "event": "initial_press", "updated": "2026-03-21T12:00:00Z" },
            "repeat_interval": 120
        });

        let out = client.extract_aux_attributes("button", &item);
        assert_eq!(
            out.get("button_event").and_then(Value::as_str),
            Some("initial_press")
        );
        assert_eq!(
            out.get("button_updated").and_then(Value::as_str),
            Some("2026-03-21T12:00:00Z")
        );
        assert_eq!(
            out.get("button_repeat_interval_ms").and_then(Value::as_u64),
            Some(120)
        );
    }

    #[test]
    fn extracts_rotary_attributes() {
        let client = test_client();
        let item = json!({
            "rotary_report": {
                "action": "start",
                "rotation": { "direction": "clock_wise", "steps": 4 },
                "updated": "2026-03-21T12:01:00Z"
            }
        });

        let out = client.extract_aux_attributes("relative_rotary", &item);
        assert_eq!(
            out.get("rotary_action").and_then(Value::as_str),
            Some("start")
        );
        assert_eq!(
            out.get("rotary_direction").and_then(Value::as_str),
            Some("clock_wise")
        );
        assert_eq!(out.get("rotary_steps").and_then(Value::as_i64), Some(4));
        assert_eq!(
            out.get("rotary_updated").and_then(Value::as_str),
            Some("2026-03-21T12:01:00Z")
        );
    }

    #[test]
    fn extracts_entertainment_and_bridge_home_attributes() {
        let client = test_client();

        let entertainment = json!({
            "status": { "active": true, "status": "active" },
            "configuration_type": "screen",
            "name": "TV Area",
            "owner": { "rid": "owner-rid-1" },
            "channels": [{"channel_id": 1}, {"channel_id": 2}],
            "segments": [{"id": "seg1"}],
            "stream_proxy": { "node": "ent_proxy_v2" }
        });
        let out_ent = client.extract_aux_attributes("entertainment_configuration", &entertainment);
        assert_eq!(
            out_ent.get("entertainment_active").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            out_ent.get("entertainment_status").and_then(Value::as_str),
            Some("active")
        );
        assert_eq!(
            out_ent.get("entertainment_type").and_then(Value::as_str),
            Some("screen")
        );
        assert_eq!(
            out_ent.get("entertainment_name").and_then(Value::as_str),
            Some("TV Area")
        );
        assert_eq!(
            out_ent.get("entertainment_owner").and_then(Value::as_str),
            Some("owner-rid-1")
        );
        assert_eq!(
            out_ent
                .get("entertainment_channel_count")
                .and_then(Value::as_u64),
            Some(2)
        );
        assert_eq!(
            out_ent
                .get("entertainment_segment_count")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            out_ent
                .get("entertainment_proxy_type")
                .and_then(Value::as_str),
            Some("ent_proxy_v2")
        );

        let bridge_home = json!({
            "children": [{"rid":"a"}, {"rid":"b"}],
            "id_v1": "/bridge_home/1"
        });
        let out_home = client.extract_aux_attributes("bridge_home", &bridge_home);
        assert_eq!(out_home.get("child_count").and_then(Value::as_u64), Some(2));
        assert_eq!(
            out_home.get("id_v1").and_then(Value::as_str),
            Some("/bridge_home/1")
        );
    }

    #[test]
    fn extracts_nested_device_power_attributes() {
        let client = test_client();
        let item = json!({
            "power_state": {
                "battery_level": 100.0,
                "battery_state": "normal"
            }
        });

        let out = client.extract_aux_attributes("device_power", &item);
        assert_eq!(out.get("battery_pct").and_then(Value::as_f64), Some(100.0));
        assert_eq!(
            out.get("battery_state").and_then(Value::as_str),
            Some("normal")
        );
    }

    #[test]
    fn extracts_nested_temperature_and_light_level_attributes() {
        let client = test_client();

        let temperature_item = json!({
            "temperature": { "temperature": 2150.0, "temperature_valid": true },
            "enabled": true
        });
        let out_temp = client.extract_aux_attributes("temperature", &temperature_item);
        assert_eq!(
            out_temp.get("temperature_c").and_then(Value::as_f64),
            Some(21.5)
        );
        assert_eq!(
            out_temp.get("temperature_f").and_then(Value::as_f64),
            Some(70.7)
        );
        assert_eq!(
            out_temp.get("temperature_valid").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(out_temp.get("enabled").and_then(Value::as_bool), Some(true));

        let light_item = json!({
            "light": { "light_level": 19000.0, "light_level_valid": true },
            "enabled": true
        });
        let out_light = client.extract_aux_attributes("light_level", &light_item);
        assert_eq!(
            out_light.get("illuminance_raw").and_then(Value::as_f64),
            Some(19000.0)
        );
        assert_eq!(
            out_light.get("illuminance_valid").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            out_light.get("enabled").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            out_light.get("illuminance_unit").and_then(Value::as_str),
            Some("lux")
        );
        assert!(out_light
            .get("illuminance_lux")
            .and_then(Value::as_f64)
            .is_some());
    }

    #[test]
    fn extracts_grouped_motion_attributes() {
        let client = test_client();
        let item = json!({
            "motion": { "motion": true },
            "motion_valid": true,
            "temperature": { "temperature": 2150.0, "temperature_valid": true },
            "light": { "light_level": 19000.0, "light_level_valid": true },
            "enabled": true
        });

        let out = client.extract_aux_attributes("grouped_motion", &item);
        assert_eq!(out.get("motion").and_then(Value::as_bool), Some(true));
        assert_eq!(out.get("temperature_c").and_then(Value::as_f64), Some(21.5));
        assert_eq!(
            out.get("temperature_valid").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("illuminance_raw").and_then(Value::as_f64),
            Some(19000.0)
        );
        assert_eq!(
            out.get("illuminance_valid").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(out.get("enabled").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn extracts_grouped_light_level_attributes() {
        let client = test_client();
        let item = json!({
            "light": { "light_level": 17500.0, "light_level_valid": false },
            "enabled": false
        });

        let out = client.extract_aux_attributes("grouped_light_level", &item);
        assert_eq!(
            out.get("illuminance_raw").and_then(Value::as_f64),
            Some(17500.0)
        );
        assert_eq!(
            out.get("illuminance_valid").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(out.get("enabled").and_then(Value::as_bool), Some(false));
    }
}
