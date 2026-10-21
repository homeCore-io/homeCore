//! What each Caséta device reports and accepts, declared for any client.
//!
//! A Caséta Pico fires button events all day and nothing ever said which
//! buttons it has. The integration report has always listed them — `import.rs`
//! used their *existence* to recognise a Pico and then dropped the numbers —
//! so a rule editor asking "which button?" had nothing to offer.
//!
//! A Pico declares its buttons — the configured ones, the ones a press
//! taught, or the usual five — what it reports when one is used (`button_N`,
//! and `pushed` / `held` / `released` for homeCore's button trigger), and one
//! action, `press`, which presses a button as if by hand. Everything else
//! declares exactly what `translate_command` takes and
//! `translate_output_state` reports — the two are checked against each other
//! in the tests below.

use std::collections::HashMap;

use plugin_sdk_rs::device_actions::{with_actions, Action};
use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use serde_json::Value;

use plugin_sdk_rs::device_actions::{Param, Source};

use crate::config::{DeviceConfig, DeviceKind};
use crate::devices::DeviceEntry;

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

/// A percentage the zone reports to a tenth: `brightness_pct`, `speed_pct`,
/// `position` — decimals, 0–100, as the shared vocabulary has them.
fn pct(display: &str) -> AttributeSchema {
    AttributeSchema {
        min: Some(0.0),
        max: Some(100.0),
        ..rw(AttributeKind::Float, display, Some("%"))
    }
}

/// `transition_ms`: the fade for the level change it is sent with. Written
/// alongside a level or `on`, never on its own; it is not published.
fn transition_ms() -> AttributeSchema {
    AttributeSchema {
        min: Some(0.0),
        category: Some(AttributeCategory::Config),
        ..rw(AttributeKind::Integer, "Fade", Some("ms"))
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
            ("brightness_pct".into(), pct("Brightness")),
            ("transition_ms".into(), transition_ms()),
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
            ("speed_pct".into(), pct("Speed")),
        ]),
        DeviceKind::Shade => Some(vec![("position".into(), pct("Position"))]),
        // `occupied`, the shared name. The sensor used to publish the same
        // reading a second time as `occupancy`; one name is enough.
        DeviceKind::OccupancySensor => Some(vec![(
            "occupied".into(),
            AttributeSchema {
                states: Some(BoolStates {
                    when_true: StateLabel::verbed("occupied", "detects occupancy"),
                    when_false: StateLabel::verbed("vacant", "becomes vacant"),
                }),
                ..ro(AttributeKind::Bool, "Occupied")
            },
        )]),
        DeviceKind::Pico => None,
    }
}

/// `last_command_error`: why the last command did not happen, in words; null
/// once one does. Diagnostic, out of the way.
fn last_command_error() -> (String, AttributeSchema) {
    (
        "last_command_error".into(),
        AttributeSchema {
            category: Some(AttributeCategory::Diagnostic),
            ..ro(AttributeKind::String, "Last command error")
        },
    )
}

/// A shade's momentary verbs, and a dimmer's ramp, which are not attribute
/// writes.
fn output_actions(kind: &DeviceKind) -> Vec<Action> {
    match kind {
        DeviceKind::Dimmer => vec![
            Action::new("raise")
                .label("Start brightening")
                .category("Level")
                .icon("arrow-up")
                .sentence("start brightening {device}"),
            Action::new("lower")
                .label("Start dimming")
                .category("Level")
                .icon("arrow-down")
                .sentence("start dimming {device}"),
            Action::new("stop")
                .label("Stop brightening or dimming")
                .category("Level")
                .icon("stop")
                .sentence("stop {device} brightening or dimming"),
        ],
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
        &DeviceSchema {
            attributes: [last_command_error()].into_iter().collect(),
            ..Default::default()
        },
        vec![Action::new("activate")
            .label("Activate the scene")
            .category("Scenes")
            .icon("scene")
            .sentence("activate {device}")],
    )
}

