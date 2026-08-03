//! Roku External Control Protocol (ECP) client.
//!
//! ECP is a plain-HTTP API on port 8060: `GET /query/*` returns XML,
//! `POST /keypress/*`, `/launch/*`, `/install/*`, `/input` take no body
//! and answer 200 with nothing useful in it. There is no push channel and
//! no authentication — reachability on the LAN *is* the authorisation
//! model, which is why Roku gates the interesting half of it behind a
//! device setting (see [`EcpError::Forbidden`]).
//!
//! Query responses are parsed into `BTreeMap<String, String>` rather than
//! fixed structs wherever Roku's own schema is open-ended (`device-info`
//! gains fields with almost every OS release — `supports-tv-power-control`
//! and `supports-audio-volume-control` arrived in Roku OS 15.0). Typed
//! accessors sit on top for the fields the plugin actually reasons about,
//! so new firmware fields reach homeCore without a code change.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use tracing::debug;

use crate::keys;

/// Default ECP port. Not configurable on the device; overridable in
/// config only because port-forwarding a Roku through a router is a thing
/// people do.
pub const DEFAULT_PORT: u16 = 8060;

/// Errors worth distinguishing from "the request failed".
///
/// `Display` + `Error` are written out by hand rather than derived —
/// two variants do not justify pulling a proc-macro crate into the
/// build graph, and the 403 message needs to be prose anyway.
#[derive(Debug)]
pub enum EcpError {
    /// The device answered 403. On a Roku this means *"Control by mobile
    /// apps"* is set to **Disabled**, which blocks `keypress`, `keydown`,
    /// `keyup`, `query/icon`, and both TV-channel queries while leaving
    /// `query/device-info` and `launch` working — so the plugin looks
    /// half-alive rather than dead, and the operator needs telling why.
    Forbidden,
    /// Endpoint absent on this model or OS version (e.g. TV queries on a
    /// streaming stick, `query/media-player` before Roku OS 9.4).
    NotSupported,
}

impl std::fmt::Display for EcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EcpError::Forbidden => write!(
                f,
                "Roku refused the request (HTTP 403). Enable Settings → System → \
                 Advanced system settings → Control by mobile apps → Network access = \
                 \"Default\" or \"Permissive\" on the device"
            ),
            EcpError::NotSupported => write!(
                f,
                "this Roku model / OS version does not implement that ECP endpoint"
            ),
        }
    }
}

impl std::error::Error for EcpError {}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// `GET /query/device-info` — every element verbatim, kebab-case keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub fields: BTreeMap<String, String>,
}

impl DeviceInfo {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// ECP renders booleans as the strings `"true"` / `"false"`.
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some("true"))
    }

    /// `PowerOn` | `DisplayOff` | `Headless` | `Ready` | `PowerOff`.
    /// Absent on very old firmware, which is treated as powered on —
    /// those devices have no standby mode to report.
    pub fn power_mode(&self) -> &str {
        self.get("power-mode").unwrap_or("PowerOn")
    }

    /// True only for `PowerOn`. `Ready`/`PowerOff` are standby (ECP still
    /// answers), `DisplayOff` is a TV with the panel off but audio live,
    /// `Headless` is a TV mid-boot.
    pub fn is_powered_on(&self) -> bool {
        self.power_mode() == "PowerOn"
    }

    pub fn is_tv(&self) -> bool {
        self.flag("is-tv")
    }

    /// Whether the device will accept remote-control commands at all.
    ///
    /// `ecp-setting-mode` is Roku's own report of *Settings → System →
    /// Advanced system settings → Control by mobile apps*: `enabled`
    /// when set to Default or Permissive, something else when Disabled.
    /// Reading it means the plugin can say "this device is refusing
    /// control, here is the setting" at registration, instead of the
    /// operator discovering it later when a keypress 403s.
    ///
    /// Absent on firmware that predates the field, which is reported as
    /// enabled — those devices have no such restriction to report.
    pub fn ecp_control_enabled(&self) -> bool {
        match self.get("ecp-setting-mode") {
            None | Some("") => true,
            Some(mode) => mode.eq_ignore_ascii_case("enabled"),
        }
    }

    pub fn serial(&self) -> Option<&str> {
        self.get("serial-number").or_else(|| self.get("device-id"))
    }

    /// Best display name, most-specific first. `friendly-device-name` is
    /// what the Roku app shows ("42\" Onn Roku TV"); `user-device-name`
    /// is what the owner typed; the model name is the fallback.
    pub fn display_name(&self) -> Option<&str> {
        self.get("friendly-device-name")
            .or_else(|| self.get("user-device-name"))
            .or_else(|| self.get("friendly-model-name"))
            .or_else(|| self.get("model-name"))
            .filter(|s| !s.is_empty())
    }

    /// MAC addresses usable as a Wake-on-LAN target, wired first — a TV
    /// that is fully powered off drops off Wi-Fi entirely on most models,
    /// so the Ethernet MAC is the one with a chance of working.
    pub fn wake_macs(&self) -> Vec<String> {
        ["ethernet-mac", "wifi-mac"]
            .iter()
            .filter_map(|k| self.get(k))
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// One entry from `query/apps` / `query/active-app`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub name: String,
    /// `appl` (channel), `tvin` (TV input), `ssvr` (screensaver).
    /// Absent on pre-installed apps in older firmware.
    pub app_type: Option<String>,
    pub version: Option<String>,
}

