use anyhow::{anyhow, Result};
use hc_state::StateStore;
use hc_types::{
    alert::AlertFilter,
    device::DeviceState,
    rule::{Action, Condition, IfCheck, Rule, RuleAction, Trigger},
};

use crate::device_naming::{canonical_name_base, normalize_name_segment};

#[derive(Debug, Clone)]
struct MatchCandidate {
    device_id: String,
    label: String,
}

fn name_matches(reference: &str, device: &DeviceState) -> bool {
    // Match either label a person could have written the rule against: the
    // plugin-delivered name, or a user's override of it. Matching only one
    // would break rules the moment a device was renamed on either side.
    let reference = normalize_name_segment(reference);
    reference == normalize_name_segment(&device.name)
        || device
            .name_override
            .as_deref()
            .is_some_and(|o| reference == normalize_name_segment(o))
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

/// Every place a rule names a device, handed to `f` one at a time so it can
/// read or rewrite the reference.
///
/// This is the single list of device fields. The resolver uses it to turn
/// names into ids, the stale reference report uses it to find ids that no
/// longer exist, and device delete and repoint use it to rewrite references.
/// A field missing here is missed by all of them alike, so a new device field
/// on a trigger, check or step belongs here.
pub fn visit_device_refs_mut(
    rule: &mut Rule,
    f: &mut dyn FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    for trigger in rule.all_triggers_mut() {
        visit_trigger(trigger, f)?;
    }
    for condition in &mut rule.conditions {
        visit_condition(condition, f)?;
    }
    for RuleAction { action, .. } in &mut rule.actions {
        visit_action(action, f)?;
    }
    Ok(())
}

/// Every device reference in `rule`, in the order [`visit_device_refs_mut`]
/// visits them, duplicates included.
pub fn device_refs(rule: &Rule) -> Vec<String> {
    let mut copy = rule.clone();
    let mut refs = Vec::new();
    let _ = visit_device_refs_mut(&mut copy, &mut |r| {
        refs.push(r.clone());
        Ok(())
    });
    refs
}

fn visit_filter(
    filter: &mut AlertFilter,
    f: &mut dyn FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    for device_ref in &mut filter.device_ids {
        f(device_ref)?;
    }
    Ok(())
}

fn visit_trigger(
    trigger: &mut Trigger,
    f: &mut dyn FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    match trigger {
        Trigger::DeviceStateChanged {
            device_id,
            device_ids,
            ..
        } => {
            f(device_id)?;
            for device_ref in device_ids {
                f(device_ref)?;
            }
        }
        Trigger::DeviceAvailabilityChanged { device_id, .. }
        | Trigger::ButtonEvent { device_id, .. }
        | Trigger::NumericThreshold { device_id, .. } => {
            f(device_id)?;
        }
        Trigger::DeviceBatteryLow {
            device_id: Some(device_id),
        }
        | Trigger::DeviceBatteryRecovered {
            device_id: Some(device_id),
        } => {
            f(device_id)?;
        }
        Trigger::AlertRaised { filter }
        | Trigger::AlertCleared { filter }
        | Trigger::AlertAcknowledged { filter }
        | Trigger::AlertActiveFor { filter, .. } => {
            visit_filter(filter, f)?;
        }
        _ => {}
    }
    Ok(())
}

fn visit_condition(
    condition: &mut Condition,
    f: &mut dyn FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    match condition {
        Condition::DeviceState { device_id, .. }
        | Condition::TimeElapsed { device_id, .. }
        | Condition::DeviceLastChange { device_id, .. } => {
            f(device_id)?;
        }
        Condition::AlertActive { filter, .. } | Condition::NoAlertActive { filter } => {
            visit_filter(filter, f)?;
        }
        Condition::Not { condition } => visit_condition(condition, f)?,
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => {
            for condition in conditions {
                visit_condition(condition, f)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn visit_actions(
    actions: &mut [Action],
    f: &mut dyn FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    for action in actions {
        visit_action(action, f)?;
    }
    Ok(())
}

fn visit_action(action: &mut Action, f: &mut dyn FnMut(&mut String) -> Result<()>) -> Result<()> {
    match action {
        Action::SetDeviceState { device_id, .. }
        | Action::SetDeviceStatePerMode { device_id, .. }
        | Action::FadeDevice { device_id, .. } => {
            f(device_id)?;
        }
        Action::WaitForEvent {
            device_id: Some(device_id),
            ..
        } => {
            f(device_id)?;
        }
        Action::Notify {
            picture_from: Some(device_id),
            ..
        } => {
            f(device_id)?;
        }
        Action::PlayFromRotation {
            device_id: Some(device_id),
            ..
        } => {
            f(device_id)?;
        }
        Action::CaptureDeviceState { device_ids, .. } => {
            for device_ref in device_ids {
                f(device_ref)?;
            }
        }
        Action::IgnoreAlerts { filter, .. } => visit_filter(filter, f)?,
        Action::ForEachAlert { filter, actions } => {
            visit_filter(filter, f)?;
            visit_actions(actions, f)?;
        }
        Action::WaitAlertCleared { otherwise, .. } => visit_actions(otherwise, f)?,
        Action::Parallel { actions }
        | Action::RepeatUntil { actions, .. }
        | Action::RepeatWhile { actions, .. }
        | Action::RepeatCount { actions, .. } => visit_actions(actions, f)?,
        Action::Conditional {
            condition,
            then_actions,
            else_if,
            else_actions,
        } => {
            if let IfCheck::Check(check) = condition {
                visit_condition(check, f)?;
            }
            visit_actions(then_actions, f)?;
            for branch in else_if {
                if let IfCheck::Check(check) = &mut branch.condition {
                    visit_condition(check, f)?;
                }
                visit_actions(&mut branch.actions, f)?;
            }
            visit_actions(else_actions, f)?;
        }
        Action::PingHost {
            then_actions,
            else_actions,
            ..
        } => {
            visit_actions(then_actions, f)?;
            visit_actions(else_actions, f)?;
        }
        _ => {}
    }
    Ok(())
}

pub fn compile_rule(rule: &Rule, devices: &[DeviceState]) -> Result<Rule> {
    // Kept after the time rule it started from was deleted: held off until
    // it is given another trigger.
    if let Some(why) = crate::alert_links::deleted_trigger(rule) {
        anyhow::bail!(why);
    }
    let mut compiled = rule.clone();
    compiled.error = None;

    visit_device_refs_mut(&mut compiled, &mut |reference| {
        resolve_reference_in_place(reference, devices)
    })?;

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
                change_kind: None,
                change_source: None,
            },
            triggers: Vec::new(),
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

    // ── Device fields the resolver used to skip ─────────────────────────────

    fn lamp_and_camera() -> Vec<DeviceState> {
        let mut lamp = DeviceState::new("lamp_1", "Floor Lamp", "plugin.test");
        lamp.canonical_name = Some("living_room.floor_lamp".into());
        let mut camera = DeviceState::new("cam_1", "Porch Camera", "plugin.test");
        camera.canonical_name = Some("porch.camera".into());
        vec![lamp, camera]
    }

    fn step(value: serde_json::Value) -> Action {
        serde_json::from_value(value).unwrap()
    }

    fn rule_with(trigger: Trigger, conditions: Vec<Condition>, actions: Vec<Action>) -> Rule {
        let mut rule = sample_rule("lamp_1");
        rule.trigger = trigger;
        rule.conditions = conditions;
        rule.actions = actions
            .into_iter()
            .map(|action| RuleAction {
                enabled: true,
                action,
            })
            .collect();
        rule
    }

    fn set_lamp(reference: &str) -> serde_json::Value {
        json!({ "SetDeviceState": { "device_id": reference, "state": { "on": true } } })
    }

    #[test]
    fn battery_triggers_resolve_names() {
        let devices = lamp_and_camera();
        for trigger in [
            json!({ "DeviceBatteryLow": { "device_id": "Floor Lamp" } }),
            json!({ "DeviceBatteryRecovered": { "device_id": "living_room.floor_lamp" } }),
        ] {
            let trigger: Trigger = serde_json::from_value(trigger).unwrap();
            let compiled = compile_rule(&rule_with(trigger, vec![], vec![]), &devices).unwrap();
            match compiled.trigger {
                Trigger::DeviceBatteryLow { device_id }
                | Trigger::DeviceBatteryRecovered { device_id } => {
                    assert_eq!(device_id.as_deref(), Some("lamp_1"));
                }
                other => panic!("unexpected trigger {other:?}"),
            }
        }

        // Left out, it still means any device.
        let any = Trigger::DeviceBatteryLow { device_id: None };
        let compiled = compile_rule(&rule_with(any.clone(), vec![], vec![]), &devices).unwrap();
        assert_eq!(compiled.trigger, any);
    }

    #[test]
    fn notify_picture_from_resolves_a_name() {
        let notify = step(json!({ "Notify": {
            "channel": "phone", "message": "Someone is here", "picture_from": "porch.camera"
        } }));
        let compiled = compile_rule(
            &rule_with(Trigger::default(), vec![], vec![notify]),
            &lamp_and_camera(),
        )
        .unwrap();
        match &compiled.actions[0].action {
            Action::Notify { picture_from, .. } => {
                assert_eq!(picture_from.as_deref(), Some("cam_1"))
            }
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn alert_filter_device_ids_resolve_everywhere_a_filter_appears() {
        let filter = json!({ "device_ids": ["Floor Lamp", "porch.camera"] });
        let trigger: Trigger =
            serde_json::from_value(json!({ "AlertRaised": { "filter": filter } })).unwrap();
        let condition: Condition =
            serde_json::from_value(json!({ "NoAlertActive": { "filter": filter } })).unwrap();
        let ignore = step(json!({ "IgnoreAlerts": { "filter": filter } }));

        let compiled = compile_rule(
            &rule_with(trigger, vec![condition], vec![ignore]),
            &lamp_and_camera(),
        )
        .unwrap();
        let want = vec!["lamp_1".to_string(), "cam_1".to_string()];
        match &compiled.trigger {
            Trigger::AlertRaised { filter } => assert_eq!(filter.device_ids, want),
            other => panic!("unexpected trigger {other:?}"),
        }
        match &compiled.conditions[0] {
            Condition::NoAlertActive { filter } => assert_eq!(filter.device_ids, want),
            other => panic!("unexpected condition {other:?}"),
        }
        match &compiled.actions[0].action {
            Action::IgnoreAlerts { filter, .. } => assert_eq!(filter.device_ids, want),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn steps_inside_for_each_alert_resolve_names() {
        let each = step(json!({ "ForEachAlert": {
            "filter": { "device_ids": ["porch.camera"] },
            "actions": [set_lamp("Floor Lamp")]
        } }));
        let compiled = compile_rule(
            &rule_with(Trigger::default(), vec![], vec![each]),
            &lamp_and_camera(),
        )
        .unwrap();
        let Action::ForEachAlert { filter, actions } = &compiled.actions[0].action else {
            panic!("unexpected action");
        };
        assert_eq!(filter.device_ids, ["cam_1"]);
        assert!(matches!(
            &actions[0],
            Action::SetDeviceState { device_id, .. } if device_id == "lamp_1"
        ));
    }

    #[test]
    fn steps_inside_wait_alert_cleared_otherwise_resolve_names() {
        // Nested two deep: the otherwise of a wait inside a for each.
        let each = step(json!({ "ForEachAlert": { "actions": [
            { "WaitAlertCleared": {
                "timeout_secs": 60,
                "otherwise": [set_lamp("living_room.floor_lamp")]
            } }
        ] } }));
        let compiled = compile_rule(
            &rule_with(Trigger::default(), vec![], vec![each]),
            &lamp_and_camera(),
        )
        .unwrap();
        let Action::ForEachAlert { actions, .. } = &compiled.actions[0].action else {
            panic!("unexpected action");
        };
        let Action::WaitAlertCleared { otherwise, .. } = &actions[0] else {
            panic!("unexpected nested action");
        };
        assert!(matches!(
            &otherwise[0],
            Action::SetDeviceState { device_id, .. } if device_id == "lamp_1"
        ));
    }

    #[test]
    fn an_unknown_name_in_a_newly_covered_field_breaks_the_rule() {
        let devices = lamp_and_camera();
        let rules = [
            rule_with(
                serde_json::from_value(json!({ "DeviceBatteryLow": { "device_id": "Gone" } }))
                    .unwrap(),
                vec![],
                vec![],
            ),
            rule_with(
                Trigger::default(),
                vec![],
                vec![step(json!({ "Notify": {
                    "channel": "phone", "message": "hi", "picture_from": "Gone"
                } }))],
            ),
            rule_with(
                Trigger::default(),
                vec![],
                vec![step(
                    json!({ "IgnoreAlerts": { "filter": { "device_ids": ["Gone"] } } }),
                )],
            ),
            rule_with(
                Trigger::default(),
                vec![],
                vec![step(
                    json!({ "ForEachAlert": { "actions": [set_lamp("Gone")] } }),
                )],
            ),
            rule_with(
                Trigger::default(),
                vec![],
                vec![step(json!({ "WaitAlertCleared": {
                    "timeout_secs": 5, "otherwise": [set_lamp("Gone")]
                } }))],
            ),
        ];
        for rule in super::compile_rules(rules.to_vec(), &devices) {
            assert!(!rule.enabled, "a rule naming a missing device is held off");
            let err = rule.error.expect("the reason is reported");
            assert!(err.contains("'Gone' did not match"), "{err}");
        }
    }

    #[test]
    fn device_refs_lists_the_newly_covered_fields() {
        let rule = rule_with(
            serde_json::from_value(json!({ "DeviceBatteryRecovered": { "device_id": "b1" } }))
                .unwrap(),
            vec![serde_json::from_value(
                json!({ "AlertActive": { "filter": { "device_ids": ["c1"] } } }),
            )
            .unwrap()],
            vec![
                step(json!({ "Notify": { "channel": "x", "message": "y", "picture_from": "p1" } })),
                step(json!({ "ForEachAlert": {
                    "filter": { "device_ids": ["f1"] },
                    "actions": [set_lamp("n1")]
                } })),
                step(json!({ "WaitAlertCleared": {
                    "timeout_secs": 5, "otherwise": [set_lamp("o1")]
                } })),
            ],
        );
        let mut refs = super::device_refs(&rule);
        refs.sort();
        assert_eq!(refs, ["b1", "c1", "f1", "n1", "o1", "p1"]);
    }
}
