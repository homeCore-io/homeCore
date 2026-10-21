//! The Roku → homeCore state projection.
//!
//! homeCore's `media_player` device type defines `state`, `source`,
//! `media_title`, `media_position` and friends; rules and the web UI are
//! written against those names. ECP reports something quite different —
//! three separate queries, all stringly typed, none of which uses those
//! words. This module is the whole of that translation, kept apart from
//! the polling loop so it can be tested against captured ECP payloads.
//!
//! **Every key is always there.** A value that can't be read now is `null`,
//! not missing: a document that sometimes lacks `source` leaves the last one
//! standing in whatever merged it. The TV-only keys (`tv_channel`, …) are the
//! exception — a soundbar never has them, so they are never part of its
//! document at all.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::ecp::{plain_name, ActiveApp, App, DeviceInfo, MediaPlayer, TvChannel};

/// Everything one poll cycle learned. Fields are `Option` because a
/// cycle legitimately skips queries: `media-player` is pointless while
/// the device is in standby, and the TV queries 404 on a soundbar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RokuSnapshot {
    pub device_info: Option<DeviceInfo>,
    pub active: Option<ActiveApp>,
    pub player: Option<MediaPlayer>,
    pub tv_channel: Option<TvChannel>,
    /// Full installed-app catalogue, inputs included. Read on its own slow
    /// cadence and kept across restarts, so it is carried forward between
    /// cycles — and is there while the device is off or unreachable.
    pub apps: Vec<App>,
    pub tv_channels: Vec<TvChannel>,
}

/// What a device can do, from its `device-info` — kept across restarts so a
/// device that is off when the plugin starts still gets the right controls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    #[serde(default)]
    pub is_tv: bool,
    #[serde(default)]
    pub is_stick: bool,
    #[serde(default)]
    pub find_remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_listening: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_on_wlan: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_power_control: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_volume_control: Option<bool>,
}

impl Caps {
    pub fn from_info(info: &DeviceInfo) -> Self {
        Self {
            is_tv: info.is_tv(),
            is_stick: info.flag("is-stick"),
            find_remote: info.flag("supports-find-remote"),
            private_listening: info.flag_opt("supports-private-listening"),
            wake_on_wlan: info.flag_opt("supports-wake-on-wlan"),
            tv_power_control: info.flag_opt("supports-tv-power-control"),
            audio_volume_control: info.flag_opt("supports-audio-volume-control"),
        }
    }
}

/// What the bridge adds to a snapshot: what it tracks itself, and what it
/// remembers.
pub struct Extras<'a> {
    pub caps: &'a Caps,
    /// Tracked from the VolumeMute presses sent; ECP cannot report it.
    pub muted: bool,
    pub last_error: Option<&'a str>,
    pub serial: Option<&'a str>,
    /// The cached icon of a channel, as a path in the plugin's files.
    pub icon: &'a dyn Fn(&App) -> Option<String>,
    /// How often each channel was opened from homeCore.
    pub launches: &'a BTreeMap<String, u32>,
}

/// `media_player.state` — the vocabulary homeCore's device-type catalogue
/// defines: `playing | paused | stopped | idle | buffering`.
///
/// Roku's `<player state>` covers only what a *video app* is doing, so
/// the home screen, a screensaver, and a TV on an input all arrive here as
/// "nothing playing" and have to be told apart from the app context.
pub fn playback_state(snap: &RokuSnapshot) -> &'static str {
    let powered = snap
        .device_info
        .as_ref()
        .map(DeviceInfo::is_powered_on)
        .unwrap_or(false);
    if !powered {
        // Off: nothing is showing. `on: false` says it is off.
        return "idle";
    }

    match snap.player.as_ref().map(|p| p.state.as_str()) {
        Some("play") => return "playing",
        Some("pause") => return "paused",
        Some("startup") | Some("buffer") => return "buffering",
        _ => {}
    }

    // The tuner is a programme the TV itself is showing — it knows the
    // channel and the title — so it reads as playing.
    if snap.tv_channel.is_some() {
        return "playing";
    }
    // An HDMI or AV input is whatever the plugged-in device is doing, which
    // the Roku cannot see: on, showing that input, nothing more. A games
    // console on HDMI 1 is not "playing" any more than it is "stopped".
    if active_app(snap).is_some_and(App::is_input) {
        return "idle";
    }

    match snap.active.as_ref() {
        Some(a) if a.is_home() => "idle",
        // An app is open but not playing — a menu, a browse screen.
        Some(_) => "stopped",
        None => "idle",
    }
}

