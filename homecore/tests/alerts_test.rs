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
    alerts::spawn(
        bus.clone(),
        store.clone(),
        handle.clone(),
        None,
        alerts::AlertNotify::default(),
    );

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
            op: Default::default(),
            notify: None,
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
        update: false,
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

/// A plugin's alert, from its manifest and its report to the bus: raised as
/// the house chose (a kind that is off by default, turned on), cleared — with
/// the alert carried, so a rule can tell which — when the plugin stops saying it.
#[tokio::test]
async fn a_plugin_alert_is_raised_and_cleared_from_what_the_plugin_says() -> Result<()> {
    use hc_types::alert::{AlertTopicSetting, PluginAlert, PluginAlertKind};
    let dir = tempfile::tempdir()?;
    let store = StateStore::open(
        &dir.path().join("state.redb").to_string_lossy(),
        &dir.path().join("history.db").to_string_lossy(),
    )
    .await?;
    let mut strip = DeviceState::new("wled_1", "Eaves lights", "plugin.wled");
    strip.available = true;
    store.upsert_device(&strip).await?;
    store
        .put_alert_topic_setting(AlertTopicSetting {
            plugin_id: "plugin.wled".into(),
            topic: "firmware_update".into(),
            enabled: Some(true),
            notify: None,
        })
        .await?;

    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let handle = AlertsHandle::new();
    alerts::spawn(
        bus.clone(),
        store.clone(),
        handle.clone(),
        None,
        alerts::AlertNotify::default(),
    );
    // Let the engine subscribe before the plugin speaks.
    tokio::time::sleep(Duration::from_millis(100)).await;

    bus.publish(Event::PluginCapabilities {
        timestamp: Utc::now(),
        plugin_id: "plugin.wled".into(),
        capabilities: hc_types::Capabilities {
            spec: "1".into(),
            plugin_id: "plugin.wled".into(),
            actions: vec![],
        },
        widgets: vec![],
        config_schema: None,
        config_descriptor: None,
        ui: None,
        alert_kinds: vec![PluginAlertKind {
            topic: "firmware_update".into(),
            label: "Firmware update available".into(),
            description: None,
            severity: AlertSeverity::Info,
            default_on: false,
        }],
    })?;
    let say = |alerts: Vec<PluginAlert>| Event::PluginAlerts {
        timestamp: Utc::now(),
        plugin_id: "plugin.wled".into(),
        device_id: "wled_1".into(),
        alerts,
    };
    bus.publish(say(vec![PluginAlert::new(
        "firmware_update",
        "Firmware 17.0.0 is out",
    )
    .key("17.0.0")]))?;

    let Event::AlertRaised { alert, .. } = next_alert_event(&mut rx).await else {
        panic!("expected alert_raised first");
    };
    assert_eq!(alert.kind, AlertKind::Plugin);
    assert_eq!(alert.id, "plugin:plugin.wled:firmware_update:wled_1:17.0.0");
    assert_eq!(alert.message, "Eaves lights: firmware 17.0.0 is out");
    assert_eq!(alert.severity, AlertSeverity::Info);
    assert_eq!(handle.list().len(), 1);
    assert_eq!(handle.plugin_kinds()["plugin.wled"].len(), 1);

    bus.publish(say(vec![]))?;
    let Event::AlertCleared {
        alert_id, alert, ..
    } = next_alert_event(&mut rx).await
    else {
        panic!("expected alert_cleared");
    };
    assert_eq!(alert_id, "plugin:plugin.wled:firmware_update:wled_1:17.0.0");
    assert_eq!(alert.unwrap().topic.as_deref(), Some("firmware_update"));
    assert!(handle.list().is_empty());
    Ok(())
}
