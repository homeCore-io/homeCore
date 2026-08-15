//! A group with a body.
//!
//! Groups have always been paths written into each widget's own config —
//! `Wall/Lights` — with no registry anywhere. That is deliberate and it is
//! unchanged here: the path is the identity, so nesting is free and an orphaned
//! group cannot exist. `DashboardGroupBox` adds only the things a path cannot
//! say: where the box is and what it looks like.
//!
//! What these tests are really guarding is that the addition is *inert* for
//! every page that does not use it. A group is a selection with a name until
//! somebody styles it, and a document that grows keys, or changes shape, merely
//! by passing through this version of core would make every diff meaningless
//! and every unstyled group suddenly a container.

use hc_types::dashboard::{
    DashboardBackground, DashboardBreakpoint, DashboardFlow, DashboardGroupBox, DashboardLayout,
    DashboardRect, DashboardWidgetPlacement,
};

fn layout(groups: Vec<DashboardGroupBox>) -> DashboardLayout {
    DashboardLayout {
        breakpoint: DashboardBreakpoint::Desktop,
        columns: 12,
        row_height: 120.0,
        gap: 12.0,
        placements: vec![DashboardWidgetPlacement {
            widget_id: "a".into(),
            x: 1,
            y: 2,
            w: 3,
            h: 2,
            rect: None,
        }],
        derived_from: None,
        flow: DashboardFlow::Free,
        frame: None,
        groups,
    }
}

fn box_for(path: &str) -> DashboardGroupBox {
    DashboardGroupBox {
        path: path.into(),
        rect: None,
        padding: 0.0,
        radius: None,
        clip: false,
        background: None,
    }
}

#[test]
fn a_styled_group_survives_a_round_trip() {
    let before = layout(vec![DashboardGroupBox {
        path: "Wall/Lights".into(),
        rect: Some(DashboardRect {
            x: 120.5,
            y: 64.25,
            w: 480.0,
            h: 300.75,
        }),
        padding: 12.0,
        radius: Some(18.0),
        clip: true,
        background: Some(DashboardBackground {
            image: Some("hc-asset://wall.jpg".into()),
            blur: 8.0,
            dim: 0.35,
        }),
    }]);
    let json = serde_json::to_string(&before).expect("serialise");
    let after: DashboardLayout = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(before, after);
}

#[test]
fn a_layout_with_no_styled_groups_writes_exactly_what_it_did_before() {
    // `skip_serializing_if = "Vec::is_empty"`. Every dashboard already in redb
    // must serialise byte-identically after this field exists, or reading a
    // page rewrites it.
    let json = serde_json::to_string(&layout(Vec::new())).expect("serialise");
    assert!(!json.contains("groups"), "{json}");
}

#[test]
fn a_dashboard_that_predates_group_boxes_still_reads() {
    let json = r#"{
        "breakpoint": "desktop",
        "columns": 12,
        "row_height": 120.0,
        "gap": 12.0,
        "placements": []
    }"#;
    let layout: DashboardLayout = serde_json::from_str(json).expect("deserialise");
    assert!(layout.groups.is_empty());
}

#[test]
fn an_unstyled_group_box_stays_a_path_and_nothing_else() {
    // The field defaults matter more than they look. A box with no rect fits
    // its members; one with no radius takes the skin's; one that does not clip
    // cannot hide a card that was visible a moment ago. Any other default would
    // change what an existing group looks like the moment somebody names it.
    let plain = box_for("Wall");
    assert!(plain.rect.is_none(), "a group with no rect fits its members");
    assert_eq!(plain.padding, 0.0);
    assert!(plain.radius.is_none(), "radius is the skin's until asked");
    assert!(!plain.clip, "clipping must be asked for, never inherited");
    assert!(plain.background.is_none());
}

#[test]
fn the_defaults_are_what_absence_means_over_the_wire_too() {
    // Same assertion as above, but through serde rather than the constructor:
    // a client that sends only a path must get the same group core would build.
    let json = r#"{"path": "Wall/Lights"}"#;
    let parsed: DashboardGroupBox = serde_json::from_str(json).expect("deserialise");
    assert_eq!(parsed, box_for("Wall/Lights"));
}

#[test]
fn nesting_needs_no_declaration() {
    // `Wall` and `Wall/Lights` are two boxes with no relationship recorded
    // anywhere — the paths already say it. If this list ever needed a parent
    // pointer, the group model would have grown the registry it was designed
    // to avoid.
    let before = layout(vec![box_for("Wall"), box_for("Wall/Lights")]);
    let json = serde_json::to_string(&before).expect("serialise");
    let after: DashboardLayout = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(after.groups.len(), 2);
    assert_eq!(after.groups[1].path, "Wall/Lights");
}

#[test]
fn a_box_outlives_the_group_it_describes() {
    // Deleting the last card in a group leaves a box whose path nobody claims.
    // That is inert, not invalid: core does not lay out, so it has no way to
    // know the difference, and rejecting it would make deleting a card fail to
    // save. The client garbage-collects when it next writes.
    let before = layout(vec![box_for("Nobody/Here")]);
    let json = serde_json::to_string(&before).expect("serialise");
    let after: DashboardLayout = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(after.groups[0].path, "Nobody/Here");
}
