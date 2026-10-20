//! What a WLED's state (`/json/state`, or the `state` of a WebSocket message)
//! is published as.
//!
//! The first segment is also flattened into flat attributes — `color_rgb`,
//! `color_temp`, `effect_id`, `palette_id` and the rest — so a rule can read
//! the strip's look without digging. Every segment, with every setting it
//! has, is in `segments`; the controller-wide settings (nightlight, sync, live
//! data, sound) each have their own attribute.
//!
//! **Shared names in shared units** (`hc_types::device_vocabulary`), converted
//! here from WLED's: `brightness_pct` (0–100, from `bri` 0–255),
//! `transition_ms` (from tenths of a second), `color_temp` in kelvin (from
//! `cct` 0–255), `color_rgb` as `{r, g, b}`.
//!
//! **A fixed key set.** Everything here is published on every change as a
//! partial; a key that is sometimes left out would be stale when it is. A
//! value WLED doesn't report is published as null, never dropped.

use serde_json::{json, Map, Value};

/// A WLED colour, `[r,g,b]` or `[r,g,b,w]`, as `#rrggbb` (white left off:
/// it is published alongside when the strip has a white channel).
pub fn hex(c: &Value) -> Value {
    let a = match c.as_array() {
        Some(a) if a.len() >= 3 => a,
        _ => return Value::Null,
    };
    let b = |i: usize| a[i].as_u64().unwrap_or(0).min(255);
    json!(format!("#{:02x}{:02x}{:02x}", b(0), b(1), b(2)))
}

/// WLED's 0–255 level as a percentage, to one decimal. Any level above 0 is
/// at least 1 %: a strip that is lit never reads as off.
pub fn pct_of_byte(raw: f64) -> f64 {
    let raw = raw.clamp(0.0, 255.0);
    let p = (raw * 1000.0 / 255.0).round() / 10.0;
    if raw > 0.0 {
        p.max(1.0)
    } else {
        0.0
    }
}

/// A percentage as WLED's 0–255 level. Anything above 0 % is at least 1, so
/// a dim setting never turns the strip off.
pub fn byte_of_pct(pct: f64) -> u64 {
    let pct = pct.clamp(0.0, 100.0);
    let raw = (pct * 255.0 / 100.0).round() as u64;
    if pct > 0.0 {
        raw.max(1)
    } else {
        0
    }
}

/// WLED's lowest and highest colour temperature: it reads a `cct` of 1900 or
/// more as kelvin and maps 1900–10091 K onto its 0–255 scale, 32 K a step
/// (`Segment::setCCT`).
pub const KELVIN_MIN: u64 = 1900;
pub const KELVIN_MAX: u64 = 10091;

/// WLED's 0–255 `cct` (warm to cold) as kelvin.
pub fn kelvin_of_cct(cct: u64) -> u64 {
    KELVIN_MIN + cct.min(255) * 32
}

/// Kelvin as the `cct` WLED takes: kelvin itself, within what it reads as
/// kelvin (it converts).
pub fn cct_of_kelvin(k: f64) -> u64 {
    (k.round() as u64).clamp(KELVIN_MIN, KELVIN_MAX)
}

/// WLED's transition, in tenths of a second, as milliseconds.
pub fn ms_of_tenths(t: u64) -> u64 {
    t * 100
}

/// Milliseconds as WLED's tenths of a second (its `tt` and `transition`).
pub fn tenths_of_ms(ms: f64) -> u64 {
    ((ms.max(0.0) / 100.0).round() as u64).min(65535)
}

fn pct(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_f64) {
        Some(b) => json!(pct_of_byte(b)),
        None => Value::Null,
    }
}

fn kelvin(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_u64) {
        Some(c) => json!(kelvin_of_cct(c)),
        None => Value::Null,
    }
}

/// A WLED colour, `[r,g,b]` or `[r,g,b,w]`, as `{r, g, b}` (the white
/// channel is in `segments[].whites`).
pub fn rgb_obj(c: &Value) -> Value {
    match c.as_array() {
        Some(a) if a.len() >= 3 => {
            let b = |i: usize| a[i].as_u64().unwrap_or(0).min(255);
            json!({ "r": b(0), "g": b(1), "b": b(2) })
        }
        _ => Value::Null,
    }
}

