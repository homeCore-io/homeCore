//! The Roku → homeCore state projection.
//!
//! homeCore's `media_player` device type defines `state`, `source`,
//! `media_title`, `media_position` and friends; rules and the web UI are
//! written against those names. ECP reports something quite different —
//! three separate queries, all stringly typed, none of which uses those
//! words. This module is the whole of that translation, kept apart from
//! the polling loop so it can be tested against captured ECP payloads.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::ecp::{ActiveApp, App, DeviceInfo, MediaPlayer, TvChannel};

/// Everything one poll cycle learned. Fields are `Option` because a
/// cycle legitimately skips queries: `media-player` is pointless while
/// the device sits on the home screen, and the TV queries 404 on a stick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RokuSnapshot {
    pub device_info: Option<DeviceInfo>,
    pub active: Option<ActiveApp>,
    pub player: Option<MediaPlayer>,
    pub tv_channel: Option<TvChannel>,
    /// Full installed-app catalogue. Refreshed on its own slow cadence,
    /// so it is carried forward between cycles rather than re-read.
    pub apps: Vec<App>,
    pub tv_channels: Vec<TvChannel>,
}

/// `media_player.state` — the vocabulary homeCore's device-type catalogue
/// defines: `playing | paused | stopped | idle | buffering | unavailable`.
///
/// Roku's `<player state>` covers only what a *video app* is doing, so
/// the home screen, a screensaver, and a tuned-but-not-streaming TV all
/// arrive here as "nothing playing" and have to be told apart from the
/// app context rather than from the player.
pub fn playback_state(snap: &RokuSnapshot) -> &'static str {
    let powered = snap
        .device_info
        .as_ref()
        .map(DeviceInfo::is_powered_on)
        .unwrap_or(false);
    if !powered {
        // Standby is not "unavailable" — ECP still answers, and a rule
        // that waits for `unavailable` would never see the device come
        // back. `stopped` is the honest reading: on, reachable, silent.
        return "stopped";
    }

    match snap.player.as_ref().map(|p| p.state.as_str()) {
        Some("play") => return "playing",
        Some("pause") => return "paused",
        Some("startup") | Some("buffer") => return "buffering",
        _ => {}
    }

    // No player activity, which on a Roku TV means very little: neither
    // the tuner nor an HDMI input goes through `query/media-player` at
    // all. Both are live video the device cannot introspect, so both
    // read as `playing` — the alternative is reporting a TV showing a
    // games console as `stopped`.
    if snap.tv_channel.is_some() {
        return "playing";
    }
    if snap
        .active
        .as_ref()
        .and_then(|a| a.app.as_ref())
        .is_some_and(|app| app.is_input())
    {
        return "playing";
    }

    match snap.active.as_ref() {
        Some(a) if a.is_home() => "idle",
        // An app is open but not playing — a menu, a paused-out browse
        // screen. Not idle (something is on screen), not playing.
        Some(_) => "stopped",
        None => "idle",
    }
}

