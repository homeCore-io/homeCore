//! The Mysa Consumer API, as much of it as this plugin needs.
//!
//! Shapes come from the API's own OpenAPI document
//! (`https://consumer-api.mysa.cloud/v1/openapi.json`, 1.0.0), not from
//! anything observed live: no live call has been made yet. Mysa says new
//! response fields can appear within `/v1`, so every response type here
//! ignores what it doesn't name, and request bodies carry only documented
//! fields, because Mysa rejects anything else.
//!
//! The API calls a thermostat a device. Temperatures are degrees Celsius.
//!
//! ## Errors are classified, not just reported
//!
//! Mysa's error body is always `{"error": "<code>"}`. [`ApiError`] keeps the
//! cases that need different handling apart: a bad token (renew and retry
//! once), a missing scope (sign in again with it), a value off the device's
//! list (a bug here, or a stale capability list), waiting (`Retry-After`),
//! and the cloud being unreachable.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use crate::auth::API_BASE;

/// Long enough for a cloud round trip, short enough that a stalled request does
/// not hold the poll loop past its next tick.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `GET /v1/devices` pages: Mysa's own maximum.
const PAGE_SIZE: usize = 200;

/// `POST /v1/state/batch` takes at most this many ids at once.
pub const BATCH_MAX: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Mysa rejected the token")]
    Unauthorized,
    #[error("the sign-in lacks a permission this needs")]
    InsufficientScope,
    #[error("Mysa doesn't know that thermostat, or this account can't reach it")]
    NotFound,
    #[error("Mysa refused the request as invalid")]
    InvalidRequest,
    #[error("Mysa asked this plugin to wait")]
    RateLimited { retry_after: Option<Duration> },
    #[error("could not reach Mysa: {0}")]
    Transport(String),
    #[error("Mysa answered {status}: {code}")]
    Api { status: u16, code: String },
}

/// One page of `GET /v1/devices`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevicesPage {
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub next_offset: Option<usize>,
}

/// A thermostat the customer has shared with this app.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    /// The name the customer gave it, or empty.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    /// BB (baseboard), AC, ST, … `None` when Mysa doesn't recognise the model.
    #[serde(default)]
    pub product_family: Option<String>,
    #[serde(default)]
    pub home_id: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

/// A value a client may or may not set, and the values it may set it to.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Choice<T> {
    #[serde(default)]
    pub user_controllable: bool,
    #[serde(default = "Vec::new")]
    pub values: Vec<T>,
}

