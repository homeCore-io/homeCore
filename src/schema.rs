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
        unit: None,
        min: None,
        max: None,
        step: None,
        options: None,
    }
}

/// The schema for one device, or `None` for kinds with nothing to declare.
///
/// A Pico gets one: it accepts no commands at all (truly read-only hardware),
/// but its button catalogue is exactly what a trigger picker needs.
pub fn device_schema_json(cfg: &DeviceConfig) -> Option<Value> {
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
    for b in &cfg.all_buttons {
        attrs.insert(
            format!("button_{b}"),
            ro(AttributeKind::String, &format!("Button {b}")),
        );
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
            // client offers this keypad's actual buttons.
            p.options_from(Source::attribute("available_buttons"))
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

/// The state a device publishes about its own buttons, merged into its first
/// state publish. Empty when discovery never learned them.
pub fn button_catalogue(cfg: &DeviceConfig) -> Option<Value> {
    if cfg.all_buttons.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "available_buttons": cfg.all_buttons }))
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
        assert!(device_schema_json(&cfg(DeviceKind::Dimmer, vec![])).is_none());
    }

    #[test]
    fn the_catalogue_is_omitted_when_unknown() {
        assert!(button_catalogue(&cfg(DeviceKind::Pico, vec![])).is_none());
        let v = button_catalogue(&cfg(DeviceKind::Pico, vec![2, 4])).unwrap();
        assert_eq!(v["available_buttons"], serde_json::json!([2, 4]));
    }
}
