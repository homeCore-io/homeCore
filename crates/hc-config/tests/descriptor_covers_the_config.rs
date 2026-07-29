//! The rule that keeps the Configuration screen honest.
//!
//! A descriptor is authoritative: the client renders it instead of guessing a
//! form from the schema, so **any key the descriptor omits becomes uneditable**
//! — silently, with no error anywhere. That is a UX regression you find months
//! later when somebody asks why they cannot change a port.
//!
//! Same rule, same helper, that every Rust plugin already runs against its own
//! descriptor.

#![cfg(all(feature = "schema", feature = "descriptor"))]

use hc_config::descriptor::{system_config_descriptor, JUSTIFIED_OMISSIONS};
use hc_config::AppConfig;
use hc_types::config_descriptor::missing_schema_coverage;

fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(AppConfig)).unwrap()
}

#[test]
fn descriptor_covers_every_config_field() {
    let missing =
        missing_schema_coverage(&schema(), &system_config_descriptor(), JUSTIFIED_OMISSIONS);

    assert!(
        missing.is_empty(),
        "{} config keys are in homecore.toml but not in the descriptor, so \
         nothing can edit them:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn every_justified_omission_is_a_real_key() {
    // A justification that no longer names a real field is worse than none: it
    // silently widens the exemption list as the config changes underneath it.
    let described = system_config_descriptor();
    let all_keys = missing_schema_coverage(&schema(), &serde_json::json!({}), &[]);

    for omitted in JUSTIFIED_OMISSIONS {
        assert!(
            all_keys.iter().any(|k| k == omitted),
            "{omitted} is justified as an omission but is not a key of \
             homecore.toml — stale justification"
        );
    }

    // And nothing is both described and justified.
    let text = serde_json::to_string(&described).unwrap();
    for omitted in JUSTIFIED_OMISSIONS {
        assert!(
            !text.contains(&format!("\"key\":\"{omitted}\"")),
            "{omitted} is both described and listed as an omission"
        );
    }
}

#[test]
fn the_descriptor_is_shaped_the_way_a_client_expects() {
    let d = system_config_descriptor();

    assert_eq!(d["plugin_id"], "homecore");
    assert_eq!(d["descriptor_version"], 1);

    let sections = d["sections"].as_array().expect("sections is an array");
    assert!(sections.len() >= 15, "one section per area of the file");

    // Every section has an id, a title, and at least one field — an empty
    // section renders as a heading over nothing.
    for s in sections {
        let id = s["id"].as_str().unwrap_or_default();
        assert!(!id.is_empty(), "section without an id: {s}");
        assert!(
            !s["title"].as_str().unwrap_or_default().is_empty(),
            "section {id} has no title"
        );
        assert!(
            !s["fields"].as_array().map(Vec::is_empty).unwrap_or(true),
            "section {id} has no fields"
        );
    }
}

#[test]
fn secrets_are_marked_so_a_client_can_mask_them() {
    // `PUT /system/config` writes what it is given. A field holding a password
    // or a token has to be declared secret, or the browser shows it in clear
    // and the operator pastes it into a screenshot.
    let d = system_config_descriptor();
    let text = serde_json::to_string(&d).unwrap();

    for key in ["influx.token", "password"] {
        let pos = text
            .find(&format!("\"key\":\"{key}\""))
            .unwrap_or_else(|| panic!("{key} should be described"));
        let window = &text[pos..(pos + 220).min(text.len())];
        assert!(
            window.contains("\"secret\":true") || window.contains("\"kind\":\"secret\""),
            "{key} is not marked secret: {window}"
        );
    }
}
