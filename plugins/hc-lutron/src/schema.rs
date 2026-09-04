//! What a keypad, Pico or VCRX reports and accepts, declared for any client.
//!
//! Buttons were the long-standing gap: a Pico fires `~DEVICE` press events all
//! day, but nothing ever said *which* buttons it has. `dbxml.rs` has parsed the
//! list since discovery was written — it was just kept for LED queries and
//! never published, so a rule editor asking "which button?" had nothing to
//! offer but a number box.
//!
//! `available_buttons` fixes that, and the action declarations give the two
//! commands these devices accept a typed form instead of a raw JSON payload.

use plugin_sdk_rs::device_actions::{with_actions, Action, Param, Source};
use plugin_sdk_rs::types::schema::{AttributeKind, AttributeSchema, DeviceSchema};
use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind};

fn ro(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
        // Button state is reported, never written: a rule presses a button
        // with `press_button`, it does not assign "press" to an attribute.
        writable: false,
        display_name: Some(display.to_string()),
        ..Default::default()
    }
}

/// A writable attribute, with its range where it has one.
fn rw(kind: AttributeKind, display: &str, unit: Option<&str>) -> AttributeSchema {
    AttributeSchema {
        kind,
        writable: true,
        display_name: Some(display.to_string()),
        unit: unit.map(str::to_string),
        ..Default::default()
    }
}

/// **What a dimmer is, said out loud.**
///
/// `translate_output_state` has always published `brightness_pct` and
/// `translate_command` has always accepted it — the level round trip works and
/// always has. What was missing was the *declaration*, and a client that will
/// not offer a control the plugin has not promised is left with nothing to
/// draw: every Lutron dimmer in the house showed a brightness it could read
/// and a slider it could not move. John, on the Office's Overhead: *"The
/// brightness shows 25% but there's no visible bar."*
///
/// Only the kinds that really take one. A Switch is on or off, a Pico accepts
/// nothing at all, and declaring a level for either would be the opposite
/// mistake.
fn output_attributes(kind: &DeviceKind) -> Option<Vec<(String, AttributeSchema)>> {
    match kind {
        DeviceKind::Dimmer => Some(vec![
            ("on".into(), rw(AttributeKind::Bool, "Power", None)),
            (
                "brightness_pct".into(),
                AttributeSchema {
                    min: Some(0.0),
                    max: Some(100.0),
                    step: Some(1.0),
                    ..rw(AttributeKind::Integer, "Brightness", Some("%"))
                },
            ),
        ]),
        DeviceKind::Switch => Some(vec![("on".into(), rw(AttributeKind::Bool, "Power", None))]),
        _ => None,
    }
}

/// The schema for one device, or `None` for kinds with nothing to declare.
///
/// A Pico gets one: it accepts no commands at all (truly read-only hardware),
/// but its button catalogue is exactly what a trigger picker needs.
pub fn device_schema_json(cfg: &DeviceConfig) -> Option<Value> {
    if let Some(attrs) = output_attributes(&cfg.kind) {
        let schema = DeviceSchema {
            attributes: attrs.into_iter().collect(),
            ..Default::default()
        };
        return serde_json::to_value(&schema).ok();
    }

    match cfg.kind {
        DeviceKind::Keypad | DeviceKind::Vcrx | DeviceKind::Pico => {}
        _ => return None,
    }

    let mut attrs = std::collections::HashMap::new();
    if !cfg.all_buttons.is_empty() {
        attrs.insert(
            "available_buttons".into(),
            ro(AttributeKind::Json, "Buttons"),
        );
    }
    // Every button gets an attribute, whether it has ever been pressed or not.
    // A button only appears in state after someone presses it, so listing them
    // from the schema is what makes an untouched keypad show all six — and
    // they are all pressable from the UI regardless.
    for (b, label) in buttons_with_labels(cfg) {
        attrs.insert(format!("button_{b}"), ro(AttributeKind::String, &label));
    }

    let schema = DeviceSchema {
        attributes: attrs,
        ..Default::default()
    };

    // A Pico accepts nothing — `translate_command` returns an empty command
    // list for it, so declaring an action would be a control that does nothing.
    if cfg.kind == DeviceKind::Pico {
        return Some(with_actions(&schema, vec![]));
    }

    let button_param = || {
        let p = Param::int("button").label("Button").required();
        if cfg.all_buttons.is_empty() {
            p
        } else {
            // The catalogue convention: the list is the device's own, so a
            // client offers this keypad's actual buttons — by engraving, since
            // that is what is printed on the wall, while still sending the
            // number the protocol wants.
            p.options_from(
                Source::attribute("available_buttons")
                    .label_key("name")
                    .value_key("number"),
            )
        }
    };

    Some(with_actions(
        &schema,
        vec![
            Action::new("press_button")
                .label("Press a button")
                .category("Buttons")
                .icon("remote")
                .sentence("press button {button} on {device}")
                .param(button_param()),
            Action::new("set_led")
                .label("Set a button LED")
                .category("Buttons")
                .icon("lightbulb")
                .description("LED component is the button number + 80; the offset is applied here.")
                .sentence("set the LED of button {button} on {device} to {state}")
                .param(button_param())
                .param(
                    Param::enum_("state")
                        .label("LED")
                        .required()
                        .labelled_options([
                            ("0", "Off"),
                            ("1", "On"),
                            ("2", "Slow flash"),
                            ("3", "Rapid flash"),
                        ])
                        .default("1"),
                ),
        ],
    ))
}

