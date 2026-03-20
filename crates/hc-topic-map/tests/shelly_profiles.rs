//! Integration tests that exercise both Shelly profile files via the full loader+router stack.

use hc_topic_map::{loader::load_profiles_from_dir, EcosystemRouter, InboundResult};
use serde_json::Value;

fn load_shelly_router() -> EcosystemRouter {
    let profiles = load_profiles_from_dir("../../config/profiles/examples")
        .expect("Failed to load example profiles");
    assert!(!profiles.is_empty(), "No profiles loaded");
    EcosystemRouter::new(profiles, None).expect("Router build failed")
}

// ─── Gen1 tests ───────────────────────────────────────────────────────────────

#[test]
fn gen1_relay_on() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyplug-s-AABBCC/relay/0", b"on").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplug-s-AABBCC_relay0");
            assert_eq!(payload["on"], true);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_relay_off() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyplug-s-AABBCC/relay/0", b"off").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => assert_eq!(payload["on"], false),
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_dual_relay_channel_1() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shelly25-112233/relay/1", b"on").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shelly25-112233_relay1");
            assert_eq!(payload["on"], true);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_relay_power() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shelly1pm-AABBCC/relay/0/power", b"57.3").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shelly1pm-AABBCC_relay0");
            assert!((payload["power_w"].as_f64().unwrap() - 57.3).abs() < 0.01);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_relay_cmd_on() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplug-s-AABBCC_relay0/cmd",
        br#"{"on":true}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellies/shellyplug-s-AABBCC/relay/0/command");
    assert_eq!(res.payload, b"ON");
}

#[test]
fn gen1_relay_cmd_off() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplug-s-AABBCC_relay0/cmd",
        br#"{"on":false}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.payload, b"OFF");
}

#[test]
fn gen1_relay1_cmd() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shelly25-112233_relay1/cmd",
        br#"{"on":true}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellies/shelly25-112233/relay/1/command");
    assert_eq!(res.payload, b"ON");
}

#[test]
fn gen1_ht_temperature() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyhthm-AABBCC/sensor/temperature", b"22.5").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyhthm-AABBCC");
            assert!((payload["temperature"].as_f64().unwrap() - 22.5).abs() < 0.01);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_ht_humidity() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyhthm-AABBCC/sensor/humidity", b"65.0").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => {
            assert!((payload["humidity"].as_f64().unwrap() - 65.0).abs() < 0.01);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_door_window_closed() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellydw-AABBCC/sensor/state", b"closed").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => assert_eq!(payload["contact"], true),
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_door_window_open() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellydw-AABBCC/sensor/state", b"open").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => assert_eq!(payload["contact"], false),
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_flood_true() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyflood-AABBCC/sensor/flood", b"true").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => assert_eq!(payload["flood"], true),
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_dimmer_status() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellies/shellydimmer-AABBCC/light/0",
        br#"{"ison":true,"brightness":75,"source":"button"}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellydimmer-AABBCC_light0");
            assert_eq!(payload["on"], true);
            assert_eq!(payload["brightness"], 75);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_dimmer_cmd() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellydimmer-AABBCC_light0/cmd",
        br#"{"on":false,"brightness":50}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellies/shellydimmer-AABBCC/light/0/set");
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["turn"], "OFF");
    assert_eq!(body["brightness"], 50);
}

#[test]
fn gen1_dimmer_on_with_brightness_cmd() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellydimmer-AABBCC_light0/cmd",
        br#"{"on":true,"brightness":80}"#,
    ).unwrap().unwrap().remove(0);
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["turn"], "ON");
    assert_eq!(body["brightness"], 80);
}

#[test]
fn gen1_roller_state() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shelly25-AABBCC/roller/0", b"open").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shelly25-AABBCC_roller0");
            assert_eq!(payload["state"], "open");
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_roller_position() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shelly25-AABBCC/roller/0/pos", b"75").unwrap().unwrap();
    match res {
        InboundResult::State { payload, .. } => assert_eq!(payload["position"], 75),
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_roller_cmd_open() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shelly25-AABBCC_roller0/cmd",
        br#"{"state":"open"}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellies/shelly25-AABBCC/roller/0/command");
    assert_eq!(res.payload, b"open");
}

#[test]
fn gen1_em_phase_power() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shelly3em-AABBCC/emeter/0/power", b"1196.5").unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shelly3em-AABBCC_em0");
            assert!((payload["power_w"].as_f64().unwrap() - 1196.5).abs() < 0.1);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen1_availability_online() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyplug-s-AABBCC/online", b"true").unwrap().unwrap();
    match res {
        InboundResult::Availability { device_id, available } => {
            assert_eq!(device_id, "shelly_shellyplug-s-AABBCC");
            assert!(available);
        }
        _ => panic!("expected Availability"),
    }
}

#[test]
fn gen1_availability_offline() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellies/shellyplug-s-AABBCC/online", b"false").unwrap().unwrap();
    match res {
        InboundResult::Availability { available, .. } => assert!(!available),
        _ => panic!("expected Availability"),
    }
}

// ─── Gen2 tests ───────────────────────────────────────────────────────────────

