//! What a WLED's state (`/json/state`, or the `state` of a WebSocket message)
//! is published as.
//!
//! The first segment is also flattened into the attributes a WLED device has
//! always had — `color`, `effect_id`, `palette_id` and the rest — so a rule
//! written against them keeps working. Every segment, with every setting it
//! has, is in `segments`; the controller-wide settings (nightlight, sync, live
//! data, sound) each have their own attribute.
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

fn pct(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_f64) {
        Some(b) => json!((b / 255.0 * 1000.0).round() / 10.0),
        None => Value::Null,
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
        "brightness": g("bri"),
        "brightness_pct": pct(s.get("bri")),
        "cct": g("cct"),
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
    j.insert("brightness".into(), g("bri"));
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
            Some(t) => json!(t * 100),
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
    j.insert("color".into(), raw_col(0));
    j.insert("color2".into(), raw_col(1));
    j.insert("color3".into(), raw_col(2));
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
        "cct",
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
        assert_eq!(a["color"], json!([106, 13, 173]));
        assert_eq!(a["color2"], json!([255, 69, 0]));
        assert_eq!(a["effect_id"], 51);
        assert_eq!(a["custom3"], 16);
        assert_eq!(a["segment_count"], 1);
        let s = &a["segments"][0];
        assert_eq!(s["colors"], json!(["#6a0dad", "#ff4500", "#00ff3c"]));
        assert!(s["whites"].is_null());
        assert_eq!(s["stop"], 400);
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
}
