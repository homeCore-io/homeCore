//! Presets and playlists, as `presets.json` holds them and as WLED's JSON
//! API saves them (`wled00/presets.cpp`, v16).
//!
//! WLED keeps one file of slots 1–250. A slot holds one of three things:
//! - a **look**: a saved state (`seg`, `bri`, `on`, …) — "save what's running"
//!   (`psave` without `o`) captures one;
//! - a **playlist**: `{"playlist": {ps, dur, transition, repeat, end, r}}`;
//! - a **command**: an HTTP API string (`win`) or a JSON API call stored as
//!   given (`psave` with `o`).
//!
//! Slot 0 is WLED's scratch slot, and an empty object a deleted preset; neither
//! is listed.

use serde_json::{json, Map, Value};

/// One slot, for a list a person picks from.
pub fn entry(id: u64, p: &Map<String, Value>) -> Value {
    let name = p
        .get("n")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| format!("Preset {id}"), str::to_string);
    let kind = if p.get("playlist").is_some_and(Value::is_object) {
        "playlist"
    } else if p.get("win").is_some() || (p.get("seg").is_none() && p.get("bri").is_none()) {
        "command"
    } else {
        "look"
    };
    let mut e = json!({ "id": id, "name": name, "kind": kind });
    if let Some(q) = p
        .get("ql")
        .and_then(Value::as_str)
        .filter(|q| !q.is_empty())
    {
        e["quick_label"] = json!(q);
    }
    if kind == "look" {
        // Its colours and effect, from its first segment that exists — a
        // card can show what it looks like without applying it.
        let first = match p.get("seg") {
            Some(Value::Array(a)) => a
                .iter()
                .find(|s| s.get("stop").and_then(Value::as_u64) != Some(0))
                .cloned(),
            Some(s @ Value::Object(_)) => Some(s.clone()),
            _ => None,
        };
        if let Some(s) = first {
            let cols: Vec<Value> = s
                .get("col")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .map(crate::state::hex)
                        .filter(|h| !h.is_null())
                        .collect()
                })
                .unwrap_or_default();
            e["colors"] = json!(cols);
            if let Some(fx) = s.get("fx") {
                e["effect_id"] = fx.clone();
            }
            if let Some(pal) = s.get("pal") {
                e["palette_id"] = pal.clone();
            }
        }
        if let Some(b) = p.get("bri").and_then(Value::as_f64) {
            e["brightness_pct"] = json!((b / 255.0 * 100.0).round());
        }
        let segments = match p.get("seg") {
            Some(Value::Array(a)) => a
                .iter()
                .filter(|s| s.get("stop").and_then(Value::as_u64) != Some(0))
                .count(),
            Some(Value::Object(_)) => 1,
            _ => 0,
        };
        e["segments"] = json!(segments);
    }
    if kind == "playlist" {
        e["playlist"] = playlist_view(&p["playlist"]);
    }
    e
}

/// Every slot that holds something, in slot order.
pub fn list(file: &Value) -> Vec<Value> {
    let Some(obj) = file.as_object() else {
        return Vec::new();
    };
    let mut slots: Vec<(u64, &Map<String, Value>)> = obj
        .iter()
        .filter_map(|(k, v)| {
            let id: u64 = k.parse().ok().filter(|&id| id > 0)?;
            let p = v.as_object().filter(|p| !p.is_empty())?;
            Some((id, p))
        })
        .collect();
    slots.sort_by_key(|(id, _)| *id);
    slots.into_iter().map(|(id, p)| entry(id, p)).collect()
}

