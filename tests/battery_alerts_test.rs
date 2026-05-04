//! Integration tests for the battery watcher: hysteresis-driven event
//! emission and the optional hc-notify shortcut. These tests wire only
//! the pieces under test (event bus + state store + watcher) without
//! standing up the embedded MQTT broker — that hop is already covered
//! by integration_test.rs.

use anyhow::Result;
use chrono::Utc;
use hc_core::battery_watcher::{self, BatteryConfig};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::device::DeviceChange;
use hc_types::event::Event;
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;

/// Process-wide async mutex used to serialize the two `#[tokio::test]`s in
/// this file. Both open their own `StateStore` against unique `/tmp` paths,
/// but cargo runs integration tests in parallel within a single binary and
/// the second concurrent SQLite open occasionally returns `SQLITE_BUSY`
/// (code 5) before either test has had a chance to do real work. Holding
/// this guard for the full test body removes the race without affecting
/// what either test asserts.
fn serialize() -> &'static tokio::sync::Mutex<()> {
    static M: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn unique_paths(tag: &str) -> (String, String) {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let state = format!("/tmp/hc-batt-{tag}-{pid}-{nanos}.redb");
    let history = format!("/tmp/hc-batt-{tag}-{pid}-{nanos}.db");
    (state, history)
}

fn battery_state_changed(device_id: &str, pct: i64) -> Event {
    let mut current = HashMap::new();
    current.insert("battery".to_string(), json!(pct));
    Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.to_string(),
        device_name: Some("Test Sensor".into()),
        previous: HashMap::new(),
        current,
        changed: vec!["battery".into()],
        change: DeviceChange::unknown(),
    }
}

#[tokio::test]
async fn battery_watcher_emits_low_then_recovered() -> Result<()> {
    let _guard = serialize().lock().await;
    let (state_path, history_path) = unique_paths("flow");
    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(&history_path);
    let store = StateStore::open(&state_path, &history_path).await?;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();

    let (cfg_tx, cfg_rx) = watch::channel(BatteryConfig {
        threshold_pct: 20.0,
        recover_band_pct: 5.0,
        notify_channel: None,
        notify_on_recovered: false,
    });

    battery_watcher::spawn(bus.clone(), store.clone(), None, cfg_rx);
    // Give the spawned watcher a moment to subscribe before publishing —
    // broadcast channels don't replay messages sent before subscription.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 18% → should latch low and emit DeviceBatteryLow once.
    bus.publish(battery_state_changed("test.sensor", 18))?;

    let saw_low = timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Event::DeviceBatteryLow {
                device_id,
                battery_pct,
                threshold_pct,
                ..
            }) = rx.recv().await
            {
                assert_eq!(device_id, "test.sensor");
                assert_eq!(battery_pct, 18.0);
                assert_eq!(threshold_pct, 20.0);
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_low, "DeviceBatteryLow never fired");

    // 22% — inside the hysteresis band, no edge should emit.
    bus.publish(battery_state_changed("test.sensor", 22))?;
    let no_edge = timeout(Duration::from_millis(500), async {
        loop {
            match rx.recv().await {
                Ok(Event::DeviceBatteryLow { .. }) | Ok(Event::DeviceBatteryRecovered { .. }) => {
                    return false;
                }
                Ok(_) => continue,
                Err(_) => return true,
            }
        }
    })
    .await
    .unwrap_or(true);
    assert!(no_edge, "Hysteresis band should suppress edge events");

    // 27% — clears the latch (threshold + recover_band = 25), emit Recovered.
    bus.publish(battery_state_changed("test.sensor", 27))?;
    let saw_recovered = timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(Event::DeviceBatteryRecovered {
                device_id,
                battery_pct,
                ..
            }) = rx.recv().await
            {
                assert_eq!(device_id, "test.sensor");
                assert_eq!(battery_pct, 27.0);
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_recovered, "DeviceBatteryRecovered never fired");

    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(&history_path);
    let _ = cfg_tx; // keep sender alive
    Ok(())
}

#[tokio::test]
async fn battery_watcher_skips_non_battery_changes() -> Result<()> {
    let _guard = serialize().lock().await;
    let (state_path, history_path) = unique_paths("skip");
    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(&history_path);
    let store = StateStore::open(&state_path, &history_path).await?;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();

    let (cfg_tx, cfg_rx) = watch::channel(BatteryConfig::default());
    battery_watcher::spawn(bus.clone(), store.clone(), None, cfg_rx);

    // Brightness changed but no battery key — must not emit a battery edge.
    let mut current = HashMap::new();
    current.insert("brightness".to_string(), json!(180));
    bus.publish(Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: "lamp.living_room".into(),
        device_name: Some("Living Room".into()),
        previous: HashMap::new(),
        current,
        changed: vec!["brightness".into()],
        change: DeviceChange::unknown(),
    })?;

    let suppressed = timeout(Duration::from_millis(400), async {
        loop {
            match rx.recv().await {
                Ok(Event::DeviceBatteryLow { .. }) | Ok(Event::DeviceBatteryRecovered { .. }) => {
                    return false;
                }
                Ok(_) => continue,
                Err(_) => return true,
            }
        }
    })
    .await
    .unwrap_or(true);
    assert!(suppressed, "Watcher should ignore non-battery changes");

    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(&history_path);
    let _ = cfg_tx;
    Ok(())
}
