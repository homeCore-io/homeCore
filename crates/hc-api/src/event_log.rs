//! In-memory event log ring buffer.
//!
//! Stores the last `capacity` events from the event bus as JSON values.
//! Shared between the background subscriber task and the `GET /events` handler.
//!
//! Events excluded from the log: `MqttMessage` and `PluginHeartbeat`
//! (high-frequency, low value for the API consumer).

use hc_types::event::Event;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum events kept when no capacity is specified.
pub const DEFAULT_CAPACITY: usize = 1_000;

/// A single entry in the event log.
#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    /// Monotonically increasing sequence number (1-based).
    pub seq: u64,
    /// Snake-case event type name.
    pub event_type: String,
    /// Device ID, if this event is device-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Full event serialized as a JSON object.
    pub event: Value,
}

/// Bounded ring buffer of recent events.
#[derive(Clone)]
pub struct EventLog {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    capacity: usize,
    next_seq: u64,
    entries: VecDeque<LogEntry>,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                capacity,
                next_seq: 1,
                entries: VecDeque::with_capacity(capacity),
            })),
        }
    }

    /// Append an event to the log.  Drops the oldest entry if at capacity.
    /// Returns without storing if the event is filtered (e.g. MqttMessage).
    pub fn push(&self, event: &Event) {
        // Skip high-frequency / low-value events.
        if matches!(
            event,
            Event::MqttMessage { .. }
                | Event::RuleEvaluationFailed { .. }
                | Event::PluginHeartbeat { .. }
        ) {
            return;
        }

        let event_type = event_type_name(event).to_string();
        let device_id = event_device_id(event).map(str::to_string);
        let json = match serde_json::to_value(event) {
            Ok(v) => v,
            Err(_) => return,
        };

        let mut g = self.inner.lock().unwrap();
        let seq = g.next_seq;
        g.next_seq += 1;

        if g.entries.len() == g.capacity {
            g.entries.pop_front();
        }
        g.entries.push_back(LogEntry {
            seq,
            event_type,
            device_id,
            event: json,
        });
    }

    /// Query the log.  Results are newest-first.
    pub fn query(&self, filter: &EventLogQuery) -> Vec<LogEntry> {
        let g = self.inner.lock().unwrap();
        let limit = filter.limit.unwrap_or(50).min(1_000) as usize;

        let type_filter: Option<Vec<&str>> = filter.event_type.as_deref().map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect()
        });

        g.entries
            .iter()
            .rev()
            .filter(|e| {
                // event_type filter
                if let Some(ref types) = type_filter {
                    if !types.contains(&e.event_type.as_str()) {
                        return false;
                    }
                }
                // device_id filter
                if let Some(ref wanted) = filter.device_id {
                    if e.device_id.as_deref() != Some(wanted.as_str()) {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Query parameters for `GET /api/v1/events`.
#[derive(serde::Deserialize, Default)]
pub struct EventLogQuery {
    /// Maximum number of events to return (default 50, max 1000).
    pub limit: Option<u32>,
    /// Comma-separated event type names to include.
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    /// Only return events for this device ID.
    pub device_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by ws.rs)
// ---------------------------------------------------------------------------

/// Extract the device_id from events that carry one.
pub fn event_device_id(event: &Event) -> Option<&str> {
    match event {
        Event::DeviceStateChanged { device_id, .. }
        | Event::DeviceAvailabilityChanged { device_id, .. }
        | Event::DeviceNameChanged { device_id, .. }
        | Event::DeviceCommandSent { device_id, .. } => Some(device_id),
        Event::TimerStateChanged { timer_id, .. } => Some(timer_id),
        _ => None,
    }
}

/// Canonical snake_case name for each event variant.
pub fn event_type_name(event: &Event) -> &'static str {
    match event {
        Event::MqttMessage { .. } => "mqtt_message",
        Event::DeviceStateChanged { .. } => "device_state_changed",
        Event::DeviceAvailabilityChanged { .. } => "device_availability_changed",
        Event::RuleFired { .. } => "rule_fired",
        Event::SceneActivated { .. } => "scene_activated",
        Event::PluginRegistered { .. } => "plugin_registered",
        Event::PluginOffline { .. } => "plugin_offline",
        Event::PluginHeartbeat { .. } => "plugin_heartbeat",
        Event::PluginStatusChanged { .. } => "plugin_status_changed",
        Event::DeviceNameChanged { .. } => "device_name_changed",
        Event::Custom { .. } => "custom",
        Event::SystemAlert { .. } => "system_alert",
        Event::RuleEvaluationFailed { .. } => "rule_evaluation_failed",
        Event::ActionFailed { .. } => "action_failed",
        Event::DeviceCommandSent { .. } => "device_command_sent",
        Event::ModeChanged { .. } => "mode_changed",
        Event::TimerStateChanged { .. } => "timer_state_changed",
        Event::PluginCapabilities { .. } => "plugin_capabilities",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use hc_types::device::DeviceChange;

    fn rule_fired(rule_id: &str) -> Event {
        Event::RuleFired {
            timestamp: Utc::now(),
            rule_id: rule_id.to_string(),
            rule_name: "test rule".to_string(),
            trigger_type: "ManualTrigger".to_string(),
            action_count: 0,
            elapsed_ms: None,
            correlation_id: None,
        }
    }

    fn device_changed(device_id: &str) -> Event {
        Event::DeviceStateChanged {
            timestamp: Utc::now(),
            device_id: device_id.to_string(),
            device_name: None,
            previous: Default::default(),
            current: Default::default(),
            changed: Default::default(),
            change: DeviceChange::unknown(),
        }
    }

    fn mqtt_msg() -> Event {
        Event::MqttMessage {
            timestamp: Utc::now(),
            topic: "test/topic".to_string(),
            payload: vec![],
            retain: false,
        }
    }

    #[test]
    fn push_and_query_all() {
        let log = EventLog::new(100);
        log.push(&rule_fired("r1"));
        log.push(&rule_fired("r2"));
        let results = log.query(&EventLogQuery::default());
        assert_eq!(results.len(), 2);
        // newest first
        assert_eq!(results[0].seq, 2);
        assert_eq!(results[1].seq, 1);
    }

    #[test]
    fn mqtt_messages_excluded() {
        let log = EventLog::new(100);
        log.push(&mqtt_msg());
        log.push(&rule_fired("r1"));
        let results = log.query(&EventLogQuery::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "rule_fired");
    }

    #[test]
    fn capacity_evicts_oldest() {
        let log = EventLog::new(3);
        for i in 0..5u64 {
            log.push(&rule_fired(&i.to_string()));
        }
        let results = log.query(&EventLogQuery::default());
        // Only last 3 kept, newest first → seqs 5, 4, 3
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].seq, 5);
        assert_eq!(results[2].seq, 3);
    }

    #[test]
    fn filter_by_event_type() {
        let log = EventLog::new(100);
        log.push(&rule_fired("r1"));
        log.push(&device_changed("light.1"));
        let results = log.query(&EventLogQuery {
            event_type: Some("rule_fired".into()),
            ..Default::default()
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "rule_fired");
    }

    #[test]
    fn filter_by_device_id() {
        let log = EventLog::new(100);
        log.push(&device_changed("light.1"));
        log.push(&device_changed("light.2"));
        log.push(&rule_fired("r1"));
        let results = log.query(&EventLogQuery {
            device_id: Some("light.1".into()),
            ..Default::default()
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].device_id, Some("light.1".into()));
    }

    #[test]
    fn limit_applied() {
        let log = EventLog::new(100);
        for i in 0..20u64 {
            log.push(&rule_fired(&i.to_string()));
        }
        let results = log.query(&EventLogQuery {
            limit: Some(5),
            ..Default::default()
        });
        assert_eq!(results.len(), 5);
        // Should be the 5 most recent
        assert_eq!(results[0].seq, 20);
    }

    #[test]
    fn limit_capped_at_1000() {
        let log = EventLog::new(2000);
        for i in 0..1500u64 {
            log.push(&rule_fired(&i.to_string()));
        }
        // Requesting more than 1000 should be capped
        let results = log.query(&EventLogQuery {
            limit: Some(9999),
            ..Default::default()
        });
        assert_eq!(results.len(), 1000);
    }

    #[test]
    fn multi_type_filter() {
        let log = EventLog::new(100);
        log.push(&rule_fired("r1"));
        log.push(&device_changed("light.1"));
        log.push(&Event::SceneActivated {
            timestamp: Utc::now(),
            scene_id: "s1".to_string(),
            scene_name: "Evening".to_string(),
        });
        let results = log.query(&EventLogQuery {
            event_type: Some("rule_fired,scene_activated".into()),
            ..Default::default()
        });
        assert_eq!(results.len(), 2);
    }
}
