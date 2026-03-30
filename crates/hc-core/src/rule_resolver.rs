use anyhow::{anyhow, Result};
use hc_state::StateStore;
use hc_types::{
    device::DeviceState,
    rule::{Action, Condition, Rule, RuleAction, Trigger},
};

use crate::device_naming::{canonical_name_base, normalize_name_segment};

#[derive(Debug, Clone)]
struct MatchCandidate {
    device_id: String,
    label: String,
}

fn name_matches(reference: &str, device: &DeviceState) -> bool {
    normalize_name_segment(reference) == normalize_name_segment(&device.name)
}

fn candidate_label(device: &DeviceState) -> String {
    device
        .canonical_name
        .clone()
        .unwrap_or_else(|| canonical_name_base(device))
}

fn resolve_reference(reference: &str, devices: &[DeviceState]) -> Result<String> {
    if let Some(device) = devices.iter().find(|device| device.device_id == reference) {
        return Ok(device.device_id.clone());
    }

    if let Some(device) = devices
        .iter()
        .find(|device| device.canonical_name.as_deref() == Some(reference))
    {
        return Ok(device.device_id.clone());
    }

    let matches: Vec<MatchCandidate> = devices
        .iter()
        .filter(|device| name_matches(reference, device))
        .map(|device| MatchCandidate {
            device_id: device.device_id.clone(),
            label: candidate_label(device),
        })
        .collect();

    match matches.as_slice() {
        [single] => Ok(single.device_id.clone()),
        [] => Err(anyhow!(
            "device reference '{}' did not match any known device",
            reference
        )),
        _ => Err(anyhow!(
            "device reference '{}' is ambiguous; matches {}",
            reference,
            matches
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn resolve_reference_in_place(reference: &mut String, devices: &[DeviceState]) -> Result<()> {
    *reference = resolve_reference(reference, devices)?;
    Ok(())
}

fn resolve_trigger(trigger: &mut Trigger, devices: &[DeviceState]) -> Result<()> {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id,
            device_ids,
            ..
        } => {
            resolve_reference_in_place(device_id, devices)?;
            for device_ref in device_ids {
                resolve_reference_in_place(device_ref, devices)?;
            }
        }
        Trigger::DeviceAvailabilityChanged { device_id, .. }
        | Trigger::ButtonEvent { device_id, .. }
        | Trigger::NumericThreshold { device_id, .. } => {
            resolve_reference_in_place(device_id, devices)?;
        }
        _ => {}
    }
    Ok(())
}

fn resolve_condition(condition: &mut Condition, devices: &[DeviceState]) -> Result<()> {
    match condition {
        Condition::DeviceState { device_id, .. } | Condition::TimeElapsed { device_id, .. } => {
            resolve_reference_in_place(device_id, devices)?;
        }
        Condition::Not { condition } => resolve_condition(condition, devices)?,
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => {
            for condition in conditions {
                resolve_condition(condition, devices)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_action(action: &mut Action, devices: &[DeviceState]) -> Result<()> {
    match action {
        Action::SetDeviceState { device_id, .. }
        | Action::SetDeviceStatePerMode { device_id, .. }
        | Action::FadeDevice { device_id, .. } => {
            resolve_reference_in_place(device_id, devices)?;
        }
        Action::WaitForEvent {
            device_id: Some(device_id),
            ..
        } => {
            resolve_reference_in_place(device_id, devices)?;
        }
        Action::CaptureDeviceState { device_ids, .. } => {
            for device_ref in device_ids {
                resolve_reference_in_place(device_ref, devices)?;
            }
        }
        Action::Parallel { actions }
        | Action::RepeatUntil { actions, .. }
        | Action::RepeatWhile { actions, .. }
        | Action::RepeatCount { actions, .. } => {
            for action in actions {
                resolve_action(action, devices)?;
            }
        }
        Action::Conditional {
            then_actions,
            else_if,
            else_actions,
            ..
        } => {
            for action in then_actions {
                resolve_action(action, devices)?;
            }
            for branch in else_if {
                for action in &mut branch.actions {
                    resolve_action(action, devices)?;
                }
            }
            for action in else_actions {
                resolve_action(action, devices)?;
            }
        }
        Action::PingHost {
            then_actions,
            else_actions,
            ..
        } => {
            for action in then_actions {
                resolve_action(action, devices)?;
            }
            for action in else_actions {
                resolve_action(action, devices)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn compile_rule(rule: &Rule, devices: &[DeviceState]) -> Result<Rule> {
    let mut compiled = rule.clone();
    compiled.error = None;

    resolve_trigger(&mut compiled.trigger, devices)?;
    for condition in &mut compiled.conditions {
        resolve_condition(condition, devices)?;
    }
    for RuleAction { action, .. } in &mut compiled.actions {
        resolve_action(action, devices)?;
    }

    Ok(compiled)
}

pub fn compile_rules(rules: Vec<Rule>, devices: &[DeviceState]) -> Vec<Rule> {
    rules
        .into_iter()
        .map(|rule| match compile_rule(&rule, devices) {
            Ok(compiled) => compiled,
            Err(err) => {
                let mut broken = rule;
                broken.enabled = false;
                broken.error = Some(err.to_string());
                broken
            }
        })
        .collect()
}

pub async fn compile_rule_for_store(store: &StateStore, rule: &Rule) -> Result<Rule> {
    let devices = store.list_devices().await?;
    compile_rule(rule, &devices)
}

pub async fn compile_rules_for_store(store: &StateStore, rules: Vec<Rule>) -> Result<Vec<Rule>> {
    let devices = store.list_devices().await?;
    Ok(compile_rules(rules, &devices))
}

pub fn reference_points_to_device(
    reference: &str,
    target_device_id: &str,
    devices: &[DeviceState],
) -> bool {
    resolve_reference(reference, devices)
        .map(|resolved| resolved == target_device_id)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{compile_rule, reference_points_to_device};
    use hc_types::{
        device::DeviceState,
        rule::{Action, CompareOp, Condition, Rule, RuleAction, RunMode, Trigger},
    };
    use serde_json::json;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn sample_rule(device_ref: &str) -> Rule {
        Rule {
            id: Uuid::new_v4(),
            name: "Test".into(),
            enabled: true,
            priority: 0,
            tags: vec![],
            trigger: Trigger::DeviceStateChanged {
                device_id: device_ref.into(),
                device_ids: vec![],
                attribute: Some("on".into()),
                to: Some(json!(true)),
                from: None,
                not_from: None,
                not_to: None,
                for_duration_secs: None,
            },
            conditions: vec![Condition::DeviceState {
                device_id: device_ref.into(),
                attribute: "on".into(),
                op: CompareOp::Eq,
                value: json!(true),
            }],
            actions: vec![RuleAction {
                enabled: true,
                action: Action::SetDeviceState {
                    device_id: device_ref.into(),
                    state: json!({"on": true}),
                    track_event_value: false,
                },
            }],
            error: None,
            cooldown_secs: None,
            log_events: false,
            log_triggers: false,
            log_actions: false,
            required_expression: None,
            cancel_on_false: false,
            trigger_condition: None,
            variables: HashMap::new(),
            trigger_label: None,
            run_mode: RunMode::Parallel,
        }
    }

    #[test]
    fn compiles_canonical_name_references() {
        let mut device = DeviceState::new("lamp_1", "Floor Lamp", "plugin.test");
        device.canonical_name = Some("living_room.floor_lamp".into());
        device.area = Some("living_room".into());

        let compiled = compile_rule(&sample_rule("living_room.floor_lamp"), &[device]).unwrap();
        match compiled.trigger {
            Trigger::DeviceStateChanged { device_id, .. } => assert_eq!(device_id, "lamp_1"),
            _ => panic!("unexpected trigger"),
        }
    }

    #[test]
    fn name_reference_must_be_unique() {
        let mut a = DeviceState::new("lamp_a", "Floor Lamp", "plugin.test");
        a.canonical_name = Some("living_room.floor_lamp".into());
        let mut b = DeviceState::new("lamp_b", "Floor Lamp", "plugin.test");
        b.canonical_name = Some("bedroom.floor_lamp".into());

        let err = compile_rule(&sample_rule("Floor Lamp"), &[a, b]).unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
    }

    #[test]
    fn delete_matching_checks_resolved_reference() {
        let mut device = DeviceState::new("lamp_1", "Floor Lamp", "plugin.test");
        device.canonical_name = Some("living_room.floor_lamp".into());

        assert!(reference_points_to_device(
            "living_room.floor_lamp",
            "lamp_1",
            &[device]
        ));
    }
}
