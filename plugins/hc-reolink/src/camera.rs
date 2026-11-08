//! A Reolink camera as homeCore devices.
//!
//! Each camera (an address on its own, or one of an NVR's or Home Hub's
//! channels) is a `camera` device: what it sees now in the shared names
//! (`motion`, `person_detected`, …), a doorbell's press as a button press
//! (`pushed`, set for a moment and cleared, as the button trigger reads it),
//! its newest snapshot, and its go2rtc streams. Its spotlight is a `light`
//! and its siren a `siren`, each a device of its own, so cards, rules and
//! scenes treat them as they treat any light or switch. Only what a camera
//! can do is declared.

use crate::api::{Detections, Features};
use plugin_sdk_rs::device_actions::{with_actions, Action, Param};
use plugin_sdk_rs::types::schema::{
    AttributeCategory, AttributeKind, AttributeOption, AttributeSchema, BoolStates, DeviceSchema,
    StateLabel,
};
use serde_json::{json, Value};
use std::collections::HashMap;

/// A camera's homeCore id: `reolink_<host>` on its own, `reolink_<host>_<n>`
/// for an NVR's channel `n - 1` (people count channels from 1).
pub fn camera_id(host: &str, channel: Option<u32>) -> String {
    let host = slug(host);
    match channel {
        Some(c) => format!("reolink_{host}_{}", c + 1),
        None => format!("reolink_{host}"),
    }
}

pub fn spotlight_id(camera: &str) -> String {
    format!("{camera}_spotlight")
}

pub fn siren_id(camera: &str) -> String {
    format!("{camera}_siren")
}

/// Lowercase letters, digits and `_`: safe in an MQTT topic and a go2rtc name.
pub fn slug(s: &str) -> String {
    let out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    out.trim_matches('_').to_string()
}

/// go2rtc's names for a camera's streams: the sub stream (H.264, for cards)
/// and the main one, for HD.
pub fn stream_names(camera: &str) -> (String, String) {
    (camera.to_string(), format!("{camera}_hd"))
}

/// Where go2rtc reads a channel from: RTSP, the sub stream as H.264 and the
/// main one as the camera sends it. The credentials are in it, so it's only
/// ever given to go2rtc, never published.
pub fn rtsp_sources(address: &str, user: &str, pass: &str, channel: u32) -> (String, String) {
    let host = address.split(':').next().unwrap_or(address);
    let nn = format!("{:02}", channel + 1);
    let auth = format!("{}:{}", encode(user), encode(pass));
    (
        format!("rtsp://{auth}@{host}:554/h264Preview_{nn}_sub"),
        format!("rtsp://{auth}@{host}:554/h264Preview_{nn}_main"),
    )
}

/// Percent-encodes what would break a URL's user part.
fn encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// What a camera publishes about what it sees, under the shared names; only
/// what it can detect.
pub fn detections_state(d: &Detections, f: &Features) -> Value {
    let mut s = json!({ "motion": d.motion });
    if f.person {
        s["person_detected"] = json!(d.person);
    }
    if f.vehicle {
        s["vehicle_detected"] = json!(d.vehicle);
    }
    if f.animal {
        s["animal_detected"] = json!(d.animal);
    }
    if f.package {
        s["package_detected"] = json!(d.package);
    }
    s
}

fn seen(name: &str, what: &str) -> AttributeSchema {
    AttributeSchema {
        kind: AttributeKind::Bool,
        writable: false,
        display_name: Some(name.to_string()),
        states: Some(BoolStates {
            when_true: StateLabel::verbed(format!("sees {what}"), format!("sees {what}")),
            when_false: StateLabel::verbed("clear", format!("stops seeing {what}")),
        }),
        ..Default::default()
    }
}

fn text(name: &str, diagnostic: bool) -> AttributeSchema {
    AttributeSchema {
        kind: AttributeKind::String,
        writable: false,
        display_name: Some(name.to_string()),
        category: diagnostic.then_some(AttributeCategory::Diagnostic),
        ..Default::default()
    }
}

