//! Reolink's HTTP API (`/cgi-bin/api.cgi`), as cameras, NVRs and Home Hubs
//! answer it.
//!
//! Every call is a JSON array of commands, `[{"cmd": "GetDevInfo", "action":
//! 0, "param": {}}]`, answered by an array in the same order, each entry with
//! `code` 0 and a `value`, or an `error` with Reolink's `rspCode`. A session
//! is a token from `Login`, good for its `leaseTime` seconds: one is kept per
//! address and renewed before it runs out, because a device allows only a few
//! sessions and a crashed client's linger for an hour. Logins are one at a
//! time, and too many refused ones lock the account for five minutes, so a
//! refusal waits rather than retrying.
//!
//! The parsing is plain functions over `serde_json::Value`, tested against
//! answers in Reolink's documented shapes; the talking is [`Reolink`].

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Renew a token this long before it runs out.
const RENEW_EARLY: Duration = Duration::from_secs(300);

/// What went wrong with one command.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub rsp_code: i64,
    pub detail: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.detail, self.rsp_code)
    }
}

impl ApiError {
    /// The token is gone or never was: sign in again and repeat.
    pub fn needs_login(&self) -> bool {
        self.rsp_code == -6 || self.detail.contains("login")
    }
}

/// Each command's answer, in order: its `value`, or what went wrong.
pub fn results(answer: &Value) -> Vec<std::result::Result<Value, ApiError>> {
    answer
        .as_array()
        .map(|list| {
            list.iter()
                .map(|e| {
                    if e["code"].as_i64() == Some(0) {
                        Ok(e["value"].clone())
                    } else {
                        Err(ApiError {
                            rsp_code: e["error"]["rspCode"].as_i64().unwrap_or(-1),
                            detail: e["error"]["detail"]
                                .as_str()
                                .unwrap_or("refused")
                                .to_string(),
                        })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A `Login` answer's token and how long it lasts.
pub fn parse_login(answer: &Value) -> std::result::Result<(String, u64), ApiError> {
    let first = results(answer).into_iter().next().ok_or(ApiError {
        rsp_code: -1,
        detail: "no answer".into(),
    })??;
    let name = first["Token"]["name"].as_str().unwrap_or_default();
    if name.is_empty() {
        return Err(ApiError {
            rsp_code: -1,
            detail: "no token".into(),
        });
    }
    Ok((
        name.to_string(),
        first["Token"]["leaseTime"].as_u64().unwrap_or(3600),
    ))
}

/// What the address is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevInfo {
    pub model: String,
    pub name: String,
    pub firmware: String,
    pub channels: u32,
    /// An NVR or Home Hub, whose cameras are channels. A camera that reports
    /// two channels (a Duo) isn't one: it is still one camera.
    pub hub: bool,
}

pub fn parse_dev_info(value: &Value) -> DevInfo {
    let d = &value["DevInfo"];
    let text = |k: &str| d[k].as_str().unwrap_or_default().to_string();
    let channels = d["channelNum"].as_u64().unwrap_or(1) as u32;
    let kind = format!("{} {}", text("type"), text("exactType")).to_uppercase();
    DevInfo {
        model: text("model"),
        name: text("name"),
        firmware: text("firmVer"),
        channels,
        hub: kind.contains("NVR") || kind.contains("HUB"),
    }
}

/// One of an NVR's or Home Hub's channels.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelInfo {
    pub channel: u32,
    pub name: String,
    pub online: bool,
    pub model: String,
    pub uid: String,
}

impl ChannelInfo {
    /// A slot with no camera in it: no name, no uid, not online. An RLN36
    /// lists all 36 whether or not a camera was ever added.
    pub fn empty(&self) -> bool {
        !self.online && self.name.trim().is_empty() && self.uid.trim().is_empty()
    }
}

/// A channel's own model (`GetChnTypeInfo`), which the channel list leaves out.
pub fn parse_type_info(value: &Value) -> String {
    value["typeInfo"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn parse_channels(value: &Value) -> Vec<ChannelInfo> {
    value["status"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|c| ChannelInfo {
                    channel: c["channel"].as_u64().unwrap_or(0) as u32,
                    name: c["name"].as_str().unwrap_or_default().to_string(),
                    online: c["online"].as_i64().unwrap_or(0) == 1,
                    model: c["typeInfo"].as_str().unwrap_or_default().to_string(),
                    uid: c["uid"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a camera can do, from its abilities (`GetAbility`) and what it said
/// it detects. Read, never guessed: they differ by model and firmware.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
    pub package: bool,
    pub doorbell: bool,
    pub spotlight: bool,
    pub siren: bool,
    pub ptz: bool,
    pub ir: bool,
    /// What the camera has but the account signed in may not use ("the
    /// spotlight"), so it can be said instead of offered.
    pub denied: Vec<&'static str>,
}

/// Whether an ability is there (`{"permit": …, "ver": n}` with `ver` above
/// 0), and whether the account signed in may use it: `permit` is a bitmask,
/// 4 to read, 2 to change a setting, 1 to operate. A user who may only
/// watch has 4 on the floodlight and 0 on the siren. No `permit` at all
/// (older firmware) is taken as allowed.
fn able(entry: &Value, keys: &[&str], bits: i64) -> Ability {
    let mut out = Ability::Absent;
    for k in keys {
        let e = &entry[*k];
        if e["ver"].as_i64().unwrap_or(0) <= 0 {
            continue;
        }
        match e["permit"].as_i64() {
            Some(p) if p & bits == 0 => out = Ability::Denied,
            _ => return Ability::Allowed,
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ability {
    Absent,
    Denied,
    Allowed,
}

const WRITE: i64 = 2;
const OPERATE: i64 = 1 | 2;

/// A channel's abilities (one entry of `Ability.abilityChn`), with the
/// spellings models use for each.
pub fn parse_abilities(chn: &Value) -> Features {
    let spotlight = able(
        chn,
        &["floodLight", "supportFLswitch", "supportFLBrightness"],
        WRITE,
    );
    let siren = able(
        chn,
        &["alarmAudio", "supportAudioAlarm", "supportAudioAlarmEnable"],
        OPERATE,
    );
    let ptz = able(chn, &["ptzCtrl", "ptzType", "ptzPreset"], 1);
    let ir = able(chn, &["alarmIrLights", "ledControl"], WRITE);
    let denied = [
        (spotlight, "the spotlight"),
        (siren, "the siren"),
        (ptz, "PTZ"),
        (ir, "infrared"),
    ]
    .into_iter()
    .filter(|(a, _)| *a == Ability::Denied)
    .map(|(_, what)| what)
    .collect();
    Features {
        motion: true,
        spotlight: spotlight == Ability::Allowed,
        siren: siren == Ability::Allowed,
        ptz: ptz == Ability::Allowed,
        ir: ir == Ability::Allowed,
        denied,
        ..Features::default()
    }
}

/// `GetAbility`'s abilities for one channel; an address on its own is channel 0.
pub fn ability_for(value: &Value, channel: u32) -> Value {
    value["Ability"]["abilityChn"]
        .as_array()
        .and_then(|l| l.get(channel as usize))
        .cloned()
        .unwrap_or(Value::Null)
}

/// What a camera sees now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Detections {
    pub motion: bool,
    pub person: bool,
    pub vehicle: bool,
    pub animal: bool,
    pub package: bool,
    /// The doorbell is being pressed.
    pub visitor: bool,
}

impl Detections {
    /// Something started that wasn't there before: worth a snapshot.
    pub fn began(&self, before: &Detections) -> bool {
        (self.motion && !before.motion)
            || (self.person && !before.person)
            || (self.vehicle && !before.vehicle)
            || (self.animal && !before.animal)
            || (self.package && !before.package)
            || (self.visitor && !before.visitor)
    }
}

fn on(v: &Value) -> bool {
    v["alarm_state"].as_i64().unwrap_or(0) == 1
}
fn supported(v: &Value) -> bool {
    v["support"].as_i64().unwrap_or(0) == 1
}

/// `GetEvents`, which newer firmware answers with everything at once.
pub fn parse_events(value: &Value) -> (Detections, Features) {
    let ai = &value["ai"];
    let det = Detections {
        motion: on(&value["md"]),
        person: on(&ai["people"]),
        vehicle: on(&ai["vehicle"]),
        animal: on(&ai["dog_cat"]),
        package: on(&ai["package"]),
        visitor: on(&value["visitor"]),
    };
    let f = Features {
        motion: supported(&value["md"]) || !value["md"].is_null(),
        person: supported(&ai["people"]),
        vehicle: supported(&ai["vehicle"]),
        animal: supported(&ai["dog_cat"]),
        package: supported(&ai["package"]),
        doorbell: supported(&value["visitor"]),
        ..Features::default()
    };
    (det, f)
}

/// `GetMdState` and `GetAiState`, for firmware without `GetEvents`.
pub fn parse_md_ai(md: &Value, ai: &Value) -> (Detections, Features) {
    let det = Detections {
        motion: md["state"].as_i64().unwrap_or(0) == 1,
        person: on(&ai["people"]),
        vehicle: on(&ai["vehicle"]),
        animal: on(&ai["dog_cat"]),
        package: on(&ai["package"]),
        visitor: false,
    };
    let f = Features {
        motion: true,
        person: supported(&ai["people"]),
        vehicle: supported(&ai["vehicle"]),
        animal: supported(&ai["dog_cat"]),
        package: supported(&ai["package"]),
        ..Features::default()
    };
    (det, f)
}

/// A PTZ camera's presets, enabled ones only, by id.
pub fn parse_presets(value: &Value) -> Vec<(u32, String)> {
    value["PtzPreset"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter(|p| p["enable"].as_i64().unwrap_or(0) == 1)
                .map(|p| {
                    let id = p["id"].as_u64().unwrap_or(0) as u32;
                    let name = p["name"].as_str().unwrap_or_default();
                    (
                        id,
                        if name.is_empty() {
                            format!("Preset {id}")
                        } else {
                            name.to_string()
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The spotlight's state: on, and its brightness 0–100.
pub fn parse_white_led(value: &Value) -> (bool, u8) {
    let w = &value["WhiteLed"];
    (
        w["state"].as_i64().unwrap_or(0) == 1,
        w["bright"].as_u64().unwrap_or(100).min(100) as u8,
    )
}

// ---- commands, as Reolink spells them ----

pub fn cmd(name: &str, param: Value) -> Value {
    json!({ "cmd": name, "action": 0, "param": param })
}

pub fn set_spotlight(channel: u32, on: bool, bright: Option<u8>) -> Value {
    let mut led = json!({ "channel": channel, "state": u8::from(on) });
    if let Some(b) = bright {
        led["bright"] = json!(b.clamp(1, 100));
    }
    cmd("SetWhiteLed", json!({ "WhiteLed": led }))
}

/// Sound the siren `times` times (about five seconds each); it stops by
/// itself after.
pub fn siren_times(channel: u32, times: u32) -> Value {
    cmd(
        "AudioAlarmPlay",
        json!({ "alarm_mode": "times", "times": times.max(1), "channel": channel }),
    )
}

/// Start (`true`) or stop (`false`) the siren. Started this way it sounds
/// until it's stopped. reolink_aio (MIT) does the same.
pub fn siren_manual(channel: u32, on: bool) -> Value {
    cmd(
        "AudioAlarmPlay",
        json!({ "alarm_mode": "manul", "manual_switch": u8::from(on), "channel": channel }),
    )
}

pub fn ptz(channel: u32, op: &str, speed: u32, preset: Option<u32>) -> Value {
    let mut p = json!({ "channel": channel, "op": op, "speed": speed });
    if let Some(id) = preset {
        p["id"] = json!(id);
    }
    cmd("PtzCtrl", p)
}

pub fn set_ir(channel: u32, auto: bool) -> Value {
    cmd(
        "SetIrLights",
        json!({ "IrLights": { "channel": channel, "state": if auto { "Auto" } else { "Off" } } }),
    )
}

/// homeCore's direction as Reolink's PTZ operation.
pub fn ptz_op(direction: &str) -> Option<&'static str> {
    Some(match direction {
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "zoom_in" => "ZoomInc",
        "zoom_out" => "ZoomDec",
        _ => return None,
    })
}

// ---- talking to it ----

/// One address, signed in to once.
pub struct Reolink {
    http: reqwest::Client,
    pub base: String,
    user: String,
    pass: String,
    session: Mutex<Option<(String, Instant)>>,
}

impl Reolink {
    pub fn new(address: &str, https: bool, user: &str, pass: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Reolink's certificates are its own, self-signed.
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base: format!("{}://{address}", if https { "https" } else { "http" }),
            user: user.into(),
            pass: pass.into(),
            session: Mutex::new(None),
        })
    }

    /// Why a request got no answer, in words someone can act on. reqwest's
    /// own message carries the URL, and with it the session token.
    fn unreachable(&self, e: &reqwest::Error) -> anyhow::Error {
        let https = self.base.starts_with("https");
        let hint = if https {
            " If HTTPS is off on the device, turn HTTPS off in this row."
        } else {
            " If the device only answers HTTPS, turn HTTPS on in this row."
        };
        let what = if e.is_timeout() {
            format!("{} didn't answer in 15 s. Is the address right?", self.base)
        } else if e.is_connect() {
            format!(
                "{} refused the connection or couldn't be reached.{hint}",
                self.base
            )
        } else {
            format!("{} broke off the request.{hint}", self.base)
        };
        anyhow!(what)
    }

    fn url(&self, cmd: &str, token: Option<&str>) -> String {
        match token {
            Some(t) => format!("{}/cgi-bin/api.cgi?cmd={cmd}&token={t}", self.base),
            None => format!("{}/cgi-bin/api.cgi?cmd={cmd}", self.base),
        }
    }

    /// The token, signing in first if there's none or it's about to run out.
    /// The lock keeps sign-ins one at a time.
    async fn token(&self) -> Result<String> {
        let mut s = self.session.lock().await;
        if let Some((t, until)) = s.as_ref() {
            if Instant::now() + RENEW_EARLY < *until {
                return Ok(t.clone());
            }
        }
        // An old token is given back first, so it doesn't hold a session.
        if let Some((old, _)) = s.take() {
            let _ = self
                .http
                .post(self.url("Logout", Some(&old)))
                .json(&json!([cmd("Logout", json!({}))]))
                .send()
                .await;
        }
        let body = json!([{
            "cmd": "Login",
            "action": 0,
            "param": { "User": { "Version": "0", "userName": self.user, "password": self.pass } }
        }]);
        let answer: Value = self
            .http
            .post(self.url("Login", None))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.unreachable(&e))?
            .json()
            .await
            .context("not a Reolink answer")?;
        let (token, lease) = parse_login(&answer).map_err(|e| anyhow!(LoginRefused(e)))?;
        *s = Some((token.clone(), Instant::now() + Duration::from_secs(lease)));
        Ok(token)
    }

    /// Give the session back (on stopping).
    pub async fn logout(&self) {
        if let Some((t, _)) = self.session.lock().await.take() {
            let _ = self
                .http
                .post(self.url("Logout", Some(&t)))
                .json(&json!([cmd("Logout", json!({}))]))
                .send()
                .await;
        }
    }

    /// Commands in one request; signs in again once if the token was refused.
    pub async fn call(&self, cmds: &[Value]) -> Result<Vec<std::result::Result<Value, ApiError>>> {
        let first = cmds
            .first()
            .and_then(|c| c["cmd"].as_str())
            .unwrap_or("GetDevInfo");
        for attempt in 0..2 {
            let token = self.token().await?;
            let answer: Value = self
                .http
                .post(self.url(first, Some(&token)))
                .json(&cmds)
                .send()
                .await
                .map_err(|e| self.unreachable(&e))?
                .json()
                .await
                .context("not a Reolink answer")?;
            let out = results(&answer);
            if attempt == 0 && out.iter().any(|r| matches!(r, Err(e) if e.needs_login())) {
                *self.session.lock().await = None;
                continue;
            }
            return Ok(out);
        }
        unreachable!("the loop returns on its second try")
    }

    /// One command's value.
    pub async fn one(&self, c: Value) -> Result<Value> {
        match self.call(&[c]).await?.into_iter().next() {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(anyhow!("{e}")),
            None => bail!("no answer"),
        }
    }

    /// A still from a channel, as JPEG bytes.
    pub async fn snap(&self, channel: u32) -> Result<Vec<u8>> {
        let token = self.token().await?;
        let rs: u32 = rand_suffix();
        let res = self
            .http
            .get(format!(
                "{}/cgi-bin/api.cgi?cmd=Snap&channel={channel}&rs={rs}&token={token}",
                self.base
            ))
            .send()
            .await
            .map_err(|e| self.unreachable(&e))?;
        let bytes = res.bytes().await?.to_vec();
        if bytes.starts_with(&[0xff, 0xd8]) {
            Ok(bytes)
        } else {
            bail!("the camera sent no picture")
        }
    }
}

/// Reolink caches a snapshot by its URL; a new `rs` each time gets a new one.
fn rand_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}

/// A refused sign-in: wrong password, or locked for a while after too many.
#[derive(Debug)]
pub struct LoginRefused(pub ApiError);

impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sign-in refused: {}", self.0)
    }
}

impl std::error::Error for LoginRefused {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_token_or_says_why_not() {
        let ok = json!([{ "cmd": "Login", "code": 0, "value": { "Token": { "leaseTime": 3600, "name": "abc123" } } }]);
        assert_eq!(parse_login(&ok).unwrap(), ("abc123".into(), 3600));
        let refused = json!([{ "cmd": "Login", "code": 1, "error": { "detail": "login failed", "rspCode": -7 } }]);
        let e = parse_login(&refused).unwrap_err();
        assert_eq!(e.rsp_code, -7);
        assert!(e.needs_login());
    }

    #[test]
    fn tells_a_hub_from_a_camera() {
        let nvr = json!({ "DevInfo": { "model": "RLN8-410", "channelNum": 8, "type": "NVR", "firmVer": "v3" } });
        assert!(parse_dev_info(&nvr).hub);
        let cam = json!({ "DevInfo": { "model": "RLC-510A", "channelNum": 1, "type": "IPC" } });
        let d = parse_dev_info(&cam);
        assert!(!d.hub);
        assert_eq!(d.model, "RLC-510A");
        // A Duo may report two channels: still a camera, not a hub.
        let duo =
            json!({ "DevInfo": { "model": "Reolink Duo 3 PoE", "channelNum": 2, "type": "IPC" } });
        let d = parse_dev_info(&duo);
        assert!(!d.hub);
        assert_eq!(d.channels, 2);
        let hub = json!({ "DevInfo": { "model": "Reolink Home Hub", "channelNum": 8, "type": "HOMEHUB" } });
        assert!(parse_dev_info(&hub).hub);
    }

    #[test]
    fn reads_an_nvrs_channels() {
        let v = json!({ "count": 2, "status": [
            { "channel": 0, "name": "Front door", "online": 1, "typeInfo": "Reolink Video Doorbell" },
            { "channel": 1, "name": "Driveway", "online": 0, "typeInfo": "RLC-823A" }
        ]});
        let c = parse_channels(&v);
        assert_eq!(c.len(), 2);
        assert!(c[0].online && !c[1].online);
        assert_eq!(c[1].name, "Driveway");
        assert!(!c[1].empty());
    }

    #[test]
    fn skips_an_nvrs_empty_slots() {
        // As an RLN36 answers: 36 slots, 4 with cameras.
        let v = json!({ "status": [
            { "channel": 0, "name": "Driveway", "online": 1, "sleep": 0, "uid": "95270008A6281Y33" },
            { "channel": 4, "name": "", "online": 0, "sleep": 0, "uid": "" },
            { "channel": 5, "name": "Shed", "online": 0, "sleep": 0, "uid": "" }
        ]});
        let c = parse_channels(&v);
        assert!(!c[0].empty() && c[1].empty());
        // A camera that's off still has its name: kept, as offline.
        assert!(!c[2].empty());
        assert_eq!(
            parse_type_info(&json!({ "typeInfo": "E1 Outdoor SE PoE", "firmVer": "v3.1" })),
            "E1 Outdoor SE PoE"
        );
    }

    #[test]
    fn reads_what_a_camera_sees_from_get_events() {
        let v = json!({ "channel": 0,
            "md": { "alarm_state": 1, "support": 1 },
            "ai": { "people": { "alarm_state": 1, "support": 1 }, "vehicle": { "alarm_state": 0, "support": 1 },
                    "dog_cat": { "alarm_state": 0, "support": 0 }, "package": { "alarm_state": 0, "support": 0 } },
            "visitor": { "alarm_state": 0, "support": 1 } });
        let (d, f) = parse_events(&v);
        assert!(d.motion && d.person && !d.vehicle && !d.visitor);
        assert!(f.person && f.vehicle && !f.animal && f.doorbell);
    }

    #[test]
    fn reads_older_firmware_too() {
        let (d, f) = parse_md_ai(
            &json!({ "state": 1 }),
            &json!({ "people": { "alarm_state": 0, "support": 1 }, "dog_cat": { "alarm_state": 1, "support": 1 } }),
        );
        assert!(d.motion && d.animal && !d.person);
        assert!(f.person && f.animal && !f.vehicle);
    }

    #[test]
    fn a_new_detection_is_worth_a_snapshot_and_a_lasting_one_isnt() {
        let before = Detections {
            motion: true,
            ..Default::default()
        };
        let now = Detections {
            motion: true,
            person: true,
            ..Default::default()
        };
        assert!(now.began(&before));
        assert!(!now.began(&now));
        assert!(!Detections::default().began(&before));
    }

    #[test]
    fn reads_abilities_by_any_of_their_spellings() {
        let chn = json!({ "floodLight": { "ver": 1 }, "ptzCtrl": { "ver": 0 }, "supportAudioAlarm": { "ver": 1 }, "alarmIrLights": { "ver": 1 } });
        let f = parse_abilities(&chn);
        assert!(f.spotlight && f.siren && f.ir && !f.ptz);
        let all =
            json!({ "Ability": { "abilityChn": [chn.clone(), { "ptzType": { "ver": 2 } }] } });
        assert!(parse_abilities(&ability_for(&all, 1)).ptz);
        assert_eq!(ability_for(&all, 5), Value::Null);
    }

    #[test]
    fn offers_only_what_the_account_may_use() {
        // As a real NVR answers for a user who may only watch: the floodlight
        // readable, the siren not at all, PTZ fully.
        let chn = json!({
            "floodLight": { "permit": 4, "ver": 2 },
            "alarmAudio": { "permit": 0, "ver": 1 },
            "ptzCtrl": { "permit": 7, "ver": 174 },
            "ledControl": { "permit": 4, "ver": 1 }
        });
        let f = parse_abilities(&chn);
        assert!(f.ptz && !f.spotlight && !f.siren && !f.ir);
        assert_eq!(f.denied, vec!["the spotlight", "the siren", "infrared"]);
        // An admin has them all.
        let admin = json!({ "floodLight": { "permit": 6, "ver": 2 }, "alarmAudio": { "permit": 6, "ver": 1 } });
        let f = parse_abilities(&admin);
        assert!(f.spotlight && f.siren && f.denied.is_empty());
    }

    #[test]
    fn reads_presets_and_the_spotlight() {
        let p = json!({ "PtzPreset": [ { "enable": 1, "id": 1, "name": "Gate" }, { "enable": 0, "id": 2, "name": "" }, { "enable": 1, "id": 3, "name": "" } ] });
        assert_eq!(
            parse_presets(&p),
            vec![(1, "Gate".into()), (3, "Preset 3".into())]
        );
        assert_eq!(
            parse_white_led(&json!({ "WhiteLed": { "state": 1, "bright": 80 } })),
            (true, 80)
        );
    }

    #[test]
    fn spells_commands_as_reolink_does() {
        assert_eq!(
            set_spotlight(2, true, Some(0))["param"]["WhiteLed"]["bright"],
            1
        );
        assert_eq!(siren_times(0, 0)["param"]["times"], 1);
        assert_eq!(siren_times(0, 3)["param"]["alarm_mode"], "times");
        // Reolink spells the mode "manul".
        let on = siren_manual(1, true);
        assert_eq!(on["cmd"], "AudioAlarmPlay");
        assert_eq!(on["param"]["alarm_mode"], "manul");
        assert_eq!(on["param"]["manual_switch"], 1);
        assert_eq!(on["param"]["channel"], 1);
        assert_eq!(siren_manual(1, false)["param"]["manual_switch"], 0);
        assert_eq!(ptz(1, "ToPos", 32, Some(4))["param"]["id"], 4);
        assert_eq!(set_ir(0, false)["param"]["IrLights"]["state"], "Off");
        assert_eq!(ptz_op("zoom_in"), Some("ZoomInc"));
        assert_eq!(ptz_op("spin"), None);
    }

    #[test]
    fn says_which_answers_failed() {
        let r = results(&json!([
            { "cmd": "GetEvents", "code": 0, "value": { "md": {} } },
            { "cmd": "GetEvents", "code": 1, "error": { "detail": "please login first", "rspCode": -6 } }
        ]));
        assert!(r[0].is_ok());
        assert!(r[1].as_ref().unwrap_err().needs_login());
    }
}
