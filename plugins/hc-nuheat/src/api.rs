//! The NuHeat OpenAPI, as much of it as this plugin needs.
//!
//! Shapes come from the live swagger documents (`/swagger/v2/swagger.json`),
//! not from the prose reference, which is out of date in places the JSON is
//! not. Everything here is v2 except where noted: v2 is the current surface,
//! and it splits what v1 did with one overloaded `PUT /api/v1/Thermostat` into
//! three explicit mode endpoints.
//!
//! ## Errors are classified, not just reported
//!
//! [`ApiError`] separates the three failures that need different handling and
//! different words on the operator's screen:
//!
//! - **Unauthorized** — the token is bad or expired. Recoverable by renewing
//!   (oauth) or re-pasting (implicit), and *not* a reason to think the
//!   thermostat is unreachable.
//! - **RateLimited** — back off; nothing is wrong with the account or the
//!   hardware.
//! - **Transport** / **Api** — the cloud is unreachable or unhappy.
//!
//! Collapsing these into one error is what produces the classic bad plugin
//! behaviour: an expired token that presents as "your thermostat is offline".

use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;

use crate::auth::API_BASE;

/// Long enough for a cloud round trip, short enough that a stalled request does
/// not hold the poll loop past its next tick.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// NuHeat's cap on a temporary hold. Documented, and enforced server-side.
pub const MAX_HOLD_HOURS: i64 = 23;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("the NuHeat API rejected the token")]
    Unauthorized,
    #[error("rate limited by the NuHeat API")]
    RateLimited { retry_after: Option<Duration> },
    #[error("could not reach the NuHeat API: {0}")]
    Transport(String),
    #[error("the NuHeat API returned {status}: {body}")]
    Api { status: u16, body: String },
}

/// A thermostat, exactly as `GET /api/v2/Thermostat` returns it.
///
/// Temperatures are NuHeat's integer wire units — see [`crate::units`]. They
/// are deliberately *not* converted here: this type is the wire, and mixing
/// decoding into it is how a value ends up divided by a hundred twice.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Thermostat {
    #[serde(default)]
    pub serial_number: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub current_temperature: Option<i64>,
    #[serde(default)]
    pub set_point_temperature: Option<i64>,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub is_heating: bool,
    /// When a temporary hold ends. Absent in Auto and in permanent hold.
    #[serde(default)]
    pub hold_until: Option<String>,
    /// `eScheduleMode`: 1 Auto, 2 Hold, 3 Permanent hold.
    #[serde(default)]
    pub mode: Option<i64>,
    /// The thermostat's own fault text — a floor sensor failure, most often.
    #[serde(default)]
    pub error_state: Option<String>,
}

/// `GET /api/v2/Account`. Used to prove a freshly pasted token works before
/// telling the operator they are linked.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub user_name: Option<String>,
    /// The account's display preference, `F` or `C`. Advisory only — the wire
    /// stays Celsius hundredths regardless.
    #[serde(default)]
    pub temperature_scale: Option<String>,
}

/// The three modes a NuHeat thermostat can be in, named the way homeCore
/// publishes them rather than by NuHeat's integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Following its own schedule.
    Auto,
    /// Holding a temperature until `holdUntil`, then back to Auto.
    Hold,
    /// Holding a temperature indefinitely. NuHeat calls this "Manual".
    PermanentHold,
}

impl Mode {
    /// From `eScheduleMode` on the wire.
    pub fn from_wire(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::Auto),
            2 => Some(Self::Hold),
            3 => Some(Self::PermanentHold),
            _ => None,
        }
    }

    /// The string homeCore publishes and accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hold => "hold",
            Self::PermanentHold => "permanent_hold",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "schedule" | "resume" => Some(Self::Auto),
            "hold" | "temporary_hold" => Some(Self::Hold),
            "permanent_hold" | "manual" | "permanent" => Some(Self::PermanentHold),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct NuHeatApi {
    http: reqwest::Client,
}

