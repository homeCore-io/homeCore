//! The Zigbee2MQTT example profile, through the loader and router.

use hc_topic_map::{loader::load_profiles_from_dir, EcosystemRouter, InboundResult};
use serde_json::Value;

fn router() -> EcosystemRouter {
    let dir = "../../config/profiles/examples";
    let profiles = load_profiles_from_dir(dir).expect("Failed to load example profiles");
    let scripts = hc_topic_map::loader::load_transforms_from_dir(dir);
    EcosystemRouter::with_scripts(profiles, &scripts)
}

fn state(topic: &str, payload: &str) -> Value {
    match router().route_inbound(topic, payload.as_bytes()).unwrap() {
        Some(InboundResult::State { payload, .. }) => payload,
        other => panic!("expected State for {topic}, got {other:?}"),
    }
}

/// A lock's `state` is LOCK or UNLOCK: it used to read as `on: false`.
#[test]
fn a_lock_is_not_off() {
    let p = state("zigbee2mqtt/front_lock", r#"{"state":"LOCK","battery":90}"#);
    assert!(p.get("on").is_none(), "{p}");
    assert_eq!(p["battery_pct"], 90);
}

#[test]
fn a_light_is_on() {
    let p = state("zigbee2mqtt/lamp", r#"{"state":"ON"}"#);
    assert_eq!(p["on"], true);
}

/// A friendly name with a space makes an id with `_`; a command to it goes
/// to the name Zigbee2MQTT listens on, not one made from the id.
#[test]
fn a_command_goes_to_the_devices_own_name() {
    let r = router();
    let Some(InboundResult::State { device_id, .. }) = r
        .route_inbound("zigbee2mqtt/Living Room", br#"{"state":"ON"}"#)
        .unwrap()
    else {
        panic!("no state");
    };
    assert_eq!(device_id, "zigbee_Living_Room");
    let out = r
        .route_outbound(
            "homecore/devices/zigbee_Living_Room/cmd",
            br#"{"on":false}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(out[0].target_topic, "zigbee2mqtt/Living Room/set");
}
