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
use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind, SceneConfig};
use crate::lip::protocol::led_component_for_phantom_button;

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
        // The ladder `translate_output_state` reports and `translate_command`
        // accepts, in both of the forms it accepts it: the named speed a
        // person picks, and the percentage the zone level really is.
        DeviceKind::FanControl => Some(vec![
            ("on".into(), rw(AttributeKind::Bool, "Power", None)),
            (
                "speed".into(),
                AttributeSchema {
                    options: Some(
                        FAN_SPEEDS
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect::<Vec<_>>(),
                    ),
                    ..rw(AttributeKind::Enum, "Speed", None)
                },
            ),
            (
                "speed_pct".into(),
                AttributeSchema {
                    min: Some(0.0),
                    max: Some(100.0),
                    step: Some(1.0),
                    ..rw(AttributeKind::Integer, "Speed", Some("%"))
                },
            ),
        ]),
        // A shade reports where it is; raising, lowering and stopping are
        // verbs, and are declared as actions rather than invented attributes.
        DeviceKind::Shade => Some(vec![(
            "position".into(),
            AttributeSchema {
                min: Some(0.0),
                max: Some(100.0),
                step: Some(1.0),
                ..rw(AttributeKind::Integer, "Position", Some("%"))
            },
        )]),
        _ => None,
    }
}

/// The speeds a Maestro fan controller has, in the order a person reads them.
///
/// One list, used for the declaration and by the tests that check it against
/// `translate_command` — a speed that is offered but not accepted is a control
/// that does nothing.
pub const FAN_SPEEDS: [&str; 5] = ["off", "low", "medium", "medium-high", "high"];

/// The momentary verbs an output accepts that are not attribute writes.
fn output_actions(kind: &DeviceKind) -> Vec<Action> {
    match kind {
        DeviceKind::Shade => vec![
            Action::new("raise")
                .label("Raise the shade")
                .category("Movement")
                .icon("arrow-up")
                .sentence("raise {device}"),
            Action::new("lower")
                .label("Lower the shade")
                .category("Movement")
                .icon("arrow-down")
                .sentence("lower {device}"),
            Action::new("stop")
                .label("Stop the shade")
                .category("Movement")
                .icon("stop")
                .sentence("stop {device}"),
        ],
        _ => vec![],
    }
}