/// One segment, in homeCore's words — the same words a command takes.
pub fn segment(s: &Value) -> Value {
    let g = |k: &str| s.get(k).cloned().unwrap_or(Value::Null);
    let cols: Vec<Value> = s
        .get("col")
        .and_then(Value::as_array)
        .map(|c| c.iter().map(hex).collect())
        .unwrap_or_default();
    let white: Vec<Value> = s
        .get("col")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .map(|x| x.get(3).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": g("id"),
        "name": g("n"),
        "start": g("start"),
        "stop": g("stop"),
        "grouping": g("grp"),
        "spacing": g("spc"),
        "offset": g("of"),
        "on": g("on"),
        "brightness_pct": pct(s.get("bri")),
        "color_temp": kelvin(s.get("cct")),
        "colors": cols,
        "whites": if white.iter().any(|w| !w.is_null()) { json!(white) } else { Value::Null },
        "effect_id": g("fx"),
        "effect_speed": g("sx"),
        "effect_intensity": g("ix"),
        "custom1": g("c1"),
        "custom2": g("c2"),
        "custom3": g("c3"),
        "option1": g("o1"),
        "option2": g("o2"),
        "option3": g("o3"),
        "palette_id": g("pal"),
        "selected": g("sel"),
        "reverse": g("rev"),
        "mirror": g("mi"),
        "freeze": g("frz"),
        "blend_mode": g("bm"),
        "sound_sim": g("si"),
        "mapping_1d2d": g("m12"),
    })
}