/// A camera's schema: what it can see, and what it can do.
pub fn camera_schema(f: &Features, presets: &[(u32, String)]) -> Value {
    let mut a = HashMap::new();
    a.insert(
        "motion".to_string(),
        AttributeSchema {
            kind: AttributeKind::Bool,
            writable: false,
            display_name: Some("Motion".to_string()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("motion", "sees motion"),
                when_false: StateLabel::verbed("still", "stops seeing motion"),
            }),
            ..Default::default()
        },
    );
    if f.person {
        a.insert("person_detected".into(), seen("Person", "a person"));
    }
    if f.vehicle {
        a.insert("vehicle_detected".into(), seen("Vehicle", "a vehicle"));
    }
    if f.animal {
        a.insert("animal_detected".into(), seen("Animal", "an animal"));
    }
    if f.package {
        a.insert("package_detected".into(), seen("Package", "a package"));
    }
    if f.doorbell {
        // As homeCore's button trigger reads it: button 1, for a moment.
        a.insert(
            "pushed".into(),
            AttributeSchema {
                kind: AttributeKind::Integer,
                writable: false,
                display_name: Some("Doorbell pressed".to_string()),
                ..Default::default()
            },
        );
    }
    if f.ir {
        a.insert(
            "ir_mode".into(),
            AttributeSchema {
                kind: AttributeKind::Enum,
                writable: true,
                display_name: Some("Infrared".to_string()),
                options: Some(vec![
                    AttributeOption::new("auto").labelled("When dark"),
                    AttributeOption::new("off").labelled("Off"),
                ]),
                ..Default::default()
            },
        );
    }
    a.insert("snapshot".into(), text("Newest snapshot", false));
    a.insert("snapshot_at".into(), text("Snapshot taken", true));
    a.insert("stream".into(), text("Live stream", true));
    a.insert("stream_hd".into(), text("Live stream, HD", true));
    a.insert("model".into(), text("Model", true));

    let mut actions = vec![Action::new("take_snapshot")
        .label("Take a snapshot")
        .icon("camera")
        .sentence("take a snapshot with {device}")];
    if f.ptz {
        actions.push(
            Action::new("ptz_move")
                .label("Turn")
                .icon("move")
                .category("PTZ")
                .sentence("turn {device} {direction}")
                .param(
                    Param::enum_("direction")
                        .label("Direction")
                        .required()
                        .options(["left", "right", "up", "down", "zoom_in", "zoom_out"]),
                )
                .param(
                    Param::int("duration_ms")
                        .label("For")
                        .unit("ms")
                        .range(100.0, 5000.0)
                        .default(500),
                ),
        );
        if !presets.is_empty() {
            let ids: Vec<(String, String)> = presets
                .iter()
                .map(|(id, name)| (id.to_string(), name.clone()))
                .collect();
            actions.push(
                Action::new("goto_preset")
                    .label("Go to a preset")
                    .icon("crosshair")
                    .category("PTZ")
                    .sentence("point {device} at {preset}")
                    .param(
                        Param::int("preset")
                            .label("Preset")
                            .required()
                            .labelled_options(ids),
                    ),
            );
        }
    }
    with_actions(
        &DeviceSchema {
            attributes: a,
            ..Default::default()
        },
        actions,
    )
}

/// The spotlight: a light like any other.
pub fn spotlight_schema() -> DeviceSchema {
    let mut a = HashMap::new();
    a.insert(
        "on".into(),
        AttributeSchema {
            kind: AttributeKind::Bool,
            writable: true,
            display_name: Some("Spotlight".to_string()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("on", "turns on"),
                when_false: StateLabel::verbed("off", "turns off"),
            }),
            ..Default::default()
        },
    );
    a.insert(
        "brightness_pct".into(),
        AttributeSchema {
            kind: AttributeKind::Float,
            writable: true,
            display_name: Some("Brightness".to_string()),
            unit: Some("%".to_string()),
            min: Some(0.0),
            max: Some(100.0),
            step: Some(1.0),
            ..Default::default()
        },
    );
    DeviceSchema {
        attributes: a,
        ..Default::default()
    }
}

