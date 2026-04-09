//! Rule generation and reconciliation helpers for criteria-driven modes.

use anyhow::{bail, Result};
use hc_core::{mode_manager::ModeConfig, rule_resolver};
use hc_types::rule::{
    Action, Condition, ModeCommand, PeriodicUnit, Rule, RuleAction, RunMode, Trigger,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use uuid::Uuid;

use crate::mode_definition_store::{CriteriaOffBehavior, ModeDefinition};
use crate::AppState;

#[derive(Debug, Clone)]
enum TriggerSource {
    SystemStarted,
    Periodic(u32),
    DeviceSet(Vec<String>),
    ModeChanged(String),
    HubVariableChanged(String),
}

#[derive(Debug, Default)]
struct ConditionRefs {
    device_refs: BTreeSet<String>,
    mode_refs: BTreeSet<String>,
    hub_variables: BTreeSet<String>,
}

fn collect_condition_refs(condition: &Condition, refs: &mut ConditionRefs) {
    match condition {
        Condition::DeviceState { device_id, .. }
        | Condition::TimeElapsed { device_id, .. }
        | Condition::DeviceLastChange { device_id, .. } => {
            refs.device_refs.insert(device_id.clone());
        }
        Condition::ModeIs { mode_id, .. } => {
            refs.mode_refs.insert(mode_id.clone());
        }
        Condition::HubVariable { name, .. } => {
            refs.hub_variables.insert(name.clone());
        }
        Condition::Not { condition } => collect_condition_refs(condition, refs),
        Condition::And { conditions }
        | Condition::Or { conditions }
        | Condition::Xor { conditions } => {
            for condition in conditions {
                collect_condition_refs(condition, refs);
            }
        }
        Condition::TimeWindow { .. }
        | Condition::ScriptExpression { .. }
        | Condition::PrivateBooleanIs { .. }
        | Condition::CalendarActive { .. } => {}
    }
}

fn criteria_refs(definition: &ModeDefinition) -> ConditionRefs {
    let mut refs = ConditionRefs::default();
    collect_condition_refs(&definition.criteria.on_condition, &mut refs);
    if let Some(condition) = &definition.criteria.off_condition {
        collect_condition_refs(condition, &mut refs);
    }
    refs
}

fn effective_off_condition(definition: &ModeDefinition) -> Result<Condition> {
    match definition.criteria.off_behavior {
        CriteriaOffBehavior::Inverse => Ok(Condition::Not {
            condition: Box::new(definition.criteria.on_condition.clone()),
        }),
        CriteriaOffBehavior::Explicit => definition
            .criteria
            .off_condition
            .clone()
            .ok_or_else(|| anyhow::anyhow!("explicit off behavior requires off_condition")),
    }
}

fn trigger_label(source: &TriggerSource) -> String {
    match source {
        TriggerSource::SystemStarted => "system_started".to_string(),
        TriggerSource::Periodic(minutes) => format!("periodic_{minutes}m"),
        TriggerSource::DeviceSet(_) => "device_changed".to_string(),
        TriggerSource::ModeChanged(mode_id) => format!("mode_changed_{mode_id}"),
        TriggerSource::HubVariableChanged(name) => format!("hub_variable_{name}"),
    }
}

fn build_trigger(source: &TriggerSource) -> Trigger {
    match source {
        TriggerSource::SystemStarted => Trigger::SystemStarted,
        TriggerSource::Periodic(minutes) => Trigger::Periodic {
            every_n: (*minutes).max(1),
            unit: PeriodicUnit::Minutes,
        },
        TriggerSource::DeviceSet(devices) => {
            let mut iter = devices.iter();
            let primary = iter.next().cloned().unwrap_or_default();
            let rest = iter.cloned().collect::<Vec<_>>();
            Trigger::DeviceStateChanged {
                device_id: primary,
                device_ids: rest,
                attribute: None,
                to: None,
                from: None,
                not_from: None,
                not_to: None,
                for_duration_secs: None,
                change_kind: None,
                change_source: None,
            }
        }
        TriggerSource::ModeChanged(mode_id) => Trigger::ModeChanged {
            mode_id: Some(mode_id.clone()),
            to: None,
        },
        TriggerSource::HubVariableChanged(name) => Trigger::HubVariableChanged {
            name: Some(name.clone()),
        },
    }
}

fn build_rule(
    mode: &ModeConfig,
    criteria_condition: Condition,
    mode_should_be_on: bool,
    source: &TriggerSource,
) -> Rule {
    let direction = if mode_should_be_on { "on" } else { "off" };
    let label = trigger_label(source);
    Rule {
        id: Uuid::new_v4(),
        name: format!("Managed Mode {} {} ({label})", mode.id, direction),
        enabled: true,
        priority: 100,
        tags: vec![
            "managed".to_string(),
            "managed_mode".to_string(),
            format!("mode:{}", mode.id),
        ],
        trigger: build_trigger(source),
        conditions: vec![
            Condition::ModeIs {
                mode_id: mode.id.clone(),
                on: !mode_should_be_on,
            },
            criteria_condition,
        ],
        actions: vec![RuleAction {
            enabled: true,
            action: Action::SetMode {
                mode_id: mode.id.clone(),
                command: if mode_should_be_on {
                    ModeCommand::On
                } else {
                    ModeCommand::Off
                },
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
        variables: Default::default(),
        trigger_label: Some(label),
        run_mode: RunMode::Parallel,
    }
}

pub fn validate_definition(
    mode: &ModeConfig,
    all_mode_ids: &HashSet<String>,
    existing_definitions: &[ModeDefinition],
    candidate: &ModeDefinition,
) -> Result<()> {
    if mode.kind != hc_core::mode_manager::ModeKind::Manual {
        bail!("criteria-driven definitions can only target manual modes");
    }
    if candidate.mode_id != mode.id {
        bail!("definition mode_id does not match target mode");
    }
    if candidate.criteria.reevaluate_every_n_minutes == 0 {
        bail!("reevaluate_every_n_minutes must be at least 1");
    }
    if matches!(
        candidate.criteria.off_behavior,
        CriteriaOffBehavior::Explicit
    ) && candidate.criteria.off_condition.is_none()
    {
        bail!("explicit off behavior requires off_condition");
    }

    let refs = criteria_refs(candidate);
    if refs.mode_refs.contains(&candidate.mode_id) {
        bail!("criteria-driven mode cannot reference itself");
    }
    for mode_id in &refs.mode_refs {
        if !all_mode_ids.contains(mode_id) {
            bail!("criteria references unknown mode '{mode_id}'");
        }
    }

    let mut definitions = existing_definitions.to_vec();
    if let Some(pos) = definitions.iter().position(|def| def.mode_id == candidate.mode_id) {
        definitions[pos] = candidate.clone();
    } else {
        definitions.push(candidate.clone());
    }
    ensure_acyclic(&definitions)
}

fn ensure_acyclic(definitions: &[ModeDefinition]) -> Result<()> {
    let managed_ids: HashSet<String> = definitions.iter().map(|def| def.mode_id.clone()).collect();
    let adjacency: BTreeMap<String, Vec<String>> = definitions
        .iter()
        .map(|definition| {
            let refs = criteria_refs(definition);
            let deps = refs
                .mode_refs
                .into_iter()
                .filter(|mode_id| managed_ids.contains(mode_id))
                .collect::<Vec<_>>();
            (definition.mode_id.clone(), deps)
        })
        .collect();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Done,
    }

    fn dfs(
        node: &str,
        adjacency: &BTreeMap<String, Vec<String>>,
        states: &mut BTreeMap<String, VisitState>,
    ) -> Result<()> {
        if matches!(states.get(node), Some(VisitState::Visiting)) {
            bail!("criteria-driven mode cycle detected at '{node}'");
        }
        if matches!(states.get(node), Some(VisitState::Done)) {
            return Ok(());
        }

        states.insert(node.to_string(), VisitState::Visiting);
        if let Some(next) = adjacency.get(node) {
            for dep in next {
                dfs(dep, adjacency, states)?;
            }
        }
        states.insert(node.to_string(), VisitState::Done);
        Ok(())
    }

    let mut states = BTreeMap::new();
    for node in adjacency.keys() {
        dfs(node, &adjacency, &mut states)?;
    }
    Ok(())
}

pub fn build_managed_rules(mode: &ModeConfig, definition: &ModeDefinition) -> Result<Vec<Rule>> {
    let refs = criteria_refs(definition);
    let mut triggers = vec![
        TriggerSource::SystemStarted,
        TriggerSource::Periodic(definition.criteria.reevaluate_every_n_minutes.max(1)),
    ];

    if !refs.device_refs.is_empty() {
        triggers.push(TriggerSource::DeviceSet(
            refs.device_refs.into_iter().collect(),
        ));
    }
    for mode_id in refs.mode_refs {
        triggers.push(TriggerSource::ModeChanged(mode_id));
    }
    for name in refs.hub_variables {
        triggers.push(TriggerSource::HubVariableChanged(name));
    }

    let on_condition = definition.criteria.on_condition.clone();
    let off_condition = effective_off_condition(definition)?;
    let mut rules = Vec::new();
    for trigger in triggers {
        rules.push(build_rule(mode, on_condition.clone(), true, &trigger));
        rules.push(build_rule(mode, off_condition.clone(), false, &trigger));
    }
    Ok(rules)
}

pub async fn install_managed_rules(
    state: &AppState,
    old_rule_ids: &[Uuid],
    rules: &[Rule],
) -> Result<Vec<Uuid>> {
    let Some(source_handle) = &state.source_rules_handle else {
        bail!("rule engine not available");
    };
    let Some(compiled_handle) = &state.rules_handle else {
        bail!("compiled rule handle not available");
    };
    let Some(file_store) = &state.rule_file_store else {
        bail!("rule file store not available");
    };

    let mut compiled_rules = Vec::with_capacity(rules.len());
    for rule in rules {
        compiled_rules.push(rule_resolver::compile_rule_for_store(&state.store, rule).await?);
    }

    for rule in rules {
        file_store.write_rule(rule)?;
    }

    let old_ids: HashSet<Uuid> = old_rule_ids.iter().copied().collect();
    {
        let mut current = source_handle.write().await;
        current.retain(|rule| !old_ids.contains(&rule.id));
        current.extend(rules.iter().cloned());
    }
    {
        let mut current = compiled_handle.write().await;
        current.retain(|rule| !old_ids.contains(&rule.id));
        current.extend(compiled_rules);
    }

    for id in old_rule_ids {
        let _ = file_store.delete_rule(*id);
    }

    Ok(rules.iter().map(|rule| rule.id).collect())
}

pub async fn remove_managed_rules(state: &AppState, rule_ids: &[Uuid]) -> Result<()> {
    let old_ids: HashSet<Uuid> = rule_ids.iter().copied().collect();
    if let Some(file_store) = &state.rule_file_store {
        for id in rule_ids {
            let _ = file_store.delete_rule(*id);
        }
    }
    if let Some(source_handle) = &state.source_rules_handle {
        source_handle
            .write()
            .await
            .retain(|rule| !old_ids.contains(&rule.id));
    }
    if let Some(compiled_handle) = &state.rules_handle {
        compiled_handle
            .write()
            .await
            .retain(|rule| !old_ids.contains(&rule.id));
    }
    Ok(())
}

pub fn managed_rule_owner<'a>(definitions: &'a [ModeDefinition], rule_id: Uuid) -> Option<&'a str> {
    definitions
        .iter()
        .find(|definition| definition.generated_rule_ids.contains(&rule_id))
        .map(|definition| definition.mode_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_definition_store::{CriteriaModeConfig, ModeDefinition};
    use hc_core::mode_manager::{ModeConfig, ModeKind};
    use hc_types::rule::CompareOp;

    fn manual_mode(id: &str) -> ModeConfig {
        ModeConfig {
            id: id.to_string(),
            name: id.to_string(),
            kind: ModeKind::Manual,
            on_event: None,
            off_event: None,
            on_offset_minutes: 0,
            off_offset_minutes: 0,
        }
    }

    #[test]
    fn cycle_detection_rejects_two_mode_loop() {
        let a = ModeDefinition {
            mode_id: "mode_a".to_string(),
            criteria: CriteriaModeConfig {
                on_condition: Condition::ModeIs {
                    mode_id: "mode_b".to_string(),
                    on: true,
                },
                off_behavior: CriteriaOffBehavior::Inverse,
                off_condition: None,
                reevaluate_every_n_minutes: 1,
            },
            generated_rule_ids: Vec::new(),
        };
        let b = ModeDefinition {
            mode_id: "mode_b".to_string(),
            criteria: CriteriaModeConfig {
                on_condition: Condition::ModeIs {
                    mode_id: "mode_a".to_string(),
                    on: true,
                },
                off_behavior: CriteriaOffBehavior::Inverse,
                off_condition: None,
                reevaluate_every_n_minutes: 1,
            },
            generated_rule_ids: Vec::new(),
        };

        let modes = HashSet::from([
            "mode_a".to_string(),
            "mode_b".to_string(),
            "mode_day".to_string(),
        ]);
        let result = validate_definition(&manual_mode("mode_a"), &modes, &[b], &a);
        assert!(result.is_err());
    }

    #[test]
    fn build_rules_adds_device_trigger_when_condition_references_device() {
        let mode = manual_mode("mode_party");
        let definition = ModeDefinition {
            mode_id: "mode_party".to_string(),
            criteria: CriteriaModeConfig {
                on_condition: Condition::DeviceState {
                    device_id: "sensor.motion".to_string(),
                    attribute: "motion".to_string(),
                    op: CompareOp::Eq,
                    value: serde_json::json!(true),
                },
                off_behavior: CriteriaOffBehavior::Inverse,
                off_condition: None,
                reevaluate_every_n_minutes: 1,
            },
            generated_rule_ids: Vec::new(),
        };

        let rules = build_managed_rules(&mode, &definition).expect("managed rules");
        assert!(rules.iter().any(|rule| matches!(
            rule.trigger,
            Trigger::DeviceStateChanged { .. }
        )));
    }
}
