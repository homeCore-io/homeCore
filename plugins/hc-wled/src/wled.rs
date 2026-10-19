//! WLED HTTP + WebSocket client.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

// ── Info response ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
///
/// Only `leds.count` is required. Everything else defaults, because a field
/// that changed type in a firmware release must not make a working controller
/// look unreachable — which is what newer firmware's numeric `leds.cct` did: the
/// whole `/json/info` failed to parse, startup took the device for offline,
/// and the plugin fell back to polling for good.
pub struct WledInfo {
    #[serde(default)]
    pub ver: String,
    #[serde(default)]
    pub name: String,
    pub leds: LedInfo,
    /// Number of connected WebSocket clients; -1 = WS not supported.
    #[serde(default = "neg_one")]
    pub ws: i32,
    #[serde(default)]
    pub fxcount: u32,
    #[serde(default)]
    pub palcount: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct LedInfo {
    pub count: u32,
    #[serde(default)]
    pub pwr: u32,
    #[serde(default)]
    pub rgbw: bool,
    /// A bool on older firmware; a capability bitmask on newer (4 = CCT).
    #[serde(default)]
    pub cct: Value,
}

fn neg_one() -> i32 {
    -1
}

// ── HTTP client ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WledClient {
    http: Client,
    base: String,
}

impl WledClient {
    pub fn new(host: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build");
        Self {
            http,
            base: format!("http://{}", host.trim_end_matches('/')),
        }
    }

    pub fn ws_url(&self) -> String {
        self.base.replacen("http://", "ws://", 1) + "/ws"
    }

    pub async fn get_info(&self) -> Result<WledInfo> {
        self.http
            .get(format!("{}/json/info", self.base))
            .send()
            .await
            .context("GET /json/info")?
            .json()
            .await
            .context("parse /json/info")
    }

    /// `/json/state`, as WLED says it.
    pub async fn get_state(&self) -> Result<Value> {
        self.http
            .get(format!("{}/json/state", self.base))
            .send()
            .await
            .context("GET /json/state")?
            .json()
            .await
            .context("parse /json/state")
    }

    /// `/json/si`: the state and the info in one request.
    pub async fn get_state_info(&self) -> Result<(Value, Option<Value>)> {
        let v: Value = self
            .http
            .get(format!("{}/json/si", self.base))
            .send()
            .await
            .context("GET /json/si")?
            .json()
            .await
            .context("parse /json/si")?;
        match v.get("state") {
            Some(st) if st.is_object() => Ok((st.clone(), v.get("info").cloned())),
            _ => anyhow::bail!("/json/si carried no state"),
        }
    }

    pub async fn post_state(&self, body: &Value) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/json/state", self.base))
            .json(body)
            .send()
            .await
            .context("POST /json/state")?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("WLED {status}: {text}");
    }

    /// Any JSON the controller serves, by path.
    pub async fn get_json(&self, path: &str) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("{path}: {}", resp.status());
        }
        resp.json().await.with_context(|| format!("parse {path}"))
    }

    pub async fn get_effect_names(&self) -> Result<Vec<String>> {
        self.http
            .get(format!("{}/json/eff", self.base))
            .send()
            .await
            .context("GET /json/eff")?
            .json()
            .await
            .context("parse /json/eff")
    }

    pub async fn get_palette_names(&self) -> Result<Vec<String>> {
        self.http
            .get(format!("{}/json/palettes", self.base))
            .send()
            .await
            .context("GET /json/palettes")?
            .json()
            .await
            .context("parse /json/palettes")
    }

    /// `/json/nodes` returns the list of peer WLED instances on the
    /// same WLED-Sync mesh — `name`, `ip`, `vid` (firmware build)
    /// per entry. The local device itself is included.
    pub async fn get_nodes(&self) -> Result<Value> {
        self.http
            .get(format!("{}/json/nodes", self.base))
            .send()
            .await
            .context("GET /json/nodes")?
            .json()
            .await
            .context("parse /json/nodes")
    }

    /// `/presets.json` is the saved-presets file. Returns a JSON object
    /// keyed by preset id (`"1"`, `"2"`, …) where each entry contains
    /// the preset's name and the captured state.
    pub async fn get_presets(&self) -> Result<Value> {
        self.http
            .get(format!("{}/presets.json", self.base))
            .send()
            .await
            .context("GET /presets.json")?
            .json()
            .await
            .context("parse /presets.json")
    }

    /// Reboot the WLED device. Uses the legacy URL API `/win&RB=1`
    /// because the JSON API doesn't expose reboot in a versioned way.
    pub async fn reboot(&self) -> Result<()> {
        let resp = self
            .http
            .get(format!("{}/win&RB=1", self.base))
            .send()
            .await
            .context("GET /win&RB=1")?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("WLED reboot failed: {status}: {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_parses_whether_cct_is_a_bool_or_a_bitmask() {
        let new = r#"{"ver":"16.0.1","name":"wled-6ace04","ws":0,"fxcount":220,"palcount":72,
                      "leds":{"count":400,"pwr":120,"rgbw":false,"cct":4}}"#;
        let i: WledInfo = serde_json::from_str(new).expect("numeric cct parses");
        assert_eq!((i.leds.count, i.ws, i.fxcount), (400, 0, 220));
        let old = r#"{"ver":"0.13.3","name":"x","leds":{"count":30,"cct":false}}"#;
        let i: WledInfo = serde_json::from_str(old).expect("bool cct parses");
        assert_eq!(i.ws, -1);
    }
}