/// The schema for one device, or `None` for a row whose kind is unset.
pub fn device_schema_json(dev: &DeviceEntry) -> Option<Value> {
    let kind = &dev.kind;

    if let Some(attrs) = output_attributes(kind) {
        let mut attrs: HashMap<String, AttributeSchema> = attrs.into_iter().collect();
        if *kind != DeviceKind::OccupancySensor {
            let (k, v) = last_command_error();
            attrs.insert(k, v);
        }
        let schema = DeviceSchema {
            attributes: attrs,
            ..Default::default()
        };
        return Some(with_actions(&schema, output_actions(kind)));
    }

    // A Pico.
    let labels = dev.buttons_with_labels();
    let named = || -> Vec<AttributeOption> {
        labels
            .iter()
            .map(|(b, name)| AttributeOption::new(b.to_string()).labelled(name.clone()))
            .collect()
    };
    let which = |display: &str| AttributeSchema {
        options: Some(named()),
        ..ro(AttributeKind::Integer, display)
    };

    let mut attrs: HashMap<String, AttributeSchema> = HashMap::new();
    attrs.insert(
        "available_buttons".into(),
        AttributeSchema {
            category: Some(AttributeCategory::Diagnostic),
            ..ro(AttributeKind::Json, "Buttons")
        },
    );
    for (b, name) in &labels {
        attrs.insert(
            format!("button_{b}"),
            AttributeSchema {
                options: Some(
                    ["press", "hold", "release"]
                        .into_iter()
                        .map(AttributeOption::from)
                        .collect(),
                ),
                ..ro(AttributeKind::Enum, name)
            },
        );
    }
    // homeCore's button trigger reads these: the button's number, set for a
    // moment as it happens and cleared at once, so the same button twice is
    // two events.
    attrs.insert("pushed".into(), which("Button pushed"));
    attrs.insert("held".into(), which("Button held"));
    attrs.insert("released".into(), which("Button released"));
    attrs.insert("last_button".into(), which("Last button"));
    attrs.insert(
        "last_button_name".into(),
        ro(AttributeKind::String, "Last button pressed"),
    );
    let (k, v) = last_command_error();
    attrs.insert(k, v);

    Some(with_actions(
        &DeviceSchema {
            attributes: attrs,
            ..Default::default()
        },
        vec![Action::new("press")
            .label("Press a button")
            .category("Buttons")
            .icon("remote")
            .description(
                "Presses and releases the button as if by hand: the bridge does what it \
                 is paired to in the Lutron app.",
            )
            .sentence("press {button} on {device}")
            .param(
                Param::int("button")
                    .label("Button")
                    .required()
                    .options_from(
                        Source::attribute("available_buttons")
                            .label_key("name")
                            .value_key("number"),
                    ),
            )],
    ))
}

/// Keys this device used to publish under names it no longer uses, as a
/// partial-state patch of `null`s — which removes them. Published at
/// registration so a retained state does not carry a stale `brightness` or
/// `occupancy` after the plugin moved on.
pub fn retired_state(cfg: &DeviceConfig) -> Option<Value> {
    let key = match cfg.kind.as_ref()? {
        DeviceKind::Dimmer => "brightness",
        DeviceKind::OccupancySensor => "occupancy",
        _ => return None,
    };
    Some(serde_json::json!({ key: null }))
}

/// The button catalogue a Pico publishes about itself: `[{number, name}]`,
/// the shape hc-lutron publishes, so a screen or rule editor reads either.
pub fn button_catalogue(dev: &DeviceEntry) -> Option<Value> {
    if dev.kind != DeviceKind::Pico {
        return None;
    }
    let list: Vec<Value> = dev
        .buttons_with_labels()
        .into_iter()
        .map(|(number, name)| serde_json::json!({ "number": number, "name": name }))
        .collect();
    Some(serde_json::json!({ "available_buttons": list }))
}

