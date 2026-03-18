//! Rule engine — listens on the event bus, evaluates rules, dispatches actions.

use crate::executor::execute_actions;
use crate::EventBus;
use anyhow::Result;
use hc_notify::NotificationService;
use hc_types::event::Event;
use hc_types::rule::{CompareOp, Condition, Rule, Trigger};
use hc_state::StateStore;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub struct RuleEngine {
    bus: EventBus,
    rules: Arc<RwLock<Vec<Rule>>>,
    state: StateStore,
    publish: Option<hc_mqtt_client::PublishHandle>,
    notify: Option<Arc<NotificationService>>,
}

impl RuleEngine {
    pub fn new(
        bus: EventBus,
        rules: Vec<Rule>,
        state: StateStore,
        publish: Option<hc_mqtt_client::PublishHandle>,
        notify: Option<Arc<NotificationService>>,
    ) -> Self {
        Self {
            bus,
            rules: Arc::new(RwLock::new(rules)),
            state,
            publish,
            notify,
        }
    }

    /// Returns a handle to update the live rule set without restart.
    pub fn rules_handle(&self) -> Arc<RwLock<Vec<Rule>>> {
        Arc::clone(&self.rules)
    }

    /// Drive the rule engine until the bus is dropped.
    pub async fn run(self) {
        let mut rx = self.bus.subscribe();
        info!("Rule engine started");
        loop {
            match rx.recv().await {
                Ok(event) => {
                    debug!("Rule engine received event");
                    if let Err(e) = self.handle_event(&event).await {
                        warn!(error = %e, "Rule engine error handling event");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Rule engine lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        info!("Rule engine stopped");
    }

    async fn handle_event(&self, event: &Event) -> Result<()> {
        let rules = self.rules.read().await;

        // For scheduler_tick events, match by rule_id embedded in the payload.
        if let Event::Custom { event_type, payload, .. } = event {
            if event_type == "scheduler_tick" {
                if let Some(rule_id_str) = payload.get("rule_id").and_then(|v| v.as_str()) {
                    if let Ok(rule_id) = uuid::Uuid::parse_str(rule_id_str) {
                        if let Some(rule) = rules.iter().find(|r| r.id == rule_id && r.enabled) {
                            self.fire_rule(rule).await?;
                        }
                    }
                }
                return Ok(());
            }
        }

        // Sort by priority descending for evaluation order.
        let mut matching: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.enabled && trigger_matches(&r.trigger, event))
            .collect();
        matching.sort_by(|a, b| b.priority.cmp(&a.priority));

        for rule in matching {
            self.fire_rule(rule).await?;
        }
        Ok(())
    }

    async fn fire_rule(&self, rule: &Rule) -> Result<()> {
        if !self.evaluate_conditions(&rule.conditions).await? {
            return Ok(());
        }
        info!(rule_id = %rule.id, rule_name = %rule.name, "Rule firing");
        let actions = rule.actions.clone();
        let publish = self.publish.clone();
        let state = self.state.clone();
        let bus = self.bus.clone();
        let rule_id = rule.id.to_string();
        let rule_name = rule.name.clone();
        let notify = self.notify.clone();
        tokio::spawn(async move {
            if let Err(e) = execute_actions(actions, publish, state, notify).await {
                warn!(rule = %rule_id, error = %e, "Action execution failed");
            }
            let _ = bus.publish(Event::RuleFired {
                timestamp: chrono::Utc::now(),
                rule_id,
                rule_name,
            });
        });
        Ok(())
    }

    async fn evaluate_conditions(&self, conditions: &[Condition]) -> Result<bool> {
        for cond in conditions {
            if !self.evaluate_one(cond).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn evaluate_one(&self, condition: &Condition) -> Result<bool> {
        match condition {
            Condition::DeviceState { device_id, attribute, op, value } => {
                let device = self.state.get_device(device_id).await?;
                let Some(device) = device else { return Ok(false) };
                let Some(actual) = device.attributes.get(attribute) else { return Ok(false) };
                Ok(compare(actual, op, value))
            }
            Condition::TimeWindow { start, end } => {
                let now = chrono::Local::now().time();
                if start <= end {
                    Ok(now >= *start && now <= *end)
                } else {
                    // Wraps midnight.
                    Ok(now >= *start || now <= *end)
                }
            }
            Condition::ScriptExpression { script } => {
                let script = script.clone();
                let result = tokio::task::spawn_blocking(move || {
                    hc_scripting::ScriptRuntime::new().eval_condition(&script)
                })
                .await??;
                Ok(result)
            }
        }
    }
}

/// Returns true if this event should cause the rule to be evaluated.
fn trigger_matches(trigger: &Trigger, event: &Event) -> bool {
    match (trigger, event) {
        (
            Trigger::DeviceStateChanged { device_id, attribute },
            Event::DeviceStateChanged { device_id: eid, current, .. },
        ) => {
            if device_id != eid {
                return false;
            }
            match attribute {
                None => true,
                Some(attr) => current.contains_key(attr),
            }
        }
        (Trigger::MqttMessage { topic_pattern }, Event::MqttMessage { topic, .. }) => {
            mqtt_topic_matches(topic_pattern, topic)
        }
        // Scheduler emits Custom{event_type:"scheduler_tick", payload:{rule_id:…}}
        // to trigger time-based rules by their specific ID.
        (
            Trigger::TimeOfDay { .. } | Trigger::SunEvent { .. },
            Event::Custom { event_type, .. },
        ) => {
            if event_type != "scheduler_tick" {
                return false;
            }
            // The scheduler encodes the rule ID in the payload.
            false // handled specially in handle_event via rule ID lookup
        }
        (Trigger::WebhookReceived { path: trigger_path }, Event::Custom { event_type, payload, .. }) => {
            event_type == "webhook"
                && payload.get("path").and_then(|v| v.as_str()) == Some(trigger_path.as_str())
        }
        (Trigger::ManualTrigger, _) => false,
        _ => false,
    }
}

/// MQTT wildcard topic matching (`+` = single level, `#` = multi-level).
fn mqtt_topic_matches(pattern: &str, topic: &str) -> bool {
    let mut pparts = pattern.split('/');
    let mut tparts = topic.split('/');
    loop {
        match (pparts.next(), tparts.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => continue,
            (Some(p), Some(t)) if p == t => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn compare(actual: &serde_json::Value, op: &CompareOp, expected: &serde_json::Value) -> bool {
    match op {
        CompareOp::Eq => actual == expected,
        CompareOp::Ne => actual != expected,
        CompareOp::Gt => num_cmp(actual, expected) == Some(std::cmp::Ordering::Greater),
        CompareOp::Gte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CompareOp::Lt => num_cmp(actual, expected) == Some(std::cmp::Ordering::Less),
        CompareOp::Lte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}

fn num_cmp(
    a: &serde_json::Value,
    b: &serde_json::Value,
) -> Option<std::cmp::Ordering> {
    let af = a.as_f64()?;
    let bf = b.as_f64()?;
    af.partial_cmp(&bf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn webhook_trigger_matches_correct_path() {
        let trigger = Trigger::WebhookReceived { path: "doorbell".into() };
        let event = Event::Custom {
            timestamp: Utc::now(),
            event_type: "webhook".into(),
            payload: json!({ "path": "doorbell", "body": {} }),
        };
        assert!(trigger_matches(&trigger, &event));
    }

    #[test]
    fn webhook_trigger_does_not_match_wrong_path() {
        let trigger = Trigger::WebhookReceived { path: "doorbell".into() };
        let event = Event::Custom {
            timestamp: Utc::now(),
            event_type: "webhook".into(),
            payload: json!({ "path": "motion", "body": {} }),
        };
        assert!(!trigger_matches(&trigger, &event));
    }

    #[test]
    fn mqtt_wildcard_hash_matches_any_suffix() {
        assert!(mqtt_topic_matches("homecore/#", "homecore/devices/light/state"));
        assert!(mqtt_topic_matches("homecore/#", "homecore/events/rule_fired"));
        assert!(!mqtt_topic_matches("homecore/#", "other/topic"));
    }

    #[test]
    fn mqtt_wildcard_plus_matches_single_level() {
        assert!(mqtt_topic_matches("homecore/devices/+/state", "homecore/devices/light/state"));
        assert!(!mqtt_topic_matches("homecore/devices/+/state", "homecore/devices/light/cmd"));
        assert!(!mqtt_topic_matches("homecore/devices/+/state", "homecore/devices/a/b/state"));
    }
}
