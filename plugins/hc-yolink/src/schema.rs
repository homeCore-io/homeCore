//! What each YoLink device reports, declared rather than guessed.
//!
//! Naming both states of a boolean is the point: a boolean attribute is two
//! events — a door opens and a door closes — and a client given only one name
//! offers one row.
//!
//! **Only what is actually published appears here**, per device: a plug that
//! meters declares `power_w` and one that does not declares nothing about
//! power; a temperature probe declares no humidity. An attribute declared but
//! never published is a control for something that does not exist — see
//! `every_declared_attribute_is_published` in the tests.

use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use std::collections::HashMap;

use crate::devices::{Caps, DeviceKind, LOCK_ACTIONS, LOCK_METHODS};

/// A read-only attribute of `kind`, labelled for display.
fn ro(kind: AttributeKind, label: &str) -> AttributeSchema {
    AttributeSchema::read_only(kind).labelled(label)
}

/// A read-only attribute with a unit.
fn ro_unit(kind: AttributeKind, label: &str, unit: &str) -> AttributeSchema {
    let mut a = ro(kind, label);
    a.unit = Some(unit.to_string());
    a
}

/// A read-only boolean whose two states have names.
fn ro_bool(label: &str, on: (&str, &str), off: (&str, &str)) -> AttributeSchema {
    ro(AttributeKind::Bool, label).with_states(BoolStates {
        when_true: StateLabel::verbed(on.0, on.1),
        when_false: StateLabel::verbed(off.0, off.1),
    })
}

fn diagnostic(mut a: AttributeSchema) -> AttributeSchema {
    a.category = Some(AttributeCategory::Diagnostic);
    a
}

fn options(list: &[(&str, &str)]) -> Vec<AttributeOption> {
    list.iter()
        .map(|(v, l)| AttributeOption::new(*v).labelled(*l))
        .collect()
}

/// The battery: YoLink's 0–4 scale × 25, and whether it is low (level 1 or
/// the device's own flag).
fn battery(a: &mut HashMap<String, AttributeSchema>) {
    let mut pct = ro_unit(AttributeKind::Float, "Battery", "%");
    pct.min = Some(0.0);
    pct.max = Some(100.0);
    a.insert("battery_pct".into(), pct);
    a.insert(
        "battery_low".into(),
        ro_bool("Battery low", ("low", "runs low"), ("fine", "is replaced")),
    );
    a.insert("last_seen".into(), ro(AttributeKind::String, "Last report"));
}

fn temperature(label: &str) -> AttributeSchema {
    // °C always; the app shows °F from the person's setting.
    ro_unit(AttributeKind::Float, label, "°C")
}

/// `{"sound_level": N}` by name: mute, low, medium, high.
pub const SOUND_LEVELS: &[(&str, &str)] =
    &[("0", "Mute"), ("1", "Low"), ("2", "Medium"), ("3", "High")];

