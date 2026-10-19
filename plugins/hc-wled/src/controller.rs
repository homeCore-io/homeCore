//! A controller's own settings (`/json/cfg`), its files, and backups of both.
//!
//! The settings are what WLED's settings pages edit: LED outputs, buttons,
//! timers, sync, the clock, Wi-Fi. Read them with [`settings`], which hides
//! every secret; change them with [`apply_settings`], which posts only what is
//! given — WLED merges it — and never a hidden value back.
//!
//! A backup is `{presets, settings}`: `presets.json` as the controller holds
//! it, and `cfg.json` with its secrets hidden — a backup isn't a place for a
//! Wi-Fi password. Restoring settings therefore keeps the controller's own
//! secrets; restoring presets replaces the file.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

/// What a hidden value reads as.
pub const HIDDEN: &str = "••••••";

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client")
}

/// Keys that hold a secret, wherever they are.
fn secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "psk" | "pwd" | "pass" | "password" | "pin" | "key" | "token" | "apikey" | "api_key"
    ) || k.ends_with("_pass")
        || k.ends_with("_psk")
        || k.ends_with("_pwd")
        || k.ends_with("token")
}

/// The same document with every secret replaced by [`HIDDEN`] (an empty one
/// stays empty, so "none set" can still be told apart).
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let v = if secret(k) && !v.is_object() && !v.is_array() {
                        match v.as_str() {
                            Some("") => json!(""),
                            _ if v.is_null() => Value::Null,
                            _ => json!(HIDDEN),
                        }
                    } else {
                        redact(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// The same document with every value still [`HIDDEN`] taken out — what is
/// posted back must never overwrite a secret with the mask.
pub fn without_hidden(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| v.as_str() != Some(HIDDEN))
                .map(|(k, v)| (k.clone(), without_hidden(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(without_hidden).collect()),
        other => other.clone(),
    }
}

async fn get(host: &str, path: &str) -> Result<Value> {
    let resp = client(Duration::from_secs(8))
        .get(format!("http://{host}{path}"))
        .send()
        .await
        .with_context(|| format!("GET {path}"))?;
    if !resp.status().is_success() {
        bail!("{path}: {}", resp.status());
    }
    resp.json().await.with_context(|| format!("parse {path}"))
}

/// The settings, secrets hidden.
pub async fn settings(host: &str) -> Result<Value> {
    Ok(redact(&get(host, "/json/cfg").await?))
}

/// The presets file as the controller holds it.
pub async fn presets_file(host: &str) -> Result<Value> {
    get(host, "/presets.json").await
}

/// Post settings; WLED merges them. Some (LED outputs, Wi-Fi) only take
/// effect after a restart, which is the caller's to ask for.
pub async fn apply_settings(host: &str, patch: &Value) -> Result<()> {
    let body = without_hidden(patch);
    if !body.is_object() {
        bail!("settings are an object");
    }
    let resp = client(Duration::from_secs(10))
        .post(format!("http://{host}/json/cfg"))
        .json(&body)
        .send()
        .await
        .context("POST /json/cfg")?;
    if !resp.status().is_success() {
        bail!("the controller said {}", resp.status());
    }
    Ok(())
}

/// Replace a file on the controller (`/upload`, as its file editor does).
/// `presets.json` and palettes are taken at once; `cfg.json` restarts it.
pub async fn upload_file(host: &str, name: &str, contents: Vec<u8>) -> Result<()> {
    if !matches!(name, "presets.json" | "cfg.json")
        && !(name.starts_with("palette") && name.ends_with(".json"))
        && !(name.starts_with("ledmap") && name.ends_with(".json"))
    {
        bail!("not a file this plugin writes: {name}");
    }
    let part = reqwest::multipart::Part::bytes(contents)
        .file_name(format!("/{name}"))
        .mime_str("application/json")?;
    let form = reqwest::multipart::Form::new().part("data", part);
    let resp = client(Duration::from_secs(20))
        .post(format!("http://{host}/upload"))
        .multipart(form)
        .send()
        .await
        .context("POST /upload")?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        if text.contains("PIN") || text.contains("unlock") {
            bail!("the controller's settings are locked with a PIN");
        }
        bail!("the controller wouldn't take {name}");
    }
    Ok(())
}

/// A backup: presets and settings (secrets hidden), and what it was taken
/// from.
pub async fn backup(host: &str) -> Result<Value> {
    let presets = presets_file(host).await?;
    let settings = settings(host).await?;
    let info = get(host, "/json/info").await.unwrap_or(Value::Null);
    Ok(json!({
        "kind": "hc-wled-backup",
        "version": 1,
        "taken_at": chrono::Utc::now().to_rfc3339(),
        "controller": {
            "name": info.get("name"),
            "firmware": info.get("ver"),
            "build": info.get("release"),
            "mac": info.get("mac"),
            "leds": info.get("leds").and_then(|l| l.get("count")),
        },
        "presets": presets,
        "settings": settings,
    }))
}

/// What a restore puts back.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreParts {
    pub presets: bool,
    pub settings: bool,
}