/// The attributes a state is published as. `info` (from a WebSocket message,
/// or `/json/info`) adds whether live data is streaming.
pub fn attributes(state: &Value, info: Option<&Value>) -> Value {
    let mut j = Map::new();
    let g = |k: &str| state.get(k).cloned().unwrap_or(Value::Null);
    j.insert("on".into(), g("on"));
    j.insert("brightness_pct".into(), pct(state.get("bri")));
    // WLED says -1 when no preset or playlist is running; homeCore says nothing.
    let positive = |k: &str| match state.get(k).and_then(Value::as_i64) {
        Some(n) if n >= 1 => json!(n),
        _ => Value::Null,
    };
    j.insert("preset_id".into(), positive("ps"));
    j.insert("playlist_id".into(), positive("pl"));
    // `transition` is in tenths of a second.
    j.insert(
        "transition_ms".into(),
        match state.get("transition").and_then(Value::as_u64) {
            Some(t) => json!(ms_of_tenths(t)),
            None => Value::Null,
        },
    );
    j.insert("main_segment".into(), g("mainseg"));

    let segs: Vec<Value> = state
        .get("seg")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|s| s.get("stop").and_then(Value::as_u64) != Some(0))
                .map(segment)
                .collect()
        })
        .unwrap_or_default();
    j.insert("segment_count".into(), json!(segs.len()));

    // The first segment, flat, as it always was published.
    let first = segs.first().cloned().unwrap_or(Value::Null);
    let f = |k: &str| first.get(k).cloned().unwrap_or(Value::Null);
    let raw_first = state
        .get("seg")
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    let raw_col = |i: usize| {
        raw_first
            .and_then(|s| s.get("col"))
            .and_then(Value::as_array)
            .and_then(|c| c.get(i))
            .cloned()
            .unwrap_or(Value::Null)
    };
    j.insert("color_rgb".into(), rgb_obj(&raw_col(0)));
    j.insert("color2".into(), rgb_obj(&raw_col(1)));
    j.insert("color3".into(), rgb_obj(&raw_col(2)));
    for k in [
        "effect_id",
        "effect_speed",
        "effect_intensity",
        "palette_id",
        "custom1",
        "custom2",
        "custom3",
        "option1",
        "option2",
        "option3",
        "color_temp",
    ] {
        j.insert(k.into(), f(k));
    }
    j.insert("segments".into(), Value::Array(segs));

    let nl = state.get("nl");
    let n = |k: &str| nl.and_then(|n| n.get(k)).cloned().unwrap_or(Value::Null);
    j.insert(
        "nightlight".into(),
        if nl.is_some() {
            json!({
                "on": n("on"),
                "duration_min": n("dur"),
                "mode": match n("mode").as_u64() {
                    Some(0) => json!("instant"),
                    Some(1) => json!("fade"),
                    Some(2) => json!("color"),
                    Some(3) => json!("sunrise"),
                    _ => Value::Null,
                },
                "target_pct": pct(nl.and_then(|n| n.get("tbri"))),
                // -1 while off.
                "remaining_s": match n("rem").as_i64() {
                    Some(r) if r >= 0 => json!(r),
                    _ => Value::Null,
                },
            })
        } else {
            Value::Null
        },
    );
    let u = state.get("udpn");
    let ug = |k: &str| u.and_then(|x| x.get(k)).cloned().unwrap_or(Value::Null);
    j.insert(
        "sync".into(),
        if u.is_some() {
            json!({
                "send": ug("send"),
                "receive": ug("recv"),
                "send_groups": ug("sgrp"),
                "receive_groups": ug("rgrp"),
            })
        } else {
            Value::Null
        },
    );
    j.insert("live_override".into(), g("lor"));
    j.insert(
        "audio_reactive".into(),
        state
            .get("AudioReactive")
            .and_then(|a| a.get("on"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    if let Some(info) = info {
        j.insert(
            "live".into(),
            info.get("live").cloned().unwrap_or(Value::Null),
        );
        j.insert(
            "live_source".into(),
            info.get("lm")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(j)
}

/// The state in one WebSocket message: WLED sends `{"state": …, "info": …}`
/// on connect and on every change; a bare state is accepted too. `None` for
/// a message that carries no state.
pub fn from_ws(text: &str) -> Option<(Value, Option<Value>)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let (state, info) = match v.get("state") {
        Some(s) if s.is_object() => (s.clone(), v.get("info").cloned()),
        _ => (v, None),
    };
    // A state has power and brightness; anything else isn't one.
    (state.get("on").is_some() && state.get("bri").is_some()).then_some((state, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn house() -> Value {
        // The house controller's state, 2026-10-18.
        json!({
            "on": true, "bri": 128, "transition": 7, "ps": 14, "pl": -1, "mainseg": 0, "lor": 0,
            "AudioReactive": {"on": false},
            "nl": {"on": false, "dur": 60, "mode": 1, "tbri": 0, "rem": -1},
            "udpn": {"send": false, "recv": true, "sgrp": 1, "rgrp": 1},
            "seg": [{
                "id": 0, "start": 0, "stop": 400, "grp": 1, "spc": 0, "of": 0, "on": true,
                "frz": false, "bri": 255, "cct": 127, "col": [[106, 13, 173], [255, 69, 0], [0, 255, 60]],
                "fx": 51, "sx": 128, "ix": 128, "pal": 0, "c1": 128, "c2": 128, "c3": 16,
                "sel": true, "rev": false, "mi": false, "o1": false, "o2": false, "o3": false,
                "si": 0, "m12": 0, "bm": 0
            }]
        })
    }

    #[test]
    fn the_house_controller_s_state_is_published_whole() {
        let a = attributes(&house(), Some(&json!({"live": false, "lm": ""})));
        assert_eq!(a["preset_id"], 14);
        assert!(a["playlist_id"].is_null());
        assert_eq!(a["transition_ms"], 700);
        assert_eq!(a["brightness_pct"], 50.2);
        assert!(a.get("brightness").is_none());
        assert_eq!(a["color_rgb"], json!({"r": 106, "g": 13, "b": 173}));
        assert_eq!(a["color2"], json!({"r": 255, "g": 69, "b": 0}));
        // cct 127 of 255: 1900 K + 127 × 32.
        assert_eq!(a["color_temp"], 5964);
        assert_eq!(a["effect_id"], 51);
        assert_eq!(a["custom3"], 16);
        assert_eq!(a["segment_count"], 1);
        let s = &a["segments"][0];
        assert_eq!(s["colors"], json!(["#6a0dad", "#ff4500", "#00ff3c"]));
        assert!(s["whites"].is_null());
        assert_eq!(s["stop"], 400);
        assert_eq!(s["brightness_pct"], 100.0);
        assert_eq!(s["color_temp"], 5964);
        assert!(s.get("brightness").is_none() && s.get("cct").is_none());
        assert_eq!(a["nightlight"]["mode"], "fade");
        assert!(a["nightlight"]["remaining_s"].is_null());
        assert_eq!(
            a["sync"],
            json!({"send": false, "receive": true, "send_groups": 1, "receive_groups": 1})
        );
        assert_eq!(a["audio_reactive"], false);
        assert_eq!(a["live"], false);
        assert!(a["live_source"].is_null());
    }

    #[test]
    fn a_deleted_segment_is_left_out() {
        let mut st = house();
        st["seg"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 1, "stop": 0}));
        assert_eq!(attributes(&st, None)["segment_count"], 1);
    }

    #[test]
    fn a_websocket_message_is_read_through_its_wrapper() {
        let msg = json!({"state": house(), "info": {"ver": "16.0.1", "live": true, "lm": "E1.31"}})
            .to_string();
        let (state, info) = from_ws(&msg).expect("wrapped state");
        assert_eq!(state["ps"], 14);
        assert_eq!(attributes(&state, info.as_ref())["live_source"], "E1.31");
        assert!(
            from_ws(&house().to_string()).is_some(),
            "a bare state still parses"
        );
        assert!(from_ws(r#"{"info":{"ver":"x"}}"#).is_none());
        assert!(from_ws("not json").is_none());
    }

    #[test]
    fn brightness_converts_both_ways() {
        assert_eq!(pct_of_byte(0.0), 0.0);
        // The lowest lit level is not 0 % (off).
        assert_eq!(pct_of_byte(1.0), 1.0);
        assert_eq!(pct_of_byte(2.0), 1.0);
        assert_eq!(pct_of_byte(3.0), 1.2);
        assert_eq!(pct_of_byte(128.0), 50.2);
        assert_eq!(pct_of_byte(255.0), 100.0);
        assert_eq!(byte_of_pct(0.0), 0);
        // A dim setting stays lit.
        assert_eq!(byte_of_pct(0.1), 1);
        assert_eq!(byte_of_pct(1.0), 3);
        assert_eq!(byte_of_pct(50.0), 128);
        assert_eq!(byte_of_pct(100.0), 255);
        assert_eq!(byte_of_pct(150.0), 255);
        assert_eq!(byte_of_pct(-5.0), 0);
        // Every level reads back as itself, but for the two lowest, which
        // read as 1 % and come back as 3.
        for raw in 0..=255u64 {
            let want = if raw == 0 { 0 } else { raw.max(3) };
            assert_eq!(byte_of_pct(pct_of_byte(raw as f64)), want, "{raw}");
        }
    }

    #[test]
    fn transitions_convert_both_ways() {
        assert_eq!(ms_of_tenths(0), 0);
        assert_eq!(ms_of_tenths(7), 700);
        assert_eq!(tenths_of_ms(0.0), 0);
        assert_eq!(tenths_of_ms(49.0), 0);
        assert_eq!(tenths_of_ms(50.0), 1);
        assert_eq!(tenths_of_ms(2000.0), 20);
        assert_eq!(tenths_of_ms(-100.0), 0);
        assert_eq!(tenths_of_ms(1e9), 65535);
    }

    #[test]
    fn colour_temperature_is_kelvin() {
        assert_eq!(kelvin_of_cct(0), 1900);
        assert_eq!(kelvin_of_cct(255), 10060);
        assert_eq!(kelvin_of_cct(999), 10060);
        assert_eq!(cct_of_kelvin(2700.0), 2700);
        // Below WLED's kelvin range it would be read as 0–255: clamped.
        assert_eq!(cct_of_kelvin(1000.0), 1900);
        assert_eq!(cct_of_kelvin(20000.0), 10091);
        // What WLED makes of it (`(k - 1900) >> 5`) reads back to within a step.
        for k in [1900u64, 2700, 4000, 6500, 10060] {
            let back = kelvin_of_cct((cct_of_kelvin(k as f64) - 1900) >> 5);
            assert!(k - back < 32, "{k} → {back}");
        }
    }
}
