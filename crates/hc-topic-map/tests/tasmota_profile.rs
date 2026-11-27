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

fn router_with_scripts() -> EcosystemRouter {
    let dir = "../../config/profiles/examples";
    let profiles = load_profiles_from_dir(dir).expect("Failed to load example profiles");
    let scripts = hc_topic_map::loader::load_transforms_from_dir(dir);
    EcosystemRouter::with_scripts(profiles, &scripts)
}

/// Every transform an example profile names is in the example script.
#[test]
fn the_example_scripts_define_every_transform_the_examples_name() {
    assert_eq!(
        router_with_scripts().missing_transforms(),
        Vec::<String>::new()
    );
}

fn command(topic: &str, payload: &str) -> (String, String) {
    let out = router_with_scripts()
        .route_outbound(topic, payload.as_bytes())
        .unwrap()
        .unwrap_or_else(|| panic!("no cmd entry for {topic}"));
    assert_eq!(out.len(), 1);
    (
        out[0].target_topic.clone(),
        String::from_utf8(out[0].payload.clone()).unwrap(),
    )
}

/// A dimmer command goes out as one Backlog. It used to sit behind a POWER
/// entry with the same source, which always matched first.
#[test]
fn a_dimmer_command_is_one_backlog() {
    let (topic, payload) = command(
        "homecore/devices/tasmota_lamp/cmd",
        r#"{"on":true,"brightness_pct":80,"color_temp":3000}"#,
    );
    assert_eq!(topic, "cmnd/lamp/Backlog");
    assert_eq!(payload, "POWER ON; Dimmer 80; CT 333");
}

#[test]
fn a_plain_off_is_a_backlog_of_one() {
    let (topic, payload) = command("homecore/devices/tasmota_plug/cmd", r#"{"on":false}"#);
    assert_eq!(topic, "cmnd/plug/Backlog");
    assert_eq!(payload, "POWER OFF");
}

/// The general entry also matches `tasmota_strip_relay1`, so the relay
/// entries must come first; they used to send `cmnd/strip_relay1/POWER`.
#[test]
fn a_relay_command_reaches_its_relay() {
    let (topic, payload) = command(
        "homecore/devices/tasmota_strip_relay1/cmd",
        r#"{"on":true}"#,
    );
    assert_eq!(topic, "cmnd/strip/POWER1");
    assert_eq!(payload, "ON");
}

fn device_of(topic: &str, payload: &str) -> (String, Option<String>) {
    let profiles = load_profiles_from_dir("../../config/profiles/examples")
        .expect("Failed to load example profiles");
    let router = EcosystemRouter::new(profiles, None).expect("Router build failed");
    match router.route_inbound(topic, payload.as_bytes()).unwrap() {
        Some(InboundResult::State {
            device_id, device, ..
        }) => (device_id, device.and_then(|d| d.device_type)),
        other => panic!("expected State for {topic}, got {other:?}"),
    }
}

/// A relay's readings and its commands are one device. The relay state
/// topics used to name it without the profile's prefix (`strip_relay1`), so
/// its readings went to one device and its commands to another.
#[test]
fn a_relay_s_readings_and_commands_are_one_device() {
    let (id, device_type) = device_of("stat/strip/POWER1", "ON");
    assert_eq!(id, "tasmota_strip_relay1");
    assert_eq!(device_type.as_deref(), Some("switch"));
    let (topic, _) = command(&format!("homecore/devices/{id}/cmd"), r#"{"on":true}"#);
    assert_eq!(topic, "cmnd/strip/POWER1");
}

/// A plain Tasmota is a switch, so it gets the switch schema and the app
/// shows its control.
#[test]
fn a_tasmota_is_a_switch() {
    let (id, device_type) = device_of("stat/plug/POWER", "ON");
    assert_eq!(id, "tasmota_plug");
    assert_eq!(device_type.as_deref(), Some("switch"));
}

fn route_full(topic: &str, payload: &str) -> (Value, bool) {
    match router_with_scripts()
        .route_inbound(topic, payload.as_bytes())
        .unwrap()
    {
        Some(InboundResult::State {
            payload, partial, ..
        }) => (payload, partial),
        other => panic!("expected State for {topic}, got {other:?}"),
    }
}

/// `tele/SENSOR` every TelePeriod used to replace the whole state, wiping the
/// `on` that `stat/POWER` set. Profile JSON topics merge now.
#[test]
fn a_sensor_reading_merges_into_what_power_said() {
    let (_, partial) = route_full("tele/plug/SENSOR", r#"{"ENERGY":{"Power":5}}"#);
    assert!(partial);
}

/// A RESULT for a command the profile doesn't map changes nothing, rather
/// than replacing the state with nothing.
#[test]
fn a_reply_with_nothing_mapped_changes_nothing() {
    let (p, partial) = route_full("stat/plug/RESULT", r#"{"Command":"Unknown"}"#);
    assert_eq!(p, serde_json::json!({}));
    assert!(partial);
}

/// An empty message (a retained topic cleared) is not "off".
#[test]
fn an_empty_power_message_is_no_reading() {
    let (p, partial) = route_full("stat/plug/POWER", "");
    assert_eq!(p, serde_json::json!({}));
    assert!(partial);
}

/// A command that doesn't set a topic's attribute publishes nothing to it.
#[test]
fn a_command_without_the_attribute_publishes_nothing() {
    let out = router_with_scripts()
        .route_outbound(
            "homecore/devices/tasmota_x_relay1/cmd",
            br#"{"brightness_pct":50}"#,
        )
        .unwrap();
    assert_eq!(out.map(|v| v.len()), Some(0));
}

/// A device named like the prefix and suffix together used to panic the
/// router, and with it the whole state bridge.
#[test]
fn a_command_to_a_device_named_like_the_pattern_does_not_panic() {
    let r = router_with_scripts();
    let _ = r.route_outbound("homecore/devices/tasmota_relay1/cmd", br#"{"on":true}"#);
    let _ = r.route_outbound("homecore/devices/shelly_relay0/cmd", br#"{"on":true}"#);
}
