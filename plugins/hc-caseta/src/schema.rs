//! What a Pico reports, declared so a client can name its buttons.
//!
//! A Caséta Pico fires button events all day and nothing ever said which
//! buttons it has. The integration report has always listed them — `import.rs`
//! used their *existence* to recognise a Pico and then dropped the numbers —
//! so a rule editor asking "which button?" had nothing to offer.
//!
//! No actions are declared. A Pico over LIP is genuinely read-only
//! (`translate_command` produces nothing for it), and a declared action would
//! be a control that does nothing.

use std::collections::HashMap;

use plugin_sdk_rs::device_actions::with_actions;
use plugin_sdk_rs::types::schema::{AttributeKind, AttributeSchema, DeviceSchema};
use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind};

fn ro(kind: AttributeKind, display: &str) -> AttributeSchema {
    AttributeSchema {
        kind,
        writable: false,
        display_name: Some(display.to_string()),
        ..Default::default()
    }
}

/// The schema for a Pico, or `None` for anything else.
pub fn device_schema_json(cfg: &DeviceConfig) -> Option<Value> {
    // `kind` is optional: a row whose kind is unset is skipped at startup
    // rather than taking the plugin offline, so it may be None here.
    if cfg.kind != Some(DeviceKind::Pico) || cfg.buttons.is_empty() {
        return None;
    }

    let mut attrs: HashMap<String, AttributeSchema> = HashMap::new();
    attrs.insert(
        "available_buttons".into(),
        ro(AttributeKind::Json, "Buttons"),
    );
    for b in &cfg.buttons {
        attrs.insert(
            format!("button_{b}"),
            ro(AttributeKind::String, &format!("Button {b}")),
        );
    }

    Some(with_actions(
        &DeviceSchema {
            attributes: attrs,
            ..Default::default()
        },
        vec![],
    ))
}

/// The button catalogue a Pico publishes about itself.
pub fn button_catalogue(cfg: &DeviceConfig) -> Option<Value> {
    // `kind` is optional: a row whose kind is unset is skipped at startup
    // rather than taking the plugin offline, so it may be None here.
    if cfg.kind != Some(DeviceKind::Pico) || cfg.buttons.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "available_buttons": cfg.buttons }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pico(buttons: Vec<u32>) -> DeviceConfig {
        DeviceConfig {
            integration_id: 5,
            name: "Kitchen Pico".into(),
            kind: Some(DeviceKind::Pico),
            area: None,
            fade_secs: None,
            invert_position: false,
            buttons,
        }
    }

    #[test]
    fn a_pico_publishes_its_buttons_and_no_actions() {
        let v = device_schema_json(&pico(vec![2, 3, 4, 5, 6])).unwrap();
        assert!(v["attributes"]["available_buttons"].is_object());
        assert!(v["attributes"]["button_4"].is_object());
        assert!(
            v.get("actions").is_none(),
            "a Caséta Pico is read-only over LIP"
        );
    }

    /// A config imported before the numbers were carried through publishes
    /// nothing rather than an empty list that looks like "no buttons".
    #[test]
    fn no_buttons_means_no_schema() {
        assert!(device_schema_json(&pico(vec![])).is_none());
        assert!(button_catalogue(&pico(vec![])).is_none());
    }

    #[test]
    fn only_picos_get_one() {
        let mut d = pico(vec![2]);
        d.kind = Some(DeviceKind::Dimmer);
        assert!(device_schema_json(&d).is_none());
    }

    #[test]
    fn the_catalogue_is_the_button_list() {
        let v = button_catalogue(&pico(vec![2, 4])).unwrap();
        assert_eq!(v["available_buttons"], serde_json::json!([2, 4]));
    }
}