impl<T> Choice<T> {
    /// Values a client may actually send.
    pub fn settable(&self) -> &[T] {
        if self.user_controllable {
            &self.values
        } else {
            &[]
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Fan {
    #[serde(default)]
    pub mode: Choice<String>,
    #[serde(default)]
    pub speed: Choice<i64>,
}

/// One mode's controls: its setpoint (or auto's band) and fan.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClimateMode {
    #[serde(default)]
    pub setpoint: Option<Choice<f64>>,
    #[serde(default)]
    pub heat_setpoint: Option<Choice<f64>>,
    #[serde(default)]
    pub cool_setpoint: Option<Choice<f64>>,
    /// Auto's band: heat and cool at least this far apart. `0` with identical
    /// lists means one setpoint drives auto.
    #[serde(default)]
    pub min_setpoint_delta: Option<f64>,
    #[serde(default)]
    pub fan: Option<Fan>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SwingAxis {
    #[serde(default)]
    pub state: Choice<i64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Swing {
    #[serde(default)]
    pub vertical: Option<SwingAxis>,
    #[serde(default)]
    pub horizontal: Option<SwingAxis>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Climate {
    #[serde(default)]
    pub off: Option<ClimateMode>,
    #[serde(default)]
    pub heat: Option<ClimateMode>,
    #[serde(default)]
    pub cool: Option<ClimateMode>,
    #[serde(default)]
    pub auto: Option<ClimateMode>,
    #[serde(default)]
    pub emergency_heat: Option<ClimateMode>,
    #[serde(default)]
    pub dry: Option<ClimateMode>,
    #[serde(default)]
    pub fan_only: Option<ClimateMode>,
    #[serde(default)]
    pub swing: Option<Swing>,
}

impl Climate {
    /// Every mode's controls, by Mysa's mode name.
    pub fn modes(&self) -> Vec<(&'static str, &ClimateMode)> {
        [
            ("off", &self.off),
            ("heat", &self.heat),
            ("cool", &self.cool),
            ("auto", &self.auto),
            ("emergencyHeat", &self.emergency_heat),
            ("dry", &self.dry),
            ("fanOnly", &self.fan_only),
        ]
        .into_iter()
        .filter_map(|(name, m)| m.as_ref().map(|m| (name, m)))
        .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Brightness {
    #[serde(default)]
    pub user_controllable: bool,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Display {
    #[serde(default)]
    pub lockout: Option<Choice<i64>>,
    #[serde(default)]
    pub brightness: Option<Brightness>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Sensors {
    #[serde(default)]
    pub humidity: bool,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    #[serde(default)]
    pub scheduling: bool,
    #[serde(default)]
    pub energy_reporting: bool,
}

/// `GET /v1/capabilities/{deviceId}`: what this hardware can do. A control
/// not described here is one it can't perform.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// False when Mysa describes nothing for this device yet. Every list is
    /// then empty and every write is refused.
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default)]
    pub climate: Climate,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub sensors: Sensors,
    #[serde(default)]
    pub features: Features,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Setpoints {
    #[serde(default)]
    pub heat: Option<f64>,
    #[serde(default)]
    pub cool: Option<f64>,
    #[serde(default)]
    pub auto: Option<f64>,
    #[serde(default)]
    pub auto_heat: Option<f64>,
    #[serde(default)]
    pub auto_cool: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FanState {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub speed: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SwingState {
    #[serde(default)]
    pub vertical: Option<i64>,
    #[serde(default)]
    pub horizontal: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrightnessState {
    #[serde(default)]
    pub active_intensity: Option<i64>,
    #[serde(default)]
    pub idle_intensity: Option<i64>,
}

/// `GET /v1/state/{deviceId}`, and each entry of `POST /v1/state/batch`. A
/// field the hardware doesn't report is absent, not zero.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub last_connected: Option<i64>,
    #[serde(default)]
    pub current_temperature: Option<f64>,
    #[serde(default)]
    pub humidity: Option<f64>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub setpoints: Option<Setpoints>,
    #[serde(default)]
    pub fan: Option<FanState>,
    #[serde(default)]
    pub swing: Option<SwingState>,
    #[serde(default)]
    pub lockout: Option<i64>,
    #[serde(default)]
    pub brightness: Option<BrightnessState>,
    /// A batch entry for a device that couldn't be read.
    #[serde(default)]
    pub error: Option<String>,
}

/// Auto mode's targets: one setpoint, or a heat and cool band.
#[derive(Debug, Clone, PartialEq)]
pub enum AutoTarget {
    Single(f64),
    Band { heat: f64, cool: f64 },
}

#[derive(Clone)]
pub struct MysaApi {
    http: reqwest::Client,
    /// Mysa's API, or a stand-in in tests.
    base: String,
}

impl MysaApi {
    pub fn new(base: Option<&str>) -> anyhow::Result<(Self, reqwest::Client)> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("hc-mysa/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let base = base
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or(API_BASE)
            .trim_end_matches('/')
            .to_string();
        Ok((
            Self {
                http: http.clone(),
                base,
            },
            http,
        ))
    }

    /// Every thermostat the customer shared, across pages.
    pub async fn devices(&self, bearer: &str) -> Result<Vec<Device>, ApiError> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let page: DevicesPage = self
                .get(
                    &format!("/v1/devices?limit={PAGE_SIZE}&offset={offset}"),
                    bearer,
                )
                .await?;
            let got = page.devices.len();
            out.extend(page.devices);
            match page.next_offset {
                // Guard against a server that hands back the same offset.
                Some(next) if next > offset && got > 0 => offset = next,
                _ => return Ok(out),
            }
        }
    }

    pub async fn capabilities(&self, bearer: &str, id: &str) -> Result<Capabilities, ApiError> {
        self.get(&format!("/v1/capabilities/{}", seg(id)), bearer)
            .await
    }

    pub async fn state(&self, bearer: &str, id: &str) -> Result<State, ApiError> {
        self.get(&format!("/v1/state/{}", seg(id)), bearer).await
    }

    /// The state of many thermostats in as few calls as Mysa allows. A device
    /// that couldn't be read comes back with `error` set, not as a failure.
    pub async fn states(
        &self,
        bearer: &str,
        ids: &[String],
    ) -> Result<HashMap<String, State>, ApiError> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(BATCH_MAX) {
            let page: HashMap<String, State> = self
                .send(
                    reqwest::Method::POST,
                    "/v1/state/batch",
                    bearer,
                    Some(&json!({ "deviceIds": chunk })),
                )
                .await
                .and_then(|body| parse(&body, "/v1/state/batch"))?;
            out.extend(page);
        }
        Ok(out)
    }

    /// Mysa's own mode name: `heat`, `cool`, `auto`, `fanOnly`, ….
    pub async fn set_mode(&self, bearer: &str, id: &str, mode: &str) -> Result<(), ApiError> {
        self.put(
            &format!("/v1/state/{}/mode", seg(id)),
            bearer,
            &json!({ "mode": mode }),
        )
        .await
    }

    /// Also switches the thermostat to heat mode, Mysa says.
    pub async fn set_heat(&self, bearer: &str, id: &str, celsius: f64) -> Result<(), ApiError> {
        self.put(
            &format!("/v1/state/{}/setpoints/heat", seg(id)),
            bearer,
            &json!({ "setpoint": celsius }),
        )
        .await
    }

    /// Also switches the thermostat to cool mode.
    pub async fn set_cool(&self, bearer: &str, id: &str, celsius: f64) -> Result<(), ApiError> {
        self.put(
            &format!("/v1/state/{}/setpoints/cool", seg(id)),
            bearer,
            &json!({ "setpoint": celsius }),
        )
        .await
    }

    /// Switches to auto with these targets.
    pub async fn set_auto(
        &self,
        bearer: &str,
        id: &str,
        target: &AutoTarget,
    ) -> Result<(), ApiError> {
        let body = match target {
            AutoTarget::Single(c) => json!({ "setpoint": c }),
            AutoTarget::Band { heat, cool } => {
                json!({ "heatSetpoint": heat, "coolSetpoint": cool })
            }
        };
        self.put(
            &format!("/v1/state/{}/setpoints/auto", seg(id)),
            bearer,
            &body,
        )
        .await
    }

    /// `auto` or `sleep` alone, or `manual` with a speed.
    pub async fn set_fan(
        &self,
        bearer: &str,
        id: &str,
        mode: &str,
        speed: Option<i64>,
    ) -> Result<(), ApiError> {
        let body = match (mode, speed) {
            ("manual", Some(s)) => json!({ "mode": "manual", "speed": s }),
            (m, _) => json!({ "mode": m }),
        };
        self.put(&format!("/v1/state/{}/fan", seg(id)), bearer, &body)
            .await
    }

    pub async fn set_lockout(&self, bearer: &str, id: &str, level: i64) -> Result<(), ApiError> {
        self.put(
            &format!("/v1/state/{}/lockout", seg(id)),
            bearer,
            &json!({ "lockout": level }),
        )
        .await
    }

    /// Either or both, 0–100.
    pub async fn set_brightness(
        &self,
        bearer: &str,
        id: &str,
        active: Option<i64>,
        idle: Option<i64>,
    ) -> Result<(), ApiError> {
        let mut body = serde_json::Map::new();
        if let Some(a) = active {
            body.insert("activeIntensity".into(), json!(a));
        }
        if let Some(i) = idle {
            body.insert("idleIntensity".into(), json!(i));
        }
        self.put(
            &format!("/v1/state/{}/brightness", seg(id)),
            bearer,
            &Value::Object(body),
        )
        .await
    }

    /// `vertical` or `horizontal`.
    pub async fn set_swing(
        &self,
        bearer: &str,
        id: &str,
        axis: &str,
        position: i64,
    ) -> Result<(), ApiError> {
        self.put(
            &format!("/v1/state/{}/swing/{axis}", seg(id)),
            bearer,
            &json!({ "state": position }),
        )
        .await
    }

    /// Keep the current climate until the schedule's next event. The device
    /// must already follow a schedule.
    pub async fn hold(&self, bearer: &str, id: &str) -> Result<(), ApiError> {
        self.send(
            reqwest::Method::POST,
            "/v1/schedule/hold",
            bearer,
            Some(&json!({ "deviceId": id })),
        )
        .await
        .map(|_| ())
    }

    /// Cancel holds and follow the schedule. Safe to repeat.
    pub async fn resume(&self, bearer: &str, id: &str) -> Result<(), ApiError> {
        self.send(
            reqwest::Method::DELETE,
            &format!("/v1/schedule/hold/{}", seg(id)),
            bearer,
            None,
        )
        .await
        .map(|_| ())
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        bearer: &str,
    ) -> Result<T, ApiError> {
        let body = self.send(reqwest::Method::GET, path, bearer, None).await?;
        parse(&body, path)
    }

    async fn put(&self, path: &str, bearer: &str, body: &Value) -> Result<(), ApiError> {
        self.send(reqwest::Method::PUT, path, bearer, Some(body))
            .await
            .map(|_| ())
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<String, ApiError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(bearer)
            .header("accept", "application/json");
        if let Some(b) = body {
            request = request.json(b);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = response.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            return Ok(text);
        }
        Err(classify(status, &text, retry_after))
    }
}

/// A path segment: Mysa ids are plain, but never trust that in a URL.
fn seg(id: &str) -> String {
    urlencoding::encode(id).into_owned()
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, path: &str) -> Result<T, ApiError> {
    serde_json::from_str(body).map_err(|e| ApiError::Api {
        status: 200,
        code: format!(
            "unreadable answer from {}: {e}",
            path.split('?').next().unwrap_or(path)
        ),
    })
}

/// Mysa's status and `{"error": code}` → what to do about it.
pub fn classify(status: u16, body: &str, retry_after: Option<Duration>) -> ApiError {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    match (status, code.as_str()) {
        (401, _) => ApiError::Unauthorized,
        (403, _) => ApiError::InsufficientScope,
        (404, _) => ApiError::NotFound,
        (400, _) => ApiError::InvalidRequest,
        (429, _) | (503, _) => ApiError::RateLimited { retry_after },
        (status, _) => ApiError::Api {
            status,
            code: if code.is_empty() {
                "no error code".into()
            } else {
                code
            },
        },
    }
}

#[cfg(test)]
pub mod fixtures {
    //! Answers shaped as Mysa's OpenAPI document describes them. Mysa
    //! publishes no examples, so these are built from the schemas.

    pub const DEVICES: &str = r#"{
        "devices": [
            {"id": "bb-1", "name": "Living Room", "model": "BB-V2-0", "productFamily": "BB",
             "homeId": "h1", "zoneId": null, "timeZone": "America/Toronto"},
            {"id": "ac-1", "name": "", "model": "AC-V1-1", "productFamily": "AC",
             "homeId": "h1", "zoneId": null, "timeZone": "America/Toronto", "somethingNew": 1}
        ],
        "total": 2
    }"#;

    /// A baseboard thermostat: heat only, half-degree steps from 5 to 30 °C.
    pub fn baseboard_capabilities() -> String {
        let steps: Vec<String> = (10..=60).map(|h| format!("{}", h as f64 / 2.0)).collect();
        format!(
            r#"{{"deviceId":"bb-1","published":true,"model":"BB-V2-0","productFamily":"BB",
            "modes":["off","heat"],
            "climate":{{"off":{{}},"heat":{{"setpoint":{{"userControllable":true,"values":[{}]}}}}}},
            "display":{{"temperatureUnits":{{"userControllable":true,"values":["C","F"]}},
                       "lockout":{{"userControllable":true,"values":[0,1]}},
                       "brightness":{{"userControllable":true}}}},
            "sensors":{{"internalTemperature":true,"externalTemperatureProbe":false,"remoteTemperature":false,"humidity":true}},
            "features":{{"scheduling":true,"savedSchedules":true,"energyReporting":true}}}}"#,
            steps.join(",")
        )
    }

    /// A mini-split: heat, cool, auto band, fan, swing; whole-degree steps.
    pub fn ac_capabilities() -> String {
        let steps: Vec<String> = (16..=30).map(|c| format!("{c}")).collect();
        let s = steps.join(",");
        let fan = r#""fan":{"mode":{"userControllable":true,"values":["auto","manual"]},"speed":{"userControllable":true,"values":[1,2,3]}}"#;
        format!(
            r#"{{"deviceId":"ac-1","published":true,"model":"AC-V1-1","productFamily":"AC",
            "modes":["off","heat","cool","auto","fanOnly","dry"],
            "climate":{{"off":{{}},
              "heat":{{"setpoint":{{"userControllable":true,"values":[{s}]}},{fan}}},
              "cool":{{"setpoint":{{"userControllable":true,"values":[{s}]}},{fan}}},
              "auto":{{"heatSetpoint":{{"userControllable":true,"values":[{s}]}},"coolSetpoint":{{"userControllable":true,"values":[{s}]}},"minSetpointDelta":2,{fan}}},
              "dry":{{"setpoint":{{"userControllable":true,"values":[{s}]}}}},
              "fanOnly":{{{fan}}},
              "swing":{{"vertical":{{"state":{{"userControllable":true,"values":[0,1,2,3]}},"toggle":{{"userControllable":true,"values":[0,1]}}}}}}}},
            "display":{{"temperatureUnits":{{"userControllable":false,"values":["C"]}},
                       "lockout":{{"userControllable":false,"values":[]}},
                       "brightness":{{"userControllable":false}}}},
            "sensors":{{"internalTemperature":true,"externalTemperatureProbe":false,"remoteTemperature":false,"humidity":true}},
            "features":{{"scheduling":true,"savedSchedules":true,"energyReporting":false}}}}"#
        )
    }

    pub const BASEBOARD_STATE: &str = r#"{"deviceId":"bb-1","connected":true,"lastConnected":1793500000,
        "currentTemperature":20.4,"humidity":38,"mode":"heat","setpoints":{"heat":21.5},
        "lockout":0,"brightness":{"activeIntensity":100,"idleIntensity":40}}"#;

