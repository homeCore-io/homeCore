//! Config parsing + validation tests.
//!
//! Full end-to-end integration tests require a live MQTT broker — see
//! README.md for the manual smoke-test procedure. These tests exercise
//! config deserialization, defaults, and validation rejection paths.

use std::io::Write;
use tempfile::NamedTempFile;

// Re-declare config here so tests can call into it without a lib target.
// `logging` used to be included alongside it, because config.rs referenced a
// local `logging::LoggingConfig`; that type now comes from the SDK, so there is
// nothing left to include.
#[path = "../src/config.rs"]
#[allow(dead_code)]
mod config;

fn write_config(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{content}").unwrap();
    f
}

#[test]
fn parses_minimal_config() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "plugin.thermostat"
broker_host = "127.0.0.1"
broker_port = 1883
password = "x"
"#,
    );
    let cfg = config::Config::load(f.path().to_str().unwrap())
        .unwrap()
        .config;
    assert_eq!(cfg.homecore.plugin_id, "plugin.thermostat");
    assert_eq!(cfg.thermostats.len(), 0);
    assert_eq!(cfg.logging.level, "info");
    assert_eq!(cfg.homecore.heartbeat_secs, 60);
}

#[test]
fn parses_full_thermostat_entry() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "plugin.thermostat"
broker_host = "127.0.0.1"
broker_port = 1883
password = "p"

[[thermostat]]
id = "lr"
name = "Living Room"
sensor_device_ids = ["s1", "s2"]
sensor_attribute = "temperature"
aggregation = "min"
setpoint = 20.5
hysteresis = 2.0
mode = "heat"
actuator_device_id = "switch_furnace"
min_on_secs = 300
min_off_secs = 180
"#,
    );
    let cfg = config::Config::load(f.path().to_str().unwrap())
        .unwrap()
        .config;
    assert_eq!(cfg.thermostats.len(), 1);
    let t = &cfg.thermostats[0];
    assert_eq!(t.id, "lr");
    assert_eq!(t.sensor_device_ids, vec!["s1", "s2"]);
    assert_eq!(t.aggregation, "min");
    assert_eq!(t.setpoint, 20.5);
    assert_eq!(t.min_on_secs, 300);
    assert_eq!(t.mode, "heat");
}

#[test]
fn applies_thermostat_defaults() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "plugin.thermostat"
broker_host = "127.0.0.1"
broker_port = 1883
password = "p"

[[thermostat]]
id = "default"
name = "Default"
sensor_device_ids = ["s1"]
setpoint = 21.0
"#,
    );
    let cfg = config::Config::load(f.path().to_str().unwrap())
        .unwrap()
        .config;
    let t = &cfg.thermostats[0];
    assert_eq!(t.sensor_attribute, "temperature");
    assert_eq!(t.aggregation, "average");
    assert_eq!(t.hysteresis, 1.0);
    assert_eq!(t.mode, "off");
    assert_eq!(t.min_on_secs, 0);
    assert_eq!(t.min_off_secs, 0);
}

#[test]
fn rejects_invalid_mode() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "p"
broker_host = "h"
broker_port = 1883
password = "p"

[[thermostat]]
id = "x"
name = "X"
sensor_device_ids = ["s"]
setpoint = 21.0
mode = "bogus"
"#,
    );
    // Left out and said — the plugin still runs (H7).
    let l = config::Config::load(f.path().to_str().unwrap()).unwrap();
    assert!(l.config.thermostats.is_empty());
    assert!(
        l.skipped[0].problem.contains("mode"),
        "{}",
        l.skipped[0].problem
    );
}

#[test]
fn rejects_invalid_aggregation() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "p"
broker_host = "h"
broker_port = 1883
password = "p"

[[thermostat]]
id = "x"
name = "X"
sensor_device_ids = ["s"]
setpoint = 21.0
aggregation = "median"
"#,
    );
    // Left out and said — the plugin still runs (H7).
    let l = config::Config::load(f.path().to_str().unwrap()).unwrap();
    assert!(l.config.thermostats.is_empty());
    assert!(
        l.skipped[0].problem.contains("aggregation"),
        "{}",
        l.skipped[0].problem
    );
}

#[test]
fn rejects_negative_hysteresis() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "p"
broker_host = "h"
broker_port = 1883
password = "p"

[[thermostat]]
id = "x"
name = "X"
sensor_device_ids = ["s"]
setpoint = 21.0
hysteresis = -0.5
"#,
    );
    // Left out and said — the plugin still runs (H7).
    let l = config::Config::load(f.path().to_str().unwrap()).unwrap();
    assert!(l.config.thermostats.is_empty());
    assert!(
        l.skipped[0].problem.contains("hysteresis"),
        "{}",
        l.skipped[0].problem
    );
}

#[test]
fn rejects_empty_plugin_id() {
    let f = write_config(
        r#"
[homecore]
plugin_id = ""
broker_host = "h"
broker_port = 1883
password = "p"
"#,
    );
    let err = config::Config::load(f.path().to_str().unwrap()).unwrap_err();
    assert!(err.to_string().contains("plugin_id"));
}

#[test]
fn rejects_a_fahrenheit_setpoint() {
    let f = write_config(
        r#"
[homecore]
plugin_id = "p"

[[thermostat]]
id = "x"
name = "X"
sensor_device_ids = ["s"]
setpoint = 68.0
"#,
    );
    // Left out and said — the plugin still runs (H7).
    let l = config::Config::load(f.path().to_str().unwrap()).unwrap();
    assert!(l.config.thermostats.is_empty());
    assert!(
        l.skipped[0].problem.contains("°F"),
        "{}",
        l.skipped[0].problem
    );
}

#[test]
fn setpoint_range_edges() {
    assert!(config::setpoint_problem(-40.0).is_none());
    assert!(config::setpoint_problem(45.0).is_none());
    assert!(config::setpoint_problem(45.1).is_some());
    assert!(config::setpoint_problem(-40.1).is_some());
}