impl App {
    /// TV inputs are apps with reserved ids (`tvinput.hdmi1`,
    /// `tvinput.dtv`, `tvinput.av1`) — launching one switches the TV's
    /// input, which is why the plugin surfaces them separately from
    /// streaming channels.
    pub fn is_input(&self) -> bool {
        self.app_type.as_deref() == Some("tvin") || self.id.starts_with("tvinput.")
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({ "id": self.id, "name": self.name });
        if let Some(t) = &self.app_type {
            v["type"] = serde_json::Value::String(t.clone());
        }
        if let Some(ver) = &self.version {
            v["version"] = serde_json::Value::String(ver.clone());
        }
        v
    }
}

/// `GET /query/active-app`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveApp {
    pub app: Option<App>,
    pub screensaver: Option<App>,
}

impl ActiveApp {
    /// Is the device sitting on its own home screen?
    ///
    /// Roku reports this two different ways depending on OS version, and
    /// only one of them is documented:
    ///
    /// * older firmware — a bare `<app>Roku</app>` with no id at all;
    /// * Roku OS 15 — `<app id="562859" type="home" ui-location="home">Roku
    ///   Dynamic Menu</app>`, indistinguishable from a running channel
    ///   unless the type is checked.
    ///
    /// Missing the second form makes a modern Roku on its home screen
    /// report `source: "Roku Dynamic Menu"` and a playback state of
    /// `stopped` — an idle device that looks like it is showing something.
    pub fn is_home(&self) -> bool {
        match &self.app {
            None => true,
            Some(app) => app.app_type.as_deref() == Some("home"),
        }
    }
}

/// `GET /query/media-player` (Roku OS 9.4+).
///
/// `state` is documented as `play`, `pause`, `close`, `startup`,
/// `buffer` or `error`. Roku OS 15 also emits `none` (nothing has played
/// since boot — what a TV on an HDMI input reports) and `open` (an app is
/// up with its player idle). Both were observed on real hardware and
/// neither appears in the docs, which is why the state mapping treats
/// anything unrecognised as "not playing" rather than matching a fixed
/// list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaPlayer {
    pub state: String,
    pub error: bool,
    pub app_id: Option<String>,
    pub app_name: Option<String>,
    pub position_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub runtime_ms: Option<u64>,
    pub is_live: bool,
    /// `<format audio= video= container= captions= drm= video_res= />`
    pub format: BTreeMap<String, String>,
}

/// One `<channel>` from `query/tv-channels` or `query/tv-active-channel`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvChannel {
    pub fields: BTreeMap<String, String>,
}

impl TvChannel {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
    pub fn number(&self) -> Option<&str> {
        self.get("number")
    }
    pub fn name(&self) -> Option<&str> {
        self.get("name")
    }
    pub fn hidden(&self) -> bool {
        matches!(self.get("user-hidden"), Some("true"))
    }
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.fields {
            map.insert(k.replace('-', "_"), json_scalar(v));
        }
        serde_json::Value::Object(map)
    }
}

