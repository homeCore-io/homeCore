//! A homeCore command, in homeCore's words, as the body WLED's JSON API takes
//! (`POST /json/state`).
//!
//! Every word here is one a client or a rule can send to a WLED device:
//!
//! **The controller:** `on` (true, false or `"toggle"`), `brightness`
//! (0–255), `brightness_pct`, `transition` (ms), `preset_id` (also `preset`,
//! `apply_preset`, and `playlist_id` — a playlist is a preset), `main_segment`,
//! `nightlight` (`{on, duration_min, mode, target_pct}`), `sync_send`,
//! `sync_receive`, `sync_send_groups`, `sync_receive_groups` (bitmasks),
//! `live_override` (0 off, 1 until the stream ends, 2 until restart),
//! `stop_live` (leave live data now), `next_in_playlist`,
//! `audio` (the AudioReactive usermod on or off), `reboot`.
//!
//! **Presets:** `save_preset` (a slot) with `preset_name`,
//! `preset_quick_label`, `include_brightness`, `include_segments`,
//! `selected_only`; `delete_preset` (a slot).
//!
//! **A segment's look**, sent at the top level for `segment` (an id,
//! `"all"`, `"selected"`; segment 0 when left out, as it always was) or as
//! `segments: [{id, …}]` for several at once: `segment_on`,
//! `segment_brightness` (0–255) or `segment_brightness_pct`, `color`,
//! `color2`, `color3` or `colors` (each `[r,g,b]`, `[r,g,b,w]`, `{r,g,b}` or
//! `"#rrggbb"`), `cct` (0–255, warm to cold), `effect_id` (also `effect`),
//! `effect_speed`, `effect_intensity`, `custom1`–`custom3`, `option1`–`option3`,
//! `palette_id` (also `palette`), `start`, `stop`, `grouping`, `spacing`,
//! `offset`, `reverse`, `mirror`, `freeze`, `selected`, `name`, `blend_mode`,
//! `sound_sim`, `mapping_1d2d`.
//!
//! **Segments themselves:** `add_segment` (`{start, stop, name}`),
//! `delete_segment` (an id).
//!
//! **Anything else WLED takes:** `raw` — an object merged into the body as
//! it is, for what this list doesn't name yet.
//!
//! Some words need to know the strip as it is (how many segments, which are
//! selected, the next free id); [`build`] is given it, and asks for it with
//! [`needs_state`].

use serde_json::{json, Map, Value};

/// What the strip is now, for the words that depend on it.
#[derive(Debug, Clone, Default)]
pub struct Strip {
    /// The ids of the segments that exist, in order.
    pub segment_ids: Vec<u64>,
    /// The ids of those selected.
    pub selected: Vec<u64>,
}

impl Strip {
    pub fn from_state(state: &Value) -> Self {
        let segs = state
            .get("seg")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let live: Vec<&Value> = segs
            .iter()
            .filter(|s| s.get("stop").and_then(Value::as_u64) != Some(0))
            .collect();
        Self {
            segment_ids: live
                .iter()
                .filter_map(|s| s.get("id").and_then(Value::as_u64))
                .collect(),
            selected: live
                .iter()
                .filter(|s| s.get("sel").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|s| s.get("id").and_then(Value::as_u64))
                .collect(),
        }
    }
}

/// Whether [`build`] needs the strip as it is to translate `cmd`.
pub fn needs_state(cmd: &Value) -> bool {
    matches!(
        cmd.get("segment").and_then(Value::as_str),
        Some("all") | Some("selected")
    ) || cmd.get("add_segment").is_some()
}

/// The keys of a segment's look, homeCore's name → WLED's, with how each
/// value is taken.
const SEGMENT_KEYS: &[(&str, &str, Kind)] = &[
    ("segment_on", "on", Kind::Bool),
    ("segment_brightness", "bri", Kind::Byte),
    ("segment_brightness_pct", "bri", Kind::Pct),
    ("cct", "cct", Kind::Byte),
    ("effect_id", "fx", Kind::Uint),
    ("effect", "fx", Kind::Uint),
    ("effect_speed", "sx", Kind::Byte),
    ("effect_intensity", "ix", Kind::Byte),
    ("custom1", "c1", Kind::Byte),
    ("custom2", "c2", Kind::Byte),
    ("custom3", "c3", Kind::Uint),
    ("option1", "o1", Kind::Bool),
    ("option2", "o2", Kind::Bool),
    ("option3", "o3", Kind::Bool),
    ("palette_id", "pal", Kind::Uint),
    ("palette", "pal", Kind::Uint),
    ("start", "start", Kind::Uint),
    ("stop", "stop", Kind::Uint),
    ("grouping", "grp", Kind::Uint),
    ("spacing", "spc", Kind::Uint),
    ("offset", "of", Kind::Uint),
    ("reverse", "rev", Kind::Bool),
    ("mirror", "mi", Kind::Bool),
    ("freeze", "frz", Kind::Bool),
    ("selected", "sel", Kind::Bool),
    ("name", "n", Kind::Text),
    ("blend_mode", "bm", Kind::Uint),
    ("sound_sim", "si", Kind::Uint),
    ("mapping_1d2d", "m12", Kind::Uint),
];