/// The app on screen, never the home screen (which modern firmware reports
/// as an app of its own).
fn active_app(snap: &RokuSnapshot) -> Option<&App> {
    snap.active
        .as_ref()
        .filter(|a| !a.is_home())
        .and_then(|a| a.app.as_ref())
}

/// The keys only a TV's document carries.
#[cfg(test)]
pub const TV_KEYS: &[&str] = &[
    "tv_channel",
    "tv_channel_name",
    "tv_channel_info",
    "available_tv_channels",
];

/// Build the full retained state document published to
/// `homecore/devices/{id}/state`. With no `device_info` (the device is not
/// answering) everything it is doing is null; what it *is* — its catalogue,
/// its capabilities — stays.
pub fn to_json(snap: &RokuSnapshot, x: &Extras<'_>) -> Value {
    let mut out = Map::new();
    let info = snap.device_info.as_ref();
    let powered = info.map(DeviceInfo::is_powered_on);
    let awake = powered == Some(true);

    out.insert(
        "state".into(),
        match info {
            Some(_) => json!(playback_state(snap)),
            None => Value::Null,
        },
    );
    out.insert("on".into(), json!(powered));
    out.insert("power_mode".into(), json!(info.map(DeviceInfo::power_mode)));
    out.insert("muted".into(), json!(x.muted));

    // ── Active app / source ──────────────────────────────────────────
    //
    // Off, nothing is on screen: `source` is null, not the "Home" the
    // standby device still reports. On the home screen it is "Home" — what
    // the device is showing.
    let app = if awake { active_app(snap) } else { None };
    let source = match (awake, app) {
        (true, Some(a)) => json!(a.plain_name()),
        (true, None) => json!("Home"),
        (false, _) => Value::Null,
    };
    out.insert("source".into(), source);
    out.insert("app_id".into(), json!(app.map(|a| a.id.clone())));
    out.insert("app_name".into(), json!(app.map(App::plain_name)));
    out.insert(
        "app_type".into(),
        json!(app.and_then(|a| a.app_type.clone())),
    );
    out.insert(
        "app_version".into(),
        json!(app.and_then(|a| a.version.clone())),
    );
    out.insert(
        "is_tv_input".into(),
        match info {
            Some(_) => json!(app.is_some_and(App::is_input)),
            None => Value::Null,
        },
    );
    let screensaver = if awake {
        snap.active.as_ref().and_then(|a| a.screensaver.as_ref())
    } else {
        None
    };
    out.insert(
        "screensaver_active".into(),
        match info {
            Some(_) => json!(screensaver.is_some()),
            None => Value::Null,
        },
    );
    out.insert(
        "screensaver_name".into(),
        json!(screensaver.map(App::plain_name)),
    );

    // ── Playback ─────────────────────────────────────────────────────
    let player = if awake { snap.player.as_ref() } else { None };
    // Only a player that belongs to what is on screen: an input's "none"
    // and a closed player say nothing about position.
    let playing_media = player.filter(|p| matches!(p.state.as_str(), "play" | "pause" | "buffer"));
    out.insert(
        "player_state".into(),
        json!(player.map(|p| p.state.clone())),
    );
    out.insert("media_error".into(), json!(player.map(|p| p.error)));
    out.insert(
        "media_is_live".into(),
        json!(playing_media.map(|p| p.is_live)),
    );
    // homeCore's media_player type declares media_position / media_duration
    // in **seconds**; ECP reports milliseconds. The bridge anchors the
    // position (`media_position_updated_at`) so it is republished only when
    // it jumps.
    out.insert(
        "media_position".into(),
        json!(playing_media
            .and_then(|p| p.position_ms)
            .map(|ms| ms / 1000)),
    );
    out.insert("media_position_updated_at".into(), Value::Null);
    out.insert(
        "media_duration".into(),
        json!(playing_media
            .and_then(|p| p.duration_ms)
            .filter(|&ms| ms > 0)
            .map(|ms| ms / 1000)),
    );
    out.insert(
        "media_format".into(),
        match playing_media.filter(|p| !p.format.is_empty()) {
            Some(p) => map_to_json(&p.format),
            None => Value::Null,
        },
    );

    // ── Live TV ──────────────────────────────────────────────────────
    let chan = if awake {
        snap.tv_channel.as_ref()
    } else {
        None
    };
    // A tuned channel's programme is the closest thing a Roku has to a media
    // title, so it fills the standard attribute.
    out.insert(
        "media_title".into(),
        json!(chan.and_then(|c| c.get("program-title")).map(plain_name)),
    );
    out.insert(
        "media_description".into(),
        json!(chan.and_then(|c| c.get("program-description"))),
    );
    if x.caps.is_tv {
        out.insert("tv_channel".into(), json!(chan.and_then(TvChannel::number)));
        out.insert(
            "tv_channel_name".into(),
            json!(chan.and_then(TvChannel::name)),
        );
        out.insert(
            "tv_channel_info".into(),
            chan.map(TvChannel::to_json).unwrap_or(Value::Null),
        );
        out.insert(
            "available_tv_channels".into(),
            Value::Array(
                snap.tv_channels
                    .iter()
                    .filter(|c| !c.hidden())
                    .map(|c| {
                        json!({
                            "number": c.number().unwrap_or_default(),
                            "name":   c.name().unwrap_or_default(),
                            "type":   c.get("type").unwrap_or_default(),
                        })
                    })
                    .collect(),
            ),
        );
    }

    // ── Catalogues ───────────────────────────────────────────────────
    //
    // In the Roku's own order (its home-screen arrangement), each with its
    // cached icon and how often it was opened from homeCore — a screen sorts
    // "most used" from those.
    let (inputs, channels): (Vec<&App>, Vec<&App>) = snap.apps.iter().partition(|a| a.is_input());
    let entry = |a: &App| {
        let mut v = a.to_json();
        v["icon"] = json!((x.icon)(a));
        if !a.is_input() {
            v["launches"] = json!(x.launches.get(&a.id).copied().unwrap_or(0));
        }
        v
    };
    out.insert(
        "available_apps".into(),
        Value::Array(channels.iter().map(|a| entry(a)).collect()),
    );
    out.insert(
        "available_inputs".into(),
        Value::Array(inputs.iter().map(|a| entry(a)).collect()),
    );
    // `available_sources` mirrors the `source` attribute's value space so a
    // client can offer a picker without knowing apps from inputs.
    out.insert(
        "available_sources".into(),
        Value::Array(snap.apps.iter().map(|a| json!(a.plain_name())).collect()),
    );

    // ── What it is ───────────────────────────────────────────────────
    //
    // Model and firmware go with the device's registration, not here; the
    // full `device-info` (MACs, network name) is behind the `device_info`
    // action and never in state.
    out.insert("serial_number".into(), json!(x.serial));
    out.insert(
        "network_type".into(),
        json!(info
            .and_then(|i| i.get("network-type"))
            .filter(|s| !s.is_empty())),
    );
    out.insert("is_tv".into(), json!(x.caps.is_tv));
    out.insert("is_stick".into(), json!(x.caps.is_stick));
    out.insert("supports_find_remote".into(), json!(x.caps.find_remote));
    out.insert(
        "supports_private_listening".into(),
        json!(x.caps.private_listening),
    );
    out.insert("supports_wake_on_wlan".into(), json!(x.caps.wake_on_wlan));
    // Whether a Roku *player* can switch the TV it is plugged into on and
    // off (over HDMI-CEC). A Roku TV is the TV, so for one it means nothing.
    out.insert(
        "supports_tv_power_control".into(),
        json!(if x.caps.is_tv {
            None
        } else {
            x.caps.tv_power_control
        }),
    );
    out.insert(
        "supports_audio_volume_control".into(),
        json!(x.caps.audio_volume_control),
    );
    out.insert(
        "headphones_connected".into(),
        json!(info.and_then(|i| i.flag_opt("headphones-connected"))),
    );
    out.insert(
        "ecp_control_enabled".into(),
        json!(info.map(DeviceInfo::ecp_control_enabled)),
    );
    out.insert("last_command_error".into(), json!(x.last_error));

    Value::Object(out)
}