/// Coerce an ECP string into the natural JSON type. ECP is all-strings on
/// the wire; publishing `"true"` where homeCore rules expect `true`
/// silently breaks every `is` comparison written against the attribute.
fn json_scalar(raw: &str) -> serde_json::Value {
    match raw {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => match raw.parse::<i64>() {
            Ok(n) => serde_json::Value::from(n),
            Err(_) => serde_json::Value::String(raw.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client bound to one Roku.
#[derive(Debug, Clone)]
pub struct EcpClient {
    base: String,
    http: Client,
}

impl EcpClient {
    pub fn new(host: &str, port: u16, timeout: Duration) -> Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            // ECP is HTTP/1.1 on the LAN; connection reuse across a
            // 10-second poll is worth more than the pool size.
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self {
            base: format!("http://{}:{}", host.trim(), port),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    // ── Queries ──────────────────────────────────────────────────────────

    pub async fn device_info(&self) -> Result<DeviceInfo> {
        let xml = self.get_text("query/device-info").await?;
        parse_device_info(&xml)
    }

    pub async fn apps(&self) -> Result<Vec<App>> {
        let xml = self.get_text("query/apps").await?;
        parse_apps(&xml)
    }

    pub async fn active_app(&self) -> Result<ActiveApp> {
        let xml = self.get_text("query/active-app").await?;
        parse_active_app(&xml)
    }

    pub async fn media_player(&self) -> Result<MediaPlayer> {
        let xml = self.get_text("query/media-player").await?;
        parse_media_player(&xml)
    }

    pub async fn tv_channels(&self) -> Result<Vec<TvChannel>> {
        let xml = self.get_text("query/tv-channels").await?;
        parse_tv_channels(&xml)
    }

    /// The currently tuned channel, or `None` when the TV is not on the
    /// tuner.
    ///
    /// A Roku TV showing an HDMI input answers this with an *empty*
    /// `<channel/>` rather than an error, so "not tuned" has to be told
    /// from "tuned" by whether a channel number came back — see
    /// [`parse_tv_channels`].
    pub async fn tv_active_channel(&self) -> Result<Option<TvChannel>> {
        let xml = self.get_text("query/tv-active-channel").await?;
        Ok(parse_tv_channels(&xml)?.into_iter().next())
    }

    /// `GET /query/icon/{app_id}` — returns `(content_type, bytes)`.
    /// Requires *Control by mobile apps* to be enabled.
    pub async fn icon(&self, app_id: &str) -> Result<(String, Vec<u8>)> {
        let url = format!("{}/query/icon/{}", self.base, keys::percent_encode(app_id));
        let resp = self.send(self.http.get(&url), &url).await?;
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/png")
            .to_string();
        Ok((ct, resp.bytes().await?.to_vec()))
    }

    /// `GET /query/app-state/{app_id}` — `active` | `background` |
    /// `inactive`. Developer mode only (Roku OS 13.0+).
    ///
    /// On a retail device this answers **HTTP 202** with a body saying
    /// why it refused, not an error status — so the failure has to be
    /// read out of the payload or it reads as an empty success.
    pub async fn app_state(&self, app_id: &str) -> Result<String> {
        let xml = self
            .get_text(&format!("query/app-state/{}", keys::percent_encode(app_id)))
            .await?;
        parse_app_state(&xml)
    }

    // ── Commands ─────────────────────────────────────────────────────────

    /// `POST /keypress/{key}` — press and release.
    pub async fn keypress(&self, key: &str) -> Result<()> {
        self.post_empty(&format!("keypress/{}", keys::encode_key_path(key)))
            .await
    }

    /// Hold a key for `hold` before releasing. ECP has no "repeat"
    /// primitive; `keydown` + delay + `keyup` is how the real remote's
    /// auto-repeat is reproduced (fast-forward speed steps, scrubbing).
    pub async fn key_hold(&self, key: &str, hold: Duration) -> Result<()> {
        let encoded = keys::encode_key_path(key);
        self.post_empty(&format!("keydown/{encoded}")).await?;
        tokio::time::sleep(hold).await;
        // Release even if the caller is cancelled mid-hold would be
        // better, but a dropped future here just leaves the key down
        // until the next command — Roku releases on its own remote
        // timeout. Not worth a guard type.
        self.post_empty(&format!("keyup/{encoded}")).await
    }

    pub async fn keydown(&self, key: &str) -> Result<()> {
        self.post_empty(&format!("keydown/{}", keys::encode_key_path(key)))
            .await
    }

    pub async fn keyup(&self, key: &str) -> Result<()> {
        self.post_empty(&format!("keyup/{}", keys::encode_key_path(key)))
            .await
    }

    /// Type a string on whatever text field is focused, one `Lit_` key
    /// per character.
    ///
    /// `inter_key_delay` exists because Roku's on-screen keyboards drop
    /// keys when they arrive faster than the UI redraws; ~50 ms is
    /// reliable on current firmware.
    pub async fn type_text(&self, text: &str, inter_key_delay: Duration) -> Result<usize> {
        let seq = keys::literal_sequence(text);
        let n = seq.len();
        for (i, key) in seq.iter().enumerate() {
            self.post_empty(&format!("keypress/{key}")).await?;
            if i + 1 < n && !inter_key_delay.is_zero() {
                tokio::time::sleep(inter_key_delay).await;
            }
        }
        Ok(n)
    }

    /// `POST /launch/{app_id}` with optional deep-link params.
    ///
    /// The same endpoint switches a Roku TV's input (`tvinput.hdmi1`) and
    /// tunes the tuner (`tvinput.dtv?ch=1.1`), so channel/input changes
    /// go through here rather than through the `Input*` remote keys —
    /// the keys only cycle, they can't target.
    pub async fn launch(&self, app_id: &str, params: &[(String, String)]) -> Result<()> {
        let mut path = format!("launch/{}", keys::percent_encode(app_id));
        append_query(&mut path, params);
        self.post_empty(&path).await
    }

    /// `POST /install/{app_id}` — leaves the current app and opens the
    /// channel store page for `app_id`. Does not install unattended.
    pub async fn install(&self, app_id: &str, params: &[(String, String)]) -> Result<()> {
        let mut path = format!("install/{}", keys::percent_encode(app_id));
        append_query(&mut path, params);
        self.post_empty(&path).await
    }

    /// `POST /input?…` — delivers arbitrary name/value pairs to the
    /// running app as an `roInput` event.
    pub async fn input(&self, params: &[(String, String)]) -> Result<()> {
        let mut path = String::from("input");
        append_query(&mut path, params);
        self.post_empty(&path).await
    }

    /// `POST /search/browse?…` — opens the search UI pre-filled.
    ///
    /// Roku sunset this endpoint in Roku OS 12.0: newer devices answer
    /// 200 and do nothing. Kept because it still works on the long tail
    /// of un-upgradable hardware, and a 200 is indistinguishable from
    /// success anyway.
    pub async fn search(&self, params: &[(String, String)]) -> Result<()> {
        let mut path = String::from("search/browse");
        append_query(&mut path, params);
        self.post_empty(&path).await
    }

    /// `POST /exit-app/{app_id}` — developer mode only (Roku OS 13.0+).
    pub async fn exit_app(&self, app_id: &str, force: bool) -> Result<()> {
        let mut path = format!("exit-app/{}", keys::percent_encode(app_id));
        if force {
            path.push_str("/true");
        }
        self.post_empty(&path).await
    }

    // ── Transport ────────────────────────────────────────────────────────

    async fn get_text(&self, path: &str) -> Result<String> {
        let url = format!("{}/{}", self.base, path);
        let resp = self.send(self.http.get(&url), &url).await?;
        Ok(resp.text().await?)
    }

    async fn post_empty(&self, path: &str) -> Result<()> {
        let url = format!("{}/{}", self.base, path);
        // ECP requires a POST with no body; some models reject a request
        // that omits Content-Length entirely, so send an explicit empty
        // body rather than a bodyless POST.
        self.send(self.http.post(&url).body(Vec::<u8>::new()), &url)
            .await?;
        Ok(())
    }

    async fn send(&self, req: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response> {
        debug!(url, "ECP request");
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("ECP request to {url} failed: {e}"))?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            StatusCode::FORBIDDEN => Err(EcpError::Forbidden.into()),
            StatusCode::NOT_FOUND => Err(EcpError::NotSupported.into()),
            s => Err(anyhow!("ECP request to {url} returned HTTP {s}")),
        }
    }
}

/// Append `?k=v&k=v` with both halves percent-encoded.
///
/// `contentID` values legitimately contain characters that would
/// otherwise split the query (Roku's own docs cap them at 255 chars and
/// forbid a raw `&`), so encoding here is what makes those values
/// passable at all.
fn append_query(path: &mut String, params: &[(String, String)]) {
    if params.is_empty() {
        return;
    }
    path.push('?');
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            path.push('&');
        }
        path.push_str(&keys::percent_encode(k));
        path.push('=');
        path.push_str(&keys::percent_encode(v));
    }
}

// ---------------------------------------------------------------------------
// XML parsing
// ---------------------------------------------------------------------------

pub fn parse_device_info(xml: &str) -> Result<DeviceInfo> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    let mut fields = BTreeMap::new();
    for child in root.children().filter(|n| n.is_element()) {
        let value = child.text().unwrap_or_default().trim().to_string();
        fields.insert(child.tag_name().name().to_string(), value);
    }
    Ok(DeviceInfo { fields })
}

pub fn parse_apps(xml: &str) -> Result<Vec<App>> {
    let doc = roxmltree::Document::parse(xml)?;
    Ok(doc
        .root_element()
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("app"))
        .map(|n| app_from_node(&n))
        .collect())
}