/// The schema for a device, or `None` for the kinds this plugin does not
/// bridge at all.
pub fn schema_for(kind: &DeviceKind, caps: &Caps) -> Option<DeviceSchema> {
    let mut a: HashMap<String, AttributeSchema> = HashMap::new();

    match kind {
        DeviceKind::Outlet
        | DeviceKind::SmartPlug
        | DeviceKind::Switch
        | DeviceKind::MultiOutlet => {
            a.insert(
                "on".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Power")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("on", "turns on"),
                        when_false: StateLabel::verbed("off", "turns off"),
                    }),
            );
            if caps.metering {
                a.insert(
                    "power_w".into(),
                    ro_unit(AttributeKind::Float, "Power draw", "W"),
                );
            }
        }

        DeviceKind::Siren => {
            a.insert(
                "on".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Siren")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("sounding", "starts sounding"),
                        when_false: StateLabel::verbed("silent", "goes silent"),
                    }),
            );
            a.insert(
                "alarm".into(),
                ro_bool(
                    "Alarm",
                    ("alarming", "starts alarming"),
                    ("quiet", "stops alarming"),
                ),
            );
        }

        DeviceKind::DoorSensor => {
            a.insert(
                "open".into(),
                ro_bool("Door", ("open", "opens"), ("closed", "closes")),
            );
            battery(&mut a);
        }

        DeviceKind::MotionSensor => {
            a.insert(
                "motion".into(),
                ro_bool(
                    "Motion",
                    ("detecting motion", "detects motion"),
                    ("clear", "stops detecting motion"),
                ),
            );
            battery(&mut a);
        }

        DeviceKind::LeakSensor => {
            a.insert(
                "water_detected".into(),
                ro_bool("Water", ("wet", "detects water"), ("dry", "dries out")),
            );
            a.insert("temperature".into(), temperature("Its temperature"));
            a.insert(
                "probe_fault".into(),
                diagnostic(ro_bool(
                    "Probe",
                    ("faulty", "reports a probe fault"),
                    ("working", "works again"),
                )),
            );
            a.insert(
                "freeze_warning".into(),
                ro_bool(
                    "Freezing",
                    ("freezing", "starts freezing"),
                    ("above freezing", "warms up"),
                ),
            );
            battery(&mut a);
        }

        DeviceKind::VibrationSensor => {
            a.insert(
                "vibration".into(),
                ro_bool(
                    "Vibration",
                    ("vibrating", "starts vibrating"),
                    ("still", "stops vibrating"),
                ),
            );
            battery(&mut a);
        }

        DeviceKind::THSensor => {
            a.insert("temperature".into(), temperature("Temperature"));
            if !caps.temp_only {
                let mut humidity = ro_unit(AttributeKind::Float, "Humidity", "%");
                humidity.min = Some(0.0);
                humidity.max = Some(100.0);
                a.insert("humidity".into(), humidity);
            }
            // The sensor's own limits, set in the YoLink app — shown here,
            // not changed. Core's alert rules are where homeCore's live.
            let mut lo = temperature("Its low limit");
            lo.category = Some(AttributeCategory::Config);
            a.insert("temp_limit_low".into(), lo);
            let mut hi = temperature("Its high limit");
            hi.category = Some(AttributeCategory::Config);
            a.insert("temp_limit_high".into(), hi);
            a.insert(
                "too_cold".into(),
                ro_bool(
                    "Below its limit",
                    ("too cold", "goes below its limit"),
                    ("not too cold", "warms back above its limit"),
                ),
            );
            a.insert(
                "too_warm".into(),
                ro_bool(
                    "Above its limit",
                    ("too warm", "goes above its limit"),
                    ("not too warm", "cools back below its limit"),
                ),
            );
            battery(&mut a);
        }

        DeviceKind::Lock | DeviceKind::LockV2 => {
            a.insert(
                "locked".into(),
                AttributeSchema::new(AttributeKind::Bool)
                    .labelled("Lock")
                    .with_states(BoolStates {
                        when_true: StateLabel::verbed("locked", "locks"),
                        when_false: StateLabel::verbed("unlocked", "unlocks"),
                    }),
            );
            if *kind == DeviceKind::LockV2 {
                // The bolt and the door are different things: a lock can be
                // thrown while the door stands open.
                a.insert(
                    "open".into(),
                    ro_bool("Door", ("open", "opens"), ("closed", "closes")),
                );
                let mut auto =
                    AttributeSchema::new(AttributeKind::Integer).labelled("Lock again after");
                auto.unit = Some("s".into());
                auto.min = Some(0.0);
                auto.max = Some(3600.0);
                auto.category = Some(AttributeCategory::Config);
                a.insert("auto_lock_secs".into(), auto);
                let mut sound =
                    AttributeSchema::new(AttributeKind::Integer).labelled("Beep volume");
                sound.options = Some(options(SOUND_LEVELS));
                sound.min = Some(0.0);
                sound.max = Some(3.0);
                sound.category = Some(AttributeCategory::Config);
                a.insert("sound_level".into(), sound);
            }
            a.insert("last_alert".into(), ro(AttributeKind::String, "Last alert"));
            a.insert(
                "last_alert_at".into(),
                ro(AttributeKind::String, "Last alert at"),
            );
            let mut source = ro(AttributeKind::Enum, "Last alert from");
            source.options = Some(options(LOCK_METHODS));
            a.insert("last_alert_source".into(), source);
            let mut action = ro(AttributeKind::Enum, "Last lock event");
            action.options = Some(options(LOCK_ACTIONS));
            a.insert("last_lock_action".into(), action);
            let mut method = ro(AttributeKind::Enum, "How");
            method.options = Some(options(LOCK_METHODS));
            a.insert("last_lock_method".into(), method);
            a.insert(
                "last_lock_event".into(),
                ro(AttributeKind::Json, "Last lock event, with when"),
            );
            battery(&mut a);
        }

        DeviceKind::Hub | DeviceKind::Unknown(_) => return None,
    }

    if kind.is_relay() || kind.is_lock() || *kind == DeviceKind::Siren {
        a.insert(
            "last_command_error".into(),
            diagnostic(ro(AttributeKind::String, "Last command error")),
        );
    }

    // Every attribute gets the shared lexicon's category where it has one.
    for (name, attr) in a.iter_mut() {
        if attr.category.is_none() {
            attr.category = AttributeCategory::for_name(name);
        }
    }

    Some(DeviceSchema {
        attributes: a,
        ..Default::default()
    })
}