/// Kebab-case ECP keys → snake_case JSON, with `"true"`/`"false"` and
/// integers coerced to real JSON types.
fn map_to_json(fields: &BTreeMap<String, String>) -> Value {
    let mut m = Map::new();
    for (k, v) in fields {
        let value = match v.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => match other.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => Value::String(other.to_string()),
            },
        };
        m.insert(k.replace('-', "_"), value);
    }
    Value::Object(m)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::ecp;

    pub fn info(power: &str, is_tv: bool) -> DeviceInfo {
        let xml = format!(
            r#"<device-info>
                <serial-number>1GU48T017973</serial-number>
                <model-name>Roku 3</model-name>
                <model-number>4200X</model-number>
                <user-device-name>Living Room</user-device-name>
                <software-version>12.0.0</software-version>
                <network-type>ethernet</network-type>
                <wifi-mac>02:00:00:00:00:01</wifi-mac>
                <network-name>home-ssid</network-name>
                <uptime>962405</uptime>
                <is-tv>{is_tv}</is-tv>
                <power-mode>{power}</power-mode>
                <supports-find-remote>true</supports-find-remote>
            </device-info>"#
        );
        ecp::parse_device_info(&xml).unwrap()
    }

    fn active(xml: &str) -> ActiveApp {
        ecp::parse_active_app(xml).unwrap()
    }

    fn doc(snap: &RokuSnapshot) -> Value {
        let caps = snap
            .device_info
            .as_ref()
            .map(Caps::from_info)
            .unwrap_or_default();
        to_json(
            snap,
            &Extras {
                caps: &caps,
                muted: false,
                last_error: None,
                serial: Some("1GU48T017973"),
                icon: &|a: &App| Some(format!("icons/{}.png", a.id)),
                launches: &BTreeMap::from([("12".to_string(), 3)]),
            },
        )
    }

    #[test]
    fn playing_app_maps_to_playing() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(
                r#"<active-app><app id="12" type="appl">Netflix</app></active-app>"#,
            )),
            player: Some(ecp::parse_media_player(
                r#"<player error="false" state="play"><position>5000 ms</position><duration>60000 ms</duration></player>"#,
            ).unwrap()),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "playing");
        assert_eq!(j["source"], "Netflix");
        assert_eq!(j["media_position"], 5);
        assert_eq!(j["media_duration"], 60);
    }

    #[test]
    fn paused_and_buffering_map_through() {
        let mut snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(
                r#"<active-app><app id="12">Netflix</app></active-app>"#,
            )),
            ..Default::default()
        };
        snap.player = Some(ecp::parse_media_player(r#"<player state="pause"/>"#).unwrap());
        assert_eq!(playback_state(&snap), "paused");
        snap.player = Some(ecp::parse_media_player(r#"<player state="buffer"/>"#).unwrap());
        assert_eq!(playback_state(&snap), "buffering");
        snap.player = Some(ecp::parse_media_player(r#"<player state="startup"/>"#).unwrap());
        assert_eq!(playback_state(&snap), "buffering");
    }

    #[test]
    fn home_screen_is_idle_and_reports_home_as_the_source() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(r#"<active-app><app>Roku</app></active-app>"#)),
            player: Some(ecp::parse_media_player(r#"<player state="close"/>"#).unwrap()),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["source"], "Home");
        assert_eq!(j["app_id"], Value::Null);
    }

    #[test]
    fn open_app_with_no_playback_is_stopped() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(
                r#"<active-app><app id="12">Netflix</app></active-app>"#,
            )),
            player: Some(ecp::parse_media_player(r#"<player state="close"/>"#).unwrap()),
            ..Default::default()
        };
        assert_eq!(playback_state(&snap), "stopped");
    }

    /// M4: off has nothing on screen. A standby Roku still answers
    /// `active-app` with the last app — or "Home" — and neither is shown.
    #[test]
    fn standby_is_off_with_no_source() {
        let snap = RokuSnapshot {
            device_info: Some(info("Ready", true)),
            active: Some(active(
                r#"<active-app><app id="562859" type="home">Roku Dynamic Menu</app></active-app>"#,
            )),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["on"], false);
        assert_eq!(j["power_mode"], "Ready");
        assert_eq!(j["source"], Value::Null, "off is not \"Home\"");
        assert_eq!(j["app_name"], Value::Null);
    }

    /// M4: `DisplayOff` — the screen off, the sound on — is on.
    #[test]
    fn screen_off_is_on_and_keeps_playing() {
        let snap = RokuSnapshot {
            device_info: Some(info("DisplayOff", true)),
            active: Some(active(
                r#"<active-app><app id="22297" type="appl">Spotify Music</app></active-app>"#,
            )),
            player: Some(ecp::parse_media_player(r#"<player state="play"/>"#).unwrap()),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(
            (j["on"].as_bool(), j["state"].as_str()),
            (Some(true), Some("playing"))
        );
        assert_eq!(j["power_mode"], "DisplayOff");
        assert_eq!(j["source"], "Spotify Music");
    }

    /// The tuner bypasses `query/media-player` entirely — without this
    /// branch a TV showing live broadcast would report `idle`.
    #[test]
    fn tuned_tv_channel_reads_as_playing_and_fills_media_title() {
        let chan = ecp::parse_tv_channels(
            r#"<tv-channel><channel>
                <number>14.3</number><name>getTV</name>
                <program-title>Airwolf</program-title>
                <program-description>Helicopter.</program-description>
            </channel></tv-channel>"#,
        )
        .unwrap()
        .remove(0);
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            active: Some(active(
                r#"<active-app><app id="tvinput.dtv" type="tvin">Live&#160;TV</app></active-app>"#,
            )),
            tv_channel: Some(chan),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "playing");
        assert_eq!(j["media_title"], "Airwolf");
        assert_eq!(j["tv_channel"], "14.3");
        assert_eq!(j["tv_channel_name"], "getTV");
        assert_eq!(j["is_tv_input"], true);
        assert_eq!(
            j["source"], "Live TV",
            "its non-breaking space is a plain one"
        );
    }

    /// Observed on the house's Office TV: active app `tvinput.hdmi1`, its
    /// name `HDMI\u{a0}1`, the player `state="none"`. On, showing HDMI 1 —
    /// not "playing": what plays on it is up to the console or player.
    #[test]
    fn hdmi_input_is_on_and_idle_showing_the_input() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            active: Some(active(
                "<active-app><app id=\"tvinput.hdmi1\" type=\"tvin\" version=\"1.0.0\">HDMI\u{a0}1</app></active-app>",
            )),
            player: Some(ecp::parse_media_player(r#"<player error="false" state="none"/>"#).unwrap()),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["on"], true);
        assert_eq!(j["is_tv_input"], true);
        assert_eq!(j["source"], "HDMI 1");
        assert_eq!(j["tv_channel_info"], Value::Null);
        assert_eq!(j["media_position"], Value::Null);
    }

    #[test]
    fn dynamic_menu_home_screen_is_idle() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            active: Some(active(
                r#"<active-app><app id="562859" type="home" version="14.10.5" ui-location="home">Roku Dynamic Menu</app></active-app>"#,
            )),
            player: Some(ecp::parse_media_player(r#"<player state="none"/>"#).unwrap()),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["source"], "Home");
        assert_eq!(j["app_id"], Value::Null);
        assert_eq!(j["is_tv_input"], false);
    }

    #[test]
    fn app_catalogue_splits_inputs_from_channels_with_icons_and_counts() {
        let apps = ecp::parse_apps(
            "<apps>
                <app id=\"tvinput.hdmi1\" type=\"tvin\" version=\"1.0.0\">HDMI\u{a0}3\u{a0}(ARC)</app>
                <app id=\"12\" type=\"appl\" version=\"4.1\">Netflix</app>
            </apps>",
        )
        .unwrap();
        let snap = RokuSnapshot {
            device_info: Some(info("Ready", true)),
            apps,
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["available_apps"].as_array().unwrap().len(), 1);
        assert_eq!(j["available_inputs"][0]["name"], "HDMI 3 (ARC)");
        assert_eq!(j["available_apps"][0]["icon"], "icons/12.png");
        assert_eq!(j["available_apps"][0]["launches"], 3);
        assert_eq!(j["available_sources"], json!(["HDMI 3 (ARC)", "Netflix"]));
    }

    /// H3/M1: a device that isn't answering has no reading — every reading is
    /// null — but its catalogue stays, so a picker is never empty.
    #[test]
    fn unreachable_keeps_the_catalogue_and_clears_the_rest() {
        let snap = RokuSnapshot {
            apps: ecp::parse_apps(r#"<apps><app id="12" type="appl">Netflix</app></apps>"#)
                .unwrap(),
            ..Default::default()
        };
        let caps = Caps {
            is_tv: true,
            ..Default::default()
        };
        let j = to_json(
            &snap,
            &Extras {
                caps: &caps,
                muted: false,
                last_error: None,
                serial: Some("X1"),
                icon: &|_| None,
                launches: &BTreeMap::new(),
            },
        );
        for k in [
            "state",
            "on",
            "source",
            "app_name",
            "media_position",
            "tv_channel",
            "power_mode",
        ] {
            assert_eq!(j[k], Value::Null, "{k}");
        }
        assert_eq!(j["available_apps"][0]["name"], "Netflix");
        assert_eq!(j["is_tv"], true);
    }

    /// M5: the same keys every time, playing or not.
    #[test]
    fn every_key_is_present_whatever_the_device_is_doing() {
        let playing = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(r#"<active-app><app id="12" type="appl">Netflix</app><screensaver id="5" type="ssvr">Nebula</screensaver></active-app>"#)),
            player: Some(ecp::parse_media_player(r#"<player error="false" state="play"><format audio="aac"/><position>5000 ms</position><duration>60000 ms</duration></player>"#).unwrap()),
            ..Default::default()
        };
        let off = RokuSnapshot {
            device_info: Some(info("Ready", false)),
            ..Default::default()
        };
        let a: Vec<String> = doc(&playing).as_object().unwrap().keys().cloned().collect();
        let b: Vec<String> = doc(&off).as_object().unwrap().keys().cloned().collect();
        let c: Vec<String> = doc(&RokuSnapshot::default())
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// M9: no firmware dump, no MAC, no network name, no ticking uptime in
    /// state — `device_info` is an action. **A device doing nothing must
    /// produce no event**: `uptime` once made 111 of 200 events in the house.
    #[test]
    fn nothing_private_or_ticking_reaches_the_state_document() {
        let j = doc(&RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            ..Default::default()
        });
        let text = j.to_string();
        for leak in [
            "02:00:00:00:00:01",
            "home-ssid",
            "962405",
            "device_info",
            "model_name",
        ] {
            assert!(!text.contains(leak), "{leak} reached state: {text}");
        }
        assert_eq!(j["serial_number"], "1GU48T017973");
        assert_eq!(j["network_type"], "ethernet");
    }

    /// A flag an older Roku OS doesn't report is unknown, not false; a TV's
    /// "TV power control" means nothing (it is the TV).
    #[test]
    fn capability_flags_say_unknown_when_unreported() {
        let j = doc(&RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            ..Default::default()
        });
        assert_eq!(j["supports_audio_volume_control"], Value::Null);
        assert_eq!(j["supports_tv_power_control"], Value::Null);
        assert_eq!(j["supports_find_remote"], true);
        assert_eq!(j["ecp_control_enabled"], true);
        assert_eq!(j["is_tv"], true);
    }

    /// A soundbar has no tuner: its document has no TV keys at all.
    #[test]
    fn a_soundbar_has_no_tv_keys() {
        let j = doc(&RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            ..Default::default()
        });
        for k in TV_KEYS {
            assert!(j.get(*k).is_none(), "{k}");
        }
    }

    /// Everything the state document can carry is declared — an undeclared
    /// attribute is one a client has to guess at.
    #[test]
    fn every_published_attribute_is_declared() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            ..Default::default()
        };
        let j = doc(&snap);
        let caps = Caps::from_info(snap.device_info.as_ref().unwrap());
        let declared = crate::schema::device_schema(&caps).attributes;
        let undeclared: Vec<_> = j
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| !declared.contains_key(*k))
            .collect();
        assert!(
            undeclared.is_empty(),
            "published, undeclared: {undeclared:?}"
        );
    }

    #[test]
    fn screensaver_is_surfaced() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            active: Some(active(
                r#"<active-app><app>Roku</app><screensaver id="55545" type="ssvr">Nebula</screensaver></active-app>"#,
            )),
            ..Default::default()
        };
        let j = doc(&snap);
        assert_eq!(j["screensaver_active"], true);
        assert_eq!(j["screensaver_name"], "Nebula");
    }
}
