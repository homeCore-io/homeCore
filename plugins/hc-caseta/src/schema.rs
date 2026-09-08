//! What each Caséta device reports and accepts, declared for any client.
//!
//! A Caséta Pico fires button events all day and nothing ever said which
//! buttons it has. The integration report has always listed them — `import.rs`
//! used their *existence* to recognise a Pico and then dropped the numbers —
//! so a rule editor asking "which button?" had nothing to offer.
//!
//! A Pico declares no actions: over LIP it is genuinely read-only
//! (`translate_command` produces nothing for it), and a declared action would
//! be a control that does nothing. Everything else declares exactly what
//! `translate_command` takes and `translate_output_state` reports — the two
//! are checked against each other in the tests below.

use std::collections::HashMap;

use plugin_sdk_rs::device_actions::{with_actions, Action};
use plugin_sdk_rs::types::schema::{
    AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema, StateLabel,
};
use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind};

fn ro(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
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

fn pct(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        min: Some(0.0),
        max: Some(100.0),
        step: Some(1.0),
        ..rw(kind, display, Some("%"))
    }
}

/// The speeds a Caséta fan controller has, in the order a person reads them.
///
/// The same ladder hc-lutron declares, because it is the same Maestro table.
pub const FAN_SPEEDS: [&str; 5] = ["off", "low", "medium", "medium-high", "high"];

/// What each kind reports and accepts, or `None` for the kinds handled
/// separately below.
fn output_attributes(kind: &DeviceKind) -> Option<Vec<(String, AttributeSchema)>> {
    match kind {
        DeviceKind::Dimmer => Some(vec![
            ("on".into(), rw(AttributeKind::Bool, "Power", None)),
            (
                "brightness_pct".into(),
                pct(AttributeKind::Integer, "Brightness"),
            ),
        ]),
        DeviceKind::Switch => Some(vec![("on".into(), rw(AttributeKind::Bool, "Power", None))]),
        DeviceKind::FanControl => Some(vec![
            ("on".into(), rw(AttributeKind::Bool, "Power", None)),
            (
                "speed".into(),
                AttributeSchema {
                    options: Some(
                        FAN_SPEEDS
                            .iter()
                            .copied()
                            .map(AttributeOption::from)
                            .collect(),
                    ),
                    ..rw(AttributeKind::Enum, "Speed", None)
                },
            ),
            ("speed_pct".into(), pct(AttributeKind::Integer, "Speed")),
        ]),
        DeviceKind::Shade => Some(vec![(
            "position".into(),
            pct(AttributeKind::Integer, "Position"),
        )]),
        // An occupancy sensor publishes the same reading under two names, and
        // has since before this schema existed. Both are declared: a client
        // that hides what a plugin never mentioned would otherwise show one
        // and silently drop the other.
        DeviceKind::OccupancySensor => {
            let occupancy = |display: &str| AttributeSchema {
                states: Some(BoolStates {
                    when_true: StateLabel::verbed("occupied", "detects occupancy"),
                    when_false: StateLabel::verbed("vacant", "becomes vacant"),
                }),
                ..ro(AttributeKind::Bool, display)
            };
            Some(vec![
                ("occupied".into(), occupancy("Occupied")),
                ("occupancy".into(), occupancy("Occupancy")),
            ])
        }
        DeviceKind::Pico => None,
    }
}

/// A shade's momentary verbs, which are not attribute writes.
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

/// **What a Caséta scene is: one thing to do, and nothing to read.**
///
/// Caséta has no LED reporting of any kind — unlike a RadioRA 2 phantom
/// button, whose LED is what makes its scene readable — so a scene here is
/// purely a trigger. Declaring an `on` would give a client a toggle over a
/// value nothing will ever confirm.
pub fn scene_schema_json() -> Value {
    with_actions(
        &DeviceSchema::default(),
        vec![Action::new("activate")
            .label("Activate the scene")
            .category("Scenes")
            .icon("scene")
            .sentence("activate {device}")],
    )
}

/// The schema for one device, or `None` for a row whose kind is unset.
pub fn device_schema_json(cfg: &DeviceConfig) -> Option<Value> {
    // `kind` is optional: a row whose kind is unset is skipped at startup
    // rather than taking the plugin offline, so it may be None here.
    let kind = cfg.kind.as_ref()?;

    if let Some(attrs) = output_attributes(kind) {
        let schema = DeviceSchema {
            attributes: attrs.into_iter().collect(),
            ..Default::default()
        };
        return Some(with_actions(&schema, output_actions(kind)));
    }

    if cfg.buttons.is_empty() {
        return None;
    }

    let mut attrs: HashMap<String, AttributeSchema> = HashMap::new();
    attrs.insert(
        "available_buttons".into(),
        ro(AttributeKind::Json, "Buttons"),
    );
    for b in &cfg.buttons {
        attrs.insert(
            format!("button_{b}"),
            ro(AttributeKind::String, &format!("Button {b}")),
        );
    }

    Some(with_actions(
        &DeviceSchema {
            attributes: attrs,
            ..Default::default()
        },
        vec![],
    ))
}

/// The button catalogue a Pico publishes about itself.
pub fn button_catalogue(cfg: &DeviceConfig) -> Option<Value> {
    // `kind` is optional: a row whose kind is unset is skipped at startup
    // rather than taking the plugin offline, so it may be None here.
    if cfg.kind != Some(DeviceKind::Pico) || cfg.buttons.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "available_buttons": cfg.buttons }))
}

