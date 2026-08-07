//! `derived_from` has to survive a round trip, and its absence has to mean
//! *authored*.
//!
//! Core does not derive layouts and has no opinion about which breakpoint is
//! primary — it stores this so a client can tell which of the other breakpoints
//! are its to recompute and which a person has taken over. That makes the whole
//! value of the field its persistence, so persistence is what is pinned here.
//!
//! The failure this guards against is silent in both directions. Without
//! `#[serde(default)]` an existing dashboard stops deserializing; without the
//! field at all a client's `derived_from` is dropped on the way through and
//! every layout reads back authored, which looks exactly like the client
//! failing to save.

use hc_types::dashboard::{DashboardBreakpoint, DashboardLayout, DashboardWidgetPlacement};

fn layout(derived_from: Option<DashboardBreakpoint>) -> DashboardLayout {
    DashboardLayout {
        breakpoint: DashboardBreakpoint::Mobile,
        columns: 4,
        row_height: 100.0,
        gap: 8.0,
        placements: vec![DashboardWidgetPlacement {
            widget_id: "a".into(),
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        }],
        derived_from,
    }
}

#[test]
fn a_derived_layout_survives_a_round_trip() {
    let before = layout(Some(DashboardBreakpoint::Desktop));
    let json = serde_json::to_string(&before).unwrap();
    let after: DashboardLayout = serde_json::from_str(&json).unwrap();
    assert_eq!(before, after);
    assert_eq!(after.derived_from, Some(DashboardBreakpoint::Desktop));
}

#[test]
fn the_wire_name_is_snake_case_and_matches_the_breakpoint_names() {
    let json = serde_json::to_value(layout(Some(DashboardBreakpoint::Tv))).unwrap();
    assert_eq!(json["derived_from"], "tv");
}

#[test]
fn a_layout_stored_before_this_field_existed_still_loads() {
    // Byte-for-byte what redb already holds for every dashboard on every box.
    let stored = r#"{
        "breakpoint": "desktop",
        "columns": 12,
        "row_height": 120.0,
        "gap": 12.0,
        "placements": [{"widget_id": "a", "x": 0, "y": 0, "w": 4, "h": 3}]
    }"#;
    let parsed: DashboardLayout = serde_json::from_str(stored).unwrap();
    assert_eq!(
        parsed.derived_from, None,
        "absent must read as authored — the interpretation that makes an \
         editor leave the layout alone"
    );
}

#[test]
fn an_authored_layout_does_not_write_the_key_at_all() {
    // Not merely `null`: omitted. Every dashboard in the wild is authored, so
    // emitting a null for each of four layouts on every GET would be noise in
    // the payload and in every future diff of a stored document.
    let json = serde_json::to_value(layout(None)).unwrap();
    assert!(
        json.get("derived_from").is_none(),
        "expected the key to be omitted, got {json}"
    );
}