/// A playlist as homeCore says it: each step's preset, how long it stays
/// (seconds) and its fade (ms); how many times it plays (0 = forever);
/// what happens at the end; shuffled or not.
pub fn playlist_view(pl: &Value) -> Value {
    let arr = |k: &str| {
        pl.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let presets = arr("ps");
    let one = |k: &str| pl.get(k).filter(|v| !v.is_array()).and_then(Value::as_u64);
    let durs = arr("dur");
    let trs = arr("transition");
    let at = |list: &[Value], fallback: Option<u64>, i: usize| {
        list.get(i)
            .or(list.last())
            .and_then(Value::as_u64)
            .or(fallback)
    };
    let steps: Vec<Value> = presets
        .iter()
        .enumerate()
        .map(|(i, ps)| {
            json!({
                "preset_id": ps,
                // Tenths of a second, both.
                "duration_s": at(&durs, one("dur"), i).map(|d| d as f64 / 10.0),
                "transition_ms": at(&trs, one("transition"), i).map(|t| t * 100),
            })
        })
        .collect();
    let repeat = pl.get("repeat").and_then(Value::as_i64).unwrap_or(0);
    json!({
        "steps": steps,
        "repeat": repeat.max(0),
        "shuffle": repeat < 0 || pl.get("r").and_then(Value::as_bool).unwrap_or(false),
        // 0 stays on the last; 255 goes back to what ran before.
        "end_preset_id": pl.get("end").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// WLED's `playlist` object from homeCore's words (the shape
/// [`playlist_view`] gives).
pub fn playlist_wire(view: &Value) -> Option<Value> {
    let steps = view.get("steps")?.as_array()?;
    if steps.is_empty() {
        return None;
    }
    let mut ps = Vec::new();
    let mut dur = Vec::new();
    let mut tr = Vec::new();
    for s in steps {
        ps.push(s.get("preset_id")?.as_u64()?);
        let d = s.get("duration_s").and_then(Value::as_f64).unwrap_or(10.0);
        dur.push((d.max(0.0) * 10.0).round() as u64);
        let t = s
            .get("transition_ms")
            .and_then(Value::as_f64)
            .unwrap_or(700.0);
        tr.push((t.max(0.0) / 100.0).round() as u64);
    }
    Some(json!({
        "ps": ps,
        "dur": dur,
        "transition": tr,
        "repeat": view.get("repeat").and_then(Value::as_u64).unwrap_or(0),
        "end": view.get("end_preset_id").and_then(Value::as_u64).unwrap_or(0),
        "r": view.get("shuffle").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// The body that saves a playlist to `slot` (and, as WLED does, starts it).
pub fn save_playlist_body(slot: u64, name: &str, view: &Value) -> Option<Value> {
    Some(json!({
        "psave": slot,
        "n": name,
        "playlist": playlist_wire(view)?,
        "on": true,
        // `o` says "store what's here", not "capture the state".
        "o": true,
    }))
}

/// The body that renames a slot (and changes its quick label) without
/// touching what it holds: what it holds, stored again as given. `existing`
/// is the slot's object from `presets.json`.
pub fn rename_body(slot: u64, existing: &Value, name: &str, quick_label: Option<&str>) -> Value {
    let mut body = existing.as_object().cloned().unwrap_or_default();
    body.insert("psave".into(), json!(slot));
    body.insert("n".into(), json!(name));
    match quick_label {
        Some(q) if !q.is_empty() => {
            body.insert("ql".into(), json!(q));
        }
        Some(_) => {
            body.remove("ql");
        }
        None => {}
    }
    body.insert("o".into(), json!(true));
    Value::Object(body)
}

/// The first slot nothing is saved in.
pub fn free_slot(file: &Value) -> Option<u64> {
    let used: Vec<u64> = list(file).iter().filter_map(|e| e["id"].as_u64()).collect();
    (1..=250).find(|i| !used.contains(i))
}

/// `to` with `from`'s preset `id` put in a slot: `slot` when given (what is
/// there is replaced), else the same number when that is free on `to`, else
/// the first free one (`same`: one controller, so a duplicate). Returns the new file and the slot used.
///
/// A playlist is copied as it is: its steps name presets by number, which on
/// the other controller may be other presets.
pub fn copy_one(
    from: &Value,
    to: &Value,
    id: u64,
    slot: Option<u64>,
    same: bool,
) -> Result<(Value, u64), String> {
    let entry = from
        .get(id.to_string())
        .filter(|p| p.as_object().is_some_and(|m| !m.is_empty()))
        .ok_or_else(|| format!("there is no preset {id}"))?;
    let free = |n: u64| {
        to.get(n.to_string())
            .and_then(Value::as_object)
            .is_none_or(|m| m.is_empty())
    };
    let slot = match slot {
        Some(s) if (1..=250).contains(&s) => s,
        Some(s) => return Err(format!("{s} isn't a preset slot (1–250)")),
        None if free(id) && !same => id,
        None => free_slot(to).ok_or("the other controller has no free slot")?,
    };
    let mut out = to.as_object().cloned().unwrap_or_default();
    out.insert(slot.to_string(), entry.clone());
    Ok((Value::Object(out), slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_preset_is_copied_into_a_free_slot_or_the_one_asked_for() {
        let from = house();
        let to = json!({"0": {}, "1": {"n": "Theirs"}, "2": {"n": "Two"}});
        // Slot 14 is free there: it keeps its number.
        let (f, at) = copy_one(&from, &to, 14, None, false).unwrap();
        assert_eq!(at, 14);
        assert_eq!(f["14"]["n"], "Halloween 1");
        assert_eq!(f["1"]["n"], "Theirs");
        // Slot 1 is taken there: the first free one.
        let (f, at) = copy_one(&from, &to, 1, None, false).unwrap();
        assert_eq!(at, 3);
        assert_eq!(f["3"]["n"], "Christmas");
        assert_eq!(f["1"]["n"], "Theirs");
        // Asked for: replaced.
        let (f, at) = copy_one(&from, &to, 1, Some(1), false).unwrap();
        assert_eq!((at, f["1"]["n"].as_str()), (1, Some("Christmas")));
        // Within one controller (a duplicate), never onto itself.
        let (_, at) = copy_one(&from, &from, 14, None, true).unwrap();
        assert_eq!(at, 2);
        assert!(
            copy_one(&from, &to, 5, None, false).is_err(),
            "an empty slot isn't a preset"
        );
        assert!(copy_one(&from, &to, 99, None, false).is_err());
        assert!(copy_one(&from, &to, 1, Some(300), false).is_err());
    }

    fn house() -> Value {
        // Shapes from the house controller's presets.json: `seg` as an object
        // (slots 1–8) and as an array (9–16).
        json!({
            "0": {},
            "1": {"n": "Christmas", "bri": 128, "seg": {"col": [[0,0,255],[8,255,0],[255,0,0]], "fx": 84, "pal": 0}},
            "14": {"n": "Halloween 1", "on": true, "bri": 128, "ql": "H1",
                   "seg": [{"id": 0, "stop": 400, "fx": 51, "pal": 0, "col": [[106,13,173],[255,69,0],[0,255,60]]}, {"stop": 0}]},
            "20": {"n": "Halloween night", "playlist": {"ps": [14, 15, 16], "dur": [6000, 6000, 3000], "transition": [20, 20, 50], "repeat": 0, "end": 0}},
            "21": {"n": "Party", "win": "FX=~&FP=~"},
            "5": {}
        })
    }

    #[test]
    fn the_slots_are_listed_by_kind_with_what_they_look_like() {
        let l = list(&house());
        let ids: Vec<u64> = l.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![1, 14, 20, 21]);
        assert_eq!(l[0]["kind"], "look");
        assert_eq!(l[0]["colors"], json!(["#0000ff", "#08ff00", "#ff0000"]));
        assert_eq!(l[0]["effect_id"], 84);
        assert_eq!(l[0]["segments"], 1);
        assert_eq!(l[1]["quick_label"], "H1");
        assert_eq!(l[1]["brightness_pct"], 50.0);
        assert_eq!(l[1]["segments"], 1);
        assert_eq!(l[2]["kind"], "playlist");
        assert_eq!(
            l[2]["playlist"]["steps"][0],
            json!({"preset_id": 14, "duration_s": 600.0, "transition_ms": 2000})
        );
        assert_eq!(l[3]["kind"], "command");
        assert_eq!(free_slot(&house()), Some(2));
    }

    #[test]
    fn a_playlist_round_trips_between_the_two_shapes() {
        let wire = house()["20"]["playlist"].clone();
        let view = playlist_view(&wire);
        let back = playlist_wire(&view).unwrap();
        assert_eq!(back["ps"], wire["ps"]);
        assert_eq!(back["dur"], wire["dur"]);
        assert_eq!(back["transition"], wire["transition"]);
        // A negative repeat is WLED's "forever, shuffled".
        let v = playlist_view(&json!({"ps": [1], "dur": 100, "repeat": -1}));
        assert_eq!(
            (v["repeat"].clone(), v["shuffle"].clone()),
            (json!(0), json!(true))
        );
        assert_eq!(v["steps"][0]["duration_s"], 10.0);
        let b = save_playlist_body(20, "Halloween night", &view).unwrap();
        assert_eq!(
            (b["psave"].clone(), b["o"].clone()),
            (json!(20), json!(true))
        );
        assert!(save_playlist_body(20, "x", &json!({"steps": []})).is_none());
    }

    #[test]
    fn renaming_keeps_what_the_slot_holds() {
        let b = rename_body(14, &house()["14"], "Halloween one", Some(""));
        assert_eq!(b["n"], "Halloween one");
        assert!(b.get("ql").is_none());
        assert_eq!(b["seg"], house()["14"]["seg"]);
        assert_eq!(
            (b["psave"].clone(), b["o"].clone()),
            (json!(14), json!(true))
        );
    }
}