#[cfg(test)]
mod output_schema_tests {
    use super::*;
    use crate::devices::DeviceEntry;
    use serde_json::json;

    fn cfg(kind: DeviceKind) -> DeviceConfig {
        DeviceConfig {
            integration_id: 5,
            name: "Test".into(),
            kind: Some(kind),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons: vec![],
        }
    }

    /// The mirror of the declaration: every speed offered is one the command
    /// path takes.
    #[test]
    fn a_fan_declares_every_speed_it_accepts_and_no_others() {
        let v = device_schema_json(&cfg(DeviceKind::FanControl)).expect("a schema");
        let offered: Vec<String> = v["attributes"]["speed"]["options"]
            .as_array()
            .expect("options")
            .iter()
            .map(|o| o.as_str().unwrap().to_string())
            .collect();
        assert_eq!(offered, FAN_SPEEDS);

        let dev = DeviceEntry::new(cfg(DeviceKind::FanControl)).expect("a device");
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
    fn a_dimmer_and_a_switch_declare_what_they_take() {
        let d = device_schema_json(&cfg(DeviceKind::Dimmer)).expect("a schema");
        assert_eq!(d["attributes"]["brightness_pct"]["writable"], true);
        assert_eq!(d["attributes"]["brightness_pct"]["unit"], "%");

        let s = device_schema_json(&cfg(DeviceKind::Switch)).expect("a schema");
        assert_eq!(s["attributes"]["on"]["writable"], true);
        assert!(!s["attributes"]
            .as_object()
            .unwrap()
            .contains_key("brightness_pct"));
    }

    /// Position is an attribute; raising and lowering are verbs.
    #[test]
    fn a_shade_declares_its_position_and_its_three_verbs() {
        let v = device_schema_json(&cfg(DeviceKind::Shade)).expect("a schema");
        assert_eq!(v["attributes"]["position"]["writable"], true);

        let ids: Vec<&str> = v["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["raise", "lower", "stop"]);

        let dev = DeviceEntry::new(cfg(DeviceKind::Shade)).expect("a device");
        for verb in ids {
            assert!(
                !dev.translate_command(&json!({ verb: true }), 0.0)
                    .is_empty(),
                "{verb} is declared but not accepted"
            );
        }
    }

    /// An occupancy sensor reports the same thing twice, and always has.
    #[test]
    fn an_occupancy_sensor_declares_both_names_it_publishes() {
        let v = device_schema_json(&cfg(DeviceKind::OccupancySensor)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        for name in ["occupied", "occupancy"] {
            assert_eq!(attrs[name]["writable"], false);
            assert_eq!(attrs[name]["states"]["when_false"]["label"], "vacant");
        }

        let dev = DeviceEntry::new(cfg(DeviceKind::OccupancySensor)).expect("a device");
        let state = dev.translate_occupancy_state(true);
        for name in ["occupied", "occupancy"] {
            assert!(
                state.get(name).is_some(),
                "{name} is declared but not published"
            );
        }
    }

    /// Caséta has no LED reporting, so a scene is a trigger and nothing more.
    #[test]
    fn a_scene_offers_activation_and_reads_nothing() {
        let v = scene_schema_json();
        assert!(v["attributes"].as_object().expect("attributes").is_empty());
        assert_eq!(v["actions"][0]["id"], "activate");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pico(buttons: Vec<u32>) -> DeviceConfig {
        DeviceConfig {
            integration_id: 5,
            name: "Kitchen Pico".into(),
            kind: Some(DeviceKind::Pico),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons,
        }
    }

    #[test]
    fn a_pico_publishes_its_buttons_and_no_actions() {
        let v = device_schema_json(&pico(vec![2, 3, 4, 5, 6])).unwrap();
        assert!(v["attributes"]["available_buttons"].is_object());
        assert!(v["attributes"]["button_4"].is_object());
        assert!(
            v.get("actions").is_none(),
            "a Caséta Pico is read-only over LIP"
        );
    }

    /// A config imported before the numbers were carried through publishes
    /// nothing rather than an empty list that looks like "no buttons".
    #[test]
    fn no_buttons_means_no_schema() {
        assert!(device_schema_json(&pico(vec![])).is_none());
        assert!(button_catalogue(&pico(vec![])).is_none());
    }

    #[test]
    fn a_dimmer_gets_a_dimmers_schema_not_a_picos() {
        // Buttons belong to the Pico that has them. A dimmer with a stray
        // button list still declares a dimmer.
        let mut d = pico(vec![2]);
        d.kind = Some(DeviceKind::Dimmer);
        let v = device_schema_json(&d).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        assert!(attrs.contains_key("brightness_pct"));
        assert!(!attrs.contains_key("available_buttons"));
    }

    #[test]
    fn a_row_with_no_kind_declares_nothing() {
        // An unset kind is skipped at startup rather than taking the plugin
        // offline, so it reaches here and must not guess.
        let mut d = pico(vec![2]);
        d.kind = None;
        assert!(device_schema_json(&d).is_none());
    }

    #[test]
    fn the_catalogue_is_the_button_list() {
        let v = button_catalogue(&pico(vec![2, 4])).unwrap();
        assert_eq!(v["available_buttons"], serde_json::json!([2, 4]));
    }
}
