//! The vocabulary must come from the TYPES, not from a list someone maintains.
//!
//! Run with:  cargo test -p hc-types --features schema

#![cfg(feature = "schema")]

use hc_types::vocabulary::Vocabulary;

#[test]
fn it_finds_every_variant_the_rule_types_declare() {
    let v = Vocabulary::derive();

    // These numbers are NOT the contract — the types are. They are here so that
    // a change to the vocabulary is impossible to make silently: add a Trigger
    // variant and this fails, which is precisely what a hand-written mirror in a
    // client can never do for you.
    assert_eq!(v.triggers.len(), 18, "triggers: {:?}", v.tags()["triggers"]);
    assert_eq!(
        v.conditions.len(),
        13,
        "conditions: {:?}",
        v.tags()["conditions"]
    );
    assert_eq!(v.actions.len(), 34, "actions: {:?}", v.tags()["actions"]);
}

#[test]
fn a_unit_variant_has_no_fields_and_a_struct_variant_does() {
    let v = Vocabulary::derive();

    let manual = v
        .triggers
        .iter()
        .find(|t| t.tag == "ManualTrigger")
        .expect("ManualTrigger");
    assert!(
        manual.fields.is_empty(),
        "a unit variant is a bare string on the wire, not an object"
    );

    let changed = v
        .triggers
        .iter()
        .find(|t| t.tag == "DeviceStateChanged")
        .expect("DeviceStateChanged");

    let names: Vec<&str> = changed.fields.iter().map(|f| f.name.as_str()).collect();
    // Including the one the client used to hide: `device_ids` is why a rule
    // watching four doors displayed only one.
    assert!(names.contains(&"device_id"), "{names:?}");
    assert!(names.contains(&"device_ids"), "{names:?}");
    assert!(names.contains(&"attribute"), "{names:?}");
    assert!(names.contains(&"to"), "{names:?}");

    let device_id = changed
        .fields
        .iter()
        .find(|f| f.name == "device_id")
        .unwrap();
    assert_eq!(device_id.r#type, "string");
    assert!(device_id.required, "device_id has no serde default");

    let attribute = changed
        .fields
        .iter()
        .find(|f| f.name == "attribute")
        .unwrap();
    assert!(!attribute.required, "attribute is an Option with a default");
}

#[test]
fn it_serialises_to_something_a_client_can_check_itself_against() {
    let v = Vocabulary::derive();
    let json = serde_json::to_value(&v).unwrap();

    assert!(json["triggers"].is_array());
    assert!(json["conditions"].is_array());
    assert!(json["actions"].is_array());

    let back: Vocabulary = serde_json::from_value(json).unwrap();
    assert_eq!(back, v);
}
