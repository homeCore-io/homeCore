//! The rules every homeCore plugin follows, as a function call.
//!
//! **These rules were already executable — they just were not reusable.** Nine
//! plugins had independently hand-written `every_boolean_names_both_of_its_states`,
//! two had written `every_published_attribute_is_declared` under two different
//! names, and two more had written the action-timeout check. A fourteenth
//! plugin had to rediscover nine tests it could not see from the
//! documentation, and a checklist in prose is something an author can skip
//! without noticing.
//!
//! So each rule below is one a plugin can fail its own build on:
//!
//! ```no_run
//! # use plugin_sdk_rs::types::schema::DeviceSchema;
//! # fn my_schema() -> DeviceSchema { DeviceSchema::default() }
//! #[test]
//! fn the_schema_says_what_it_should() {
//!     plugin_sdk_rs::conformance::check_device_schema(&my_schema()).assert_ok();
//! }
//! ```
//!
//! Same shape as [`config_descriptor::missing_schema_coverage`], which has
//! guarded the config half since before this existed.
//!
//! [`config_descriptor::missing_schema_coverage`]: crate::config_descriptor::missing_schema_coverage

use crate::types::schema::{AttributeCategory, AttributeKind, DeviceSchema};
use crate::types::Capabilities;
use serde_json::Value;

/// What a check found. Empty means conformant.
#[derive(Debug, Default)]
pub struct Findings(Vec<String>);

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Panic with every finding at once.
    ///
    /// All of them, not the first: fixing one at a time and re-running is the
    /// slow way to learn you had four.
    #[track_caller]
    pub fn assert_ok(&self) {
        if !self.is_empty() {
            panic!(
                "{} conformance finding(s):\n  - {}",
                self.0.len(),
                self.0.join("\n  - ")
            );
        }
    }

    fn push(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }

    fn merge(&mut self, other: Findings) {
        self.0.extend(other.0);
    }
}

/// Check a device's declaration.
///
/// - **Every boolean names both of its states.** A boolean is two events, not
///   one; without `states` a client offering "when the door closes" has to
///   synthesise it as "open, but Not".
/// - **Every enum offers options**, or it is a control with nothing in it.
/// - **Names are `snake_case` with no dots.** Nothing in homeCore treats a dot
///   specially, but no other plugin uses one and a client humanising
///   `led.count` renders "Led.count".
/// - **Housekeeping says so.** An attribute the shared lexicon recognises —
///   battery, rssi, firmware, ip, a `*_unit` sibling, anything `supports_*` —
///   that carries no `category` is declared as primary as the reading it sits
///   beside.
/// - **A range is a range.** `min` above `max` draws a slider that cannot move.
pub fn check_device_schema(schema: &DeviceSchema) -> Findings {
    let mut f = Findings::default();

    for (name, attr) in &schema.attributes {
        if name.contains('.') {
            f.push(format!(
                "attribute `{name}` contains a dot; homeCore attribute names are snake_case"
            ));
        }
        if name.chars().any(|c| c.is_uppercase() || c.is_whitespace()) {
            f.push(format!(
                "attribute `{name}` is not snake_case (uppercase or whitespace)"
            ));
        }

        if matches!(attr.kind, AttributeKind::Bool) {
            match &attr.states {
                None => f.push(format!(
                    "boolean `{name}` names neither of its states; a client can only \
                     offer it as \"{name}, but Not\""
                )),
                Some(s) if s.when_true.label == s.when_false.label => f.push(format!(
                    "boolean `{name}` gives both states the same name (`{}`)",
                    s.when_true.label
                )),
                Some(s) if s.when_true.label.is_empty() || s.when_false.label.is_empty() => f.push(
                    format!("boolean `{name}` leaves one of its state names empty"),
                ),
                _ => {}
            }
        }

        if matches!(attr.kind, AttributeKind::Enum)
            && attr.options.as_ref().is_none_or(|o| o.is_empty())
        {
            f.push(format!(
                "enum `{name}` declares no options; a client has nothing to offer"
            ));
        }

        if let (Some(min), Some(max)) = (attr.min, attr.max) {
            if min > max {
                f.push(format!("`{name}` declares min {min} above max {max}"));
            }
        }

        if attr.category.is_none() {
            if let Some(expected) = AttributeCategory::for_name(name) {
                f.push(format!(
                    "`{name}` is housekeeping by its name but declares no category; \
                     it ranks beside the reading the device is for. \
                     Ask `AttributeCategory::for_name` rather than repeating the list \
                     (expected {expected:?})"
                ));
            }
        }
    }

    for action in &schema.actions {
        if action.label.trim().is_empty() {
            f.push(format!("action `{}` has no label", action.id));
        }
        match &action.sentence {
            None => f.push(format!(
                "action `{}` has no sentence; a client that cannot phrase a payload \
                 shows the user raw JSON in their rule list",
                action.id
            )),
            Some(s) if !s.contains("{device}") => f.push(format!(
                "action `{}` has a sentence that never names the device: {s:?}",
                action.id
            )),
            _ => {}
        }
        if let Some(writes) = &action.writes {
            if !schema.attributes.contains_key(writes) {
                f.push(format!(
                    "action `{}` claims to write `{writes}`, which the schema does not declare",
                    action.id
                ));
            }
        }
    }

    f
}

