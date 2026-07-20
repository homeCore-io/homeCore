use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{Client, Method};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::models::{
    AccessoryCommand, BridgeTarget, HueAuxDevice, HueGroupedLight, HueLight, HueScene, LightCommand,
};

#[derive(Clone)]
struct GroupMeta {
    name: String,
    area: String,
    kind: String,
}

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

    pub async fn fetch_lights(&self) -> Result<Vec<HueLight>> {
        let Some(app_key) = self.current_app_key() else {
            return Ok(Vec::new());
        };

        let client = self.http_client();

        let devices_url = format!("https://{}/clip/v2/resource/device", self.target.host);
        let devices_payload = client
            .get(devices_url)
            .header("hue-application-key", &app_key)
            .send()
            .await
            .context("failed to fetch Hue devices")?
            .error_for_status()
            .context("Hue devices endpoint returned non-success")?
            .json::<serde_json::Value>()
            .await
            .context("failed parsing Hue devices payload")?;

        let mut device_names: HashMap<String, String> = HashMap::new();
        if let Some(items) = devices_payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let name = item
                    .get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(rid)
                    .to_string();
                device_names.insert(rid.to_string(), name);
            }
        }

        let device_area = self.fetch_device_areas(&client, &app_key).await;


        let lights_url = format!("https://{}/clip/v2/resource/light", self.target.host);
        let lights_payload = client
            .get(lights_url)
            .header("hue-application-key", &app_key)
            .send()
            .await
            .context("failed to fetch Hue lights")?
            .error_for_status()
            .context("Hue lights endpoint returned non-success")?
            .json::<serde_json::Value>()
            .await
            .context("failed parsing Hue lights payload")?;

        let mut lights = Vec::new();
        if let Some(items) = lights_payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };

                let owner_rid = item
                    .get("owner")
                    .and_then(|o| o.get("rid"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(rid);

                let name = device_names.get(owner_rid).cloned().unwrap_or_else(|| {
                    format!("Hue Light {}", rid.chars().take(8).collect::<String>())
                });

                let on = item
                    .get("on")
                    .and_then(|o| o.get("on"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let brightness_pct = item
                    .get("dimming")
                    .and_then(|d| d.get("brightness"))
                    .and_then(|v| v.as_f64());
                let supports_dimming = item.get("dimming").is_some();

                let color_temp_mirek = item
                    .get("color_temperature")
                    .and_then(|ct| ct.get("mirek"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16);

                let mirek_min = item
                    .get("color_temperature")
                    .and_then(|ct| ct.get("mirek_schema"))
                    .and_then(|s| s.get("mirek_minimum"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16);

                let mirek_max = item
                    .get("color_temperature")
                    .and_then(|ct| ct.get("mirek_schema"))
                    .and_then(|s| s.get("mirek_maximum"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u16);

                let color_xy = item.get("color").and_then(|c| c.get("xy")).and_then(|xy| {
                    let x = xy.get("x")?.as_f64()?;
                    let y = xy.get("y")?.as_f64()?;
                    Some((x, y))
                });

                let supports_color_xy = item.get("color").is_some();

                let effect = item
                    .get("effects")
                    .and_then(|e| e.get("effect"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);

                let effect_values = item
                    .get("effects")
                    .and_then(|e| e.get("effect_values"))
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(ToString::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                let dynamic_status = item
                    .get("dynamics")
                    .and_then(|d| d.get("status"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);

                let dynamic_speed = item
                    .get("dynamics")
                    .and_then(|d| d.get("speed"))
                    .and_then(|v| v.as_f64());

                let gradient_points = item
                    .get("gradient")
                    .and_then(|g| g.get("points"))
                    .and_then(|v| v.as_array())
                    .map(|points| {
                        points
                            .iter()
                            .filter_map(|point| {
                                let xy = point.get("color")?.get("xy")?;
                                let x = xy.get("x")?.as_f64()?;
                                let y = xy.get("y")?.as_f64()?;
                                Some((x, y))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let supports_gradient = item.get("gradient").is_some();
                let supports_identify = item.get("identify").is_some();

                lights.push(HueLight {
                    bridge_id: self.target.bridge_id.clone(),
                    owner_rid: owner_rid.to_string(),
                    area: device_area.get(owner_rid).cloned(),
                    resource_id: rid.to_string(),
                    device_id: self.target.light_device_id(rid),
                    name,
                    on,
                    brightness_pct,
                    supports_dimming,
                    color_temp_mirek,
                    mirek_min,
                    mirek_max,
                    color_xy,
                    supports_color_xy,
                    effect,
                    effect_values,
                    dynamic_status,
                    dynamic_speed,
                    gradient_points,
                    supports_gradient,
                    supports_identify,
                });
            }
        }

        Ok(lights)
    }

    pub async fn execute_light_command(
        &self,
        light_rid: &str,
        command: &LightCommand,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control Hue light without app_key configured");
        };

        let mut body = serde_json::Map::new();
        if let Some(on) = command.on {
            body.insert("on".to_string(), json!({ "on": on }));
        }
        if let Some(brightness) = command.brightness_pct {
            body.insert("dimming".to_string(), json!({ "brightness": brightness }));
        }
        if let Some(mirek) = command.color_temp_mirek {
            body.insert("color_temperature".to_string(), json!({ "mirek": mirek }));
        }
        if let Some((x, y)) = command.color_xy {
            body.insert(
                "color".to_string(),
                json!({
                    "xy": {
                        "x": x,
                        "y": y,
                    }
                }),
            );
        }
        if let Some(effect) = &command.effect {
            body.insert("effects".to_string(), json!({ "effect": effect }));
        }
        if let Some(speed) = command.dynamic_speed {
            body.insert(
                "dynamics".to_string(),
                json!({ "speed": speed.clamp(0.0, 1.0) }),
            );
        }
        if let Some(points) = &command.gradient_points {
            if !points.is_empty() {
                let points_payload = points
                    .iter()
                    .map(|(x, y)| {
                        json!({
                            "color": {
                                "xy": {
                                    "x": x,
                                    "y": y,
                                }
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                body.insert("gradient".to_string(), json!({ "points": points_payload }));
            }
        }
        if let Some(true) = command.identify {
            body.insert("identify".to_string(), json!({ "action": "identify" }));
        }

        if body.is_empty() {
            bail!("no writable fields for light command");
        }

        let client = self.http_client();
        let url = format!(
            "https://{}/clip/v2/resource/light/{light_rid}",
            self.target.host
        );
        let response = client
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .context("Hue light command request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue light command failed: status={status} body={text}");
        }

        Ok(())
    }

    pub async fn fetch_grouped_lights(&self) -> Result<Vec<HueGroupedLight>> {
        let Some(app_key) = self.current_app_key() else {
            return Ok(Vec::new());
        };

        let client = self.http_client();
        let mut group_meta: HashMap<String, GroupMeta> = HashMap::new();

        for kind in ["room", "zone"] {
            let url = format!("https://{}/clip/v2/resource/{kind}", self.target.host);
            let payload = client
                .get(url)
                .header("hue-application-key", &app_key)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await?;

            if let Some(items) = payload.get("data").and_then(|v| v.as_array()) {
                for item in items {
                    let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let name = item
                        .get("metadata")
                        .and_then(|m| m.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(rid)
                        .to_string();
                    group_meta.insert(
                        rid.to_string(),
                        GroupMeta {
                            area: Self::slugify(&name),
                            name,
                            kind: kind.to_string(),
                        },
                    );
                }
            }
        }

        let grouped_url = format!(
            "https://{}/clip/v2/resource/grouped_light",
            self.target.host
        );
        let grouped_payload = client
            .get(grouped_url)
            .header("hue-application-key", &app_key)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        let mut groups = Vec::new();
        if let Some(items) = grouped_payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };

                let owner_rid = item
                    .get("owner")
                    .and_then(|o| o.get("rid"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(rid);

                let meta = group_meta.get(owner_rid).cloned();
                let name = meta.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| {
                    format!("Hue Group {}", rid.chars().take(8).collect::<String>())
                });

                let on = item
                    .get("on")
                    .and_then(|o| o.get("on"))
                    .and_then(|v| v.as_bool());
                let brightness_pct = item
                    .get("dimming")
                    .and_then(|d| d.get("brightness"))
                    .and_then(|v| v.as_f64());

                groups.push(HueGroupedLight {
                    bridge_id: self.target.bridge_id.clone(),
                    resource_id: rid.to_string(),
                    device_id: self.target.group_device_id(rid),
                    name,
                    area: meta.as_ref().map(|m| m.area.clone()),
                    group_kind: meta.as_ref().map(|m| m.kind.clone()),
                    on,
                    brightness_pct,
                });
            }
        }

        Ok(groups)
    }

    pub async fn fetch_scenes(&self) -> Result<Vec<HueScene>> {
        let Some(app_key) = self.current_app_key() else {
            return Ok(Vec::new());
        };

        let client = self.http_client();
        let mut group_meta: HashMap<String, GroupMeta> = HashMap::new();

        for kind in ["room", "zone"] {
            let url = format!("https://{}/clip/v2/resource/{kind}", self.target.host);
            let payload = client
                .get(url)
                .header("hue-application-key", &app_key)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await?;

            if let Some(items) = payload.get("data").and_then(|v| v.as_array()) {
                for item in items {
                    let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let name = item
                        .get("metadata")
                        .and_then(|m| m.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(rid)
                        .to_string();
                    group_meta.insert(
                        rid.to_string(),
                        GroupMeta {
                            area: Self::slugify(&name),
                            name,
                            kind: kind.to_string(),
                        },
                    );
                }
            }
        }

        let scenes_url = format!("https://{}/clip/v2/resource/scene", self.target.host);
        let scenes_payload = client
            .get(scenes_url)
            .header("hue-application-key", &app_key)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        let mut scenes = Vec::new();
        if let Some(items) = scenes_payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };

                let name = item
                    .get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unnamed Scene")
                    .to_string();

                let group_rid = item
                    .get("group")
                    .and_then(|g| g.get("rid"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let group_name = group_rid
                    .as_ref()
                    .and_then(|rid| group_meta.get(rid).map(|m| m.name.clone()));
                let area = group_rid
                    .as_ref()
                    .and_then(|rid| group_meta.get(rid).map(|m| m.area.clone()));
                let group_kind = group_rid
                    .as_ref()
                    .and_then(|rid| group_meta.get(rid).map(|m| m.kind.clone()));
                let active = item
                    .get("status")
                    .and_then(|s| s.get("active"))
                    .and_then(|v| v.as_bool());

                scenes.push(HueScene {
                    bridge_id: self.target.bridge_id.clone(),
                    resource_id: rid.to_string(),
                    device_id: self.target.scene_device_id(rid),
                    name,
                    area,
                    group_kind,
                    active,
                    group_rid,
                    group_name,
                });
            }
        }

        Ok(scenes)
    }

    /// Map each Hue *device* rid to the room it sits in, slugified as a
    /// homeCore area, so anything owned by that device can carry the room and
    /// follow it when someone moves the device in the Hue app.
    ///
    /// Rooms only — deliberately not zones. In Hue a device belongs to at most
    /// one room (rooms are physical and exclusive), while zones are arbitrary
    /// overlapping groupings ("Downstairs", "Colour lights"), so a zone cannot
    /// answer "which room is this in" unambiguously. A room lists its devices
    /// as `children`.
    ///
    /// Shared by lights and accessories: a motion sensor is in a room just as
    /// much as a lamp is, and reading rooms only in the lights fetch left every
    /// sensor unassigned.
    async fn fetch_device_areas(
        &self,
        client: &Client,
        app_key: &str,
    ) -> HashMap<String, String> {
        let rooms_url = format!("https://{}/clip/v2/resource/room", self.target.host);
        let mut device_area: HashMap<String, String> = HashMap::new();
        match client
            .get(rooms_url)
            .header("hue-application-key", app_key)
            .send()
            .await
        {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(payload) => {
                        if let Some(items) = payload.get("data").and_then(|v| v.as_array()) {
                            for item in items {
                                let Some(name) = item
                                    .get("metadata")
                                    .and_then(|m| m.get("name"))
                                    .and_then(|v| v.as_str())
                                else {
                                    continue;
                                };
                                let area = Self::slugify(name);
                                if area.is_empty() {
                                    continue;
                                }
                                for child in item
                                    .get("children")
                                    .and_then(|v| v.as_array())
                                    .into_iter()
                                    .flatten()
                                {
                                    if let Some(rid) = child.get("rid").and_then(|v| v.as_str()) {
                                        device_area.insert(rid.to_string(), area.clone());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "Could not parse Hue rooms; devices get no area"),
                },
                Err(e) => warn!(error = %e, "Hue rooms endpoint returned non-success"),
            },
            // Rooms are an enrichment, never a reason to fail the whole sync.
            Err(e) => warn!(error = %e, "Could not fetch Hue rooms; devices get no area"),
        }
        device_area
    }

    pub async fn fetch_aux_devices(&self) -> Result<Vec<HueAuxDevice>> {
        let Some(app_key) = self.current_app_key() else {
            return Ok(Vec::new());
        };

        let client = self.http_client();
        let owner_names = self.fetch_owner_names(&client, &app_key).await?;
        let device_area = self.fetch_device_areas(&client, &app_key).await;

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

        let mut out = Vec::new();
        for resource_type in resource_types {
            let url = format!(
                "https://{}/clip/v2/resource/{resource_type}",
                self.target.host
            );
            let payload = client
                .get(url)
                .header("hue-application-key", &app_key)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await?;

            let Some(items) = payload.get("data").and_then(|v| v.as_array()) else {
                continue;
            };

            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };

                let owner_rid = item
                    .get("owner")
                    .and_then(|o| o.get("rid"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(rid);

                let name = owner_names.get(owner_rid).cloned().unwrap_or_else(|| {
                    format!(
                        "Hue {} {}",
                        resource_type,
                        rid.chars().take(8).collect::<String>()
                    )
                });

                let attributes = self.extract_aux_attributes(resource_type, item);
                out.push(HueAuxDevice {
                    bridge_id: self.target.bridge_id.clone(),
                    area: device_area.get(owner_rid).cloned(),
                    owner_rid: owner_rid.to_string(),
                    resource_type: resource_type.to_string(),
                    resource_id: rid.to_string(),
                    device_id: self.target.aux_device_id(resource_type, rid),
                    name,
                    attributes,
                });
            }
        }

        Ok(out)
    }

    pub async fn execute_grouped_light_command(
        &self,
        group_rid: &str,
        command: &LightCommand,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot control Hue grouped_light without app_key configured");
        };

        let mut body = serde_json::Map::new();
        if let Some(on) = command.on {
            body.insert("on".to_string(), json!({ "on": on }));
        }
        if let Some(brightness) = command.brightness_pct {
            body.insert("dimming".to_string(), json!({ "brightness": brightness }));
        }

        if body.is_empty() {
            bail!("no writable fields for grouped_light command");
        }

        let client = self.http_client();
        let url = format!(
            "https://{}/clip/v2/resource/grouped_light/{group_rid}",
            self.target.host
        );
        let response = client
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .context("Hue grouped_light command request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue grouped_light command failed: status={status} body={text}");
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

    fn slugify(input: &str) -> String {
        input
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string()
    }

    pub async fn activate_scene(&self, scene_rid: &str) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot activate Hue scene without app_key configured");
        };

        let client = self.http_client();
        let url = format!(
            "https://{}/clip/v2/resource/scene/{scene_rid}",
            self.target.host
        );
        let response = client
            .put(url)
            .header("hue-application-key", &app_key)
            .json(&json!({ "recall": { "action": "active" } }))
            .send()
            .await
            .context("Hue scene activation request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());
            bail!("Hue scene activation failed: status={status} body={text}");
        }

        Ok(())
    }

    pub async fn run_eventstream(
        &self,
        notify_tx: mpsc::Sender<EventstreamSignal>,
        reconnect_secs: u64,
    ) -> Result<()> {
        let Some(app_key) = self.current_app_key() else {
            bail!("cannot start eventstream without app_key configured");
        };

        let sleep_dur = Duration::from_secs(reconnect_secs.max(1));
        let bridge_id = self.target.bridge_id.clone();

        loop {
            let client = self.http_client();
            let url = format!("https://{}/eventstream/clip/v2", self.target.host);
            let response = client
                .get(&url)
                .header("hue-application-key", &app_key)
                .header("Accept", "text/event-stream")
                .send()
                .await;

            let Ok(response) = response else {
                tokio::time::sleep(sleep_dur).await;
                continue;
            };

            let Ok(response) = response.error_for_status() else {
                tokio::time::sleep(sleep_dur).await;
                continue;
            };

            let mut stream = response.bytes_stream();
            let mut buf = String::new();
            let mut last_emit = tokio::time::Instant::now() - Duration::from_secs(1);

            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    break;
                };

                buf.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(idx) = buf.find('\n') {
                    let mut line = buf.drain(..=idx).collect::<String>();
                    line = line.trim().to_string();

                    if let Some(rest) = line.strip_prefix("data:") {
                        let payload = rest.trim();
                        if !payload.is_empty() && last_emit.elapsed() >= Duration::from_millis(500)
                        {
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
                            last_emit = tokio::time::Instant::now();
                        }
                    }
                }
            }

            tokio::time::sleep(sleep_dur).await;
        }
    }

    async fn fetch_owner_names(
        &self,
        client: &Client,
        app_key: &str,
    ) -> Result<HashMap<String, String>> {
        let devices_url = format!("https://{}/clip/v2/resource/device", self.target.host);
        let devices_payload = client
            .get(devices_url)
            .header("hue-application-key", app_key)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        let mut out = HashMap::new();
        if let Some(items) = devices_payload.get("data").and_then(|v| v.as_array()) {
            for item in items {
                let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let name = item
                    .get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(rid)
                    .to_string();
                out.insert(rid.to_string(), name);
            }
        }
        Ok(out)
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
                if let Some(v) = item
                    .get("button_report")
                    .and_then(|r| r.get("event"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("button_event".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("button_report")
                    .and_then(|r| r.get("updated"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("button_updated".to_string(), json!(v));
                }
                if let Some(v) = item.get("repeat_interval").and_then(|v| v.as_u64()) {
                    out.insert("button_repeat_interval_ms".to_string(), json!(v));
                }
            }
            "relative_rotary" => {
                if let Some(v) = item
                    .get("rotary_report")
                    .and_then(|r| r.get("action"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_action".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("rotary_report")
                    .and_then(|r| r.get("rotation"))
                    .and_then(|r| r.get("direction"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_direction".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("rotary_report")
                    .and_then(|r| r.get("rotation"))
                    .and_then(|r| r.get("steps"))
                    .and_then(|v| v.as_i64())
                {
                    out.insert("rotary_steps".to_string(), json!(v));
                }
                if let Some(v) = item
                    .get("rotary_report")
                    .and_then(|r| r.get("updated"))
                    .and_then(|v| v.as_str())
                {
                    out.insert("rotary_updated".to_string(), json!(v));
                }
            }
            "entertainment_configuration" => {
                if let Some(v) = item
                    .get("status")
                    .and_then(|s| s.get("active"))
                    .and_then(|v| v.as_bool())
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
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
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
