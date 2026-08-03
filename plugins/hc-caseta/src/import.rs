//! Parse the Caséta **integration report** into config rows.
//!
//! The Lutron app emails this JSON (Settings → Advanced → Integration → Send
//! Integration Report). It is the only machine-readable inventory Caséta
//! offers — the bridge itself cannot be queried over LIP — so pasting it beats
//! copying integration IDs by hand.
//!
//! Shape, trimmed to what matters:
//!
//! ```json
//! { "LIPIdList": {
//!     "Zones":   [ { "ID": 2, "Name": "…", "Area": { "Name": "…" } } ],
//!     "Devices": [ { "ID": 6, "Name": "Pico", "Buttons": [ { "Number": 2 } ] } ] } }
//! ```
//!
//! `Zones` are controllable loads; `Devices` are things with buttons. The
//! Smart Bridge is itself a device (ID 1) whose hundred "buttons" are the
//! phantom buttons behind scenes.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The bridge's own integration ID. Fixed by Caséta — its buttons are scenes,
/// not a remote's.
const SMART_BRIDGE_ID: u64 = 1;

/// Rows to append, plus a note on what was deliberately left out.
#[derive(Debug, Default)]
pub struct Import {
    pub devices: Vec<Value>,
    pub scenes: Vec<Value>,
    pub skipped: Vec<String>,
}