/// Number and engraving for every button, with a sensible name where Lutron
/// engraved nothing.
pub fn buttons_with_labels(cfg: &DeviceConfig) -> Vec<(u32, String)> {
    cfg.all_buttons
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let engraved = cfg.button_names.get(i).map(String::as_str).unwrap_or("");
            let label = if engraved.is_empty() {
                format!("Button {b}")
            } else {
                engraved.to_string()
            };
            (*b, label)
        })
        .collect()
}

/// The state a device publishes about its own buttons, merged into its first
/// state publish. Empty when discovery never learned them.
///
/// Objects rather than bare numbers so a client can show the engraving and
/// send the number — `label_key` / `value_key` on the action parameter.
pub fn button_catalogue(cfg: &DeviceConfig) -> Option<Value> {
    if cfg.all_buttons.is_empty() {
        return None;
    }
    let list: Vec<Value> = buttons_with_labels(cfg)
        .into_iter()
        .map(|(number, name)| serde_json::json!({ "number": number, "name": name }))
        .collect();
    Some(serde_json::json!({ "available_buttons": list }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: DeviceKind, all_buttons: Vec<u32>) -> DeviceConfig {
        DeviceConfig {
            integration_id: 1,
            name: "Test".into(),
            kind,
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
            all_buttons,
            button_names: vec![],
            ccis: vec![],
        }
    }

    /// The whole point: a Pico has no LEDs, so the old `buttons` list is empty
    /// for it, and it still has buttons a person presses.
    #[test]
    fn a_pico_publishes_its_buttons_and_no_actions() {
        let v = device_schema_json(&cfg(DeviceKind::Pico, vec![2, 3, 4, 5, 6])).unwrap();
        assert!(v["attributes"]["available_buttons"].is_object());
        assert!(v["attributes"]["button_2"].is_object());
        assert!(
            v.get("actions").is_none(),
            "a Pico accepts no commands; declaring one would be a dead control"
        );
    }

    #[test]
    fn a_keypad_declares_press_and_led_against_its_own_buttons() {
        let v = device_schema_json(&cfg(DeviceKind::Keypad, vec![1, 2, 3])).unwrap();
        let actions = v["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);

        let press = &actions[0];
        assert_eq!(press["id"], "press_button");
        assert_eq!(
            press["params"][0]["options_from"]["attribute"]["attribute"],
            "available_buttons"
        );

        let led = &actions[1];
        assert_eq!(led["id"], "set_led");
        assert_eq!(led["params"][1]["options"][2]["label"], "Slow flash");
    }

    /// A config written before discovery learned buttons must still work — it
    /// simply offers a number rather than a list.
    #[test]
    fn no_catalogue_leaves_a_plain_number_param() {
        let v = device_schema_json(&cfg(DeviceKind::Keypad, vec![])).unwrap();
        let press = &v["actions"][0];
        assert_eq!(press["params"][0]["kind"], "int");
        assert!(press["params"][0].get("options_from").is_none());
        assert!(v["attributes"].as_object().unwrap().is_empty());
    }

    #[test]
    fn a_dimmer_has_no_button_schema() {
        // It has a schema now — its level, which it always accepted and never
        // declared — but nothing about buttons, which is what this has always
        // been about. A dimmer has no buttons and takes no press.
        let v = device_schema_json(&cfg(DeviceKind::Dimmer, vec![])).unwrap();
        let attrs = v["attributes"].as_object().unwrap();
        assert!(!attrs.keys().any(|k| k.starts_with("button")));
        assert!(attrs.get("available_buttons").is_none());
        assert!(v.get("actions").is_none());
    }

    #[test]
    fn the_catalogue_is_omitted_when_unknown() {
        assert!(button_catalogue(&cfg(DeviceKind::Pico, vec![])).is_none());
    }

    /// Engravings are what is printed on the wall, so they are what a person
    /// picks by — while the number is still what goes on the wire.
    #[test]
    fn the_catalogue_carries_engravings() {
        let mut c = cfg(DeviceKind::Keypad, vec![1, 2, 3]);
        c.button_names = vec!["Overhead On".into(), String::new(), "All Off".into()];
        let v = button_catalogue(&c).unwrap();
        assert_eq!(
            v["available_buttons"],
            serde_json::json!([
                { "number": 1, "name": "Overhead On" },
                // No engraving — named for its number rather than left blank.
                { "number": 2, "name": "Button 2" },
                { "number": 3, "name": "All Off" },
            ])
        );
        // And the attribute a device sheet renders carries it too.
        let schema = device_schema_json(&c).unwrap();
        assert_eq!(
            schema["attributes"]["button_1"]["display_name"],
            "Overhead On"
        );
        assert_eq!(schema["attributes"]["button_2"]["display_name"], "Button 2");
    }

    /// The picker must send a number while showing the engraving.
    #[test]
    fn the_button_param_maps_label_to_value() {
        let mut c = cfg(DeviceKind::Keypad, vec![1]);
        c.button_names = vec!["Overhead On".into()];
        let v = device_schema_json(&c).unwrap();
        let src = &v["actions"][0]["params"][0]["options_from"]["attribute"];
        assert_eq!(src["label_key"], "name");
        assert_eq!(src["value_key"], "number");
    }
}

#[cfg(test)]
mod output_schema_tests {
    use super::*;
    use crate::config::DeviceConfig;

    fn cfg(kind: DeviceKind) -> DeviceConfig {
        DeviceConfig {
            integration_id: 1,
            name: "Test".into(),
            kind,
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
            all_buttons: vec![],
            button_names: vec![],
            ccis: vec![],
        }
    }

    /// **A dimmer that never said it could be dimmed.**
    ///
    /// The level round trip has always worked — `translate_output_state`
    /// publishes `brightness_pct` and `translate_command` accepts it — but
    /// nothing declared it, and a client that refuses to offer a control the
    /// plugin has not promised had nothing to draw. Every Lutron dimmer in the
    /// house showed a brightness it could read and a slider it could not move.
    #[test]
    fn a_dimmer_declares_the_level_it_already_takes() {
        let v = device_schema_json(&cfg(DeviceKind::Dimmer)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        let b = &attrs["brightness_pct"];
        assert_eq!(b["writable"], true);
        assert_eq!(b["min"], 0.0);
        assert_eq!(b["max"], 100.0);
        assert_eq!(b["unit"], "%");
        assert_eq!(attrs["on"]["writable"], true);
    }

    #[test]
    fn a_switch_declares_power_and_no_level() {
        // Declaring a level for something that is on or off would be the
        // opposite mistake: a slider that cannot mean anything.
        let v = device_schema_json(&cfg(DeviceKind::Switch)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        assert_eq!(attrs["on"]["writable"], true);
        assert!(!attrs.contains_key("brightness_pct"));
    }

    #[test]
    fn a_shade_is_left_alone() {
        // Phase 2, and a control declared before the command path takes it is
        // exactly the failure this fixes, pointed the other way.
        assert!(device_schema_json(&cfg(DeviceKind::Shade)).is_none());
    }
}