/// The same checks against the JSON form [`with_actions`] produces.
///
/// [`with_actions`]: crate::device_actions::with_actions
pub fn check_device_schema_json(schema: &Value) -> Findings {
    match serde_json::from_value::<DeviceSchema>(schema.clone()) {
        Ok(parsed) => check_device_schema(&parsed),
        Err(e) => {
            let mut f = Findings::default();
            f.push(format!("schema does not parse as a DeviceSchema: {e}"));
            f
        }
    }
}

/// Check that every attribute the schema declares writable is one the command
/// path really takes.
///
/// `accepts` is the plugin's own dispatcher, asked one attribute at a time —
/// usually a closure that builds a command payload and reports whether it
/// produced anything. **A declared control the plugin ignores is worse than no
/// control**: the user discovers it by using it.
pub fn check_writables(schema: &DeviceSchema, accepts: impl Fn(&str) -> bool) -> Findings {
    let mut f = Findings::default();
    for (name, attr) in &schema.attributes {
        if attr.writable && !accepts(name) {
            f.push(format!(
                "`{name}` is declared writable but the command path ignores it"
            ));
        }
    }
    f
}

/// Check a plugin's capability manifest.
///
/// - **Every action declares a timeout.** Core's default window is 5 s, which
///   504s anything that talks to hardware. Declare one even when the default
///   would do, so the choice is visible rather than inherited.
/// - **Every action has an id and a label**, because the label is what a person
///   presses.
/// - **`item_key` is required when `item_operations` is set**, per the manifest
///   spec — without it a streaming action's items cannot be deduplicated.
pub fn check_manifest(caps: &Capabilities) -> Findings {
    let mut f = Findings::default();

    for action in &caps.actions {
        if action.id.trim().is_empty() {
            f.push("an action has an empty id".to_string());
        }
        if action.label.trim().is_empty() {
            f.push(format!("action `{}` has no label", action.id));
        }
        if action.timeout_ms.is_none() {
            f.push(format!(
                "action `{}` declares no timeout_ms; core's default window is 5000ms, \
                 which fails anything that talks to hardware",
                action.id
            ));
        }
        if action.item_operations.is_some() && action.item_key.is_none() {
            f.push(format!(
                "action `{}` declares item_operations without an item_key",
                action.id
            ));
        }
    }

    f
}

/// Check that the manifest and the handler agree, in both directions.
///
/// `routed` is the list of ids the plugin's command handler matches. An action
/// advertised but not routed is a button that does nothing; one routed but not
/// advertised is a capability nobody can find.
pub fn check_actions_routed(caps: &Capabilities, routed: &[&str]) -> Findings {
    let mut f = Findings::default();

    for action in &caps.actions {
        if !routed.contains(&action.id.as_str()) {
            f.push(format!(
                "action `{}` is advertised but not routed; it is a button that does nothing",
                action.id
            ));
        }
    }
    for id in routed {
        if !caps.actions.iter().any(|a| a.id == *id) {
            f.push(format!(
                "action `{id}` is routed but not advertised; nobody can find it"
            ));
        }
    }

    f
}

