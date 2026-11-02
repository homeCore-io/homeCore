//! The Tasmota example profile, through the full loader and router. The first
//! pattern that matches a topic is the only one used, so each Tasmota topic
//! has one entry carrying every field it can hold.

use hc_topic_map::{loader::load_profiles_from_dir, EcosystemRouter, InboundResult};
use serde_json::Value;

fn route(topic: &str, payload: &str) -> Value {
    let profiles = load_profiles_from_dir("../../config/profiles/examples")
        .expect("Failed to load example profiles");
    let router = EcosystemRouter::new(profiles, None).expect("Router build failed");
    match router.route_inbound(topic, payload.as_bytes()).unwrap() {
        Some(InboundResult::State { payload, .. }) => payload,
        other => panic!("expected State for {topic}, got {other:?}"),
    }
}

/// Temperature and humidity used to sit in a second `tele/SENSOR` entry that
/// the energy entry always matched first, so they were never published.
#[test]
fn a_sensor_reading_carries_temperature_and_energy() {
    let p = route(
        "tele/kitchen/SENSOR",
        r#"{"AM2301":{"Temperature":21.5,"Humidity":40},"ENERGY":{"Power":12}}"#,
    );
    assert_eq!(p["temperature"], 21.5);
    assert_eq!(p["humidity"], 40);
    assert_eq!(p["power_w"], 12);
}

/// The same for the dimmer fields in `stat/RESULT`.
#[test]
fn a_dimmer_reply_carries_brightness_and_power() {
    let p = route("stat/lamp/RESULT", r#"{"POWER":"ON","Dimmer":40}"#);
    assert_eq!(p["on"], true);
    assert_eq!(p["brightness_pct"], 40);
}