pub fn parse_active_app(xml: &str) -> Result<ActiveApp> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    let mut active = ActiveApp::default();
    for node in root.children().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            // The home screen is `<app>Roku</app>` — no id attribute.
            // Anything with an id is a real running app.
            "app" if node.attribute("id").is_some() => {
                active.app = Some(app_from_node(&node));
            }
            "screensaver" => active.screensaver = Some(app_from_node(&node)),
            _ => {}
        }
    }
    Ok(active)
}

fn app_from_node(node: &roxmltree::Node) -> App {
    App {
        id: node.attribute("id").unwrap_or_default().to_string(),
        name: node.text().unwrap_or_default().trim().to_string(),
        app_type: node.attribute("type").map(str::to_string),
        version: node.attribute("version").map(str::to_string),
    }
}

pub fn parse_media_player(xml: &str) -> Result<MediaPlayer> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    let mut mp = MediaPlayer {
        state: root.attribute("state").unwrap_or("close").to_string(),
        error: root.attribute("error") == Some("true"),
        ..Default::default()
    };
    for node in root.children().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            "plugin" => {
                mp.app_id = node.attribute("id").map(str::to_string);
                mp.app_name = node.attribute("name").map(str::to_string);
            }
            "format" => {
                for attr in node.attributes() {
                    mp.format
                        .insert(attr.name().to_string(), attr.value().to_string());
                }
            }
            "position" => mp.position_ms = parse_ms(node.text()),
            "duration" => mp.duration_ms = parse_ms(node.text()),
            "runtime" => mp.runtime_ms = parse_ms(node.text()),
            "is_live" => mp.is_live = node.text().map(str::trim) == Some("true"),
            _ => {}
        }
    }
    Ok(mp)
}