impl Import {
    /// One line for the operator: what landed, what still needs a decision,
    /// and what did not land at all.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "Imported {} device{}, {} scene{}.",
            self.devices.len(),
            plural(self.devices.len()),
            self.scenes.len(),
            plural(self.scenes.len()),
        );

        // The count that actually explains what happens next.
        //
        // A zone arrives without a kind and is skipped at startup until one is
        // chosen, which is correct but invisible: the plugin warns to its own
        // log, and an operator watching the device list just sees most of the
        // import missing. Saying it here puts the number where the import
        // happened.
        let undecided = self
            .devices
            .iter()
            .filter(|d| d.get("kind").is_none_or(Value::is_null))
            .count();
        if undecided > 0 {
            s.push_str(&format!(
                " {undecided} need{} a Kind before {} appear as device{} — the report does not \
                 say whether a zone is a dimmer, a switch or a shade.",
                if undecided == 1 { "s" } else { "" },
                if undecided == 1 {
                    "it will"
                } else {
                    "they will"
                },
                plural(undecided),
            ));
        }

        for note in &self.skipped {
            s.push(' ');
            s.push_str(note);
        }
        s
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// An unprogrammed phantom button is named `Button 47` by the app. Those are
/// placeholders for the 100 slots, not scenes anyone made.
fn is_placeholder_button(name: &str) -> bool {
    name.strip_prefix("Button ")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn area_of(node: &Value) -> Option<String> {
    node.get("Area")?
        .get("Name")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse an integration report into rows for `[[devices]]` and `[[scenes]]`.
///
/// Deliberately does **not** guess `kind` for a zone: the report carries no
/// load type, and driving a switch as a dimmer sends levels it may not honour.
/// Rows arrive with `kind` unset so the operator picks it.
pub fn parse_integration_report(text: &str) -> Result<Import> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("Paste the integration report first."));
    }
    let root: Value = serde_json::from_str(text)
        .map_err(|e| anyhow!("That does not parse as JSON ({e}). Paste the whole report."))?;

    let list = root
        .get("LIPIdList")
        .ok_or_else(|| anyhow!("No `LIPIdList` — is this a Caséta integration report?"))?;

    let mut out = Import::default();

    // Zones are the controllable loads.
    for zone in list
        .get("Zones")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
    {
        let (Some(id), Some(name)) = (
            zone.get("ID").and_then(Value::as_u64),
            zone.get("Name").and_then(Value::as_str),
        ) else {
            continue;
        };
        let mut row = json!({ "integration_id": id, "name": name });
        if let Some(area) = area_of(zone) {
            row["area"] = json!(area);
        }
        out.devices.push(row);
    }

    // Devices are the things with buttons: Picos, and the bridge itself.
    for dev in list
        .get("Devices")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
    {
        let Some(id) = dev.get("ID").and_then(Value::as_u64) else {
            continue;
        };
        let name = dev.get("Name").and_then(Value::as_str).unwrap_or("");
        let buttons = dev.get("Buttons").and_then(Value::as_array);

        if id == SMART_BRIDGE_ID {
            let all = buttons.map_or(0, Vec::len);
            for button in buttons.unwrap_or(&vec![]) {
                let (Some(number), Some(label)) = (
                    button.get("Number").and_then(Value::as_u64),
                    button.get("Name").and_then(Value::as_str),
                ) else {
                    continue;
                };
                if is_placeholder_button(label) {
                    continue;
                }
                out.scenes.push(json!({
                    "name": label,
                    "button_component": number,
                    "bridge_id": id,
                }));
            }
            let unnamed = all - out.scenes.len();
            if unnamed > 0 {
                out.skipped.push(format!(
                    "Ignored {unnamed} unprogrammed phantom button{} on the bridge.",
                    plural(unnamed)
                ));
            }
            continue;
        }

        // A non-bridge device with no buttons used to fall off the end of this
        // loop and vanish — no row, and not even a line in `skipped`, so the
        // report and the imported list simply disagreed with no explanation.
        // Occupancy sensors arrive exactly this way, and `occupancy_sensor` is
        // a kind this plugin supports, so import could never produce one.
        //
        // Imported as a kind-less row, like a zone: the report does not say
        // what it is either, and a row the operator can classify beats a device
        // that silently does not exist.
        if buttons.is_none_or(|b| b.is_empty()) {
            let mut row = json!({ "integration_id": id, "name": name });
            if let Some(area) = area_of(dev) {
                row["area"] = json!(area);
            }
            out.devices.push(row);
            continue;
        }

        // Anything else carrying buttons is a Pico. Unlike a zone's load type,
        // this one the report does tell us.
        {
            let mut row = json!({ "integration_id": id, "name": name, "kind": "pico" });
            if let Some(area) = area_of(dev) {
                row["area"] = json!(area);
            }
            // The report tells us exactly which buttons this Pico has, and
            // until now only their *existence* was used — to recognise a Pico —
            // while the numbers were dropped. They are what a rule editor needs
            // to offer "button 2" instead of asking for a component number.
            let numbers: Vec<u64> = buttons
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|b| b.get("Number").and_then(Value::as_u64))
                .collect();
            if !numbers.is_empty() {
                row["buttons"] = json!(numbers);
            }
            out.devices.push(row);
        }
    }

    if out.devices.is_empty() && out.scenes.is_empty() {
        return Err(anyhow!("The report contained no zones, devices or scenes."));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{"LIPIdList":{
      "Devices":[
        {"ID":1,"Name":"Smart Bridge 2","Buttons":[
          {"Name":"Solstice Lights On","Number":1},
          {"Name":"Sonos Play","Number":3},
          {"Name":"Button 5","Number":5},
          {"Name":"Button 6","Number":6}]},
        {"ID":6,"Area":{"Name":"Living Room"},"Name":"Pico","Buttons":[{"Number":2},{"Number":3}]},
        {"ID":9,"Name":"Remote 1","Buttons":[{"Number":2}]},
        {"ID":11,"Name":"Hall Sensor","Area":{"Name":"Hallway"}},
        {"ID":12,"Name":"Porch Sensor","Buttons":[]}],
      "Zones":[
        {"ID":2,"Name":"Holiday Lights 1","Area":{"Name":"Living Room"}},
        {"ID":10,"Name":"String Lights","Area":{"Name":"Outdoor"}}]}}"#;

    #[test]
    fn zones_become_devices_without_a_guessed_kind() {
        let out = parse_integration_report(REPORT).unwrap();
        let zone = &out.devices[0];
        assert_eq!(zone["integration_id"], 2);
        assert_eq!(zone["name"], "Holiday Lights 1");
        assert_eq!(zone["area"], "Living Room");
        // The report has no load type; guessing one would drive a switch as a
        // dimmer. The operator picks.
        assert!(zone.get("kind").is_none());
    }

    #[test]
    fn a_device_with_no_buttons_still_imports() {
        // These used to fall off the end of the Devices loop: no row, and not
        // even a line in `skipped`, so the report and the imported list
        // disagreed with nothing to explain it. Occupancy sensors arrive
        // exactly this way, and `occupancy_sensor` is a kind this plugin
        // supports — so import could never produce one.
        let out = parse_integration_report(REPORT).unwrap();

        let sensor = out
            .devices
            .iter()
            .find(|d| d["integration_id"] == 11)
            .expect("a buttonless device must import, not vanish");
        assert_eq!(sensor["name"], "Hall Sensor");
        assert_eq!(sensor["area"], "Hallway");
        // Kind-less like a zone: the report does not say what it is, and the
        // operator classifies it. Guessing `occupancy_sensor` would be wrong
        // for anything else that ships without buttons.
        assert!(sensor.get("kind").is_none());

        // An empty Buttons array is the same case as none at all.
        assert!(
            out.devices.iter().any(|d| d["integration_id"] == 12),
            "an empty Buttons list is not a Pico, but it is still a device"
        );
    }

    #[test]
    fn the_summary_names_the_rows_that_still_need_a_kind() {
        // The count that explains why a 6-row import shows 2 devices. Without
        // it the only notice is a warn! in the plugin's own log.
        let out = parse_integration_report(REPORT).unwrap();
        let summary = out.summary();
        let undecided = out
            .devices
            .iter()
            .filter(|d| d.get("kind").is_none_or(Value::is_null))
            .count();
        assert_eq!(undecided, 4, "2 zones + 2 buttonless devices");
        assert!(
            summary.contains(&format!("{undecided} need")),
            "summary must say how many need a Kind, got: {summary}"
        );
    }

    #[test]
    fn button_devices_become_picos_and_the_bridge_does_not() {
        let out = parse_integration_report(REPORT).unwrap();
        let picos: Vec<&Value> = out.devices.iter().filter(|d| d["kind"] == "pico").collect();
        assert_eq!(picos.len(), 2, "two Picos, and the bridge is not one");
        assert_eq!(picos[0]["integration_id"], 6);
        assert_eq!(picos[0]["area"], "Living Room");
        // No Area in the report means no area key at all, not an empty string.
        assert!(picos[1].get("area").is_none());
        assert!(
            !out.devices.iter().any(|d| d["integration_id"] == 1),
            "the Smart Bridge must never import as a device"
        );
    }

    #[test]
    fn only_named_phantom_buttons_become_scenes() {
        let out = parse_integration_report(REPORT).unwrap();
        assert_eq!(out.scenes.len(), 2);
        assert_eq!(out.scenes[0]["name"], "Solstice Lights On");
        assert_eq!(out.scenes[0]["button_component"], 1);
        assert_eq!(out.scenes[0]["bridge_id"], 1);
        assert_eq!(out.scenes[1]["name"], "Sonos Play");
        // "Button 5"/"Button 6" are unprogrammed slots, and the operator is told.
        assert!(out.summary().contains("Ignored 2 unprogrammed"));
    }

    #[test]
    fn placeholder_names_are_recognised_precisely() {
        assert!(is_placeholder_button("Button 5"));
        assert!(is_placeholder_button("Button 100"));
        // A real scene that merely starts with the word.
        assert!(!is_placeholder_button("Button by the door"));
        assert!(!is_placeholder_button("Button"));
        assert!(!is_placeholder_button("Buttons 5"));
    }

    #[test]
    fn junk_input_explains_itself() {
        assert!(parse_integration_report("   ")
            .unwrap_err()
            .to_string()
            .contains("Paste the integration report"));
        assert!(parse_integration_report("not json")
            .unwrap_err()
            .to_string()
            .contains("does not parse as JSON"));
        assert!(parse_integration_report(r#"{"something":1}"#)
            .unwrap_err()
            .to_string()
            .contains("LIPIdList"));
    }
}