/// A Pico at rest: every button released and no event standing. Published
/// at registration and after every reconnect, so a press the plugin missed
/// the release of (`button_5: "press"` for good) cannot block the next one.
pub fn pico_at_rest(dev: &DeviceEntry) -> Value {
    let mut patch = serde_json::Map::new();
    // A button it turned out not to have (learned from a press) leaves the
    // state: every Pico component number it lacks is cleared.
    for b in (1..=18).filter(|b| !dev.buttons.contains(b)) {
        patch.insert(format!("button_{b}"), Value::Null);
    }
    for b in &dev.buttons {
        patch.insert(format!("button_{b}"), Value::from("release"));
    }
    for k in ["pushed", "held", "released"] {
        patch.insert(k.into(), Value::Null);
    }
    Value::Object(patch)
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
            button_names: vec![],
        }
    }

    fn ent(kind: DeviceKind) -> DeviceEntry {
        DeviceEntry::new(cfg(kind)).expect("a device")
    }

    /// The mirror of the declaration: every speed offered is one the command
    /// path takes.
    #[test]
    fn a_fan_declares_every_speed_it_accepts_and_no_others() {
        let v = device_schema_json(&ent(DeviceKind::FanControl)).expect("a schema");
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
        let d = device_schema_json(&ent(DeviceKind::Dimmer)).expect("a schema");
        assert_eq!(d["attributes"]["brightness_pct"]["writable"], true);
        assert_eq!(d["attributes"]["brightness_pct"]["unit"], "%");

        let s = device_schema_json(&ent(DeviceKind::Switch)).expect("a schema");
        assert_eq!(s["attributes"]["on"]["writable"], true);
        assert!(!s["attributes"]
            .as_object()
            .unwrap()
            .contains_key("brightness_pct"));
    }

    /// Position is an attribute; raising and lowering are verbs.
    #[test]
    fn a_shade_declares_its_position_and_its_three_verbs() {
        let v = device_schema_json(&ent(DeviceKind::Shade)).expect("a schema");
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

    /// `occupied`, once. The old `occupancy` twin is neither declared nor
    /// published.
    #[test]
    fn an_occupancy_sensor_declares_occupied_only() {
        let v = device_schema_json(&ent(DeviceKind::OccupancySensor)).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs["occupied"]["writable"], false);
        assert_eq!(attrs["occupied"]["states"]["when_false"]["label"], "vacant");

        let dev = DeviceEntry::new(cfg(DeviceKind::OccupancySensor)).expect("a device");
        assert_eq!(
            dev.translate_occupancy_state(true),
            json!({"occupied": true})
        );
    }

    /// The shared names, in the shared kinds: decimals for the percentages,
    /// `transition_ms` writable beside a dimmer's level, no raw `brightness`.
    #[test]
    fn levels_are_declared_as_the_vocabulary_has_them() {
        let d = device_schema_json(&ent(DeviceKind::Dimmer)).expect("a schema");
        let attrs = d["attributes"].as_object().expect("attributes");
        assert!(!attrs.contains_key("brightness"));
        assert_eq!(attrs["brightness_pct"]["kind"], "float");
        assert_eq!(attrs["transition_ms"]["unit"], "ms");
        assert_eq!(attrs["transition_ms"]["writable"], true);
        let dev = DeviceEntry::new(cfg(DeviceKind::Dimmer)).expect("a device");
        for k in dev
            .translate_output_state(42.0)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
        {
            assert!(attrs.contains_key(k), "{k} is published but not declared");
        }

        let f = device_schema_json(&ent(DeviceKind::FanControl)).expect("a schema");
        assert_eq!(f["attributes"]["speed_pct"]["kind"], "float");
        let s = device_schema_json(&ent(DeviceKind::Shade)).expect("a schema");
        assert_eq!(s["attributes"]["position"]["kind"], "float");
    }

    #[test]
    fn retired_names_are_cleared() {
        assert_eq!(
            retired_state(&cfg(DeviceKind::Dimmer)),
            Some(json!({"brightness": null}))
        );
        assert_eq!(
            retired_state(&cfg(DeviceKind::OccupancySensor)),
            Some(json!({"occupancy": null}))
        );
        assert_eq!(retired_state(&cfg(DeviceKind::Switch)), None);
    }

    /// Caséta has no LED reporting, so a scene is a trigger and nothing more.
    #[test]
    fn a_scene_offers_activation_and_reads_nothing() {
        let v = scene_schema_json();
        let attrs = v["attributes"].as_object().expect("attributes");
        assert_eq!(attrs.len(), 1, "only why a run failed");
        assert_eq!(attrs["last_command_error"]["category"], "diagnostic");
        assert_eq!(v["actions"][0]["id"], "activate");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pico(buttons: Vec<u32>) -> DeviceEntry {
        DeviceEntry::new(DeviceConfig {
            integration_id: 5,
            name: "Kitchen Pico".into(),
            kind: Some(DeviceKind::Pico),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons,
            button_names: vec![],
        })
        .expect("a Pico")
    }

    #[test]
    fn a_pico_declares_its_buttons_events_and_press() {
        let v = device_schema_json(&pico(vec![2, 3, 4, 5, 6])).unwrap();
        let a = &v["attributes"];
        assert!(a["available_buttons"].is_object());
        assert_eq!(a["button_4"]["display_name"], "Off");
        for k in ["pushed", "held", "released", "last_button"] {
            assert_eq!(a[k]["kind"], "integer", "{k}");
            assert_eq!(a[k]["options"][0]["value"], "2");
            assert_eq!(a[k]["options"][0]["label"], "On");
        }
        assert_eq!(a["last_command_error"]["category"], "diagnostic");
        assert_eq!(v["actions"][0]["id"], "press");
    }

    /// No buttons configured is the usual 5-button Pico, not "no schema" —
    /// "Holiday Lights Remote 1" had nothing at all to offer a rule.
    #[test]
    fn no_buttons_means_the_usual_five() {
        let v = button_catalogue(&pico(vec![])).unwrap();
        let names: Vec<&str> = v["available_buttons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["On", "Favorite", "Off", "Raise", "Lower"]);
        assert!(device_schema_json(&pico(vec![])).unwrap()["attributes"]["button_6"].is_object());
    }

    /// The same shape hc-lutron publishes: `[{number, name}]`.
    #[test]
    fn the_catalogue_is_numbers_and_names() {
        let v = button_catalogue(&pico(vec![2, 4])).unwrap();
        assert_eq!(
            v["available_buttons"],
            serde_json::json!([{"number": 2, "name": "On"}, {"number": 4, "name": "Off"}])
        );
    }

    #[test]
    fn a_pico_at_rest_has_every_button_released_and_no_event() {
        let v = pico_at_rest(&pico(vec![8, 9, 10, 11]));
        assert_eq!(v["button_8"], "release");
        assert_eq!(v["button_11"], "release");
        assert!(v["pushed"].is_null() && v.get("pushed").is_some());
        assert!(
            v.get("button_2").is_some_and(Value::is_null),
            "not its button"
        );
    }

    #[test]
    fn a_dimmer_gets_a_dimmers_schema_not_a_picos() {
        let mut d = pico(vec![2]);
        d.kind = DeviceKind::Dimmer;
        d.config.kind = Some(DeviceKind::Dimmer);
        let v = device_schema_json(&d).expect("a schema");
        let attrs = v["attributes"].as_object().expect("attributes");
        assert!(attrs.contains_key("brightness_pct"));
        assert!(!attrs.contains_key("available_buttons"));
        assert!(button_catalogue(&d).is_none());
    }
}
