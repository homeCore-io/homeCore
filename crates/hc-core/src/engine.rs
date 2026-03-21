//! Rule engine — listens on the event bus, evaluates rules, dispatches actions.

use crate::executor::execute_actions;
use crate::EventBus;
use anyhow::Result;
use hc_notify::NotificationService;
use hc_state::StateStore;
use hc_types::event::Event;
use hc_types::rule::{CompareOp, Condition, Rule, Trigger};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
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

        // Scheduler tick events are dispatched to a specific rule by ID.
        if let Event::Custom { event_type, payload, .. } = event {
            if event_type == "scheduler_tick" {
                if let Some(rule_id_str) = payload.get("rule_id").and_then(|v| v.as_str()) {
                    if let Ok(rule_id) = uuid::Uuid::parse_str(rule_id_str) {
                        if let Some(rule) = rules.iter().find(|r| r.id == rule_id && r.enabled) {
                            debug!(
                                rule_name = %rule.name,
                                rule_id   = %rule.id,
                                "rule.trigger: scheduler_tick matched"
                            );
                            self.fire_rule(rule).await?;
                        } else {
                            debug!(rule_id = %rule_id_str, "rule.trigger: scheduler_tick — no matching enabled rule");
                        }
                    }
                }
                return Ok(());
            }
        }

        log_incoming_event(event);

        // Check every enabled rule's trigger, log each result.
        let mut matching: Vec<&Rule> = Vec::new();
        for rule in rules.iter() {
            if !rule.enabled {
                debug!(rule_name = %rule.name, "rule.trigger: SKIP (disabled)");
                continue;
            }
            match trigger_check(&rule.trigger, event) {
                TriggerResult::Matched => {
                    debug!(
                        rule_name = %rule.name,
                        rule_id   = %rule.id,
                        trigger   = trigger_type(&rule.trigger),
                        matched   = true,
                        "rule.trigger"
                    );
                    matching.push(rule);
                }
                TriggerResult::NoMatch(reason) => {
                    debug!(
                        rule_name = %rule.name,
                        rule_id   = %rule.id,
                        trigger   = trigger_type(&rule.trigger),
                        matched   = false,
                        reason,
                        "rule.trigger"
                    );
                }
            }
        }

        if matching.is_empty() {
            debug!("rule.trigger: no rules matched this event");
            return Ok(());
        }

        // Higher priority rules fire first.
        matching.sort_by(|a, b| b.priority.cmp(&a.priority));
        debug!(count = matching.len(), "rule.trigger: {} rule(s) matched, evaluating conditions", matching.len());

        for rule in matching {
            self.fire_rule(rule).await?;
        }
        Ok(())
    }

    async fn fire_rule(&self, rule: &Rule) -> Result<()> {
        let eval_start = Instant::now();

        match self.evaluate_conditions(rule).await? {
            Some(failed_idx) => {
                let eval_ms = eval_start.elapsed().as_millis();
                info!(
                    rule_name       = %rule.name,
                    rule_id         = %rule.id,
                    fired           = false,
                    reason          = "condition_failed",
                    failed_cond     = failed_idx,
                    conditions      = rule.conditions.len(),
                    eval_ms,
                    "rule.eval"
                );
                return Ok(());
            }
            None => {
                // All conditions passed (or there were none).
            }
        }

        let eval_ms = eval_start.elapsed().as_millis();
        info!(
            rule_name  = %rule.name,
            rule_id    = %rule.id,
            fired      = true,
            conditions = rule.conditions.len(),
            actions    = rule.actions.len(),
            eval_ms,
            "rule.eval"
        );

        let actions = rule.actions.clone();
        let publish = self.publish.clone();
        let state = self.state.clone();
        let bus = self.bus.clone();
        let rule_id = rule.id.to_string();
        let rule_name = rule.name.clone();
        let notify = self.notify.clone();

        tokio::spawn(async move {
            let action_start = Instant::now();
            debug!(
                rule_name = %rule_name,
                rule_id   = %rule_id,
                count     = actions.len(),
                "rule.actions: starting"
            );
            match execute_actions(actions, publish, state, notify).await {
                Ok(()) => {
                    let action_ms = action_start.elapsed().as_millis();
                    info!(
                        rule_name = %rule_name,
                        rule_id   = %rule_id,
                        action_ms,
                        "rule.actions: completed"
                    );
                }
                Err(e) => {
                    warn!(
                        rule_name = %rule_name,
                        rule_id   = %rule_id,
                        error     = %e,
                        "rule.actions: failed"
                    );
                }
            }
            let _ = bus.publish(Event::RuleFired {
                timestamp: chrono::Utc::now(),
                rule_id,
                rule_name,
            });
        });
        Ok(())
    }

    /// Evaluate all conditions for a rule.
    ///
    /// Returns `None` if all conditions pass (fire the rule).
    /// Returns `Some(i)` if condition `i` failed (skip the rule).
    async fn evaluate_conditions(&self, rule: &Rule) -> Result<Option<usize>> {
        if rule.conditions.is_empty() {
            debug!(rule_name = %rule.name, "rule.conditions: none — auto-pass");
            return Ok(None);
        }

        debug!(
            rule_name = %rule.name,
            count     = rule.conditions.len(),
            "rule.conditions: evaluating"
        );

        for (i, cond) in rule.conditions.iter().enumerate() {
            let passed = self.evaluate_one(&rule.name, i, rule.conditions.len(), cond).await?;
            if !passed {
                return Ok(Some(i));
            }
        }

        debug!(rule_name = %rule.name, "rule.conditions: all passed");
        Ok(None)
    }

    async fn evaluate_one(
        &self,
        rule_name: &str,
        idx: usize,
        total: usize,
        condition: &Condition,
    ) -> Result<bool> {
        match condition {
            Condition::DeviceState { device_id, attribute, op, value } => {
                let device = self.state.get_device(device_id).await?;
                let Some(device) = device else {
                    info!(
                        rule_name,
                        cond      = format!("{}/{}", idx + 1, total),
                        device_id,
                        "rule.condition: FAIL — device not found"
                    );
                    return Ok(false);
                };
                let Some(actual) = device.attributes.get(attribute) else {
                    info!(
                        rule_name,
                        cond      = format!("{}/{}", idx + 1, total),
                        device_id,
                        attribute,
                        "rule.condition: FAIL — attribute not present on device"
                    );
                    return Ok(false);
                };
                let result = compare(actual, op, value);
                if result {
                    debug!(
                        rule_name,
                        cond     = format!("{}/{}", idx + 1, total),
                        device_id,
                        attribute,
                        op       = ?op,
                        expected = %value,
                        actual   = %actual,
                        "rule.condition: pass"
                    );
                } else {
                    info!(
                        rule_name,
                        cond     = format!("{}/{}", idx + 1, total),
                        device_id,
                        attribute,
                        op       = ?op,
                        expected = %value,
                        actual   = %actual,
                        "rule.condition: FAIL"
                    );
                }
                Ok(result)
            }

            Condition::TimeWindow { start, end } => {
                let now = chrono::Local::now().time();
                let result = if start <= end {
                    now >= *start && now <= *end
                } else {
                    now >= *start || now <= *end
                };
                debug!(
                    rule_name,
                    cond   = format!("{}/{}", idx + 1, total),
                    %start,
                    %end,
                    now    = %now,
                    result,
                    "rule.condition: TimeWindow"
                );
                Ok(result)
            }

            Condition::ScriptExpression { script } => {
                let snippet = if script.len() > 80 { &script[..80] } else { script };
                debug!(
                    rule_name,
                    cond   = format!("{}/{}", idx + 1, total),
                    script = %snippet,
                    "rule.condition: ScriptExpression — evaluating"
                );
                // Snapshot all device states so the script can call device_state("id").
                let snapshot = device_snapshot(&self.state).await;
                let script = script.clone();
                let result = tokio::task::spawn_blocking(move || {
                    hc_scripting::ScriptRuntime::new_with_devices(snapshot)
                        .eval_condition(&script)
                })
                .await??;
                debug!(
                    rule_name,
                    cond   = format!("{}/{}", idx + 1, total),
                    result,
                    "rule.condition: ScriptExpression result"
                );
                Ok(result)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a device-id → attributes snapshot for Rhai script access.
async fn device_snapshot(state: &StateStore) -> HashMap<String, serde_json::Value> {
    match state.list_devices().await {
        Ok(devices) => devices
            .into_iter()
            .map(|d| {
                let attrs = serde_json::Value::Object(d.attributes.into_iter().collect());
                (d.device_id, attrs)
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "device_snapshot: list_devices failed; scripts will see empty state");
            HashMap::new()
        }
    }
}

/// Short human-readable label for the trigger variant (for log fields).
fn trigger_type(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::DeviceStateChanged { .. } => "DeviceStateChanged",
        Trigger::MqttMessage { .. }        => "MqttMessage",
        Trigger::TimeOfDay { .. }          => "TimeOfDay",
        Trigger::SunEvent { .. }           => "SunEvent",
        Trigger::WebhookReceived { .. }    => "WebhookReceived",
        Trigger::ManualTrigger             => "ManualTrigger",
    }
}

/// Log key fields from the incoming event so the rules log shows what arrived.
fn log_incoming_event(event: &Event) {
    match event {
        Event::DeviceStateChanged { device_id, current, previous, .. } => {
            // Only log attributes that actually changed.
            let changes: Vec<String> = current
                .keys()
                .filter(|k| previous.get(*k) != current.get(*k))
                .map(|k| {
                    let prev = previous.get(k).map(|v| v.to_string()).unwrap_or_else(|| "(none)".into());
                    let curr = current.get(k).map(|v| v.to_string()).unwrap_or_else(|| "(none)".into());
                    format!("{k}: {prev} → {curr}")
                })
                .collect();
            debug!(
                device_id,
                changes = %changes.join(", "),
                "rule.event: DeviceStateChanged"
            );
        }
        Event::DeviceAvailabilityChanged { device_id, available, .. } => {
            debug!(device_id, available, "rule.event: DeviceAvailabilityChanged");
        }
        Event::MqttMessage { topic, .. } => {
            debug!(topic, "rule.event: MqttMessage");
        }
        Event::Custom { event_type, .. } => {
            debug!(event_type, "rule.event: Custom");
        }
        _ => {}
    }
}

enum TriggerResult {
    Matched,
    NoMatch(&'static str),
}

/// Check whether an event matches a trigger, returning a reason on mismatch for logging.
fn trigger_check(trigger: &Trigger, event: &Event) -> TriggerResult {
    use TriggerResult::*;
    match (trigger, event) {
        (
            Trigger::DeviceStateChanged { device_id, attribute },
            Event::DeviceStateChanged { device_id: eid, current, previous, .. },
        ) => {
            if device_id != eid {
                return NoMatch("device_id mismatch");
            }
            match attribute {
                None => Matched,
                Some(attr) => {
                    if !current.contains_key(attr.as_str()) {
                        return NoMatch("attribute not in device state");
                    }
                    // Only fire when the attribute value actually changed.
                    if previous.get(attr.as_str()) == current.get(attr.as_str()) {
                        return NoMatch("attribute value unchanged");
                    }
                    Matched
                }
            }
        }
        (_, Event::DeviceStateChanged { .. }) => NoMatch("wrong trigger type for DeviceStateChanged event"),
        (Trigger::MqttMessage { topic_pattern }, Event::MqttMessage { topic, .. }) => {
            if mqtt_topic_matches(topic_pattern, topic) {
                Matched
            } else {
                NoMatch("topic pattern mismatch")
            }
        }
        // Scheduler emits Custom{event_type:"scheduler_tick"} — handled separately above.
        (Trigger::TimeOfDay { .. } | Trigger::SunEvent { .. }, _) => NoMatch("handled by scheduler"),
        (Trigger::WebhookReceived { path: trigger_path }, Event::Custom { event_type, payload, .. }) => {
            if event_type != "webhook" {
                return NoMatch("not a webhook event");
            }
            if payload.get("path").and_then(|v| v.as_str()) == Some(trigger_path.as_str()) {
                Matched
            } else {
                NoMatch("webhook path mismatch")
            }
        }
        (Trigger::ManualTrigger, _) => NoMatch("manual trigger only fires via API"),
        _ => NoMatch("event type does not match trigger type"),
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
        CompareOp::Eq  => actual == expected,
        CompareOp::Ne  => actual != expected,
        CompareOp::Gt  => num_cmp(actual, expected) == Some(std::cmp::Ordering::Greater),
        CompareOp::Gte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
        CompareOp::Lt  => num_cmp(actual, expected) == Some(std::cmp::Ordering::Less),
        CompareOp::Lte => matches!(
            num_cmp(actual, expected),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
    }
}

fn num_cmp(a: &serde_json::Value, b: &serde_json::Value) -> Option<std::cmp::Ordering> {
    let af = a.as_f64()?;
    let bf = b.as_f64()?;
    af.partial_cmp(&bf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        assert!(matches!(trigger_check(&trigger, &event), TriggerResult::Matched));
    }

    #[test]
    fn webhook_trigger_does_not_match_wrong_path() {
        let trigger = Trigger::WebhookReceived { path: "doorbell".into() };
        let event = Event::Custom {
            timestamp: Utc::now(),
            event_type: "webhook".into(),
            payload: json!({ "path": "motion", "body": {} }),
        };
        assert!(matches!(trigger_check(&trigger, &event), TriggerResult::NoMatch(_)));
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
