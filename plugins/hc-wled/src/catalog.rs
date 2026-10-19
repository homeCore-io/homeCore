//! The effect and palette catalogue a controller carries: each effect with
//! the controls it uses, and each palette with its colours.
//!
//! An effect's metadata (`/json/fxdata`) is WLED's own string,
//! `<sliders>;<colours>;<palette>;<flags>;<defaults>`, and is read here
//! exactly as WLED's own page reads it (`setEffectParameters`, data/index.js,
//! v16): up to five sliders (speed, intensity, custom 1–3) then up to three
//! options; `!` is the control's own name, empty is "not used". An effect with
//! no metadata at all uses everything its page would show by default.

use serde_json::{json, Value};

const SLIDERS: [(&str, &str); 5] = [
    ("effect_speed", "Speed"),
    ("effect_intensity", "Intensity"),
    ("custom1", "Custom 1"),
    ("custom2", "Custom 2"),
    ("custom3", "Custom 3"),
];
const COLOURS: [&str; 3] = ["Primary", "Background", "Third"];

/// One effect's controls, from its id, name and metadata string.
pub fn effect(id: usize, name: &str, meta: &str) -> Value {
    let defined = !meta.is_empty();
    let parts: Vec<&str> = if meta.is_empty() {
        Vec::new()
    } else {
        meta.split(';').collect()
    };
    let list = |i: usize| -> Vec<&str> {
        match parts.get(i) {
            Some(p) if !p.is_empty() => p.split(',').collect(),
            _ => Vec::new(),
        }
    };
    let sl = list(0);
    let co = list(1);
    let pa = list(2);

    let mut sliders = Vec::new();
    // WLED shows the first two sliders of an old effect with no metadata,
    // and all five of a newer one.
    let undefined_count = if id < 128 { 2 } else { 5 };
    for (i, (key, own)) in SLIDERS.iter().enumerate() {
        let label = sl.get(i).copied().unwrap_or("");
        if (!defined && i < undefined_count) || !label.is_empty() {
            let name = if label.is_empty() || label == "!" {
                own.to_string()
            } else {
                label.to_string()
            };
            sliders.push(json!({
                "key": key,
                "label": name,
                // custom3 is five bits in WLED: 0–31.
                "max": if i == 4 { 31 } else { 255 },
            }));
        }
    }
    let mut options = Vec::new();
    if sl.len() > 5 {
        for i in 0..3 {
            let label = sl.get(5 + i).copied().unwrap_or("");
            if !label.is_empty() {
                options.push(json!({
                    "key": format!("option{}", i + 1),
                    "label": if label == "!" { format!("Option {}", i + 1) } else { label.to_string() },
                }));
            }
        }
    }
    let mut colours = Vec::new();
    for (i, own) in COLOURS.iter().enumerate() {
        match co.get(i).copied() {
            Some(l) if !l.is_empty() => colours.push(json!({
                "slot": i,
                "label": match l {
                    "!" => own.to_string(),
                    "1" | "2" | "3" => format!("Colour {l}"),
                    other => other.to_string(),
                },
            })),
            _ if !defined => {
                colours.push(json!({ "slot": i, "label": format!("Colour {}", i + 1) }))
            }
            _ => {}
        }
    }
    let first_pal = pa.first().copied().unwrap_or("");
    // A number there is a default, not a label: WLED hides the palette then.
    let pal_named = !first_pal.is_empty() && first_pal.parse::<f64>().is_err();
    let palette = if !defined || pal_named {
        let label = first_pal.split('=').next().unwrap_or("");
        Some(if label.is_empty() || label == "!" {
            "Palette".to_string()
        } else {
            label.to_string()
        })
    } else {
        None
    };
    let flags = parts.get(3).copied().unwrap_or("");
    let defaults: serde_json::Map<String, Value> = parts
        .get(4)
        .map(|d| {
            d.split(',')
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    let key = match k.trim() {
                        "sx" => "effect_speed",
                        "ix" => "effect_intensity",
                        "c1" => "custom1",
                        "c2" => "custom2",
                        "c3" => "custom3",
                        "o1" => "option1",
                        "o2" => "option2",
                        "o3" => "option3",
                        "pal" => "palette_id",
                        "m12" => "mapping_1d2d",
                        "si" => "sound_sim",
                        "rev" => "reverse",
                        "mi" => "mirror",
                        _ => return None,
                    };
                    let n: f64 = v.trim().parse().ok()?;
                    Some((key.to_string(), json!(n as i64)))
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": id,
        "name": name,
        "sliders": sliders,
        "options": options,
        "colors": colours,
        "palette": palette,
        "one_d": flags.contains('1') || flags.is_empty(),
        "two_d": flags.contains('2'),
        "single_pixel": flags.contains('0'),
        "sound": flags.contains('v') || flags.contains('f'),
        "defaults": defaults,
    })
}

/// Every effect, from `/json/effects` (names) and `/json/fxdata` (metadata).
/// WLED's reserved slots ("RSVD", "-") are left out; ids stay as WLED has them.
pub fn effects(names: &[Value], meta: &[Value]) -> Vec<Value> {
    names
        .iter()
        .enumerate()
        .filter_map(|(id, n)| {
            let name = n.as_str()?;
            if matches!(name, "RSVD" | "-") {
                return None;
            }
            let m = meta.get(id).and_then(Value::as_str).unwrap_or("");
            Some(effect(id, name, m))
        })
        .collect()
}