/// Activating is the whole of what a scene — phantom or contact-closure — does.
fn activate_action() -> Action {
    Action::new("activate")
        .label("Activate the scene")
        .category("Scenes")
        .icon("scene")
        .sentence("activate {device}")
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
        let actions = output_actions(&cfg.kind);
        return if actions.is_empty() {
            serde_json::to_value(&schema).ok()
        } else {
            Some(with_actions(&schema, actions))
        };
    }

    // A pulsed CCO is published as a scene because that is how it behaves: it
    // takes `activate`, never latches, and — the Integration Guide is explicit
    // that momentary outputs must not be queried — reports nothing at all. An
    // empty attribute set is the honest declaration, not a missing one.
    if cfg.kind == DeviceKind::CcoPulsed {
        return Some(with_actions(
            &DeviceSchema::default(),
            vec![activate_action()],
        ));
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

/// **What a phantom scene is, including whether it can be read back.**
///
/// Every scene takes `activate`. What differs between them is whether anything
/// comes back: a scene's state is its phantom button's LED, and RadioRA 2 only
/// reports one where the programming assigned one — an unassigned button
/// answers 255, which is not a state at all. So the house has scenes that
/// genuinely report on/off and scenes whose `on` is only what this plugin
/// optimistically wrote when it pressed the button, and a client that cannot
/// tell them apart shows a confident toggle for both.
///
/// `reports_state` is learned, not configured: it turns true the first time a
/// real LED state arrives for the scene, and the schema is republished. Until
/// then the scene declares the action and nothing to read.
pub fn scene_schema_json(reports_state: bool) -> Value {
    let mut attrs = std::collections::HashMap::new();
    // Which phantom button this scene is, and — when there is one — which LED
    // reports it. Declared and published so "supports status" is something a
    // client can *show*, with the plumbing behind it, rather than a fact it
    // has to infer from an attribute being absent. It is also the first thing
    // anyone needs when a scene will not report: the button is right, the LED
    // was never assigned.
    attrs.insert(
        "phantom_button".to_string(),
        AttributeSchema {
            category: Some(AttributeCategory::Diagnostic),
            ..ro(AttributeKind::Integer, "Phantom button")
        },
    );
    if reports_state {
        attrs.insert(
            "led_component".to_string(),
            AttributeSchema {
                category: Some(AttributeCategory::Diagnostic),
                ..ro(AttributeKind::Integer, "Status LED")
            },
        );
        attrs.insert(
            "on".to_string(),
            AttributeSchema {
                // Written by pressing the button, never by assigning to `on` —
                // `handle_homecore_command` dispatches a scene on `activate`
                // alone, so a writable `on` would be a control that does
                // nothing.
                states: Some(BoolStates {
                    when_true: StateLabel::verbed("active", "activates"),
                    when_false: StateLabel::verbed("inactive", "deactivates"),
                }),
                ..ro(AttributeKind::Bool, "Active")
            },
        );
    }
    let schema = DeviceSchema {
        attributes: attrs,
        ..Default::default()
    };
    with_actions(&schema, vec![activate_action()])
}

/// **What a timeclock event is: a switch that can also be fired by hand.**
///
/// `enabled` is writable because enabling one is what an operator does with
/// it, and it matches what the device publishes — the wire key was `enable`
/// while the state said `enabled`, so a client echoing back what it read was
/// ignored. Both spellings are accepted now.
///
/// The value is optimistic: RA2 has no query for an individual event's
/// enabled state, so what is published is what this plugin last sent. There is
/// no better source, and saying nothing would leave a client unable to show
/// the switch at all.
pub fn timeclock_schema_json() -> Value {
    let mut attrs = std::collections::HashMap::new();
    attrs.insert(
        "enabled".to_string(),
        AttributeSchema {
            states: Some(BoolStates {
                when_true: StateLabel::verbed("enabled", "is enabled"),
                when_false: StateLabel::verbed("disabled", "is disabled"),
            }),
            ..rw(AttributeKind::Bool, "Enabled", None)
        },
    );
    let schema = DeviceSchema {
        attributes: attrs,
        ..Default::default()
    };
    with_actions(
        &schema,
        vec![Action::new("execute")
            .label("Run the event now")
            .category("Timeclock")
            .icon("play")
            .description("Fires the event once, for testing. Does not change its schedule.")
            .sentence("run {device} now")],
    )
}

/// What a scene publishes about its own plumbing, to fill the attributes
/// [`scene_schema_json`] declares.
///
/// `led_component` appears only once the scene is known to report: an
/// unassigned phantom button has no LED to name.
pub fn scene_plumbing_state(cfg: &SceneConfig, reports_state: bool) -> Value {
    let mut state = serde_json::json!({ "phantom_button": cfg.button_component });
    if reports_state {
        state["led_component"] =
            serde_json::json!(led_component_for_phantom_button(cfg.button_component));
    }
    state
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
    use crate::devices::DeviceEntry;
    use serde_json::json;

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

    /// The mirror of the promise: every speed offered is a speed the command
    /// path really takes. A picker listing "turbo" would be a control that
    /// does nothing.
    #[test]
    fn a_fan_declares_every_speed_it_accepts_and_no_others() {
        let v = device_schema_json(&cfg(DeviceKind::FanControl)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        let offered: Vec<String> = attrs["speed"]["options"]
            .as_array()
            .expect("options")
            .iter()
            .map(|o| o.as_str().unwrap().to_string())
            .collect();
        assert_eq!(offered, FAN_SPEEDS);

        let dev = DeviceEntry::new(cfg(DeviceKind::FanControl));
        for speed in FAN_SPEEDS {
            assert!(
                !dev.translate_command(&json!({ "speed": speed }), 0.0)
                    .is_empty(),
                "{speed} is offered but not accepted"
            );
        }
        assert!(dev
            .translate_command(&json!({ "speed": "turbo" }), 0.0)
            .is_empty());
    }

    #[test]
    fn a_fan_declares_the_percentage_and_the_power_it_also_takes() {
        let v = device_schema_json(&cfg(DeviceKind::FanControl)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        assert_eq!(attrs["speed_pct"]["max"], 100.0);
        assert_eq!(attrs["speed_pct"]["unit"], "%");
        assert_eq!(attrs["on"]["writable"], true);

        let dev = DeviceEntry::new(cfg(DeviceKind::FanControl));
        assert!(!dev
            .translate_command(&json!({ "speed_pct": 40 }), 0.0)
            .is_empty());
        assert!(!dev
            .translate_command(&json!({ "on": true }), 0.0)
            .is_empty());
    }

    /// A shade's position is an attribute; raising and lowering are verbs.
    /// Declaring the verbs as attributes would put three checkboxes on a
    /// client that stay checked.
    #[test]
    fn a_shade_declares_its_position_and_its_three_verbs() {
        let v = device_schema_json(&cfg(DeviceKind::Shade)).expect("a schema");
        assert_eq!(v["attributes"]["position"]["writable"], true);
        assert_eq!(v["attributes"]["position"]["unit"], "%");

        let ids: Vec<&str> = v["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["raise", "lower", "stop"]);

        let dev = DeviceEntry::new(cfg(DeviceKind::Shade));
        for verb in ids {
            assert!(
                !dev.translate_command(&json!({ verb: true }), 0.0)
                    .is_empty(),
                "{verb} is declared but not accepted"
            );
        }
    }

    /// A momentary output has nothing to read — the Integration Guide forbids
    /// querying it — so it declares an action and an empty attribute set,
    /// which is a different statement from declaring nothing at all.
    #[test]
    fn a_pulsed_cco_offers_activation_and_reads_nothing() {
        let v = device_schema_json(&cfg(DeviceKind::CcoPulsed)).expect("a schema");
        assert!(v["attributes"].as_object().expect("attributes").is_empty());
        assert_eq!(v["actions"][0]["id"], "activate");

        let dev = DeviceEntry::new(cfg(DeviceKind::CcoPulsed));
        assert!(!dev
            .translate_command(&json!({ "activate": true }), 0.0)
            .is_empty());
    }
}

#[cfg(test)]
mod timeclock_schema_tests {
    use super::*;

    /// The state said `enabled` and the command wanted `enable`, so a client
    /// echoing back the attribute it read was silently ignored. The schema
    /// declares `enabled` writable, which is now true.
    #[test]
    fn a_timeclock_declares_the_switch_it_publishes() {
        let v = timeclock_schema_json();
        let enabled = &v["attributes"]["enabled"];
        assert_eq!(enabled["writable"], true);
        assert_eq!(enabled["states"]["when_true"]["label"], "enabled");
    }

    #[test]
    fn a_timeclock_can_be_fired_by_hand() {
        let v = timeclock_schema_json();
        assert_eq!(v["actions"][0]["id"], "execute");
    }
}

#[cfg(test)]
mod scene_schema_tests {
    use super::*;

    /// **The distinction the house actually has.** A phantom button with no
    /// LED assigned answers 255 — no state — and this plugin's `on` for that
    /// scene is only what it optimistically wrote when it pressed the button.
    /// Declaring `on` there would give a client a confident toggle reporting a
    /// value nothing confirms.
    #[test]
    fn a_scene_declares_no_state_until_its_led_reports() {
        let v = scene_schema_json(false);
        let attrs = v["attributes"].as_object().expect("attributes");
        assert!(
            !attrs.contains_key("on"),
            "nothing confirms it, so nothing claims it"
        );
        assert_eq!(v["actions"][0]["id"], "activate");
    }

    fn cfg() -> SceneConfig {
        SceneConfig {
            name: "Deck On".into(),
            main_repeater_id: 1,
            button_component: 3,
        }
    }

    /// "Supports status" is something a client can show, not infer: the scene
    /// names the button it is and the LED that reports it.
    #[test]
    fn a_reporting_scene_names_the_led_behind_its_status() {
        let v = scene_schema_json(true);
        assert_eq!(v["attributes"]["led_component"]["category"], "diagnostic");

        let state = scene_plumbing_state(&cfg(), true);
        assert_eq!(state["phantom_button"], 3);
        assert_eq!(state["led_component"], 103); // button + 100
    }

    /// A scene with no LED still says which button it is — that is the first
    /// thing anyone needs when asking why it never reports.
    #[test]
    fn a_scene_without_one_still_names_its_button() {
        let v = scene_schema_json(false);
        let attrs = v["attributes"].as_object().expect("attributes");
        assert!(attrs.contains_key("phantom_button"));
        assert!(!attrs.contains_key("led_component"));

        let state = scene_plumbing_state(&cfg(), false);
        assert_eq!(state["phantom_button"], 3);
        assert!(state.get("led_component").is_none());
    }

    #[test]
    fn a_scene_with_an_led_declares_what_it_reports() {
        let v = scene_schema_json(true);
        let on = &v["attributes"]["on"];
        // Activation goes through the action; assigning to `on` is not a
        // command this plugin dispatches.
        assert_eq!(on["writable"], false);
        assert_eq!(on["states"]["when_true"]["label"], "active");
        assert_eq!(on["states"]["when_false"]["label"], "inactive");
        assert_eq!(v["actions"][0]["id"], "activate");
    }
}
