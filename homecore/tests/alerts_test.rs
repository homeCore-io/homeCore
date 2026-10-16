//! The alert engine end to end (H4): a time rule saved in the store, a device
//! entering and leaving the state, and `alert_raised` / `alert_cleared` on the
//! bus — with the handle the API serves kept in step. Wires only the store,
//! the bus and the engine, as the battery watcher's test does.

use anyhow::Result;
use chrono::Utc;
use hc_core::alerts::{self, AlertsHandle};
use hc_core::EventBus;
use hc_state::StateStore;
use hc_types::alert::{AlertKind, AlertRule};
use hc_types::device::{DeviceChange, DeviceState};
use hc_types::event::{AlertSeverity, Event};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use tokio::time::timeout;

async fn next_alert_event(rx: &mut Receiver<Event>) -> Event {
    timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(e @ (Event::AlertRaised { .. } | Event::AlertCleared { .. })) => return e,
                Ok(_) => continue,
                Err(e) => panic!("bus closed: {e}"),
            }
        }
    })
    .await
    .expect("an alert event within 5 s")
}

fn changed(device_id: &str, open: bool) -> Event {
    Event::DeviceStateChanged {
        timestamp: Utc::now(),
        device_id: device_id.into(),
        device_name: None,
        previous: HashMap::new(),
        current: HashMap::from([("open".to_string(), json!(open))]),
        changed: vec!["open".into()],
        change: DeviceChange::unknown(),
    }
}

#[tokio::test]
async fn a_time_rule_raises_and_clears_an_alert() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(
        &dir.path().join("state.redb").to_string_lossy(),
        &dir.path().join("history.db").to_string_lossy(),
    )
    .await?;

    let mut door = DeviceState::new("garage_door", "Garage Door", "test");
    door.available = true;
    door.attributes.insert("open".into(), json!(false));
    store.upsert_device(&door).await?;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let handle = AlertsHandle::new();
    alerts::spawn(bus.clone(), store.clone(), handle.clone(), None);

    // Always, so the test doesn't depend on the time of day.
    store
        .upsert_alert_rule(AlertRule {
            id: "r1".into(),
            name: "Garage left open".into(),
            enabled: true,
            device_ids: vec!["garage_door".into()],
            attribute: "open".into(),
            value: json!(true),
            state_label: Some("Open".into()),
            after: None,
            until: None,
            for_minutes: None,
            severity: AlertSeverity::Critical,
        })
        .await?;
    handle.rules_changed();

    // The door opens.
    door.attributes.insert("open".into(), json!(true));
    store.upsert_device(&door).await?;
    bus.publish(changed("garage_door", true))?;

    let Event::AlertRaised { alert, .. } = next_alert_event(&mut rx).await else {
        panic!("expected alert_raised first");
    };
    assert_eq!(alert.id, "rule:r1:garage_door");
    assert_eq!(alert.kind, AlertKind::Rule);
    assert_eq!(alert.message, "Garage Door open");
    assert_eq!(alert.severity, AlertSeverity::Critical);
    assert_eq!(handle.list().len(), 1, "the API's list has it");

    // On the wire it is named as clients expect.
    let wire = serde_json::to_value(Event::AlertRaised {
        timestamp: Utc::now(),
        alert: alert.clone(),
    })?;
    assert_eq!(wire["type"], "alert_raised");
    assert_eq!(wire["alert"]["kind"], "rule");
    assert_eq!(wire["alert"]["severity"], "critical");

    // The door closes.
    door.attributes.insert("open".into(), json!(false));
    store.upsert_device(&door).await?;
    bus.publish(changed("garage_door", false))?;

    let Event::AlertCleared {
        alert_id,
        device_id,
        ..
    } = next_alert_event(&mut rx).await
    else {
        panic!("expected alert_cleared");
    };
    assert_eq!(alert_id, "rule:r1:garage_door");
    assert_eq!(device_id, "garage_door");
    assert!(handle.list().is_empty());

    // Deleting a rule while its alert is up clears it too.
    door.attributes.insert("open".into(), json!(true));
    store.upsert_device(&door).await?;
    bus.publish(changed("garage_door", true))?;
    assert!(matches!(
        next_alert_event(&mut rx).await,
        Event::AlertRaised { .. }
    ));
    store.delete_alert_rule("r1").await?;
    handle.rules_changed();
    assert!(matches!(
        next_alert_event(&mut rx).await,
        Event::AlertCleared { .. }
    ));
    Ok(())
}