/// One palette's colours as `/json/palx` gives them: `[[pos, r, g, b], …]`
/// gradient stops, or `["r", …]` (random) and `["c1", …]` (the segment's own
/// colours) for the palettes made from those. Kept as WLED says it; a client
/// draws `c1`–`c3` with the colours it is showing.
pub fn palette(id: i64, name: &str, colours: Option<&Value>) -> Value {
    let stops: Vec<Value> = colours
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| match s {
                    Value::Array(p) if p.len() >= 4 => json!({
                        "at": p[0].as_u64().unwrap_or(0) as f64 / 255.0,
                        "color": crate::state::hex(&json!([p[1], p[2], p[3]])),
                    }),
                    Value::String(t) if t == "r" => json!({ "random": true }),
                    Value::String(t) => json!({ "slot": t.trim_start_matches('c').parse::<u8>().unwrap_or(1).saturating_sub(1) }),
                    _ => Value::Null,
                })
                .filter(|v| !v.is_null())
                .collect()
        })
        .unwrap_or_default();
    json!({ "id": id, "name": name, "stops": stops })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_read_as_wled_s_own_page_reads_them() {
        // Real metadata from the house controller.
        let fairy = effect(51, "Fairytwinkle", "!,!;!,!;!;;m12=0");
        assert_eq!(fairy["sliders"].as_array().unwrap().len(), 2);
        assert_eq!(fairy["sliders"][0]["label"], "Speed");
        assert_eq!(fairy["colors"].as_array().unwrap().len(), 2);
        assert_eq!(fairy["colors"][1]["label"], "Background");
        assert_eq!(fairy["palette"], "Palette");
        assert_eq!(fairy["defaults"]["mapping_1d2d"], 0);

        let scanner = effect(
            40,
            "Scanner",
            "!,Trail,Delay,,,Dual,Bi-delay;!,!,!;!;;m12=0,c1=0",
        );
        let labels: Vec<&str> = scanner["sliders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["Speed", "Trail", "Delay"]);
        assert_eq!(scanner["sliders"][2]["key"], "custom1");
        let opts: Vec<&str> = scanner["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["label"].as_str().unwrap())
            .collect();
        assert_eq!(opts, ["Dual", "Bi-delay"]);
        assert_eq!(scanner["options"][1]["key"], "option2");

        let dual = effect(52, "Running Dual", "!,Wave width;L,!,R;!");
        let cols: Vec<&str> = dual["colors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["label"].as_str().unwrap())
            .collect();
        assert_eq!(cols, ["L", "Background", "R"]);

        let tri = effect(56, "Tri Fade", "!;1,2,3;!");
        assert_eq!(tri["colors"][2]["label"], "Colour 3");

        let rainbow = effect(9, "Rainbow", "!,Size;;!");
        assert!(rainbow["colors"].as_array().unwrap().is_empty());

        let fire = effect(
            66,
            "Fire 2012",
            "Cooling,Spark rate,,2D Blur,Boost;;!;1;pal=35,sx=64,ix=160,m12=1,c2=128",
        );
        // Its first custom slider is unused: "2D Blur" is custom 2.
        assert_eq!(fire["sliders"].as_array().unwrap().len(), 4);
        assert_eq!(fire["sliders"][2]["key"], "custom2");
        assert_eq!(fire["sliders"][2]["label"], "2D Blur");
        assert_eq!(fire["sliders"][3]["key"], "custom3");
        assert_eq!(fire["defaults"]["palette_id"], 35);
        assert_eq!(fire["one_d"], true);

        let pixels = effect(128, "Pixels", "Fade rate,# of pixels;!,!;!;1v;m12=0,si=0");
        assert_eq!(pixels["sound"], true);

        let copy = effect(77, "Copy Segment", ",Color shift,Lighten,Brighten,ID,Axis(2D),FullStack(last frame);;;12;ix=0,c1=0,c2=0,c3=0");
        assert!(copy["palette"].is_null());
        assert_eq!(copy["two_d"], true);
        assert_eq!(copy["sliders"][3]["max"], 31);
    }

    #[test]
    fn an_effect_with_no_metadata_shows_what_wled_shows() {
        let solid = effect(0, "Solid", "");
        assert_eq!(solid["sliders"].as_array().unwrap().len(), 2);
        assert_eq!(solid["colors"].as_array().unwrap().len(), 3);
        assert_eq!(solid["palette"], "Palette");
        // A newer one shows all five sliders.
        assert_eq!(effect(130, "X", "")["sliders"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn reserved_effects_are_left_out_and_ids_kept() {
        let names = vec![json!("Solid"), json!("RSVD"), json!("Breathe")];
        let meta = vec![json!(""), json!(""), json!("!;!,!;!;01")];
        let list = effects(&names, &meta);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1]["id"], 2);
    }

    #[test]
    fn a_palette_s_stops_are_read() {
        let p = palette(
            9,
            "Ocean",
            Some(&json!([[0, 25, 25, 112], [255, 0, 255, 255]])),
        );
        assert_eq!(p["stops"][0], json!({"at": 0.0, "color": "#191970"}));
        assert_eq!(p["stops"][1]["at"], 1.0);
        let c = palette(3, "* Colors 1&2", Some(&json!(["c1", "c1", "c2", "c2"])));
        assert_eq!(c["stops"][2], json!({"slot": 1}));
        let r = palette(1, "* Random Cycle", Some(&json!(["r", "r"])));
        assert_eq!(r["stops"][0], json!({"random": true}));
    }
}
