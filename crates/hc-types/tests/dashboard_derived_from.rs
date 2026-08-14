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

use hc_types::dashboard::{
    DashboardBreakpoint, DashboardFlow, DashboardLayout, DashboardWidgetPlacement,
};

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
            rect: None,
        }],
        derived_from,
        flow: DashboardFlow::default(),
        frame: None,
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

// ---------------------------------------------------------------------------
// flow
// ---------------------------------------------------------------------------

/// A gap is content, and the document has to be able to say so.
///
/// `hc_types` structs have no `deny_unknown_fields`, so a field core does not
/// know about is dropped silently on the way through — which is how a client
/// can send `flow: free`, get a 200, and find every gap closed on reload with
/// nothing anywhere saying why. Same reasoning as `derived_from` above.
#[test]
fn flow_survives_a_round_trip() {
    let mut l = layout(None);
    l.flow = DashboardFlow::Free;
    let json = serde_json::to_string(&l).unwrap();
    let after: DashboardLayout = serde_json::from_str(&json).unwrap();
    assert_eq!(after.flow, DashboardFlow::Free);
}

#[test]
fn flow_is_snake_case_on_the_wire() {
    let mut l = layout(None);
    l.flow = DashboardFlow::Free;
    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
    assert_eq!(json["flow"], "free");
}

/// Every dashboard already in redb predates this field.
///
/// Absence must read as `Packed`: a gap could not be expressed at all before
/// `flow`, so that is not a guess about what those documents meant, it is the
/// only thing they can have meant. Erring the other way would let a client stop
/// packing a layout that was authored expecting it.
#[test]
fn a_layout_without_flow_is_packed() {
    let json = r#"{
        "breakpoint": "desktop",
        "columns": 12,
        "row_height": 120.0,
        "gap": 12.0,
        "placements": []
    }"#;
    let parsed: DashboardLayout = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.flow, DashboardFlow::Packed);
}