#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Byte,
    Pct,
    Uint,
    Text,
}

fn take(kind: Kind, v: &Value) -> Option<Value> {
    match kind {
        Kind::Bool => v.as_bool().map(Value::Bool),
        Kind::Byte => number(v).map(|n| json!(n.clamp(0.0, 255.0).round() as u64)),
        Kind::Pct => {
            number(v).map(|p| json!(((p.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u64))
        }
        Kind::Uint => number(v).filter(|n| *n >= 0.0).map(|n| json!(n as u64)),
        Kind::Text => v.as_str().map(|s| json!(s)),
    }
}

/// A colour in any of the shapes a client sends, as WLED's `[r,g,b]` (or
/// `[r,g,b,w]`).
pub fn colour(v: &Value) -> Option<Value> {
    let byte = |x: &Value| x.as_f64().map(|n| n.clamp(0.0, 255.0).round() as u64);
    match v {
        Value::Array(a) if (3..=4).contains(&a.len()) => {
            let c: Option<Vec<u64>> = a.iter().map(byte).collect();
            c.map(|c| json!(c))
        }
        Value::Object(o) => {
            let mut c = vec![byte(o.get("r")?)?, byte(o.get("g")?)?, byte(o.get("b")?)?];
            if let Some(w) = o.get("w").and_then(byte) {
                c.push(w);
            }
            Some(json!(c))
        }
        Value::String(s) => {
            let h = s.trim().trim_start_matches('#');
            if h.len() != 6 && h.len() != 8 {
                return None;
            }
            let part = |i: usize| u64::from_str_radix(&h[i..i + 2], 16).ok();
            let mut c = vec![part(0)?, part(2)?, part(4)?];
            if h.len() == 8 {
                c.push(part(6)?);
            }
            Some(json!(c))
        }
        _ => None,
    }
}

/// The look words in `src` as a WLED segment object (without its `id`).
fn segment_look(src: &Map<String, Value>) -> Map<String, Value> {
    let mut seg = Map::new();
    for (ours, theirs, kind) in SEGMENT_KEYS {
        if let Some(v) = src.get(*ours).and_then(|v| take(*kind, v)) {
            seg.insert((*theirs).into(), v);
        }
    }
    // Colours: `colors` whole, or one slot at a time. A slot left out is
    // sent as `[]`, which WLED reads as "leave this one".
    let mut cols: Vec<Value> = match src.get("colors").and_then(Value::as_array) {
        Some(list) => list.iter().take(3).filter_map(colour).collect(),
        None => Vec::new(),
    };
    for (i, key) in ["color", "color2", "color3"].iter().enumerate() {
        if let Some(c) = src.get(*key).and_then(colour) {
            while cols.len() <= i {
                cols.push(json!([]));
            }
            cols[i] = c;
        }
    }
    if !cols.is_empty() {
        seg.insert("col".into(), Value::Array(cols));
    }
    seg
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The body to `POST /json/state` for `cmd`, or `None` when it says nothing
/// WLED would act on. `strip` answers the words that depend on the strip.
pub fn build(cmd: &Value, strip: &Strip) -> Option<Value> {
    let src = cmd.as_object()?;
    let mut body = Map::new();
    // A screen's choice may arrive as text ("2"); a number is a number.
    let num = |k: &str| src.get(k).and_then(number);
    let int = |k: &str| num(k).map(|n| n as i64);
    let action = src.get("action").and_then(Value::as_str);

    match src.get("on") {
        Some(Value::Bool(b)) => {
            body.insert("on".into(), json!(b));
        }
        Some(Value::String(s)) if s == "toggle" => {
            body.insert("on".into(), json!("t"));
        }
        _ => {}
    }
    if let Some(b) = num("brightness") {
        body.insert("bri".into(), json!(b.clamp(0.0, 255.0).round() as u64));
    }
    if let Some(p) = num("brightness_pct") {
        body.insert(
            "bri".into(),
            json!(((p.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u64),
        );
    }
    if let Some(ms) = num("transition") {
        body.insert(
            "tt".into(),
            json!(((ms.max(0.0) / 100.0).round() as u64).min(65535)),
        );
    }
    // `preset_id` is what the device publishes; `preset` is the original wire
    // key; `apply_preset` the action; `playlist_id` a playlist, which WLED
    // stores — and starts — as a preset.
    if let Some(ps) = ["preset_id", "preset", "apply_preset", "playlist_id"]
        .iter()
        .find_map(|k| int(k))
    {
        body.insert("ps".into(), json!(ps));
    }
    if let Some(m) = int("main_segment") {
        body.insert("mainseg".into(), json!(m));
    }
    // The nightlight action sends it flat; a rule may send it whole.
    let flat_nl: Map<String, Value> = [
        ("nightlight_on", "on"),
        ("nightlight_duration_min", "duration_min"),
        ("nightlight_mode", "mode"),
        ("nightlight_target_pct", "target_pct"),
    ]
    .iter()
    .filter_map(|(ours, inner)| src.get(*ours).map(|v| ((*inner).to_string(), v.clone())))
    .collect();
    let nl_source = src
        .get("nightlight")
        .and_then(Value::as_object)
        .cloned()
        .or((!flat_nl.is_empty()).then_some(flat_nl));
    if let Some(nl) = nl_source.as_ref() {
        let mut out = Map::new();
        if let Some(on) = nl.get("on").and_then(Value::as_bool) {
            out.insert("on".into(), json!(on));
        }
        if let Some(d) = nl.get("duration_min").and_then(number) {
            out.insert("dur".into(), json!(d.clamp(1.0, 255.0).round() as u64));
        }
        if let Some(m) = nl.get("mode").and_then(Value::as_str) {
            let mode = match m {
                "instant" => 0,
                "fade" => 1,
                "color" | "colour" | "color_fade" => 2,
                "sunrise" => 3,
                _ => 1,
            };
            out.insert("mode".into(), json!(mode));
        }
        if let Some(p) = nl.get("target_pct").and_then(number) {
            out.insert(
                "tbri".into(),
                json!(((p.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u64),
            );
        }
        if !out.is_empty() {
            body.insert("nl".into(), Value::Object(out));
        }
    }
    // WLED takes `send` and the two group masks; it reports `recv`, but only
    // as "receive groups isn't empty" (json.cpp) — so following others is
    // switched with the receive mask: off is none, on is the groups given,
    // else group 1.
    let mut udpn = Map::new();
    if let Some(b) = src.get("sync_send").and_then(Value::as_bool) {
        udpn.insert("send".into(), json!(b));
    }
    if let Some(n) = src.get("sync_send_groups").and_then(Value::as_u64) {
        udpn.insert("sgrp".into(), json!(n.min(255)));
    }
    let rgrp = src.get("sync_receive_groups").and_then(Value::as_u64);
    match src.get("sync_receive").and_then(Value::as_bool) {
        Some(false) => {
            udpn.insert("rgrp".into(), json!(0));
        }
        Some(true) => {
            udpn.insert("rgrp".into(), json!(rgrp.unwrap_or(1).clamp(1, 255)));
        }
        None => {
            if let Some(n) = rgrp {
                udpn.insert("rgrp".into(), json!(n.min(255)));
            }
        }
    }
    if !udpn.is_empty() {
        body.insert("udpn".into(), Value::Object(udpn));
    }
    if let Some(l) = int("live_override") {
        body.insert("lor".into(), json!(l.clamp(0, 2)));
    }
    if src.get("stop_live").and_then(Value::as_bool) == Some(true) || action == Some("stop_live") {
        body.insert("live".into(), json!(false));
    }
    if src.get("next_in_playlist").and_then(Value::as_bool) == Some(true)
        || action == Some("next_in_playlist")
    {
        body.insert("np".into(), json!(true));
    }
    if let Some(a) = src.get("audio").and_then(Value::as_bool) {
        body.insert("AudioReactive".into(), json!({ "on": a }));
    }
    if src.get("reboot").and_then(Value::as_bool) == Some(true) {
        body.insert("rb".into(), json!(true));
    }

    // Presets.
    if let Some(slot) = int("save_preset") {
        body.insert("psave".into(), json!(slot));
        if let Some(n) = src.get("preset_name").and_then(Value::as_str) {
            body.insert("n".into(), json!(n));
        }
        if let Some(q) = src.get("preset_quick_label").and_then(Value::as_str) {
            body.insert("ql".into(), json!(q));
        }
        for (ours, theirs, default) in [
            ("include_brightness", "ib", true),
            ("include_segments", "sb", false),
            ("selected_only", "sc", false),
        ] {
            let v = src.get(ours).and_then(Value::as_bool).unwrap_or(default);
            body.insert(theirs.into(), json!(v));
        }
    }
    if let Some(slot) = int("delete_preset") {
        body.insert("pdel".into(), json!(slot));
    }

    // Segments: the look words for one target, `segments` for several, and
    // adding or removing one.
    let mut segs: Vec<Value> = Vec::new();
    let look = segment_look(src);
    if !look.is_empty() {
        let ids: Vec<u64> = match src.get("segment") {
            Some(Value::String(s)) if s == "all" => strip.segment_ids.clone(),
            Some(Value::String(s)) if s == "selected" => strip.selected.clone(),
            Some(v) => number(v).map(|n| n as u64).into_iter().collect(),
            None => vec![0],
        };
        for id in ids {
            let mut s = look.clone();
            s.insert("id".into(), json!(id));
            segs.push(Value::Object(s));
        }
    }
    if let Some(list) = src.get("segments").and_then(Value::as_array) {
        for item in list {
            let Some(o) = item.as_object() else { continue };
            let Some(id) = o.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let mut s = segment_look(o);
            s.insert("id".into(), json!(id));
            segs.push(Value::Object(s));
        }
    }
    if let Some(add) = src.get("add_segment").and_then(Value::as_object) {
        let next = (0..)
            .find(|i| !strip.segment_ids.contains(i))
            .unwrap_or_default();
        let mut s = segment_look(add);
        s.insert("id".into(), json!(next));
        segs.push(Value::Object(s));
    }
    if let Some(id) = src.get("delete_segment").and_then(Value::as_u64) {
        // A segment whose stop is 0 is deleted.
        segs.push(json!({ "id": id, "stop": 0 }));
    }
    // Identify: bright white on every segment, from the first LED.
    let identify =
        src.get("identify").and_then(Value::as_bool) == Some(true) || action == Some("identify");
    if identify {
        body.insert("on".into(), json!(true));
        body.insert("bri".into(), json!(255));
        let ids = if strip.segment_ids.is_empty() {
            vec![0]
        } else {
            strip.segment_ids.clone()
        };
        for id in ids {
            segs.push(json!({ "id": id, "col": [[255, 255, 255]], "fx": 0 }));
        }
    }
    if !segs.is_empty() {
        body.insert("seg".into(), Value::Array(segs));
    }

    if let Some(raw) = src.get("raw").and_then(Value::as_object) {
        for (k, v) in raw {
            body.insert(k.clone(), v.clone());
        }
    }

    // A command that sets nothing but asks for the state back would be a
    // wasted request.
    (!body.is_empty()).then_some(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(cmd: Value) -> Value {
        build(&cmd, &Strip::default()).expect("a body")
    }

    #[test]
    fn the_controller_words_become_wled_s() {
        let b = one(json!({
            "on": "toggle", "brightness_pct": 50, "transition": 2000, "preset_id": 14,
            "nightlight": {"on": true, "duration_min": 30, "mode": "sunrise", "target_pct": 0},
            "sync_send": false, "sync_receive": true, "live_override": 1, "audio": true
        }));
        assert_eq!(b["on"], "t");
        assert_eq!(b["bri"], 128);
        assert_eq!(b["tt"], 20);
        assert_eq!(b["ps"], 14);
        assert_eq!(
            b["nl"],
            json!({"on": true, "dur": 30, "mode": 3, "tbri": 0})
        );
        assert_eq!(b["udpn"], json!({"send": false, "rgrp": 1}));
        assert_eq!(b["lor"], 1);
        assert_eq!(b["AudioReactive"], json!({"on": true}));
        let b = one(
            json!({"sync_receive": false, "sync_send_groups": 3, "stop_live": true, "next_in_playlist": true}),
        );
        assert_eq!(b["udpn"], json!({"sgrp": 3, "rgrp": 0}));
        assert_eq!(
            (b["live"].clone(), b["np"].clone()),
            (json!(false), json!(true))
        );
    }

    #[test]
    fn a_look_goes_to_segment_zero_unless_told() {
        let b = one(json!({
            "effect_id": 51, "effect_speed": 300, "custom1": 12, "option2": true,
            "palette_id": 3, "color": "#6a0dad", "color3": {"r": 0, "g": 255, "b": 60}
        }));
        assert_eq!(
            b["seg"],
            json!([{
                "id": 0, "fx": 51, "sx": 255, "c1": 12, "o2": true, "pal": 3,
                "col": [[106, 13, 173], [], [0, 255, 60]]
            }])
        );
    }

    #[test]
    fn all_and_selected_need_the_strip() {
        let cmd = json!({"segment": "all", "effect_id": 2});
        assert!(needs_state(&cmd));
        let strip = Strip::from_state(&json!({"seg": [
            {"id": 0, "stop": 240, "sel": true},
            {"id": 1, "stop": 320, "sel": false},
            {"id": 3, "stop": 0}
        ]}));
        assert_eq!(strip.segment_ids, vec![0, 1]);
        let b = build(&cmd, &strip).unwrap();
        assert_eq!(b["seg"], json!([{"id": 0, "fx": 2}, {"id": 1, "fx": 2}]));
        let b = build(&json!({"segment": "selected", "segment_on": false}), &strip).unwrap();
        assert_eq!(b["seg"], json!([{"id": 0, "on": false}]));
        // Adding takes the first free id; deleting stops it at 0.
        let b = build(
            &json!({"add_segment": {"start": 320, "stop": 400, "name": "Garage"}}),
            &strip,
        )
        .unwrap();
        assert_eq!(
            b["seg"],
            json!([{"id": 2, "start": 320, "stop": 400, "n": "Garage"}])
        );
        let b = one(json!({"delete_segment": 1}));
        assert_eq!(b["seg"], json!([{"id": 1, "stop": 0}]));
    }

    #[test]
    fn several_segments_at_once() {
        let b = one(json!({"segments": [
            {"id": 0, "segment_brightness_pct": 100, "reverse": true},
            {"id": 1, "colors": [[255, 0, 0], "#00ff00"], "name": "Posts"},
            {"no_id": true}
        ]}));
        assert_eq!(
            b["seg"],
            json!([
                {"id": 0, "bri": 255, "rev": true},
                {"id": 1, "col": [[255, 0, 0], [0, 255, 0]], "n": "Posts"}
            ])
        );
    }

    #[test]
    fn presets_are_saved_and_deleted() {
        let b = one(json!({
            "save_preset": 20, "preset_name": "Halloween night", "preset_quick_label": "🎃",
            "include_segments": true
        }));
        assert_eq!(b["psave"], 20);
        assert_eq!(b["n"], "Halloween night");
        assert_eq!(b["ql"], "🎃");
        assert_eq!(
            (b["ib"].clone(), b["sb"].clone(), b["sc"].clone()),
            (json!(true), json!(true), json!(false))
        );
        assert_eq!(one(json!({"delete_preset": 3}))["pdel"], 3);
    }

    #[test]
    fn the_nightlight_action_s_flat_words_and_text_numbers_work() {
        let b = one(json!({
            "action": "nightlight", "nightlight_on": true, "nightlight_duration_min": "30",
            "nightlight_mode": "fade", "nightlight_target_pct": 0
        }));
        assert_eq!(
            b["nl"],
            json!({"on": true, "dur": 30, "mode": 1, "tbri": 0})
        );
        assert_eq!(
            one(json!({"action": "set_live_override", "live_override": "2"}))["lor"],
            2
        );
        assert_eq!(one(json!({"action": "next_in_playlist"}))["np"], true);
        assert_eq!(
            one(json!({"effect_id": 9, "segment": "1"}))["seg"][0]["id"],
            1
        );
    }

    #[test]
    fn the_old_words_still_work() {
        let b = one(json!({"preset": 2, "effect": 9, "palette": 1, "color": [1, 2, 3]}));
        assert_eq!(b["ps"], 2);
        assert_eq!(
            b["seg"],
            json!([{"id": 0, "fx": 9, "pal": 1, "col": [[1, 2, 3]]}])
        );
        assert_eq!(
            one(json!({"action": "apply_preset", "apply_preset": 5}))["ps"],
            5
        );
    }

    #[test]
    fn identify_lights_every_segment_white_and_raw_passes_through() {
        let strip = Strip {
            segment_ids: vec![0, 1],
            selected: vec![],
        };
        let b = build(&json!({"action": "identify"}), &strip).unwrap();
        assert_eq!(b["bri"], 255);
        assert_eq!(b["seg"].as_array().unwrap().len(), 2);
        let b = one(json!({"raw": {"lor": 2, "v": true}}));
        assert_eq!(b, json!({"lor": 2, "v": true}));
        assert!(build(&json!({"nothing": 1}), &Strip::default()).is_none());
    }
}