    pub const AC_STATE: &str = r#"{"deviceId":"ac-1","connected":true,"lastConnected":1793500000,
        "currentTemperature":24,"humidity":51,"mode":"cool","setpoints":{"heat":20,"cool":23,"autoHeat":20,"autoCool":24},
        "fan":{"mode":"manual","speed":2},"swing":{"vertical":1}}"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_parse_and_unknown_fields_are_ignored() {
        let page: DevicesPage = serde_json::from_str(fixtures::DEVICES).unwrap();
        assert_eq!(page.devices.len(), 2);
        assert_eq!(page.devices[0].id, "bb-1");
        assert_eq!(page.devices[0].product_family.as_deref(), Some("BB"));
        assert_eq!(page.devices[1].name, "");
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn capabilities_parse_per_mode_and_say_what_may_be_set() {
        let bb: Capabilities = serde_json::from_str(&fixtures::baseboard_capabilities()).unwrap();
        assert!(bb.published);
        assert_eq!(bb.modes, vec!["off", "heat"]);
        let heat = bb.climate.heat.as_ref().unwrap().setpoint.as_ref().unwrap();
        assert_eq!(heat.settable().first(), Some(&5.0));
        assert_eq!(heat.settable().last(), Some(&30.0));
        assert_eq!(bb.display.lockout.as_ref().unwrap().settable(), &[0, 1]);
        let ac: Capabilities = serde_json::from_str(&fixtures::ac_capabilities()).unwrap();
        let auto = ac.climate.auto.as_ref().unwrap();
        assert_eq!(auto.min_setpoint_delta, Some(2.0));
        assert!(ac.climate.swing.as_ref().unwrap().vertical.is_some());
        // Not controllable means nothing may be sent, whatever the list says.
        assert!(ac.display.lockout.as_ref().unwrap().settable().is_empty());
        let names: Vec<&str> = ac.climate.modes().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["off", "heat", "cool", "auto", "dry", "fanOnly"]);
    }

    #[test]
    fn state_parses_and_absent_fields_stay_absent() {
        let s: State = serde_json::from_str(fixtures::BASEBOARD_STATE).unwrap();
        assert!(s.connected);
        assert_eq!(s.current_temperature, Some(20.4));
        assert_eq!(s.setpoints.as_ref().unwrap().heat, Some(21.5));
        assert_eq!(s.setpoints.as_ref().unwrap().cool, None);
        assert_eq!(s.fan, None);
        let batch: HashMap<String, State> = serde_json::from_str(
            r#"{"bb-1":{"deviceId":"bb-1","connected":true,"lastConnected":0},"gone":{"deviceId":"gone","connected":false,"lastConnected":0,"error":"not_found"}}"#,
        )
        .unwrap();
        assert_eq!(batch["gone"].error.as_deref(), Some("not_found"));
    }

    #[test]
    fn errors_are_classified_by_status_and_retry_after_is_kept() {
        assert!(matches!(
            classify(401, r#"{"error":"unauthorized"}"#, None),
            ApiError::Unauthorized
        ));
        assert!(matches!(
            classify(403, r#"{"error":"insufficient_scope"}"#, None),
            ApiError::InsufficientScope
        ));
        assert!(matches!(classify(404, "{}", None), ApiError::NotFound));
        assert!(matches!(
            classify(400, r#"{"error":"invalid_request"}"#, None),
            ApiError::InvalidRequest
        ));
        match classify(
            429,
            r#"{"error":"too_many_requests"}"#,
            Some(Duration::from_secs(7)),
        ) {
            ApiError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            classify(503, r#"{"error":"temporarily_unavailable"}"#, None),
            ApiError::RateLimited { .. }
        ));
        match classify(500, r#"{"error":"server_error"}"#, None) {
            ApiError::Api { status, code } => {
                assert_eq!(status, 500);
                assert_eq!(code, "server_error");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_id_is_escaped_in_a_path() {
        assert_eq!(seg("a/b c"), "a%2Fb%20c");
    }
}
