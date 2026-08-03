//! WLED HTTP + WebSocket client.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

// ── Info response ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WledInfo {
    pub ver: String,
    pub name: String,
    pub leds: LedInfo,
    /// Number of connected WebSocket clients; -1 = WS not supported.
    #[serde(default = "neg_one")]
    pub ws: i32,
    pub fxcount: u32,
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
    #[serde(default)]
    pub cct: bool,
}

// ── State response ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct WledState {
    pub on: bool,
    pub bri: u8,
    #[serde(default = "neg_one")]
    pub ps: i32,
    #[serde(default)]
    pub seg: Vec<Segment>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Segment {
    #[serde(default)]
    pub id: u32,
    /// [[r,g,b], [r,g,b], [r,g,b]] — primary, secondary, tertiary.
    #[serde(default)]
    pub col: Vec<Value>,
    #[serde(default)]
    pub fx: u32,
    #[serde(default = "mid_u8")]
    pub sx: u8,
    #[serde(default = "mid_u8")]
    pub ix: u8,
    #[serde(default)]
    pub pal: u32,
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(default = "max_u8")]
    pub bri: u8,
}

fn neg_one() -> i32 {
    -1
}
fn mid_u8() -> u8 {
    128
}
fn max_u8() -> u8 {
    255
}
fn yes() -> bool {
    true
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

    pub async fn get_state(&self) -> Result<WledState> {
        self.http
            .get(format!("{}/json/state", self.base))
            .send()
            .await
            .context("GET /json/state")?
            .json()
            .await
            .context("parse /json/state")
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