/// Every check that needs nothing but a schema and a manifest.
pub fn check_all(schema: &DeviceSchema, caps: &Capabilities) -> Findings {
    let mut f = check_device_schema(schema);
    f.merge(check_manifest(caps));
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::schema::{AttributeSchema, BoolStates, DeviceAction, StateLabel};
    use std::collections::HashMap;

    fn schema(attrs: Vec<(&str, AttributeSchema)>) -> DeviceSchema {
        DeviceSchema {
            attributes: attrs
                .into_iter()
                .map(|(n, a)| (n.to_string(), a))
                .collect::<HashMap<_, _>>(),
            ..Default::default()
        }
    }

    fn action_json(v: serde_json::Value) -> DeviceAction {
        serde_json::from_value(v).expect("a DeviceAction")
    }

    fn manifest_action(v: serde_json::Value) -> crate::types::Action {
        serde_json::from_value(v).expect("a manifest Action")
    }

    fn named_bool() -> AttributeSchema {
        AttributeSchema {
            states: Some(BoolStates {
                when_true: StateLabel::verbed("on", "turns on"),
                when_false: StateLabel::verbed("off", "turns off"),
            }),
            ..AttributeSchema::new(AttributeKind::Bool)
        }
    }

    /// The invariant nine plugins had each written by hand.
    #[test]
    fn a_boolean_that_names_one_state_is_caught() {
        let bare = AttributeSchema::new(AttributeKind::Bool);
        let f = check_device_schema(&schema(vec![("on", bare)]));
        assert_eq!(f.iter().count(), 1);
        assert!(f.iter().next().unwrap().contains("but Not"));

        check_device_schema(&schema(vec![("on", named_bool())])).assert_ok();
    }

    #[test]
    fn housekeeping_declared_as_a_reading_is_caught() {
        let battery = AttributeSchema::read_only(AttributeKind::Integer);
        let f = check_device_schema(&schema(vec![("battery", battery)]));
        assert_eq!(f.iter().count(), 1);
        assert!(f.iter().next().unwrap().contains("housekeeping"));
    }

    #[test]
    fn a_dotted_name_and_an_empty_enum_are_both_caught() {
        let f = check_device_schema(&schema(vec![
            (
                "led.count",
                AttributeSchema::read_only(AttributeKind::Integer),
            ),
            ("speed", AttributeSchema::new(AttributeKind::Enum)),
        ]));
        assert_eq!(f.iter().count(), 2);
    }

    /// A declared control the plugin ignores is worse than no control: the
    /// user discovers it by using it.
    #[test]
    fn a_writable_the_command_path_ignores_is_caught() {
        let s = schema(vec![("on", named_bool())]);
        check_writables(&s, |_| true).assert_ok();
        let f = check_writables(&s, |_| false);
        assert_eq!(f.iter().count(), 1);
    }

    #[test]
    fn an_action_that_cannot_be_phrased_is_caught() {
        let mut s = schema(vec![]);
        s.actions = vec![action_json(serde_json::json!({
            "id": "activate", "label": "Activate"
        }))];
        let f = check_device_schema(&s);
        assert!(f.iter().next().unwrap().contains("raw JSON"));

        s.actions[0].sentence = Some("activate it".into());
        assert!(check_device_schema(&s)
            .iter()
            .next()
            .unwrap()
            .contains("never names the device"));

        s.actions[0].sentence = Some("activate {device}".into());
        check_device_schema(&s).assert_ok();
    }

    #[test]
    fn an_action_writing_something_undeclared_is_caught() {
        let mut s = schema(vec![("on", named_bool())]);
        s.actions = vec![action_json(serde_json::json!({
            "id": "set_source",
            "label": "Set source",
            "sentence": "set the source of {device}",
            "writes": "source",
        }))];
        assert!(check_device_schema(&s)
            .iter()
            .next()
            .unwrap()
            .contains("does not declare"));
    }

    #[test]
    fn an_action_with_no_timeout_is_caught() {
        let caps = Capabilities {
            spec: "1".into(),
            plugin_id: "plugin.test".into(),
            actions: vec![manifest_action(serde_json::json!({
                "id": "rescan", "label": "Rescan"
            }))],
        };
        let f = check_manifest(&caps);
        assert!(f.iter().next().unwrap().contains("5000ms"));
    }

    /// Advertised-but-not-routed is a button that does nothing; the reverse is
    /// a capability nobody can find.
    #[test]
    fn the_manifest_and_the_handler_must_agree_both_ways() {
        let caps = Capabilities {
            spec: "1".into(),
            plugin_id: "plugin.test".into(),
            actions: vec![manifest_action(serde_json::json!({
                "id": "rescan", "label": "Rescan", "timeout_ms": 10000
            }))],
        };
        check_actions_routed(&caps, &["rescan"]).assert_ok();
        assert!(check_actions_routed(&caps, &[])
            .iter()
            .next()
            .unwrap()
            .contains("does nothing"));
        assert!(check_actions_routed(&caps, &["rescan", "reboot"])
            .iter()
            .next()
            .unwrap()
            .contains("nobody can find"));
    }
}
