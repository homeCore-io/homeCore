//! The composition frame has to survive a round trip, and its absence has to
//! leave every dashboard already in redb byte-identical.
//!
//! `frame` and `rect` are the first fields that describe where something sits
//! in units other than cells. Core does not lay anything out and does not act
//! on either — it stores them for the reason it stores `flow`: where a person
//! put something is a property of the document, and two clients that disagreed
//! about it would draw the same page differently.
//!
//! The failure this guards against is the one that makes a design tool
//! unusable without erroring: a client composes a page, saves it, and the
//! rectangles are dropped on the way through — so the page reads back as the
//! snapped grid approximation and every fractional position the person set is
//! silently lost. It looks exactly like the client failing to save.

use hc_types::dashboard::{
    DashboardBreakpoint, DashboardFlow, DashboardFrame, DashboardFrameFit, DashboardLayout,
    DashboardRect, DashboardWidgetPlacement,
};

fn placement(rect: Option<DashboardRect>) -> DashboardWidgetPlacement {
    DashboardWidgetPlacement {
        widget_id: "a".into(),
        x: 1,
        y: 2,
        w: 3,
        h: 2,
        rect,
    }
}

fn layout(frame: Option<DashboardFrame>, rect: Option<DashboardRect>) -> DashboardLayout {
    DashboardLayout {
        breakpoint: DashboardBreakpoint::Desktop,
        columns: 12,
        row_height: 120.0,
        gap: 12.0,
        placements: vec![placement(rect)],
        derived_from: None,
        flow: DashboardFlow::Free,
        frame,
        groups: Vec::new(),
    }
}

#[test]
fn a_composed_layout_survives_a_round_trip() {
    let before = layout(
        Some(DashboardFrame {
            width: 1600.0,
            height: 900.0,
            fit: DashboardFrameFit::Fixed,
        }),
        Some(DashboardRect {
            x: 134.5,
            y: 66.25,
            w: 420.0,
            h: 260.5,
        }),
    );
    let json = serde_json::to_string(&before).expect("serialise");
    let after: DashboardLayout = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(before, after);
}

#[test]
fn the_fractions_are_not_rounded_on_the_way_through() {
    // The whole point of the field. Rounding here would look like a client that
    // cannot place anything off a cell boundary.
    let before = layout(
        Some(DashboardFrame {
            width: 1600.0,
            height: 900.0,
            fit: DashboardFrameFit::Scroll,
        }),
        Some(DashboardRect {
            x: 0.5,
            y: 0.25,
            w: 1.125,
            h: 2.75,
        }),
    );
    let json = serde_json::to_string(&before).expect("serialise");
    let after: DashboardLayout = serde_json::from_str(&json).expect("deserialise");
    let rect = after.placements[0].rect.expect("rect kept");
    assert_eq!(rect.x, 0.5);
    assert_eq!(rect.y, 0.25);
    assert_eq!(rect.w, 1.125);
    assert_eq!(rect.h, 2.75);
}

#[test]
fn a_dashboard_that_predates_frames_still_reads() {
    // Exactly the shape already in redb: no `frame`, no `rect`.
    let json = r#"{
        "breakpoint": "desktop",
        "columns": 12,
        "row_height": 120.0,
        "gap": 12.0,
        "placements": [{"widget_id": "a", "x": 1, "y": 2, "w": 3, "h": 2}]
    }"#;
    let layout: DashboardLayout = serde_json::from_str(json).expect("deserialise");
    assert!(layout.frame.is_none());
    assert!(layout.placements[0].rect.is_none());
    assert_eq!(layout.flow, DashboardFlow::Packed);
}

#[test]
fn an_uncomposed_layout_writes_exactly_what_it_did_before() {
    // `skip_serializing_if` on both, so a page nobody has composed does not
    // grow two null fields the moment this version touches it. A document that
    // gains keys by being read is a document whose diffs stop meaning anything.
    let json = serde_json::to_string(&layout(None, None)).expect("serialise");
    assert!(!json.contains("frame"), "{json}");
    assert!(!json.contains("rect"), "{json}");
}

#[test]
fn the_cells_are_kept_beside_the_rectangle() {
    // The safety property the whole design rests on: a composed placement still
    // carries a legal whole-cell approximation, so core's validation still has
    // something to check and a client that predates frames still draws
    // something approximately right rather than nothing at all.
    let composed = placement(Some(DashboardRect {
        x: 134.5,
        y: 66.25,
        w: 420.0,
        h: 260.5,
    }));
    assert_eq!(composed.x, 1);
    assert_eq!(composed.w, 3);
}

#[test]
fn scroll_is_what_absence_means() {
    // Every dashboard authored before this grew downward past its height, so
    // reading a missing `fit` as anything else would change pages nobody
    // touched.
    let json = r#"{"width": 1600.0, "height": 900.0}"#;
    let frame: DashboardFrame = serde_json::from_str(json).expect("deserialise");
    assert_eq!(frame.fit, DashboardFrameFit::Scroll);
}