impl NuHeatApi {
    pub fn new() -> Result<(Self, reqwest::Client)> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("hc-nuheat/", env!("CARGO_PKG_VERSION")))
            .build()?;
        // The same client is handed to `Auth`: connection reuse across the
        // identity server and the API, and one place that owns the timeout.
        Ok((Self { http: http.clone() }, http))
    }

    pub async fn thermostats(&self, bearer: &str) -> Result<Vec<Thermostat>, ApiError> {
        self.get("/api/v2/Thermostat", bearer).await
    }

    pub async fn thermostat(&self, bearer: &str, serial: &str) -> Result<Thermostat, ApiError> {
        self.get(&format!("/api/v2/Thermostat/{serial}"), bearer)
            .await
    }

    pub async fn account(&self, bearer: &str) -> Result<Account, ApiError> {
        self.get("/api/v2/Account", bearer).await
    }

    /// Return the thermostat to its schedule.
    pub async fn set_auto(&self, bearer: &str, serial: &str) -> Result<(), ApiError> {
        self.put(
            "/api/v2/Mode/Auto",
            bearer,
            &serde_json::json!({ "serialNumber": serial }),
        )
        .await
    }

    /// Hold a temperature until a moment in time, then resume the schedule.
    ///
    /// `hold_until` omitted means "until the next scheduled event", which is
    /// NuHeat's own default and usually what a person means by "warmer for
    /// now". `temperatureType` is pinned to 0 (absolute): relative holds exist
    /// on the wire but nothing in homeCore's command vocabulary asks for one,
    /// and sending 1 by accident would make every setpoint an offset.
    pub async fn set_hold(
        &self,
        bearer: &str,
        serial: &str,
        temperature: i64,
        hold_until: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut body = serde_json::json!({
            "serialNumber": serial,
            "temperature": temperature,
            "temperatureType": 0,
        });
        if let Some(until) = hold_until {
            body["holdUntil"] = serde_json::Value::String(until.to_string());
        }
        self.put("/api/v2/Mode/Hold", bearer, &body).await
    }

    /// Hold a temperature indefinitely.
    pub async fn set_permanent_hold(
        &self,
        bearer: &str,
        serial: &str,
        temperature: i64,
    ) -> Result<(), ApiError> {
        self.put(
            "/api/v2/Mode/Manual",
            bearer,
            &serde_json::json!({
                "serialNumber": serial,
                "temperature": temperature,
                "temperatureType": 0,
            }),
        )
        .await
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        bearer: &str,
    ) -> Result<T, ApiError> {
        let response = self
            .http
            .get(format!("{API_BASE}{path}"))
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;

        let body = Self::body_or_error(response).await?;
        serde_json::from_str(&body).map_err(|e| ApiError::Api {
            status: 200,
            body: format!("could not parse {path}: {e}"),
        })
    }

    async fn put(
        &self,
        path: &str,
        bearer: &str,
        body: &serde_json::Value,
    ) -> Result<(), ApiError> {
        let response = self
            .http
            .put(format!("{API_BASE}{path}"))
            .bearer_auth(bearer)
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        Self::body_or_error(response).await.map(|_| ())
    }

    async fn body_or_error(response: reqwest::Response) -> Result<String, ApiError> {
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ApiError::Unauthorized);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // NuHeat publishes `X-Rate-Limit-Reset`; `Retry-After` is the
            // standard header and may appear instead. Either is better than
            // guessing an interval.
            let retry_after = response
                .headers()
                .get("retry-after")
                .or_else(|| response.headers().get("x-rate-limit-reset"))
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(ApiError::RateLimited { retry_after });
        }
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::Api {
                status: status.as_u16(),
                // Cloud error bodies can be a whole HTML page; the first line
                // is the part that ever helps.
                body: body.chars().take(300).collect(),
            });
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thermostat_parses_from_what_the_api_documents() {
        let t: Thermostat = serde_json::from_str(
            r#"{
                "serialNumber": "12345678",
                "name": "Master Bath",
                "currentTemperature": 2224,
                "setPointTemperature": 2500,
                "online": true,
                "isHeating": true,
                "holdUntil": "2026-09-04T19:00:00Z",
                "mode": 2,
                "errorState": null
            }"#,
        )
        .expect("parses");
        assert_eq!(t.serial_number, "12345678");
        assert_eq!(t.current_temperature, Some(2224));
        assert_eq!(Mode::from_wire(t.mode.unwrap()), Some(Mode::Hold));
        assert!(t.is_heating);
    }

    /// The SDK's ABI rule applied to someone else's API: a field NuHeat adds or
    /// stops sending must not cost us the whole device.
    #[test]
    fn a_sparse_thermostat_still_parses() {
        let t: Thermostat =
            serde_json::from_str(r#"{"serialNumber": "1", "online": false}"#).expect("parses");
        assert_eq!(t.serial_number, "1");
        assert_eq!(t.current_temperature, None);
        assert_eq!(t.mode, None);
        assert!(!t.online);
    }

    #[test]
    fn modes_round_trip_between_the_wire_and_what_we_publish() {
        for (wire, name) in [(1, "auto"), (2, "hold"), (3, "permanent_hold")] {
            let mode = Mode::from_wire(wire).expect("known mode");
            assert_eq!(mode.as_str(), name);
            assert_eq!(Mode::parse(name), Some(mode));
        }
        assert_eq!(Mode::from_wire(99), None);
    }

    /// The names a person or a rule actually writes, rather than only the
    /// canonical ones. "Manual" is NuHeat's own word for permanent hold, so it
    /// arrives in anything ported from their app.
    #[test]
    fn mode_names_accept_the_obvious_synonyms() {
        assert_eq!(Mode::parse("Manual"), Some(Mode::PermanentHold));
        assert_eq!(Mode::parse("resume"), Some(Mode::Auto));
        assert_eq!(Mode::parse("  HOLD "), Some(Mode::Hold));
        assert_eq!(Mode::parse("eco"), None);
    }
}