/// The names a kind used to publish and no longer does, as a partial of
/// `null`s that removes them from homeCore's stored state. `None` when the
/// kind retired nothing.
pub fn retired_state(kind: &DeviceKind) -> Option<serde_json::Value> {
    let names: &[&str] = match kind {
        DeviceKind::DoorSensor
        | DeviceKind::MotionSensor
        | DeviceKind::LeakSensor
        | DeviceKind::VibrationSensor => &["battery"],
        DeviceKind::THSensor => &["battery", "humidity_pct", "temperature_unit"],
        DeviceKind::Lock | DeviceKind::LockV2 => &["battery", "door_open"],
        // `watt` and `electricity` were never documented; `energy_kwh` was
        // published from them and is gone.
        DeviceKind::Outlet
        | DeviceKind::SmartPlug
        | DeviceKind::Switch
        | DeviceKind::MultiOutlet => &["energy_kwh"],
        _ => &[],
    };
    if names.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(
        names
            .iter()
            .map(|n| (n.to_string(), serde_json::Value::Null))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::read;
    use serde_json::json;

    fn all_kinds() -> Vec<DeviceKind> {
        vec![
            DeviceKind::Outlet,
            DeviceKind::SmartPlug,
            DeviceKind::Switch,
            DeviceKind::MultiOutlet,
            DeviceKind::DoorSensor,
            DeviceKind::MotionSensor,
            DeviceKind::LeakSensor,
            DeviceKind::THSensor,
            DeviceKind::VibrationSensor,
            DeviceKind::Lock,
            DeviceKind::LockV2,
            DeviceKind::Siren,
        ]
    }
    fn meter() -> Caps {
        Caps {
            metering: true,
            temp_only: false,
        }
    }

    #[test]
    fn temperature_is_celsius_and_a_probe_has_no_humidity() {
        let s = schema_for(&DeviceKind::THSensor, &Caps::default()).unwrap();
        assert_eq!(s.attributes["temperature"].unit.as_deref(), Some("°C"));
        assert!(s.attributes.contains_key("humidity"));
        let probe = schema_for(
            &DeviceKind::THSensor,
            &Caps {
                metering: false,
                temp_only: true,
            },
        )
        .unwrap();
        assert!(!probe.attributes.contains_key("humidity"));
    }

    #[test]
    fn only_a_metering_plug_declares_power() {
        assert!(schema_for(&DeviceKind::Outlet, &meter())
            .unwrap()
            .attributes
            .contains_key("power_w"));
        let plain = schema_for(&DeviceKind::Outlet, &Caps::default()).unwrap();
        assert!(!plain.attributes.contains_key("power_w"));
        assert!(!plain.attributes.contains_key("energy_kwh"));
    }

    #[test]
    fn no_old_spelling_is_declared() {
        for kind in all_kinds() {
            let s = schema_for(&kind, &meter()).unwrap();
            for old in [
                "battery",
                "humidity_pct",
                "door_open",
                "temperature_unit",
                "energy_kwh",
                "leak",
                "contact",
            ] {
                assert!(!s.attributes.contains_key(old), "{kind:?} declares {old}");
            }
        }
    }

    #[test]
    fn the_beep_volume_is_named() {
        let s = schema_for(&DeviceKind::LockV2, &Caps::default()).unwrap();
        let names: Vec<_> = s.attributes["sound_level"]
            .options
            .as_ref()
            .unwrap()
            .iter()
            .map(|o| o.label.clone().unwrap())
            .collect();
        assert_eq!(names, ["Mute", "Low", "Medium", "High"]);
        assert!(s.attributes["sound_level"].writable);
        assert!(s.attributes["auto_lock_secs"].writable);
        // A v1 lock takes neither.
        let v1 = schema_for(&DeviceKind::Lock, &Caps::default()).unwrap();
        assert!(!v1.attributes.contains_key("sound_level"));
    }

    #[test]
    fn retired_names_are_cleared_per_kind() {
        assert_eq!(
            retired_state(&DeviceKind::THSensor),
            Some(json!({ "battery": null, "humidity_pct": null, "temperature_unit": null }))
        );
        assert_eq!(
            retired_state(&DeviceKind::Outlet),
            Some(json!({ "energy_kwh": null }))
        );
        for kind in all_kinds() {
            let declared = schema_for(&kind, &meter()).unwrap();
            if let Some(gone) = retired_state(&kind) {
                for name in gone.as_object().unwrap().keys() {
                    assert!(!declared.attributes.contains_key(name), "{kind:?} {name}");
                }
            }
        }
    }

    #[test]
    fn every_bridged_kind_has_a_schema() {
        for kind in all_kinds() {
            assert!(schema_for(&kind, &Caps::default()).is_some(), "{kind:?}");
        }
        assert!(schema_for(&DeviceKind::Hub, &Caps::default()).is_none());
    }

    #[test]
    fn every_boolean_names_both_of_its_states() {
        for kind in all_kinds() {
            let schema = schema_for(&kind, &meter()).unwrap();
            for (name, attr) in &schema.attributes {
                if !matches!(attr.kind, AttributeKind::Bool) {
                    continue;
                }
                let states = attr
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{kind:?}.{name}"));
                assert_ne!(
                    states.when_true.label, states.when_false.label,
                    "{kind:?}.{name}"
                );
            }
        }
    }

    /// Walk the real translator: every name declared can appear. The few
    /// the bridge adds itself (history, `last_seen`, power, command errors)
    /// are listed.
    #[test]
    fn every_declared_attribute_is_published() {
        let by_bridge = [
            "power_w",
            "last_seen",
            "last_command_error",
            "last_alert",
            "last_alert_at",
            "last_alert_source",
            "last_lock_action",
            "last_lock_method",
            "last_lock_event",
        ];
        let cases: Vec<(DeviceKind, serde_json::Value)> = vec![
            (DeviceKind::Switch, json!({ "state": "open", "power": 125 })),
            (DeviceKind::Siren, json!({ "state": "open", "alarm": true })),
            (
                DeviceKind::DoorSensor,
                json!({ "state": "open", "battery": 4 }),
            ),
            (
                DeviceKind::MotionSensor,
                json!({ "state": "alert", "battery": 3 }),
            ),
            (
                DeviceKind::LeakSensor,
                json!({ "state": "alert", "battery": 2, "devTemperature": 18, "alarmState": { "detectorError": false, "freezeError": false } }),
            ),
            (
                DeviceKind::VibrationSensor,
                json!({ "state": "alert", "battery": 1 }),
            ),
            (
                DeviceKind::THSensor,
                json!({ "temperature": 21.5, "humidity": 44.0, "battery": 4, "tempLimit": { "min": 0, "max": 30 }, "alarm": { "lowTemp": false, "highTemp": false } }),
            ),
            (
                DeviceKind::Lock,
                json!({ "state": "locked", "battery": 4, "alert": { "type": "LockFailed" } }),
            ),
            (
                DeviceKind::LockV2,
                json!({ "state": { "lock": "locked", "door": "open" }, "battery": 4, "alert": { "type": "DoorOpenAlarm" }, "attributes": { "autoLock": 30, "soundLevel": 2 } }),
            ),
        ];
        for (kind, report) in cases {
            let declared = schema_for(&kind, &meter()).unwrap();
            let published = read(&kind, &meter(), &report).unwrap().attrs;
            for name in declared.attributes.keys() {
                assert!(
                    published.contains_key(name) || by_bridge.contains(&name.as_str()),
                    "{kind:?} declares `{name}` but a full report does not publish it"
                );
            }
        }
    }

    /// Writable means the command path takes it.
    #[test]
    fn nothing_is_writable_that_the_command_path_refuses() {
        for kind in all_kinds() {
            let schema = schema_for(&kind, &meter()).unwrap();
            for (name, attr) in &schema.attributes {
                if attr.writable {
                    let v = match attr.kind {
                        AttributeKind::Bool => json!(true),
                        _ => json!(1),
                    };
                    let cmd = json!({ name.as_str(): v });
                    assert!(
                        kind.translate_command(&cmd).is_ok(),
                        "{kind:?} declares `{name}` writable but rejects it"
                    );
                }
            }
        }
    }
}
