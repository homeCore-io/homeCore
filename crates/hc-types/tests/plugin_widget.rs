//! The widget `type` is a string, and an unknown one must survive untouched.
//!
//! This used to be a 15-variant `Copy` enum that every client mirrored by hand,
//! and the mirror cracked: core grew `HouseStatusHero`, shipped it on its own
//! default dashboard, and the Dart client's enum never learned about it — so the
//! client coerced the unknown type to `markdown` and would have SAVED it back as
//! one, destroying the card.
//!
//! The property that replaces the enum is the one worth testing: a dashboard
//! authored against a newer core — or by a plugin nobody here has heard of —
//! round-trips through this type without being coerced, renamed, or dropped.

use hc_types::dashboard::{DashboardWidget, DashboardWidgetPlacement};

#[test]
fn a_widget_type_is_a_string_on_the_wire() {
    let w = DashboardWidget {
        id: "cam".into(),
        r#type: "plugin_widget".into(),
        title: "Driveway".into(),
        subtitle: None,
        config: serde_json::json!({"plugin_id": "plugin.go2rtc", "widget_id": "camera"}),
    };

    let json = serde_json::to_value(&w).unwrap();
    assert_eq!(json["type"], "plugin_widget");

    let back: DashboardWidget = serde_json::from_value(json).unwrap();
    assert_eq!(back, w);
}

#[test]
fn a_type_core_has_never_heard_of_round_trips_untouched() {
    // The card an older core must not eat.
    let raw = serde_json::json!({
        "id": "w1",
        "type": "quantum_flux_gauge",
        "title": "Flux",
        "config": {"nested": {"anything": [1, 2, 3]}}
    });

    let w: DashboardWidget = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(w.r#type, "quantum_flux_gauge");

    // Not coerced to markdown, not dropped, and the opaque config is intact.
    let out = serde_json::to_value(&w).unwrap();
    assert_eq!(out["type"], "quantum_flux_gauge");
    assert_eq!(out["config"], raw["config"]);
}

#[test]
fn a_placement_carries_only_geometry() {
    // `section_id` is gone: sections were a second layout axis competing with the
    // x/y/w/h that placements already carry. A document written by the old core
    // still loads — serde ignores the field — it simply stops being settable.
    let raw = serde_json::json!({
        "widget_id": "w1", "x": 0, "y": 2, "w": 6, "h": 3,
        "section_id": "desktop-overview"
    });

    let p: DashboardWidgetPlacement = serde_json::from_value(raw).unwrap();
    assert_eq!(p.widget_id, "w1");
    assert_eq!((p.x, p.y, p.w, p.h), (0, 2, 6, 3));

    let out = serde_json::to_value(&p).unwrap();
    assert!(out.get("section_id").is_none());
}

#[test]
fn a_dashboard_stored_by_the_old_core_still_loads() {
    // The migration, such as it is: serde ignores unknown fields, so every
    // dashboard already sitting in redb keeps working. It simply drops the
    // fields nothing was reading.
    let old = serde_json::json!({
        "id": "d1",
        "name": "Getting Started",
        "owner_user_id": "u1",
        "icon": "home",
        "visibility": "private",
        "sections": [{
            "id": "desktop-overview", "breakpoint": "desktop", "title": "Overview",
            "order": 0, "y": 0, "layout_policy": "grid", "min_h": 4, "hidden": false
        }],
        "layouts": [{
            "breakpoint": "desktop", "columns": 12, "row_height": 150.0, "gap": 12.0,
            "placements": [
                {"widget_id": "w1", "x": 0, "y": 0, "w": 6, "h": 2,
                 "section_id": "desktop-overview"}
            ]
        }],
        "widgets": [{
            "id": "w1", "type": "house_status_hero", "title": "House",
            "refresh_policy": "live", "config": {}
        }]
    });

    let d: hc_types::dashboard::DashboardDefinition = serde_json::from_value(old).unwrap();

    assert_eq!(d.name, "Getting Started");
    assert_eq!(d.widgets.len(), 1);
    assert_eq!(d.widgets[0].r#type, "house_status_hero");
    assert_eq!(d.layouts[0].placements[0].w, 6);
}