/// Put a backup (or another controller's presets) on a controller.
/// Presets replace the file; settings are merged, the controller's own
/// secrets kept.
pub async fn restore(host: &str, backup: &Value, parts: RestoreParts) -> Result<Value> {
    if backup.get("kind").and_then(Value::as_str) != Some("hc-wled-backup") {
        bail!("this isn't a WLED backup from homeCore");
    }
    let mut done = Vec::new();
    if parts.presets {
        let p = backup
            .get("presets")
            .filter(|p| p.is_object())
            .context("the backup has no presets")?;
        upload_file(host, "presets.json", serde_json::to_vec(p)?).await?;
        done.push("presets");
    }
    if parts.settings {
        let s = backup
            .get("settings")
            .filter(|s| s.is_object())
            .context("the backup has no settings")?;
        apply_settings(host, s).await?;
        done.push("settings");
    }
    Ok(json!({ "restored": done }))
}

/// A controller's settings that this plugin shows and edits by name — the
/// rest are in `settings` whole. Each section is the part of `cfg.json` it
/// came from, kept in WLED's shape.
pub fn sections(cfg: &Value) -> Value {
    let pick = |path: &[&str]| {
        let mut v = cfg;
        for p in path {
            match v.get(*p) {
                Some(x) => v = x,
                None => return Value::Null,
            }
        }
        v.clone()
    };
    let mut out = Map::new();
    for (name, path) in [
        ("led_outputs", &["hw", "led", "ins"][..]),
        ("power_limit_ma", &["hw", "led", "maxpwr"][..]),
        ("white_mode", &["hw", "led", "rgbwm"][..]),
        ("buttons", &["hw", "btn", "ins"][..]),
        ("ir", &["hw", "ir"][..]),
        ("power_on", &["def"][..]),
        ("nightlight_defaults", &["light", "nl"][..]),
        ("transition_defaults", &["light", "tr"][..]),
        ("gamma", &["light", "gc"][..]),
        ("sync", &["if", "sync"][..]),
        ("live", &["if", "live"][..]),
        ("clock", &["if", "ntp"][..]),
        ("timers", &["timers", "ins"][..]),
        ("network", &["nw"][..]),
        ("access_point", &["ap"][..]),
        ("identity", &["id"][..]),
        ("ota", &["ota"][..]),
        ("audio", &["um", "AudioReactive"][..]),
    ] {
        out.insert(name.into(), pick(path));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_hidden_and_never_written_back() {
        let cfg = json!({
            "nw": {"ins": [{"ssid": "house", "psk": "hunter2", "ip": [0,0,0,0]}]},
            "ap": {"ssid": "WLED-AP", "psk": ""},
            "ota": {"lock": false, "pwd": "x"},
            "if": {"mqtt": {"en": false, "pass": "p", "user": "u"}, "hue": {"key": "k"}},
            "hw": {"led": {"maxpwr": 0}}
        });
        let r = redact(&cfg);
        assert_eq!(r["nw"]["ins"][0]["psk"], HIDDEN);
        assert_eq!(r["nw"]["ins"][0]["ssid"], "house");
        assert_eq!(r["ap"]["psk"], "", "an empty one says none is set");
        assert_eq!(r["ota"]["pwd"], HIDDEN);
        assert_eq!(r["if"]["mqtt"]["pass"], HIDDEN);
        assert_eq!(r["if"]["hue"]["key"], HIDDEN);
        assert!(!r.to_string().contains("hunter2"));
        // Posting the masked document back leaves every secret out.
        let back = without_hidden(&r);
        assert!(back["nw"]["ins"][0].get("psk").is_none());
        assert_eq!(back["nw"]["ins"][0]["ssid"], "house");
        assert_eq!(back["hw"]["led"]["maxpwr"], 0);
    }

    #[test]
    fn the_named_sections_come_from_where_wled_keeps_them() {
        // From the house controller's cfg.json.
        let cfg = json!({
            "hw": {"led": {"ins": [{"pin": [16], "type": 22, "len": 400, "start": 0, "order": 1, "rev": false}], "maxpwr": 0, "rgbwm": 255},
                   "btn": {"ins": [{"pin": [17], "type": 2}]}},
            "def": {"ps": 16, "on": false, "bri": 128},
            "timers": {"ins": [{"en": 0, "hour": 255, "min": 0, "macro": 0, "dow": 127}]},
            "um": {"AudioReactive": {"enabled": true}}
        });
        let s = sections(&cfg);
        assert_eq!(s["led_outputs"][0]["len"], 400);
        assert_eq!(s["buttons"][0]["pin"], json!([17]));
        assert_eq!(s["power_on"]["ps"], 16);
        assert_eq!(s["timers"][0]["hour"], 255);
        assert_eq!(s["audio"]["enabled"], true);
        assert!(s["sync"].is_null());
    }
}