/// Build the full retained state document published to
/// `homecore/devices/{id}/state`.
pub fn to_json(snap: &RokuSnapshot) -> Value {
    let mut out = Map::new();

    out.insert("state".into(), json!(playback_state(snap)));

    // ── Power ────────────────────────────────────────────────────────
    if let Some(info) = &snap.device_info {
        out.insert("on".into(), json!(info.is_powered_on()));
        out.insert("power_mode".into(), json!(info.power_mode()));
        insert_device_info(&mut out, info);
    }

    // ── Active app / source ──────────────────────────────────────────
    //
    // Filtered through `is_home` rather than reading `app` directly:
    // modern firmware puts a real app entry there for the home screen,
    // which would otherwise be published as the active source.
    let active_app = snap
        .active
        .as_ref()
        .filter(|a| !a.is_home())
        .and_then(|a| a.app.as_ref());
    match active_app {
        Some(app) => {
            out.insert("source".into(), json!(app.name));
            out.insert("app_id".into(), json!(app.id));
            out.insert("app_name".into(), json!(app.name));
            if let Some(t) = &app.app_type {
                out.insert("app_type".into(), json!(t));
            }
            if let Some(v) = &app.version {
                out.insert("app_version".into(), json!(v));
            }
            out.insert("is_tv_input".into(), json!(app.is_input()));
        }
        None => {
            // Home screen. `source` stays populated rather than going
            // null so a dashboard tile never blanks out — "Home" is
            // what the device is actually showing.
            out.insert("source".into(), json!("Home"));
            out.insert("app_id".into(), Value::Null);
            out.insert("app_name".into(), Value::Null);
            out.insert("is_tv_input".into(), json!(false));
        }
    }
    let screensaver = snap.active.as_ref().and_then(|a| a.screensaver.as_ref());
    out.insert("screensaver_active".into(), json!(screensaver.is_some()));
    if let Some(ss) = screensaver {
        out.insert("screensaver_name".into(), json!(ss.name));
    }

    // ── Playback ─────────────────────────────────────────────────────
    if let Some(p) = &snap.player {
        out.insert("player_state".into(), json!(p.state));
        out.insert("media_error".into(), json!(p.error));
        out.insert("media_is_live".into(), json!(p.is_live));
        // homeCore's media_player type declares media_position /
        // media_duration in **seconds**; ECP reports milliseconds. Both
        // are published — the canonical seconds for rules and the raw
        // milliseconds for anything drawing a progress bar.
        if let Some(ms) = p.position_ms {
            out.insert("media_position".into(), json!(ms / 1000));
            out.insert("media_position_ms".into(), json!(ms));
        }
        if let Some(ms) = p.duration_ms {
            out.insert("media_duration".into(), json!(ms / 1000));
            out.insert("media_duration_ms".into(), json!(ms));
        }
        if !p.format.is_empty() {
            out.insert("media_format".into(), map_to_json(&p.format));
        }
    }

    // ── Live TV ──────────────────────────────────────────────────────
    if let Some(chan) = &snap.tv_channel {
        if let Some(n) = chan.number() {
            out.insert("tv_channel".into(), json!(n));
        }
        if let Some(n) = chan.name() {
            out.insert("tv_channel_name".into(), json!(n));
        }
        // A tuned channel's programme is the closest thing a Roku has to
        // a media title, so it fills the standard attribute as well as
        // its own — otherwise `media_title` would be null on the one
        // source that actually knows what it is showing.
        if let Some(title) = chan.get("program-title") {
            out.insert("media_title".into(), json!(title));
        }
        if let Some(desc) = chan.get("program-description") {
            out.insert("media_description".into(), json!(desc));
        }
        out.insert("tv_channel_info".into(), chan.to_json());
    }

    // ── Catalogues ───────────────────────────────────────────────────
    if !snap.apps.is_empty() {
        let (inputs, channels): (Vec<&App>, Vec<&App>) =
            snap.apps.iter().partition(|a| a.is_input());
        out.insert(
            "available_apps".into(),
            Value::Array(channels.iter().map(|a| a.to_json()).collect()),
        );
        out.insert(
            "available_inputs".into(),
            Value::Array(inputs.iter().map(|a| a.to_json()).collect()),
        );
        // `available_sources` mirrors the `source` attribute's value
        // space so a UI can render a picker straight from state without
        // knowing that Roku distinguishes apps from inputs.
        out.insert(
            "available_sources".into(),
            Value::Array(snap.apps.iter().map(|a| json!(a.name)).collect()),
        );
    }
    if !snap.tv_channels.is_empty() {
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

    Value::Object(out)
}

/// Promote the `device-info` fields worth having as first-class
/// attributes, and keep the rest in a nested object.
///
/// The split is deliberate: rules and dashboards want `model_name` and
/// `is_tv` addressable, but publishing all ~50 firmware fields flat would
/// bury the media attributes and churn the retained document every time
/// Roku adds one.
fn insert_device_info(out: &mut Map<String, Value>, info: &DeviceInfo) {
    const PROMOTED: &[(&str, &str)] = &[
        ("model-name", "model_name"),
        ("model-number", "model_number"),
        ("serial-number", "serial_number"),
        ("software-version", "software_version"),
        ("network-type", "network_type"),
    ];
    for (ecp_key, attr) in PROMOTED {
        if let Some(v) = info.get(ecp_key) {
            if !v.is_empty() {
                out.insert((*attr).into(), json!(v));
            }
        }
    }
    if let Some(name) = info.display_name() {
        out.insert("friendly_name".into(), json!(name));
    }

    const PROMOTED_FLAGS: &[(&str, &str)] = &[
        ("is-tv", "is_tv"),
        ("is-stick", "is_stick"),
        ("headphones-connected", "headphones_connected"),
        ("supports-find-remote", "supports_find_remote"),
        ("supports-private-listening", "supports_private_listening"),
        ("supports-wake-on-wlan", "supports_wake_on_lan"),
        ("supports-tv-power-control", "supports_tv_power_control"),
        (
            "supports-audio-volume-control",
            "supports_audio_volume_control",
        ),
    ];
    for (ecp_key, attr) in PROMOTED_FLAGS {
        // Absent capability flags are reported as false rather than
        // omitted: a rule asking "does this device do volume?" needs an
        // answer on a 2015 Roku that predates the field.
        out.insert((*attr).into(), json!(info.flag(ecp_key)));
    }

    // Not a "supports-" flag, but the same shape of question and the one
    // that actually determines whether commands work at all.
    out.insert(
        "ecp_control_enabled".into(),
        json!(info.ecp_control_enabled()),
    );

    out.insert("device_info".into(), map_to_json(&info.fields));
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
mod tests {
    use super::*;
    use crate::ecp;

    fn info(power: &str, is_tv: bool) -> DeviceInfo {
        let xml = format!(
            r#"<device-info>
                <serial-number>1GU48T017973</serial-number>
                <model-name>Roku 3</model-name>
                <model-number>4200X</model-number>
                <user-device-name>Living Room</user-device-name>
                <software-version>12.0.0</software-version>
                <network-type>ethernet</network-type>
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
        let j = to_json(&snap);
        assert_eq!(j["state"], "playing");
        assert_eq!(j["source"], "Netflix");
        // Seconds for the canonical attribute, milliseconds alongside.
        assert_eq!(j["media_position"], 5);
        assert_eq!(j["media_position_ms"], 5000);
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
        let j = to_json(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["source"], "Home");
        assert_eq!(j["app_id"], Value::Null);
    }

    /// An app sitting on its own menu is neither idle nor playing.
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

    /// Standby still answers ECP, so it must not read as `unavailable` —
    /// that value is reserved for a device we cannot reach at all.
    #[test]
    fn standby_is_stopped_not_unavailable() {
        let snap = RokuSnapshot {
            device_info: Some(info("Ready", true)),
            ..Default::default()
        };
        let j = to_json(&snap);
        assert_eq!(j["state"], "stopped");
        assert_eq!(j["on"], false);
        assert_eq!(j["power_mode"], "Ready");
    }

    /// The tuner bypasses `query/media-player` entirely — without this
    /// branch a TV showing live broadcast would report `stopped`.
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
                r#"<active-app><app id="tvinput.dtv" type="tvin">Antenna TV</app></active-app>"#,
            )),
            tv_channel: Some(chan),
            ..Default::default()
        };
        let j = to_json(&snap);
        assert_eq!(j["state"], "playing");
        assert_eq!(j["media_title"], "Airwolf");
        assert_eq!(j["tv_channel"], "14.3");
        assert_eq!(j["tv_channel_name"], "getTV");
        assert_eq!(j["is_tv_input"], true);
    }

    /// Observed on a real Roku TV: active app is `tvinput.hdmi1`, the
    /// media player reports `state="none"`, and the tuner query comes
    /// back empty. Reporting that as `stopped` would mean a TV showing a
    /// console or a Blu-ray reads as idle-ish to every rule.
    #[test]
    fn hdmi_input_reads_as_playing() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            active: Some(active(
                r#"<active-app><app id="tvinput.hdmi1" type="tvin" version="1.0.0">HDMI 1</app></active-app>"#,
            )),
            player: Some(ecp::parse_media_player(r#"<player state="none"/>"#).unwrap()),
            ..Default::default()
        };
        let j = to_json(&snap);
        assert_eq!(j["state"], "playing");
        assert_eq!(j["is_tv_input"], true);
        assert_eq!(j["source"], "HDMI 1");
        // Nothing was tuned, so no stale channel info rides along.
        assert!(j.get("tv_channel_info").is_none());
    }

    /// The Roku OS 15 home screen, which arrives as a typed app rather
    /// than as an absent one.
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
        let j = to_json(&snap);
        assert_eq!(j["state"], "idle");
        assert_eq!(j["source"], "Home");
        assert_eq!(j["app_id"], Value::Null);
        assert_eq!(j["is_tv_input"], false);
    }

    #[test]
    fn app_catalogue_splits_inputs_from_channels() {
        let apps = ecp::parse_apps(
            r#"<apps>
                <app id="tvinput.hdmi1" type="tvin" version="1.0.0">Blu-ray</app>
                <app id="12" type="appl" version="4.1">Netflix</app>
            </apps>"#,
        )
        .unwrap();
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            apps,
            ..Default::default()
        };
        let j = to_json(&snap);
        assert_eq!(j["available_apps"].as_array().unwrap().len(), 1);
        assert_eq!(j["available_inputs"].as_array().unwrap().len(), 1);
        assert_eq!(j["available_apps"][0]["name"], "Netflix");
        assert_eq!(j["available_inputs"][0]["id"], "tvinput.hdmi1");
        assert_eq!(j["available_sources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn hidden_tv_channels_are_left_out_of_the_picker() {
        let chans = ecp::parse_tv_channels(
            r#"<tv-channels>
                <channel><number>1.1</number><name>Shown</name><user-hidden>false</user-hidden></channel>
                <channel><number>1.2</number><name>Hidden</name><user-hidden>true</user-hidden></channel>
            </tv-channels>"#,
        )
        .unwrap();
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", true)),
            tv_channels: chans,
            ..Default::default()
        };
        let list = to_json(&snap)["available_tv_channels"].clone();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "Shown");
    }

    /// Capability flags must be present even on firmware that predates
    /// them, so a rule can ask and get `false` instead of nothing.
    #[test]
    fn capability_flags_are_always_published() {
        let snap = RokuSnapshot {
            device_info: Some(info("PowerOn", false)),
            ..Default::default()
        };
        let j = to_json(&snap);
        assert_eq!(j["supports_audio_volume_control"], false);
        // No `ecp-setting-mode` in the fixture — nothing to restrict.
        assert_eq!(j["ecp_control_enabled"], true);
        assert_eq!(j["supports_find_remote"], true);
        assert_eq!(j["is_tv"], false);
        assert_eq!(j["model_name"], "Roku 3");
        assert_eq!(j["friendly_name"], "Living Room");
        // The untouched firmware dump rides along, snake-cased.
        assert_eq!(j["device_info"]["software_version"], "12.0.0");
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
        let j = to_json(&snap);
        assert_eq!(j["screensaver_active"], true);
        assert_eq!(j["screensaver_name"], "Nebula");
    }
}
