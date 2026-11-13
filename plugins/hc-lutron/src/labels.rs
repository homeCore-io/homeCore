//! Names the household gives a keypad's buttons.
//!
//! The engraving is the installer's: kept in the plugin's settings
//! (`button_names` on a device row) and filled in by DbXML discovery. A
//! household often calls a button something else, "Movie" where the keypad
//! says "Scene 3", and should not need the settings page to say so.
//! `set_button_name` keeps the name in the plugin's learned state, which
//! homeCore stores and hands back on every start. One key per button, so
//! naming one button never overwrites another.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Map, Value};

/// Button number → the household's name for it.
pub type Labels = BTreeMap<u32, String>;

const PREFIX: &str = "button_name:";

/// Longest name kept. A keypad button is a few words at most; this keeps a
/// pasted paragraph out of every picker.
pub const MAX_LEN: usize = 64;

/// The learned-state key for one button: `button_name:lutron_12:3`.
pub fn key(hc_id: &str, button: u32) -> String {
    format!("{PREFIX}{hc_id}:{button}")
}

/// Every device's names, from the learned-state document.
pub fn from_doc(doc: &Value) -> HashMap<String, Labels> {
    let mut out: HashMap<String, Labels> = HashMap::new();
    let Some(obj) = doc.as_object() else {
        return out;
    };
    for (k, v) in obj {
        let Some(rest) = k.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((hc_id, button)) = rest.rsplit_once(':') else {
            continue;
        };
        let (Ok(button), Some(name)) = (button.parse::<u32>(), v.as_str().and_then(clean)) else {
            continue;
        };
        out.entry(hc_id.to_string())
            .or_default()
            .insert(button, name);
    }
    out
}

/// The learned-state change that names one button, or clears its name
/// (`None`) so it reads as its engraving again.
pub fn delta(hc_id: &str, button: u32, name: Option<&str>) -> Value {
    let mut m = Map::new();
    m.insert(key(hc_id, button), name.map_or(Value::Null, |n| json!(n)));
    Value::Object(m)
}

/// A name as kept: trimmed and capped, `None` when nothing is left.
pub fn clean(name: &str) -> Option<String> {
    let t = name.trim();
    if t.is_empty() {
        return None;
    }
    Some(
        t.chars()
            .take(MAX_LEN)
            .collect::<String>()
            .trim_end()
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_each_devices_names_and_ignores_the_rest() {
        let doc = json!({
            "button_name:lutron_12:1": "Movie",
            "button_name:lutron_12:2": "  Dinner ",
            "button_name:lutron_40:3": "Porch",
            "button_name:lutron_12:x": "bad number",
            "button_name:lutron_12:4": "   ",
            "button_name:lutron_12:5": 7,
            "something_else": "kept by someone else",
        });
        let all = from_doc(&doc);
        assert_eq!(all.len(), 2);
        let k = &all["lutron_12"];
        assert_eq!(k.len(), 2);
        assert_eq!(k[&1], "Movie");
        assert_eq!(k[&2], "Dinner");
        assert_eq!(all["lutron_40"][&3], "Porch");
    }

    #[test]
    fn a_delta_sets_or_clears_one_button() {
        assert_eq!(
            delta("lutron_12", 3, Some("Movie")),
            json!({ "button_name:lutron_12:3": "Movie" })
        );
        assert_eq!(
            delta("lutron_12", 3, None),
            json!({ "button_name:lutron_12:3": null })
        );
    }

    #[test]
    fn names_are_trimmed_and_capped() {
        assert_eq!(clean("  Movie  ").as_deref(), Some("Movie"));
        assert_eq!(clean(""), None);
        assert_eq!(clean(" \t "), None);
        let long = "x".repeat(MAX_LEN + 10);
        assert_eq!(clean(&long).unwrap().chars().count(), MAX_LEN);
    }

    #[test]
    fn an_empty_or_odd_document_names_nothing() {
        assert!(from_doc(&json!(null)).is_empty());
        assert!(from_doc(&json!([])).is_empty());
        assert!(from_doc(&json!({})).is_empty());
    }
}