/// The siren: `on` sounds it, and it reads off again once it has.
pub fn siren_schema() -> Value {
    let mut a = HashMap::new();
    a.insert(
        "on".into(),
        AttributeSchema {
            kind: AttributeKind::Bool,
            writable: true,
            display_name: Some("Siren".to_string()),
            states: Some(BoolStates {
                when_true: StateLabel::verbed("sounding", "sounds"),
                when_false: StateLabel::verbed("quiet", "stops"),
            }),
            ..Default::default()
        },
    );
    with_actions(
        &DeviceSchema {
            attributes: a,
            ..Default::default()
        },
        vec![Action::new("sound")
            .label("Sound it")
            .icon("siren")
            .confirm("Sound the siren?")
            .sentence("sound {device} {times} times")
            .param(
                Param::int("times")
                    .label("Times")
                    .range(1.0, 10.0)
                    .default(3),
            )],
    )
}

/// homeCore's 0–100 % as the spotlight's 1–100; 0 is off.
pub fn spotlight_level(pct: f64) -> Option<u8> {
    if pct <= 0.0 {
        None
    } else {
        Some((pct.min(100.0).round() as u8).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every() -> Features {
        Features {
            motion: true,
            person: true,
            vehicle: true,
            animal: true,
            package: true,
            doorbell: true,
            spotlight: true,
            siren: true,
            ptz: true,
            ir: true,
        }
    }

    #[test]
    fn names_cameras_for_good() {
        assert_eq!(camera_id("Front-Door", None), "reolink_front_door");
        assert_eq!(camera_id("nvr", Some(0)), "reolink_nvr_1");
        assert_eq!(spotlight_id("reolink_nvr_2"), "reolink_nvr_2_spotlight");
        assert_eq!(stream_names("reolink_nvr_2").1, "reolink_nvr_2_hd");
    }

    #[test]
    fn rtsp_carries_the_channel_and_escapes_the_password() {
        let (sub, main) = rtsp_sources("10.0.0.5:443", "admin", "p@ss:w/rd", 1);
        assert_eq!(
            sub,
            "rtsp://admin:p%40ss%3Aw%2Frd@10.0.0.5:554/h264Preview_02_sub"
        );
        assert!(main.ends_with("h264Preview_02_main"));
    }

    #[test]
    fn publishes_only_what_a_camera_detects() {
        let f = Features {
            motion: true,
            person: true,
            ..Default::default()
        };
        let d = Detections {
            motion: true,
            person: true,
            vehicle: true,
            ..Default::default()
        };
        assert_eq!(
            detections_state(&d, &f),
            json!({ "motion": true, "person_detected": true })
        );
    }

    #[test]
    fn declares_only_what_a_camera_can_do() {
        let fixed = camera_schema(
            &Features {
                motion: true,
                ..Default::default()
            },
            &[],
        );
        assert!(fixed["attributes"].get("person_detected").is_none());
        assert!(fixed["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["id"] != "ptz_move"));
        let ptz = camera_schema(&every(), &[(1, "Gate".into())]);
        let ids: Vec<&str> = ptz["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["id"].as_str())
            .collect();
        assert!(ids.contains(&"ptz_move") && ids.contains(&"goto_preset"));
        assert!(ptz["attributes"].get("pushed").is_some());
    }

    #[test]
    fn spotlight_levels_keep_off_and_lit_apart() {
        assert_eq!(spotlight_level(0.0), None);
        assert_eq!(spotlight_level(0.2), Some(1));
        assert_eq!(spotlight_level(100.0), Some(100));
        assert_eq!(spotlight_level(250.0), Some(100));
    }

    #[test]
    fn the_declarations_follow_the_rules() {
        plugin_sdk_rs::conformance::check_device_schema_json(&camera_schema(
            &every(),
            &[(1, "Gate".into())],
        ))
        .assert_ok();
        plugin_sdk_rs::conformance::check_device_schema_json(&siren_schema()).assert_ok();
        plugin_sdk_rs::conformance::check_device_schema_json(&with_actions(
            &spotlight_schema(),
            vec![],
        ))
        .assert_ok();
    }
}