/// ECP writes durations as `"38813 ms"`, unit included.
fn parse_ms(text: Option<&str>) -> Option<u64> {
    let t = text?.trim();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Parse `<channel>` elements, skipping placeholders.
///
/// `query/tv-active-channel` on a TV that is *not* on the tuner returns a
/// well-formed document containing an empty `<channel/>`. Yielding that
/// as a channel makes "TV is showing HDMI 1" indistinguishable from "TV
/// is tuned to something", which propagates straight into the published
/// playback state — so a channel with no number is dropped here rather
/// than guarded at every call site.
/// `<app-state>` carries either the app's state or an `<error>` saying
/// why it could not be read. Roku returns 202 in both cases, so the
/// distinction lives entirely in the body.
pub fn parse_app_state(xml: &str) -> Result<String> {
    let doc = roxmltree::Document::parse(xml)?;
    let text = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    };
    if let Some(err) = text("error") {
        return Err(anyhow!("{err}"));
    }
    text("state")
        .or_else(|| text("status"))
        .ok_or_else(|| anyhow!("app-state response carried neither a state nor an error"))
}

pub fn parse_tv_channels(xml: &str) -> Result<Vec<TvChannel>> {
    let doc = roxmltree::Document::parse(xml)?;
    let mut out = Vec::new();
    for chan in doc
        .root_element()
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("channel"))
    {
        let mut fields = BTreeMap::new();
        for f in chan.children().filter(|n| n.is_element()) {
            fields.insert(
                f.tag_name().name().to_string(),
                f.text().unwrap_or_default().trim().to_string(),
            );
        }
        let channel = TvChannel { fields };
        if channel.number().is_some_and(|n| !n.is_empty()) {
            out.push(channel);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_INFO: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<device-info>
    <udn>015e5108-9000-1046-8035-b0a737964dfb</udn>
    <serial-number>1GU48T017973</serial-number>
    <device-id>1GU48T017973</device-id>
    <vendor-name>Roku</vendor-name>
    <model-number>4200X</model-number>
    <model-name>Roku 3</model-name>
    <is-tv>false</is-tv>
    <wifi-mac>b0:a7:37:96:4d:fb</wifi-mac>
    <ethernet-mac>b0:a7:37:96:4d:fa</ethernet-mac>
    <user-device-name>My Roku 3</user-device-name>
    <software-version>7.5.0</software-version>
    <power-mode>PowerOn</power-mode>
    <supports-find-remote>false</supports-find-remote>
</device-info>"#;

    #[test]
    fn device_info_keeps_every_field_and_types_the_ones_we_use() {
        let d = parse_device_info(DEVICE_INFO).unwrap();
        assert_eq!(d.get("model-name"), Some("Roku 3"));
        assert_eq!(d.serial(), Some("1GU48T017973"));
        assert!(d.is_powered_on());
        assert!(!d.is_tv());
        assert!(!d.flag("supports-find-remote"));
        // Unknown-to-us fields survive so new firmware reaches homeCore.
        assert_eq!(d.get("udn"), Some("015e5108-9000-1046-8035-b0a737964dfb"));
    }

    /// Captured from a real Roku TV: the device reports the control
    /// setting itself, so the plugin never has to guess.
    #[test]
    fn ecp_control_setting_is_read_from_device_info() {
        let enabled = parse_device_info(
            "<device-info><ecp-setting-mode>enabled</ecp-setting-mode></device-info>",
        )
        .unwrap();
        assert!(enabled.ecp_control_enabled());

        let disabled = parse_device_info(
            "<device-info><ecp-setting-mode>disabled</ecp-setting-mode></device-info>",
        )
        .unwrap();
        assert!(!disabled.ecp_control_enabled());

        // Firmware that predates the field has no restriction to report.
        assert!(parse_device_info("<device-info/>")
            .unwrap()
            .ecp_control_enabled());
    }

    #[test]
    fn standby_is_not_powered_on() {
        let xml = DEVICE_INFO.replace("PowerOn", "Ready");
        assert!(!parse_device_info(&xml).unwrap().is_powered_on());
    }

    #[test]
    fn wake_macs_prefer_ethernet() {
        let d = parse_device_info(DEVICE_INFO).unwrap();
        assert_eq!(
            d.wake_macs(),
            vec!["b0:a7:37:96:4d:fa", "b0:a7:37:96:4d:fb"]
        );
    }

    #[test]
    fn apps_parse_with_and_without_attributes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<apps>
    <app id="tvinput.hdmi1" type="tvin" version="1.0.0">Blu-ray player</app>
    <app id="12">Netflix</app>
    <app id="74519" subtype="rsga" type="appl" version="5.2.0">Pluto TV - It's Free TV</app>
</apps>"#;
        let apps = parse_apps(xml).unwrap();
        assert_eq!(apps.len(), 3);
        assert!(apps[0].is_input());
        assert_eq!(apps[0].name, "Blu-ray player");
        // Pre-installed channels on old firmware carry no type/version.
        assert_eq!(apps[1].name, "Netflix");
        assert!(apps[1].app_type.is_none());
        assert!(!apps[1].is_input());
        assert_eq!(apps[2].version.as_deref(), Some("5.2.0"));
    }

    /// The home screen and a running app are both `<app>` elements; only
    /// the id tells them apart. Treating "Roku" as an app would report
    /// the home screen as the active source forever.
    #[test]
    fn home_screen_is_not_an_active_app() {
        let home = r#"<active-app><app>Roku</app></active-app>"#;
        assert!(parse_active_app(home).unwrap().is_home());

        let netflix =
            r#"<active-app><app id="12" type="appl" version="4.1.218">Netflix</app></active-app>"#;
        let a = parse_active_app(netflix).unwrap();
        assert!(!a.is_home());
        assert_eq!(a.app.unwrap().name, "Netflix");
    }

    /// Captured from a Roku TV on Roku OS 15.2. Before this was
    /// handled, the home screen registered as a running channel called
    /// "Roku Dynamic Menu".
    #[test]
    fn modern_firmware_reports_the_home_screen_as_a_typed_app() {
        let xml = r#"<active-app><app id="562859" type="home" version="14.10.5" ui-location="home">Roku Dynamic Menu</app></active-app>"#;
        let a = parse_active_app(xml).unwrap();
        assert!(a.is_home());
        // The app itself is still parsed — only the interpretation changes.
        assert_eq!(a.app.unwrap().id, "562859");
    }

    #[test]
    fn screensaver_is_reported_alongside_the_home_screen() {
        let xml = r#"<active-app>
            <app>Roku</app>
            <screensaver id="55545" type="ssvr" version="2.0.1">Default screensaver</screensaver>
        </active-app>"#;
        let a = parse_active_app(xml).unwrap();
        assert!(a.is_home());
        assert_eq!(a.screensaver.unwrap().id, "55545");
    }

    #[test]
    fn tv_input_reports_as_the_active_app() {
        let xml = r#"<active-app><app id="tvinput.dtv" type="tvin" version="1.0.0">Antenna TV</app></active-app>"#;
        let app = parse_active_app(xml).unwrap().app.unwrap();
        assert!(app.is_input());
        assert_eq!(app.id, "tvinput.dtv");
    }

    #[test]
    fn media_player_parses_positions_with_units() {
        let xml = r#"<player error="false" state="play">
  <plugin bandwidth="10000000 bps" id="74519" name="Pluto TV - It's Free TV"/>
  <format audio="aac_adts" captions="webvtt" container="hls" drm="none" video="mpeg4_10b"/>
  <position>38813 ms</position>
  <duration>6496762 ms</duration>
  <is_live>false</is_live>
  <runtime>15000 ms</runtime>
</player>"#;
        let mp = parse_media_player(xml).unwrap();
        assert_eq!(mp.state, "play");
        assert!(!mp.error);
        assert_eq!(mp.position_ms, Some(38813));
        assert_eq!(mp.duration_ms, Some(6_496_762));
        assert_eq!(mp.app_name.as_deref(), Some("Pluto TV - It's Free TV"));
        assert_eq!(mp.format.get("container").map(String::as_str), Some("hls"));
        assert!(!mp.is_live);
    }

    /// Nothing playing: no `<plugin>`, no position, no duration. Parsing
    /// must not invent zeros — a `media_position` of 0 would look like a
    /// stream sitting at the start.
    #[test]
    fn closed_player_has_no_position() {
        let xml = r#"<player error="false" state="close">
  <format audio="eac3" captions="none" drm="none" video="hevc_b"/>
  <is_live>false</is_live>
</player>"#;
        let mp = parse_media_player(xml).unwrap();
        assert_eq!(mp.state, "close");
        assert!(mp.position_ms.is_none());
        assert!(mp.duration_ms.is_none());
        assert!(mp.app_id.is_none());
    }

    #[test]
    fn live_stream_is_flagged() {
        let xml = r#"<player error="false" state="play"><is_live>true</is_live></player>"#;
        assert!(parse_media_player(xml).unwrap().is_live);
    }

    #[test]
    fn tv_channels_parse_and_expose_hidden_flag() {
        let xml = r#"<tv-channels>
    <channel><number>1.1</number><name>WhatsOn</name><type>air-digital</type><user-hidden>false</user-hidden></channel>
    <channel><number>1.3</number><name>QVC</name><type>air-digital</type><user-hidden>true</user-hidden></channel>
</tv-channels>"#;
        let chans = parse_tv_channels(xml).unwrap();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[0].number(), Some("1.1"));
        assert!(!chans[0].hidden());
        assert!(chans[1].hidden());
    }

    #[test]
    fn active_channel_carries_program_metadata() {
        let xml = r#"<tv-channel><channel>
        <number>14.3</number><name>getTV</name><signal-quality>20</signal-quality>
        <program-title>Airwolf</program-title><program-has-cc>true</program-has-cc>
        </channel></tv-channel>"#;
        let c = parse_tv_channels(xml).unwrap().remove(0);
        assert_eq!(c.get("program-title"), Some("Airwolf"));
        let j = c.to_json();
        // Kebab keys become snake for homeCore, and ECP's stringly-typed
        // values become real JSON types so rules can compare them.
        assert_eq!(j["program_has_cc"], serde_json::json!(true));
        assert_eq!(j["signal_quality"], serde_json::json!(20));
        assert_eq!(j["number"], serde_json::json!("14.3"));
    }

    /// Captured from a retail Roku TV (OS 15.2): HTTP 202, and the
    /// reason is only in the body. Returning `Ok("")` here would tell an
    /// operator the app is in some nameless state rather than that
    /// developer mode is off.
    #[test]
    fn app_state_surfaces_the_refusal_instead_of_an_empty_success() {
        let xml = r#"<app-state>
            <app-id>12</app-id>
            <status>FAILED</status>
            <error>Development Application installer is not enabled.</error>
        </app-state>"#;
        let err = parse_app_state(xml).unwrap_err().to_string();
        assert!(err.contains("Development Application installer"), "{err}");
    }

    #[test]
    fn app_state_reads_the_state_when_there_is_one() {
        assert_eq!(
            parse_app_state("<app-state><state>active</state></app-state>").unwrap(),
            "active"
        );
    }

    /// Observed on a Roku TV (OS 15.2) showing HDMI 1: the query
    /// succeeds and returns a channel element with nothing in it.
    #[test]
    fn an_untuned_tv_yields_no_active_channel() {
        assert!(parse_tv_channels("<tv-channel><channel/></tv-channel>")
            .unwrap()
            .is_empty());
        assert!(parse_tv_channels(
            "<tv-channel><channel><number></number><name></name></channel></tv-channel>"
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn query_string_is_encoded() {
        let mut p = String::from("launch/12");
        append_query(
            &mut p,
            &[
                ("contentID".into(), "a b&c".into()),
                ("MediaType".into(), "movie".into()),
            ],
        );
        assert_eq!(p, "launch/12?contentID=a%20b%26c&MediaType=movie");
    }

    #[test]
    fn empty_params_leave_the_path_alone() {
        let mut p = String::from("input");
        append_query(&mut p, &[]);
        assert_eq!(p, "input");
    }
}

/// End-to-end tests against a stub ECP server.
///
/// The unit tests above prove the XML parses and the query string
/// assembles; these prove the client actually puts that on the wire —
/// the right method, the right path, the right number of requests. A
/// wrong verb or a double-encoded path segment is invisible to a parser
/// test and produces a device that silently ignores every command.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::testutil::{paths, stub};

    #[tokio::test]
    async fn device_info_round_trips() {
        let (client, log) = stub(
            1,
            200,
            "<device-info><model-name>Roku Ultra</model-name>\
             <power-mode>PowerOn</power-mode></device-info>",
        )
        .await;
        let info = client.device_info().await.unwrap();
        assert_eq!(info.get("model-name"), Some("Roku Ultra"));
        assert!(info.is_powered_on());
        assert_eq!(paths(&log), vec!["/query/device-info"]);
    }

    /// ECP keypresses are POSTs. Sending a GET returns 200 and does
    /// nothing, which is the hardest possible failure to notice.
    #[tokio::test]
    async fn keypress_is_a_post_to_the_unescaped_key_path() {
        let (client, log) = stub(1, 200, "").await;
        client.keypress("Home").await.unwrap();
        assert_eq!(
            log.lock().unwrap().as_slice(),
            [("POST".to_string(), "/keypress/Home".to_string())]
        );
    }

    #[tokio::test]
    async fn launch_carries_encoded_deep_link_params() {
        let (client, log) = stub(1, 200, "").await;
        client
            .launch(
                "12",
                &[
                    ("contentID".into(), "tt 99&x".into()),
                    ("MediaType".into(), "movie".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            paths(&log)[0],
            "/launch/12?contentID=tt%2099%26x&MediaType=movie"
        );
    }

    /// One `Lit_` request per character, each percent-encoded once.
    #[tokio::test]
    async fn typing_sends_one_request_per_character() {
        let (client, log) = stub(3, 200, "").await;
        let n = client
            .type_text("a \u{20ac}", Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            paths(&log),
            vec![
                "/keypress/Lit_a",
                "/keypress/Lit_%20",
                "/keypress/Lit_%E2%82%AC",
            ]
        );
    }

    /// A 403 is the "Control by mobile apps is disabled" case and must
    /// surface as that, not as a generic HTTP error — it is the single
    /// most common reason a Roku accepts nothing.
    #[tokio::test]
    async fn forbidden_is_reported_with_the_fix() {
        let (client, _log) = stub(1, 403, "").await;
        let err = client.keypress("Home").await.unwrap_err();
        assert!(err.downcast_ref::<EcpError>().is_some());
        assert!(
            err.to_string().contains("Control by mobile apps"),
            "unhelpful error: {err}"
        );
    }

    #[tokio::test]
    async fn key_hold_sends_down_then_up() {
        let (client, log) = stub(2, 200, "").await;
        client
            .key_hold("Right", Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(paths(&log), vec!["/keydown/Right", "/keyup/Right"]);
    }
}