#[test]
fn gen2_switch0_on_with_metering() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplus2pm-083AF2123456/status/switch:0",
        br#"{"id":0,"output":true,"apower":125.4,"voltage":230.1,"current":0.545,"aenergy":{"total":1234.56}}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplus2pm-083AF2123456_switch_0");
            assert_eq!(payload["on"], true);
            assert!((payload["power_w"].as_f64().unwrap() - 125.4).abs() < 0.1);
            assert!((payload["energy_wh"].as_f64().unwrap() - 1234.56).abs() < 0.1);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_switch1_off() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplus2pm-083AF2123456/status/switch:1",
        br#"{"id":1,"output":false}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplus2pm-083AF2123456_switch_1");
            assert_eq!(payload["on"], false);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_switch_cmd_on() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplus2pm-083AF2123456_switch_0/cmd",
        br#"{"on":true}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellyplus2pm-083AF2123456/rpc");
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["method"], "Switch.Set");
    assert_eq!(body["params"]["on"], true);
    assert_eq!(body["params"]["id"], 0);
}

#[test]
fn gen2_switch1_cmd_off() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplus2pm-083AF2123456_switch_1/cmd",
        br#"{"on":false}"#,
    ).unwrap().unwrap().remove(0);
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["method"], "Switch.Set");
    assert_eq!(body["params"]["on"], false);
    assert_eq!(body["params"]["id"], 1);
}

#[test]
fn gen2_pro4pm_switch3_cmd() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellypro4pm-AABBCC_switch_3/cmd",
        br#"{"on":true}"#,
    ).unwrap().unwrap().remove(0);
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["method"], "Switch.Set");
    assert_eq!(body["params"]["id"], 3);
}

#[test]
fn gen2_light_status() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplusdimmer-AABBCC/status/light:0",
        br#"{"id":0,"output":true,"brightness":75.0,"apower":8.5}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplusdimmer-AABBCC_light_0");
            assert_eq!(payload["on"], true);
            assert_eq!(payload["brightness"], 75.0);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_light_cmd_brightness() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplusdimmer-AABBCC_light_0/cmd",
        br#"{"on":true,"brightness":60}"#,
    ).unwrap().unwrap().remove(0);
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["method"], "Light.Set");
    assert_eq!(body["params"]["on"], true);
    assert_eq!(body["params"]["brightness"], 60);
    assert_eq!(body["params"]["id"], 0);
}

#[test]
fn gen2_cover_status() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplus2pm-AABBCC/status/cover:0",
        br#"{"id":0,"state":"stopped","current_pos":75,"apower":0.0}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplus2pm-AABBCC_cover_0");
            assert_eq!(payload["state"], "stopped");
            assert_eq!(payload["position"], 75);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_cover_cmd_position() {
    let r = load_shelly_router();
    let res = r.route_outbound(
        "homecore/devices/shelly_shellyplus2pm-AABBCC_cover_0/cmd",
        br#"{"position":50}"#,
    ).unwrap().unwrap().remove(0);
    assert_eq!(res.target_topic, "shellyplus2pm-AABBCC/rpc");
    let body: Value = serde_json::from_slice(&res.payload).unwrap();
    assert_eq!(body["method"], "Cover.GoToPosition");
    assert_eq!(body["params"]["pos"], 50);
    assert_eq!(body["params"]["id"], 0);
}

#[test]
fn gen2_ht_temperature() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplusht-AABBCC/status/temperature:0",
        br#"{"id":0,"tC":22.5,"tF":72.5,"errors":[]}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplusht-AABBCC_sensor");
            assert!((payload["temperature"].as_f64().unwrap() - 22.5).abs() < 0.01);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_ht_humidity() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellyplusht-AABBCC/status/humidity:0",
        br#"{"id":0,"rh":55.2,"errors":[]}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellyplusht-AABBCC_sensor");
            assert!((payload["humidity"].as_f64().unwrap() - 55.2).abs() < 0.01);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_pro3em_em_status() {
    let r = load_shelly_router();
    let res = r.route_inbound(
        "shellypro3em-AABBCC/status/em:0",
        br#"{"id":0,"a_act_power":1196.5,"a_voltage":230.1,"a_current":5.2,"total_act_power":3706.5}"#,
    ).unwrap().unwrap();
    match res {
        InboundResult::State { device_id, payload, .. } => {
            assert_eq!(device_id, "shelly_shellypro3em-AABBCC_em");
            assert!((payload["power_w_a"].as_f64().unwrap() - 1196.5).abs() < 0.1);
            assert!((payload["power_w"].as_f64().unwrap() - 3706.5).abs() < 0.1);
        }
        _ => panic!("expected State"),
    }
}

#[test]
fn gen2_availability_online() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellyplus2pm-083AF2123456/online", b"true").unwrap().unwrap();
    match res {
        InboundResult::Availability { device_id, available } => {
            assert_eq!(device_id, "shelly_shellyplus2pm-083AF2123456");
            assert!(available);
        }
        _ => panic!("expected Availability"),
    }
}

#[test]
fn gen2_availability_offline() {
    let r = load_shelly_router();
    let res = r.route_inbound("shellyplus2pm-083AF2123456/online", b"false").unwrap().unwrap();
    match res {
        InboundResult::Availability { available, .. } => assert!(!available),
        _ => panic!("expected Availability"),
    }
}
