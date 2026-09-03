//! A template is a dashboard that says it is a starting point.
//!
//! The whole of the template model, on the wire: one boolean. That is a
//! consequence of a decision rather than a shortcut — John: *"template are
//! starting points"*. Making a page from a template copies it and the two have
//! nothing to do with each other afterwards, so there is no instance to track,
//! nothing to re-sync, and no question about who wins when a template changes
//! under a page made from it.
//!
//! What has to be true of one boolean is only this: absence means *page*, and
//! writing one back does not change a document that never had it.

use hc_types::dashboard::DashboardDefinition;

fn parse(json: &str) -> DashboardDefinition {
    serde_json::from_str(json).expect("deserialise")
}

/// The smallest dashboard core will read.
const MINIMAL: &str = r#"{
    "id": "dashboard_1",
    "name": "Kitchen",
    "owner_user_id": "u1",
    "icon": "home"
}"#;

#[test]
fn a_dashboard_that_predates_templates_is_a_page() {
    // Which is every dashboard anybody has ever saved.
    assert!(!parse(MINIMAL).template);
}

#[test]
fn a_page_writes_no_key_at_all() {
    // The rule every optional field here follows: a document must not grow
    // entries by being read, or its diffs stop meaning anything.
    let json = serde_json::to_string(&parse(MINIMAL)).expect("serialise");
    assert!(
        !json.contains("template"),
        "a page should say nothing about templates: {json}"
    );
}

#[test]
fn a_template_says_so_and_survives_the_round_trip() {
    let mut template = parse(MINIMAL);
    template.template = true;

    let json = serde_json::to_string(&template).expect("serialise");
    assert!(json.contains("\"template\":true"), "{json}");
    assert!(parse(&json).template);
}

#[test]
fn everything_else_about_it_is_an_ordinary_dashboard() {
    // The point of the boolean. A template has the same widgets, the same
    // layouts, the same access rules and the same editor — it differs in which
    // list it appears in, and in nothing else. A separate type would have had
    // to reimplement all of that to say one thing.
    let mut template = parse(MINIMAL);
    template.template = true;
    let round_tripped = parse(&serde_json::to_string(&template).expect("serialise"));

    assert_eq!(round_tripped.widgets, template.widgets);
    assert_eq!(round_tripped.layouts, template.layouts);
    assert_eq!(round_tripped.access, template.access);
    assert_eq!(round_tripped.name, "Kitchen");
}

#[test]
fn saying_it_is_not_one_is_the_same_as_not_saying() {
    // Both directions, because they have to agree: `false` reads as a page,
    // and writing that page back drops the key rather than carrying a `false`
    // around forever. A client that states the default explicitly must not
    // leave a trail.
    let stated = parse(
        r#"{
            "id": "dashboard_1",
            "name": "Kitchen",
            "owner_user_id": "u1",
            "icon": "home",
            "template": false
        }"#,
    );
    assert!(!stated.template);
    assert_eq!(
        serde_json::to_string(&stated).expect("serialise"),
        serde_json::to_string(&parse(MINIMAL)).expect("serialise"),
    );
}
